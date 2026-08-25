use async_speed_limit::Limiter;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use futures::stream::TryStreamExt;
use serde::{Deserialize, Serialize};
use sjmcl_types::error::{BGUMCLError, BGUMCLResult};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager, Url};
use tauri_plugin_http::reqwest;
use tauri_plugin_http::reqwest::header::ACCEPT_RANGES;
use tauri_plugin_http::reqwest::header::CONTENT_RANGE;
use tauri_plugin_http::reqwest::header::RANGE;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use tokio_util::bytes;
use tokio_util::compat::FuturesAsyncReadCompatExt;

use crate::launcher_config::commands::retrieve_launcher_config;
use crate::launcher_config::models::{LauncherConfig, ProxyType};
use crate::resource::helpers::curseforge::misc::{
  CURSEFORGE_API_KEY, is_curseforge_authenticated_url,
};
use crate::tasks::streams::desc::{PDesc, PStatus};
use crate::tasks::streams::reporter::Reporter;
use crate::tasks::*;
use crate::utils::fs::validate_sha1;
use std::sync::OnceLock;

mod source;

use crate::tasks::download::source::{FileSourceTracker, SourceFailureKind};

/// Browser-like User-Agent for file downloads. Some acceleration proxies
/// (e.g. gh-proxy) throttle non-browser User-Agents, which made large update
/// downloads extremely slow. Also use a long total timeout so big files
/// (launcher updates / modpacks) are not cut off by the default 10s timeout.
const DOWNLOAD_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

const SEGMENT_DOWNLOAD_RETRIES: usize = 4;
const RECOVERY_DOWNLOAD_ATTEMPTS: usize = 3;
// V2 only opens extra ranges for files large enough to amortize the request
// and assembly overhead.
const SEGMENTED_DOWNLOAD_THRESHOLD: i64 = 8 * 1024 * 1024;
const MAX_DOWNLOAD_SEGMENTS: i64 = 8;
const SOURCE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const HEDGED_REQUEST_DELAY: Duration = Duration::from_millis(2500);
const DOWNLOAD_READ_TIMEOUT: Duration = Duration::from_secs(10);
const LIMITED_DOWNLOAD_READ_TIMEOUT: Duration = Duration::from_secs(60);
const BMCLAPI_REQUEST_INTERVAL: Duration = Duration::from_millis(100);

type SourceTracker = Mutex<FileSourceTracker>;
pub(crate) type RequestGate = Arc<AsyncMutex<Instant>>;

struct TransferReport {
  bytes: i64,
  elapsed: Duration,
}

struct CancelledDownloadCleanup {
  dest_path: PathBuf,
  task_handle: Arc<RwLock<PTaskHandle>>,
  download_group: DownloadGroupStateHandle,
}

impl Drop for CancelledDownloadCleanup {
  fn drop(&mut self) {
    let task_cancelled = self
      .task_handle
      .read()
      .map(|handle| handle.status().is_cancelled())
      .unwrap_or(false);
    if !task_cancelled && !self.download_group.is_cancelled() {
      return;
    }
    let _ = std::fs::remove_file(&self.dest_path);
    let _ = std::fs::remove_dir_all(DownloadTask::segment_directory(&self.dest_path));
  }
}

/// Shared state for the PCL-style progressive download scheduler.
///
/// Source health remains local to each file. This state only contains
/// aggregate transfer information, so a slow or broken source for one file
/// cannot poison another file while the scheduler can still make a global
/// speed decision for the whole task group.
pub struct DownloadGroupState {
  max_connections: usize,
  active_connections: AtomicUsize,
  downloaded_bytes: AtomicI64,
  cancelled: AtomicBool,
  stream_budget: Semaphore,
  target_connections: AtomicUsize,
  speed_state: Mutex<GroupSpeedState>,
}

struct GroupSpeedState {
  last_sample_at: Instant,
  last_sample_bytes: i64,
  bytes_per_second: f64,
  last_growth_at: Instant,
  last_growth_bytes: i64,
  last_growth_speed: f64,
}

pub(crate) struct GroupStreamGuard<'a> {
  group: &'a DownloadGroupState,
}

impl DownloadGroupState {
  pub fn new(max_connections: usize) -> Self {
    let now = Instant::now();
    let max_connections = max_connections.clamp(1, 128);
    // Start conservatively. PCL-style scheduling only grows after a measured
    // throughput improvement; starting dozens of streams on mainland home
    // networks otherwise turns thousands of small files into a queue of
    // throttled connections.
    let initial_connections = max_connections.min(16);
    Self {
      max_connections,
      active_connections: AtomicUsize::new(0),
      downloaded_bytes: AtomicI64::new(0),
      cancelled: AtomicBool::new(false),
      stream_budget: Semaphore::new(initial_connections),
      target_connections: AtomicUsize::new(initial_connections),
      speed_state: Mutex::new(GroupSpeedState {
        last_sample_at: now,
        last_sample_bytes: 0,
        bytes_per_second: 0.0,
        last_growth_at: now,
        last_growth_bytes: 0,
        last_growth_speed: 0.0,
      }),
    }
  }

  pub fn cancel(&self) {
    self.cancelled.store(true, Ordering::Release);
  }

  pub fn is_cancelled(&self) -> bool {
    self.cancelled.load(Ordering::Acquire)
  }

  pub fn enter_stream(&self) -> GroupStreamGuard<'_> {
    self.active_connections.fetch_add(1, Ordering::Relaxed);
    GroupStreamGuard { group: self }
  }

  async fn acquire_stream_slot(&self) -> BGUMCLResult<tokio::sync::SemaphorePermit<'_>> {
    self
      .stream_budget
      .acquire()
      .await
      .map_err(|_| BGUMCLError("Download group stream budget closed".to_string()))
  }

  fn record_bytes(&self, bytes: i64) {
    let total = self
      .downloaded_bytes
      .fetch_add(bytes, Ordering::Relaxed)
      .saturating_add(bytes);
    let now = Instant::now();
    let Ok(mut state) = self.speed_state.lock() else {
      return;
    };
    let growth_elapsed = now.duration_since(state.last_growth_at).as_secs_f64();
    if growth_elapsed >= 2.0 {
      let speed = total.saturating_sub(state.last_growth_bytes).max(0) as f64 / growth_elapsed;
      let target = self.target_connections.load(Ordering::Relaxed);
      let active = self.active_connections.load(Ordering::Relaxed);
      let preserved_throughput =
        state.last_growth_speed == 0.0 || speed >= state.last_growth_speed * 1.1;
      if target < self.max_connections
        && active >= target.saturating_sub(4)
        && speed > 0.0
        && preserved_throughput
      {
        let growth = 8.min(self.max_connections - target);
        self.stream_budget.add_permits(growth);
        self
          .target_connections
          .store(target + growth, Ordering::Relaxed);
        log::info!(
          "Download Engine V2 expanded stream budget from {} to {} at {} B/s",
          target,
          target + growth,
          speed as i64
        );
      }
      state.last_growth_at = now;
      state.last_growth_bytes = total;
      state.last_growth_speed = speed;
    }

    let sample_elapsed = now.duration_since(state.last_sample_at).as_secs_f64();
    if sample_elapsed >= 0.5 {
      let delta = total.saturating_sub(state.last_sample_bytes).max(0) as f64;
      state.bytes_per_second = delta / sample_elapsed;
      state.last_sample_bytes = total;
      state.last_sample_at = now;
      log::debug!(
        "Download Engine V2 speed sample: aggregate_speed={} B/s active_connections={}/{} target={}",
        state.bytes_per_second as i64,
        self.active_connections.load(Ordering::Relaxed),
        self.max_connections,
        self.target_connections.load(Ordering::Relaxed)
      );
    }
  }
}

impl Default for DownloadGroupState {
  fn default() -> Self {
    Self::new(24)
  }
}

impl Drop for GroupStreamGuard<'_> {
  fn drop(&mut self) {
    self
      .group
      .active_connections
      .fetch_sub(1, Ordering::Relaxed);
  }
}

pub(crate) type DownloadGroupStateHandle = Arc<DownloadGroupState>;

fn retry_delay(attempt: usize) -> Duration {
  Duration::from_millis(500u64.saturating_mul(1u64 << attempt.min(3)))
}

fn is_range_reset_error(error: &BGUMCLError) -> bool {
  let message = error.0.to_ascii_lowercase();
  message.contains("416")
    || message.contains("range not satisfiable")
    || message.contains("invalid resumed")
    || message.contains("resumed response starts")
}

pub(crate) fn download_client() -> &'static reqwest::Client {
  static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
  CLIENT.get_or_init(|| {
    reqwest::Client::builder()
      .timeout(Duration::from_secs(600))
      .connect_timeout(Duration::from_secs(10))
      .tcp_keepalive(Duration::from_secs(30))
      .tcp_nodelay(true)
      .pool_idle_timeout(Duration::from_secs(90))
      .pool_max_idle_per_host(128)
      .user_agent(DOWNLOAD_USER_AGENT)
      .build()
      .unwrap_or_else(|_| reqwest::Client::new())
  })
}

fn hedged_request_budget() -> &'static Semaphore {
  static HEDGE_BUDGET: OnceLock<Semaphore> = OnceLock::new();
  HEDGE_BUDGET.get_or_init(|| Semaphore::new(8))
}

/// Whether this URL belongs to CurseForge / Modrinth (Minecraft resource
/// CDNs). Only these downloads should go through the user-configured proxy;
/// everything else (GitHub / Gitee / Mojang, etc.) keeps using the direct /
/// mirrored download path.
fn is_curseforge_or_modrinth_url(url: &url::Url) -> bool {
  matches!(
    url.host_str(),
    Some(
      "api.curseforge.com"
        | "edge.forgecdn.net"
        | "media.forgecdn.net"
        | "mediafilez.forgecdn.net"
        | "edge-service.overwolf.wtf"
        | "media-service.overwolf.wtf"
        | "curseforge.com"
        | "api.modrinth.com"
        | "cdn.modrinth.com"
        | "modrinth.com"
    )
  )
}

/// Build a download client that honors the user-configured proxy, so files
/// from CurseForge / Modrinth CDNs can be downloaded in regions where they are
/// blocked or slow without a proxy.
fn build_download_client_with_proxy(
  proxy_cfg: &crate::launcher_config::models::ProxyConfig,
) -> reqwest::Client {
  let mut builder = reqwest::Client::builder()
    .timeout(Duration::from_secs(600))
    .connect_timeout(Duration::from_secs(10))
    .tcp_keepalive(Duration::from_secs(30))
    .tcp_nodelay(true)
    .pool_idle_timeout(Duration::from_secs(90))
    .pool_max_idle_per_host(128)
    .user_agent(DOWNLOAD_USER_AGENT);
  if proxy_cfg.enabled {
    let proxy_url = match proxy_cfg.selected_type {
      ProxyType::Http => format!("http://{}:{}", proxy_cfg.host, proxy_cfg.port),
      ProxyType::Socks => format!("socks5h://{}:{}", proxy_cfg.host, proxy_cfg.port),
    };
    if let Ok(proxy) = reqwest::Proxy::all(&proxy_url) {
      builder = builder.proxy(proxy);
    }
  }
  builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DownloadParam {
  pub src: Url,
  pub dest: PathBuf,
  pub filename: Option<String>,
  pub sha1: Option<String>,
}

pub struct DownloadTask {
  p_handle: PTaskHandle,
  param: DownloadParam,
  dest_path: PathBuf,
}

impl DownloadTask {
  pub fn new(
    app_handle: AppHandle,
    task_id: u32,
    task_group: Option<String>,
    param: DownloadParam,
    _report_interval: Duration,
  ) -> Self {
    let cache_dir = retrieve_launcher_config(app_handle.clone())
      .unwrap()
      .download
      .cache
      .directory;
    DownloadTask {
      p_handle: PTaskHandle::new(
        PDesc::<PTaskParam>::new(
          task_id,
          task_group.clone(),
          0,
          PTaskParam::Download(param.clone()),
          PStatus::InProgress,
        ),
        Duration::from_secs(1),
        cache_dir.clone().join(format!("task-{task_id}.json")),
        Reporter::new(
          0,
          Duration::from_millis(250),
          TauriEventSink::new(app_handle.clone()),
        ),
      ),
      param: param.clone(),
      dest_path: cache_dir.clone().join(param.dest.clone()),
    }
  }

  pub fn from_descriptor(
    app_handle: AppHandle,
    desc: PTaskDesc,
    _report_interval: Duration,
    reset: bool,
  ) -> Self {
    let param = match &desc.payload {
      PTaskParam::Download(param) => param.clone(),
    };

    let cache_dir = retrieve_launcher_config(app_handle.clone())
      .unwrap()
      .download
      .cache
      .directory;
    let task_id = desc.task_id;
    let path = cache_dir.join(format!("task-{task_id}.json"));
    DownloadTask {
      p_handle: PTaskHandle::new(
        if reset {
          PTaskDesc {
            status: PStatus::Waiting,
            current: 0,
            ..desc
          }
        } else {
          PTaskDesc {
            status: PStatus::Waiting,
            ..desc
          }
        },
        Duration::from_secs(1),
        path,
        Reporter::new(
          desc.total,
          Duration::from_millis(250),
          TauriEventSink::new(app_handle.clone()),
        ),
      ),
      param: param.clone(),
      dest_path: cache_dir.clone().join(param.dest.clone()),
    }
  }

  fn candidate_urls(app_handle: &AppHandle, param: &DownloadParam) -> Vec<String> {
    let is_cf_mr = is_curseforge_or_modrinth_url(&param.src);
    if is_cf_mr {
      // Keep the exact CurseForge order used by PCL: mediafilez first,
      // followed by the normalized official CDN variants. This is an ordered
      // fallback list; candidates are never raced or probed concurrently.
      let source = param.src.as_str();
      let is_curseforge = matches!(
        param.src.host_str(),
        Some(
          "edge.forgecdn.net"
            | "media.forgecdn.net"
            | "mediafilez.forgecdn.net"
            | "edge-service.overwolf.wtf"
            | "media-service.overwolf.wtf"
        )
      );
      let mut official = if is_curseforge {
        vec![
          source
            .replace("-service.overwolf.wtf", ".forgecdn.net")
            .replace("://edge.", "://mediafilez.")
            .replace("://media.", "://mediafilez."),
          source
            .replace("://edge.", "://mediafilez.")
            .replace("://media.", "://mediafilez."),
          source.replace("-service.overwolf.wtf", ".forgecdn.net"),
          source.replace("://media.", "://edge."),
          source.to_string(),
        ]
      } else {
        vec![source.to_string()]
      };
      let mut mirrors = Vec::new();
      for source in &official {
        if let Some(mirror) = crate::utils::web::mcim_mirror_url(source)
          && !mirrors.contains(&mirror)
        {
          mirrors.push(mirror);
        }
      }
      official.dedup();
      // Keep the API for metadata discovery independent from file delivery.
      // CurseForge's official CDN is often slow or intermittently reachable
      // from mainland China, while MCIM is the fast path used by PCL-style
      // downloads. Always try the mirror first for the file itself; retain
      // the official CDN variants as a fallback.
      let mirror_first = true;
      let mut candidates = Vec::new();
      if mirror_first {
        candidates.extend(mirrors);
        candidates.extend(official);
      } else {
        candidates.extend(official);
        candidates.extend(mirrors);
      }
      return candidates;
    }

    if param.src.host_str() == Some("optifine.net") {
      let mut candidates = vec![param.src.as_str().to_string()];
      if let Some(file_name) = param.filename.as_deref() {
        let file_name = file_name.strip_suffix(".jar").unwrap_or(file_name);
        if let Some(rest) = file_name.strip_prefix("OptiFine_")
          && let Some((game_version, patch)) = rest.split_once("_HD_U_")
        {
          let bmcl =
            format!("https://bmclapi2.bangbang93.com/optifine/{game_version}/HD_U/{patch}");
          let mirror_first = app_handle
            .state::<Mutex<LauncherConfig>>()
            .lock()
            .ok()
            .is_some_and(|config| config.download.source.strategy == "mirror");
          if mirror_first {
            candidates.insert(0, bmcl);
          } else {
            candidates.push(bmcl);
          }
        }
      }
      return candidates;
    }

    let stripped = crate::utils::web::strip_gh_proxy_prefix(param.src.as_str());
    if stripped != param.src.as_str()
      || param
        .src
        .host_str()
        .is_some_and(|host| host == "github.com" || host == "raw.githubusercontent.com")
    {
      return crate::utils::web::gh_proxy_candidates(param.src.as_str());
    }

    // PCL gives Forge's Maven files a mainland mirror first because this is
    // the path used by Forge installers and their libraries. Other Maven and
    // Mojang files keep the normal official-first order.
    let mut candidates = crate::utils::web::minecraft_download_candidates(param.src.as_str());
    let is_forge_maven =
      param.src.host_str().is_some_and(|host| {
        host == "files.minecraftforge.net" || host == "maven.minecraftforge.net"
      }) && param.src.path().contains("/net/minecraftforge/");
    if is_forge_maven {
      if let Some(index) = candidates
        .iter()
        .position(|candidate| candidate.contains("bmclapi"))
      {
        candidates.swap(0, index);
      }
    }
    candidates
  }

  /* Legacy source selection retained for the experimental rollback diff. The
  active V2 path below does not compile or execute this implementation.
  async fn send_request(
    app_handle: &AppHandle,
    current: i64,
    range_end: Option<i64>,
    candidate_offset: usize,
    param: &DownloadParam,
    source_health: &SourceHealth,
    preferred_source: &PreferredSource,
    request_gate: &RequestGate,
    download_group: &DownloadGroupState,
  ) -> BGUMCLResult<(reqwest::Response, String)> {
    let is_cf_mr = is_curseforge_or_modrinth_url(&param.src);
    let candidates = Self::candidate_urls(app_handle, param);

    /*
    // Build the list of candidate URLs to try, in order.
    let mut candidates: Vec<String> = Vec::new();
    if is_cf_mr {
      // Keep the same source order as PCL for MODPACK files by default:
      // official CDN variants first, then MCIM. The explicit mirror setting
      // still keeps MCIM first. This matters because a mirror can respond at
      // a few KiB/s without ever producing a transport timeout.
      let mut official_candidates = vec![param.src.as_str().to_string()];
      if let Ok(parsed) = url::Url::parse(param.src.as_str()) {
        if matches!(
          parsed.host_str(),
          Some(
            "edge.forgecdn.net"
              | "media.forgecdn.net"
              | "mediafilez.forgecdn.net"
              | "edge-service.overwolf.wtf"
              | "media-service.overwolf.wtf"
          )
        ) {
          for host in [
            "edge.forgecdn.net",
            "media.forgecdn.net",
            "mediafilez.forgecdn.net",
          ] {
            let mut candidate = parsed.clone();
            let _ = candidate.set_host(Some(host));
            official_candidates.push(candidate.to_string());
          }
          if let Some(host) = parsed.host_str()
            && let Some(prefix) = host.strip_suffix("-service.overwolf.wtf")
          {
            let mut candidate = parsed.clone();
            let _ = candidate.set_host(Some(&format!("{prefix}.forgecdn.net")));
            official_candidates.push(candidate.to_string());
          }
        }
      }
      official_candidates.sort();
      official_candidates.dedup();
      let mut mirrors = Vec::new();
      for official in &official_candidates {
        if let Some(mirror) = crate::utils::web::mcim_mirror_url(official) {
          if !mirrors.contains(&mirror) {
            mirrors.push(mirror);
          }
        }
      }
      let strategy = app_handle
        .state::<Mutex<LauncherConfig>>()
        .lock()
        .ok()
        .map(|config| config.download.source.strategy.clone())
        .unwrap_or_else(|| "auto".to_string());
      if strategy == "mirror" {
        candidates.extend(mirrors);
        candidates.extend(official_candidates);
      } else {
        candidates.extend(official_candidates);
        candidates.extend(mirrors);
      }
    } else if {
      let stripped = crate::utils::web::strip_gh_proxy_prefix(param.src.as_str());
      stripped != param.src.as_str()
        || param.src.host_str().is_some_and(|host| {
          host == "github.com" || host == "raw.githubusercontent.com"
        })
    } {
      // GitHub-related downloads: Gitee mirror -> gh-proxy v4 -> cdn -> direct.
      candidates = crate::utils::web::gh_proxy_candidates(param.src.as_str());
    } else {
      // Mojang metadata, libraries, and assets: BMCLAPI mirrors first, then
      // the official URL as a fallback, matching PCL's source order.
      candidates = crate::utils::web::minecraft_download_candidates(param.src.as_str());
    }
    */

    // Use the user-configured proxy ONLY for the official CurseForge /
    // Modrinth CDNs. The MCIM mirror is directly reachable in mainland China,
    // and everything else (GitHub / Gitee / Mojang) keeps the direct / mirrored
    // path so it is not broken by a proxy that only accelerates Minecraft
    // resource sites.
    let proxy_cfg = if is_cf_mr {
      app_handle
        .state::<Mutex<LauncherConfig>>()
        .lock()
        .ok()
        .map(|c| c.download.proxy.clone())
        .filter(|c| c.enabled)
    } else {
      None
    };
    let plain = download_client().clone();
    let proxy = proxy_cfg
      .as_ref()
      .map(|cfg| build_download_client_with_proxy(cfg))
      .unwrap_or_else(|| plain.clone());
    // Do not wrap a download candidate in the general one-hour retry
    // middleware. PCL-style source failover needs the next candidate to be
    // attempted quickly; per-segment retries still handle transient failures.
    let plain_client = reqwest_middleware::ClientBuilder::new(plain).build();
    let proxy_client = reqwest_middleware::ClientBuilder::new(proxy).build();

    let mut available_candidates: Vec<String> = candidates
      .iter()
      .filter(|candidate| {
        source_health
          .lock()
          .ok()
          .and_then(|health| health.get(*candidate).copied())
          .unwrap_or(0)
          < SOURCE_FAILURE_LIMIT
      })
      .cloned()
      .collect();
    if available_candidates.is_empty() {
      // A temporary outage should not permanently poison a file. Once all
      // candidates have been tried, clear their short-term health state and
      // give the complete source set another round.
      if let Ok(mut health) = source_health.lock() {
        health.clear();
      }
      available_candidates = candidates;
    }

    // Once one source has produced a valid response, keep all segments of
    // this file on that source while it remains healthy. PCL also abandons a
    // source that keeps returning data below its low-speed threshold; this is
    // what prevents a reachable but unusably slow Forge CDN from pinning the
    // rest of an import.
    if let Some(preferred) = preferred_source.lock().ok().and_then(|value| value.clone()) {
      if download_group.source_is_slow(&preferred) {
        if let Ok(mut source) = preferred_source.lock() {
          *source = None;
        }
        log::info!(
          "Download source is below the low-speed threshold; allowing source switch: {preferred}"
        );
      } else if let Some(index) = available_candidates
        .iter()
        .position(|candidate| candidate == &preferred)
      {
        available_candidates.swap(0, index);
      }
    }

    let mut last_err: Option<String> = None;
    let candidate_count = available_candidates.len();
    for attempt in 0..candidate_count {
      let candidate_index = (candidate_offset + attempt) % candidate_count;
      let candidate = &available_candidates[candidate_index];
      let is_mirror = is_cf_mr && candidate.as_str() != param.src.as_str();
      let client = if is_cf_mr {
        if is_mirror {
          &plain_client
        } else {
          &proxy_client
        }
      } else {
        &plain_client
      };
      let url = url::Url::parse(candidate)
        .map_err(|e| BGUMCLError(format!("Invalid url {}: {}", candidate, e)))?;
      let mut request = match range_end {
        Some(end) => client
          .get(url)
          .header(RANGE, format!("bytes={current}-{end}")),
        None if current > 0 => client.get(url).header(RANGE, format!("bytes={current}-")),
        None => client.get(url),
      };
      // A compressed response would make Content-Length and Content-Range
      // unreliable for segmented downloads.
      request = request.header("Accept-Encoding", "identity");

      // add api key header for CurseForge download urls (#1679)
      // ref: https://blog.curseforge.com/introducing-api-key-authentication-for-curseforge-file-downloads
      if is_curseforge_authenticated_url(&param.src) && !CURSEFORGE_API_KEY.is_empty() {
        request = request.header("x-api-key", CURSEFORGE_API_KEY.as_str());
      }

      if candidate.contains("bmclapi") {
        let mut last_request = request_gate.lock().await;
        let elapsed = last_request.elapsed();
        if elapsed < BMCLAPI_REQUEST_INTERVAL {
          tokio::time::sleep(BMCLAPI_REQUEST_INTERVAL - elapsed).await;
        }
        *last_request = Instant::now();
      }

      let request_started = Instant::now();
      match tokio::time::timeout(SOURCE_RESPONSE_TIMEOUT, request.send()).await {
        Ok(Ok(resp)) => match resp.error_for_status() {
          Ok(resp) => {
            // A segmented request must be answered with 206. If a mirror
            // ignores Range, try the next candidate instead of corrupting a
            // partial segment with a complete response body.
            if range_end.is_some() && resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
              if let Ok(mut health) = source_health.lock() {
                let failures = health.entry(candidate.clone()).or_default();
                *failures = failures.saturating_add(1);
              }
              last_err = Some(format!(
                "{} does not support the requested byte range (status {})",
                candidate,
                resp.status()
              ));
              continue;
            }
            if let Ok(mut health) = source_health.lock() {
              health.remove(candidate);
            }
            if let Ok(mut preferred) = preferred_source.lock() {
              *preferred = Some(candidate.clone());
            }
            download_group.record_source_response(candidate, request_started.elapsed());
            log::info!("Download using candidate: {}", candidate);
            return Ok((resp, candidate.clone()));
          }
          Err(e) => {
            if let Ok(mut health) = source_health.lock() {
              let failures = health.entry(candidate.clone()).or_default();
              *failures = failures.saturating_add(1);
            }
            last_err = Some(format!("HTTP request rejected for {candidate}: {e}"));
          }
        },
        Ok(Err(e)) => {
          if let Ok(mut health) = source_health.lock() {
            let failures = health.entry(candidate.clone()).or_default();
            *failures = failures.saturating_add(1);
          }
          last_err = Some(format!("HTTP request failed for {candidate}: {e}"));
        }
        Err(_) => {
          if let Ok(mut health) = source_health.lock() {
            let failures = health.entry(candidate.clone()).or_default();
            *failures = failures.saturating_add(1);
          }
          last_err = Some(format!(
            "{} did not respond within {:?}",
            candidate, SOURCE_RESPONSE_TIMEOUT
          ));
        }
      }
    }
    Err(BGUMCLError(
      last_err.unwrap_or_else(|| "Download request failed".to_string()),
    ))
  }

  */

  async fn request_candidate(
    candidate: String,
    client: reqwest::Client,
    current: i64,
    range_end: Option<i64>,
    request_gate: RequestGate,
  ) -> (
    String,
    Duration,
    Result<reqwest::Response, (SourceFailureKind, String)>,
  ) {
    let started = Instant::now();
    let url = match url::Url::parse(&candidate) {
      Ok(url) => url,
      Err(error) => {
        return (
          candidate.clone(),
          started.elapsed(),
          Err((
            SourceFailureKind::Connection,
            format!("Invalid url {candidate}: {error}"),
          )),
        );
      }
    };
    let authenticated = is_curseforge_authenticated_url(&url);
    let mut request = match range_end {
      Some(end) => client
        .get(url)
        .header(RANGE, format!("bytes={current}-{end}")),
      None if current > 0 => client.get(url).header(RANGE, format!("bytes={current}-")),
      None => client.get(url),
    }
    .header("Accept-Encoding", "identity");
    if authenticated && !CURSEFORGE_API_KEY.is_empty() {
      request = request.header("x-api-key", CURSEFORGE_API_KEY.as_str());
    }
    if candidate.contains("bmclapi") {
      let mut last_request = request_gate.lock().await;
      let elapsed = last_request.elapsed();
      if elapsed < BMCLAPI_REQUEST_INTERVAL {
        tokio::time::sleep(BMCLAPI_REQUEST_INTERVAL - elapsed).await;
      }
      *last_request = Instant::now();
    }

    let result = match tokio::time::timeout(SOURCE_RESPONSE_TIMEOUT, request.send()).await {
      Ok(Ok(response)) => {
        let status = response.status();
        if !status.is_success() {
          let kind = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            SourceFailureKind::RateLimited
          } else {
            SourceFailureKind::Http
          };
          Err((
            kind,
            format!("HTTP request rejected for {candidate}: status {status}"),
          ))
        } else if range_end.is_some() && status != reqwest::StatusCode::PARTIAL_CONTENT {
          Err((
            SourceFailureKind::Range,
            format!(
              "{candidate} ignored byte range {current}-{} (status {status})",
              range_end.unwrap_or_default()
            ),
          ))
        } else {
          Ok(response)
        }
      }
      Ok(Err(error)) => Err((
        SourceFailureKind::Connection,
        format!("HTTP request failed for {candidate}: {error}"),
      )),
      Err(_) => Err((
        SourceFailureKind::Timeout,
        format!("{candidate} did not respond within {SOURCE_RESPONSE_TIMEOUT:?}"),
      )),
    };
    (candidate, started.elapsed(), result)
  }

  fn finish_candidate_attempt(
    source_tracker: &SourceTracker,
    outcome: (
      String,
      Duration,
      Result<reqwest::Response, (SourceFailureKind, String)>,
    ),
  ) -> Result<(reqwest::Response, String), String> {
    let (candidate, latency, result) = outcome;
    match result {
      Ok(response) => {
        if let Ok(mut tracker) = source_tracker.lock() {
          tracker.record_response(&candidate, latency);
        }
        log::info!(
          "Download Engine V2 selected candidate in {} ms: {}",
          latency.as_millis(),
          candidate
        );
        Ok((response, candidate))
      }
      Err((kind, message)) => {
        if let Ok(mut tracker) = source_tracker.lock() {
          tracker.record_failure(&candidate, kind);
        }
        log::warn!("Download candidate failed ({kind:?}): {message}");
        Err(message)
      }
    }
  }

  async fn send_request(
    app_handle: &AppHandle,
    current: i64,
    range_end: Option<i64>,
    candidate_offset: usize,
    source_tracker: &SourceTracker,
    request_gate: &RequestGate,
  ) -> BGUMCLResult<(reqwest::Response, String)> {
    let proxy_cfg = app_handle
      .state::<Mutex<LauncherConfig>>()
      .lock()
      .ok()
      .map(|config| config.download.proxy.clone())
      .filter(|config| config.enabled);
    let plain_client = download_client().clone();
    let proxy_client = proxy_cfg
      .as_ref()
      .map(build_download_client_with_proxy)
      .unwrap_or_else(|| plain_client.clone());
    let candidates = source_tracker
      .lock()
      .map(|tracker| tracker.ordered_candidates(candidate_offset))
      .unwrap_or_default();
    if candidates.is_empty() {
      return Err(BGUMCLError(
        "No download candidate is available".to_string(),
      ));
    }
    let cooldown = source_tracker
      .lock()
      .map(|tracker| tracker.wait_before_retry(&candidates[0]))
      .unwrap_or_default();
    if !cooldown.is_zero() {
      let delay = cooldown.min(Duration::from_secs(5));
      log::debug!(
        "All file candidates are cooling down; delaying next attempt for {:?}",
        delay
      );
      tokio::time::sleep(delay).await;
    }

    let choose_client = |candidate: &str| {
      if proxy_cfg.is_some()
        && url::Url::parse(candidate)
          .ok()
          .as_ref()
          .is_some_and(is_curseforge_or_modrinth_url)
      {
        proxy_client.clone()
      } else {
        plain_client.clone()
      }
    };

    let mut index = 0usize;
    let mut last_error = None;
    while index < candidates.len() {
      let primary_candidate = candidates[index].clone();
      let primary = Self::request_candidate(
        primary_candidate.clone(),
        choose_client(&primary_candidate),
        current,
        range_end,
        request_gate.clone(),
      );
      tokio::pin!(primary);

      if index + 1 >= candidates.len() {
        match Self::finish_candidate_attempt(source_tracker, primary.await) {
          Ok(result) => return Ok(result),
          Err(error) => last_error = Some(error),
        }
        break;
      }

      match tokio::time::timeout(HEDGED_REQUEST_DELAY, &mut primary).await {
        Ok(outcome) => {
          index += 1;
          match Self::finish_candidate_attempt(source_tracker, outcome) {
            Ok(result) => return Ok(result),
            Err(error) => last_error = Some(error),
          }
        }
        Err(_) => {
          let Ok(_hedge_permit) = hedged_request_budget().try_acquire() else {
            index += 1;
            match Self::finish_candidate_attempt(source_tracker, primary.await) {
              Ok(result) => return Ok(result),
              Err(error) => last_error = Some(error),
            }
            continue;
          };
          let secondary_candidate = candidates[index + 1].clone();
          log::info!(
            "Download candidate has not produced headers after {} ms; racing backup {} against {}",
            HEDGED_REQUEST_DELAY.as_millis(),
            secondary_candidate,
            primary_candidate
          );
          let secondary = Self::request_candidate(
            secondary_candidate,
            choose_client(&candidates[index + 1]),
            current,
            range_end,
            request_gate.clone(),
          );
          tokio::pin!(secondary);

          let primary_finished_first;
          let first = tokio::select! {
            outcome = &mut primary => {
              primary_finished_first = true;
              outcome
            }
            outcome = &mut secondary => {
              primary_finished_first = false;
              outcome
            }
          };
          match Self::finish_candidate_attempt(source_tracker, first) {
            Ok(result) => return Ok(result),
            Err(error) => log::debug!("First raced candidate failed: {error}"),
          }

          let second = if primary_finished_first {
            secondary.await
          } else {
            primary.await
          };
          match Self::finish_candidate_attempt(source_tracker, second) {
            Ok(result) => return Ok(result),
            Err(error) => last_error = Some(error),
          }
          index += 2;
        }
      }
    }

    Err(BGUMCLError(last_error.unwrap_or_else(|| {
      "All download candidates failed".to_string()
    })))
  }

  async fn create_resp_stream(
    app_handle: &AppHandle,
    current: i64,
    candidate_offset: usize,
    source_tracker: &SourceTracker,
    request_gate: &RequestGate,
  ) -> BGUMCLResult<(
    impl Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send + use<>,
    i64,
    bool,
    String,
    bool,
  )> {
    let (resp, active_source) = Self::send_request(
      app_handle,
      current,
      None,
      candidate_offset,
      source_tracker,
      request_gate,
    )
    .await?;
    // If we asked for a range (current > 0) but the server replied with the
    // full 200 OK body instead of 206 Partial Content, the server ignored our
    // Range header. Restart from zero to avoid corrupting the partial file.
    let restart_from_zero = current > 0 && resp.status() != reqwest::StatusCode::PARTIAL_CONTENT;
    let total_progress = if current > 0 && !restart_from_zero {
      let content_range = resp
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| BGUMCLError("Resumed response omitted Content-Range".to_string()))?;
      let (range, total) = content_range
        .strip_prefix("bytes ")
        .and_then(|value| value.split_once('/'))
        .ok_or_else(|| BGUMCLError("Invalid resumed Content-Range".to_string()))?;
      let (start, _end) = range
        .split_once('-')
        .ok_or_else(|| BGUMCLError("Invalid resumed byte range".to_string()))?;
      let start = start
        .parse::<i64>()
        .map_err(|_| BGUMCLError("Invalid resumed range start".to_string()))?;
      let total = total
        .parse::<i64>()
        .map_err(|_| BGUMCLError("Invalid resumed range total".to_string()))?;
      if start != current || total < current {
        return Err(BGUMCLError(format!(
          "Resumed response starts at {start}, expected {current}, total {total}"
        )));
      }
      total - current
    } else {
      resp.content_length().map_or(-1, |length| length as i64)
    };
    let supports_ranges = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT
      || resp
        .headers()
        .get(ACCEPT_RANGES)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("bytes"));
    Ok((
      resp
        .bytes_stream()
        .map_err(|err| std::io::Error::other(format!("download stream failed: {err}"))),
      total_progress,
      restart_from_zero,
      active_source,
      supports_ranges,
    ))
  }

  fn segment_directory(dest_path: &Path) -> PathBuf {
    let file_name = dest_path
      .file_name()
      .and_then(|name| name.to_str())
      .unwrap_or("download");
    dest_path
      .parent()
      .unwrap_or_else(|| Path::new("."))
      .join(format!(".{file_name}.bgumcl-parts"))
  }

  fn segment_count_for_size(total_size: i64) -> i64 {
    let count = if total_size >= 64 * 1024 * 1024 {
      8
    } else if total_size >= 32 * 1024 * 1024 {
      4
    } else if total_size >= SEGMENTED_DOWNLOAD_THRESHOLD {
      2
    } else {
      1
    };
    count.min(MAX_DOWNLOAD_SEGMENTS)
  }

  async fn validate_sha1_async(path: PathBuf, truth: Option<String>) -> BGUMCLResult<()> {
    let Some(truth) = truth else {
      return Ok(());
    };
    tokio::task::spawn_blocking(move || validate_sha1(path, truth))
      .await
      .map_err(|error| BGUMCLError(format!("SHA1 verification task failed: {error}")))?
  }

  async fn download_segment(
    app_handle: &AppHandle,
    part_path: &Path,
    segment_start: i64,
    segment_end: i64,
    source_offset: usize,
    task_handle: &Arc<RwLock<PTaskHandle>>,
    limiter: Option<Limiter>,
    connection_semaphore: &Arc<Semaphore>,
    source_tracker: &SourceTracker,
    request_gate: &RequestGate,
    download_group: &DownloadGroupState,
  ) -> BGUMCLResult<()> {
    let expected_size = segment_end - segment_start + 1;
    tokio::fs::create_dir_all(part_path.parent().unwrap()).await?;

    for attempt in 0..SEGMENT_DOWNLOAD_RETRIES {
      let mut existing = tokio::fs::metadata(part_path)
        .await
        .map(|meta| meta.len() as i64)
        .unwrap_or(0);
      if existing > expected_size {
        tokio::fs::remove_file(part_path).await?;
        existing = 0;
      }
      if existing == expected_size {
        return Ok(());
      }

      let request_start = segment_start + existing;
      let _group_slot = download_group.acquire_stream_slot().await?;
      let _connection_permit = connection_semaphore
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| BGUMCLError("Download connection pool closed".to_string()))?;
      let _stream_guard = download_group.enter_stream();
      let (response, active_source) = match Self::send_request(
        app_handle,
        request_start,
        Some(segment_end),
        source_offset + attempt,
        source_tracker,
        request_gate,
      )
      .await
      {
        Ok(response) => response,
        Err(err) if is_range_reset_error(&err) => {
          // A stale piece can be larger than the current remote file or can
          // point past its end. PCL treats this as a bad resume point, not as
          // a permanent source failure.
          let _ = tokio::fs::remove_file(part_path).await;
          if let Ok(mut tracker) = source_tracker.lock() {
            tracker.reset_for_recovery();
          }
          continue;
        }
        Err(err) if attempt + 1 < SEGMENT_DOWNLOAD_RETRIES => {
          log::warn!(
            "Segment request failed (attempt {}/{}): {:?}",
            attempt + 1,
            SEGMENT_DOWNLOAD_RETRIES,
            err
          );
          tokio::time::sleep(retry_delay(attempt)).await;
          continue;
        }
        Err(err) => return Err(err),
      };

      let expected_response_size = segment_end - request_start + 1;
      let range_matches = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("bytes "))
        .and_then(|value| value.split_once('/'))
        .and_then(|(range, _)| range.split_once('-'))
        .and_then(|(start, end)| Some((start.parse::<i64>().ok()?, end.parse::<i64>().ok()?)))
        == Some((request_start, segment_end));
      if !range_matches || response.content_length() != Some(expected_response_size as u64) {
        if let Ok(mut tracker) = source_tracker.lock() {
          tracker.record_failure(&active_source, SourceFailureKind::Range);
        }
        if attempt + 1 < SEGMENT_DOWNLOAD_RETRIES {
          log::warn!(
            "Segment response mismatch (attempt {}/{}): expected range {}-{} and length {}, got range={} length={:?}",
            attempt + 1,
            SEGMENT_DOWNLOAD_RETRIES,
            request_start,
            segment_end,
            expected_response_size,
            range_matches,
            response.content_length()
          );
          continue;
        }
        return Err(BGUMCLError(format!(
          "Segment response mismatch: expected range {}-{} and length {}, got range={} length={:?}",
          request_start,
          segment_end,
          expected_response_size,
          range_matches,
          response.content_length()
        )));
      }

      let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(part_path)
        .await?;
      let stream = response
        .bytes_stream()
        .map_err(|err| std::io::Error::other(format!("download stream failed: {err}")));
      let reader = stream.into_async_read();
      let mut pending_progress = 0i64;
      let mut last_report = Instant::now();
      let transfer_started = Instant::now();
      let copy_result = if let Some(limiter_instance) = limiter.clone() {
        Self::copy_segment_reader(
          limiter_instance.limit(reader).compat(),
          &mut file,
          task_handle,
          &mut pending_progress,
          &mut last_report,
          download_group,
          LIMITED_DOWNLOAD_READ_TIMEOUT,
        )
        .await
      } else {
        Self::copy_segment_reader(
          reader.compat(),
          &mut file,
          task_handle,
          &mut pending_progress,
          &mut last_report,
          download_group,
          DOWNLOAD_READ_TIMEOUT,
        )
        .await
      };
      file.flush().await?;
      if let Ok(report) = &copy_result
        && let Ok(mut tracker) = source_tracker.lock()
      {
        tracker.record_transfer(
          &active_source,
          report.bytes,
          transfer_started.elapsed().max(report.elapsed),
        );
      }
      if let Err(err) = copy_result {
        if let Ok(mut tracker) = source_tracker.lock() {
          tracker.record_failure(&active_source, SourceFailureKind::Transfer);
        }
        if attempt + 1 < SEGMENT_DOWNLOAD_RETRIES {
          tokio::time::sleep(retry_delay(attempt)).await;
          continue;
        }
        return Err(err);
      }

      let final_size = tokio::fs::metadata(part_path).await?.len() as i64;
      if final_size == expected_size {
        return Ok(());
      }
      if let Ok(mut tracker) = source_tracker.lock() {
        tracker.record_failure(&active_source, SourceFailureKind::Transfer);
      }
      if attempt + 1 < SEGMENT_DOWNLOAD_RETRIES {
        tokio::time::sleep(retry_delay(attempt)).await;
      }
    }

    Err(BGUMCLError(format!(
      "Segment download did not complete: {}-{}",
      segment_start, segment_end
    )))
  }

  async fn copy_segment_reader<R>(
    mut reader: R,
    file: &mut tokio::fs::File,
    task_handle: &Arc<RwLock<PTaskHandle>>,
    pending_progress: &mut i64,
    last_report: &mut Instant,
    download_group: &DownloadGroupState,
    read_timeout: Duration,
  ) -> BGUMCLResult<TransferReport>
  where
    R: AsyncRead + Unpin,
  {
    let started = Instant::now();
    let mut transferred = 0i64;
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
      loop {
        let status = task_handle.read().unwrap().status().clone();
        if status.is_cancelled() || download_group.is_cancelled() {
          return Err(BGUMCLError("Download cancelled".to_string()));
        }
        if !status.is_stopped() {
          break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
      }
      let size = tokio::time::timeout(read_timeout, reader.read(&mut buffer))
        .await
        .map_err(|_| {
          BGUMCLError(format!(
            "Download source produced no data for {read_timeout:?}"
          ))
        })??;
      if size == 0 {
        break;
      }
      file.write_all(&buffer[..size]).await?;
      *pending_progress += size as i64;
      transferred = transferred.saturating_add(size as i64);
      download_group.record_bytes(size as i64);
      if last_report.elapsed() >= Duration::from_millis(250) {
        task_handle
          .write()
          .unwrap()
          .report_progress_now(*pending_progress);
        *pending_progress = 0;
        *last_report = Instant::now();
      }
    }
    if *pending_progress > 0 {
      task_handle
        .write()
        .unwrap()
        .report_progress_now(*pending_progress);
      *pending_progress = 0;
    }
    Ok(TransferReport {
      bytes: transferred,
      elapsed: started.elapsed(),
    })
  }

  async fn download_segmented(
    app_handle: &AppHandle,
    dest_path: &Path,
    total_size: i64,
    segment_count: i64,
    task_handle: &Arc<RwLock<PTaskHandle>>,
    limiter: Option<Limiter>,
    connection_semaphore: &Arc<Semaphore>,
    source_tracker: &SourceTracker,
    request_gate: &RequestGate,
    download_group: &DownloadGroupState,
  ) -> BGUMCLResult<()> {
    let segment_size = (total_size + segment_count - 1) / segment_count;
    let part_dir = Self::segment_directory(dest_path);
    let marker_path = part_dir.join("meta");
    let marker = format!("{total_size}:{segment_count}");
    match tokio::fs::read_to_string(&marker_path).await {
      Ok(existing) if existing == marker => {}
      Ok(_) | Err(_) => {
        if tokio::fs::try_exists(&part_dir).await.unwrap_or(false) {
          tokio::fs::remove_dir_all(&part_dir).await?;
        }
        tokio::fs::create_dir_all(&part_dir).await?;
        tokio::fs::write(&marker_path, marker.as_bytes()).await?;
      }
    }
    tokio::fs::create_dir_all(&part_dir).await?;

    let existing_total = futures::future::join_all((0..segment_count).map(|index| {
      let part_path = part_dir.join(format!("part-{index}"));
      async move {
        tokio::fs::metadata(part_path)
          .await
          .map(|meta| meta.len() as i64)
          .unwrap_or(0)
      }
    }))
    .await
    .into_iter()
    .sum::<i64>();
    let current = task_handle.read().unwrap().desc.current;
    if existing_total != current {
      task_handle.write().unwrap().set_current(existing_total);
    }
    let jobs = (0..segment_count)
      .map(|index| {
        let start = index * segment_size;
        let end = ((index + 1) * segment_size - 1).min(total_size - 1);
        let part_path = part_dir.join(format!("part-{index}"));
        let handle = task_handle.clone();
        let app_handle = app_handle.clone();
        let limiter = limiter.clone();
        async move {
          Self::download_segment(
            &app_handle,
            &part_path,
            start,
            end,
            0,
            &handle,
            limiter,
            &connection_semaphore,
            source_tracker,
            &request_gate,
            download_group,
          )
          .await
        }
      })
      .collect::<Vec<_>>();

    // V2 starts all selected ranges immediately. The shared connection
    // semaphore remains the hard global ceiling, while size-based segment
    // counts keep a single file from taking the whole pool.
    let mut pending = FuturesUnordered::from_iter(jobs);
    while let Some(result) = pending.next().await {
      result?;
    }

    let mut output = tokio::fs::File::create(dest_path).await?;
    for index in 0..segment_count {
      let part_path = part_dir.join(format!("part-{index}"));
      let mut part = tokio::fs::File::open(part_path).await?;
      tokio::io::copy(&mut part, &mut output).await?;
    }
    output.flush().await?;
    tokio::fs::remove_dir_all(part_dir).await?;
    Ok(())
  }

  async fn future_impl(
    self,
    app_handle: AppHandle,
    limiter: Option<Limiter>,
    connection_semaphore: Arc<Semaphore>,
    request_gate: RequestGate,
    download_group: DownloadGroupStateHandle,
  ) -> BGUMCLResult<(
    impl Future<Output = BGUMCLResult<()>> + Send,
    Arc<RwLock<PTaskHandle>>,
  )> {
    let current = self.p_handle.desc.current;
    let handle = Arc::new(RwLock::new(self.p_handle));
    let task_handle = handle.clone();
    let param = self.param.clone();
    Ok((
      async move {
        let _cancel_cleanup = CancelledDownloadCleanup {
          dest_path: self.dest_path.clone(),
          task_handle: task_handle.clone(),
          download_group: download_group.clone(),
        };
        let source_tracker = Mutex::new(FileSourceTracker::new(Self::candidate_urls(
          &app_handle,
          &param,
        )));
        tokio::fs::create_dir_all(self.dest_path.parent().unwrap()).await?;
        {
          let mut task_handle = task_handle.write().unwrap();
          // The first ordinary response supplies the total length. Avoid a
          // separate Range probe: PCL starts the file stream directly and
          // only creates another request when its global scheduler needs it.
          task_handle.set_total(-1);
          task_handle.mark_started();
        }

        let mut resume_current = current;
        let mut last_error = None;
        let total_attempts = SEGMENT_DOWNLOAD_RETRIES + RECOVERY_DOWNLOAD_ATTEMPTS;
        for attempt in 0..total_attempts {
          let in_recovery_pass = attempt >= SEGMENT_DOWNLOAD_RETRIES;
          if attempt == SEGMENT_DOWNLOAD_RETRIES {
            // PCL-style final recovery: clear the short-lived source state and
            // retry the file from zero with a clean single stream. This is
            // especially important for Forge's small Maven libraries, where
            // a partial response should not make the whole task group fail.
            if let Ok(mut tracker) = source_tracker.lock() {
              tracker.reset_for_recovery();
            }
            resume_current = 0;
            let _ = tokio::fs::remove_file(&self.dest_path).await;
            task_handle.write().unwrap().set_current(0);
            log::warn!(
              "Entering final single-stream recovery for {}",
              self.dest_path.display()
            );
          }
          let group_slot = download_group.acquire_stream_slot().await?;
          let connection_permit = connection_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| BGUMCLError("Download connection pool closed".to_string()))?;
          let stream_guard = download_group.enter_stream();
          let (resp, total_progress, restart_from_zero, active_source, supports_ranges) =
            match Self::create_resp_stream(
              &app_handle,
              resume_current,
              0,
              &source_tracker,
              &request_gate,
            )
            .await
            {
              Ok(value) => value,
              Err(err) if is_range_reset_error(&err) => {
                // Reset an invalid persisted offset immediately. Keeping the
                // stale length would make every retry send the same invalid
                // Range header and reproduce HTTP 416 forever.
                resume_current = 0;
                let _ = tokio::fs::remove_file(&self.dest_path).await;
                if let Ok(mut tracker) = source_tracker.lock() {
                  tracker.reset_for_recovery();
                }
                task_handle.write().unwrap().set_current(0);
                if attempt + 1 < total_attempts {
                  tokio::time::sleep(retry_delay(attempt)).await;
                  continue;
                }
                return Err(err);
              }
              Err(err) if attempt + 1 < total_attempts => {
                last_error = Some(err);
                tokio::time::sleep(retry_delay(attempt)).await;
                continue;
              }
              Err(err) => return Err(err),
            };
          let effective_current = if restart_from_zero { 0 } else { resume_current };
          if restart_from_zero {
            task_handle.write().unwrap().set_current(0);
          }

          let segment_count = Self::segment_count_for_size(total_progress);
          if !in_recovery_pass
            && attempt == 0
            && effective_current == 0
            && supports_ranges
            && segment_count > 1
            && limiter.is_none()
          {
            drop(resp);
            drop(stream_guard);
            drop(connection_permit);
            drop(group_slot);
            task_handle.write().unwrap().set_total(total_progress);
            log::info!(
              "Download Engine V2 starting {} ranges for {} ({} bytes)",
              segment_count,
              self.dest_path.display(),
              total_progress
            );
            match Self::download_segmented(
              &app_handle,
              &self.dest_path,
              total_progress,
              segment_count,
              &task_handle,
              limiter.clone(),
              &connection_semaphore,
              &source_tracker,
              &request_gate,
              &download_group,
            )
            .await
            {
              Ok(()) => {
                match Self::validate_sha1_async(self.dest_path.clone(), param.sha1.clone()).await {
                  Ok(()) => {
                    task_handle.write().unwrap().mark_completed();
                    return Ok(());
                  }
                  Err(error) => {
                    if let Ok(mut tracker) = source_tracker.lock() {
                      tracker.record_failure(&active_source, SourceFailureKind::Integrity);
                    }
                    last_error = Some(error);
                  }
                }
              }
              Err(error) => {
                log::warn!(
                  "Segmented V2 download failed for {}; falling back to a clean stream: {:?}",
                  self.dest_path.display(),
                  error
                );
                last_error = Some(error);
              }
            }
            let _ = tokio::fs::remove_dir_all(Self::segment_directory(&self.dest_path)).await;
            let _ = tokio::fs::remove_file(&self.dest_path).await;
            resume_current = 0;
            task_handle.write().unwrap().set_current(0);
            continue;
          }

          let mut file = if effective_current == 0 {
            // File::create truncates any existing partial file when restarting.
            tokio::fs::File::create(&self.dest_path).await?
          } else {
            let mut f = tokio::fs::OpenOptions::new()
              .write(true)
              .open(&self.dest_path)
              .await?;
            f.seek(std::io::SeekFrom::Start(effective_current as u64))
              .await?;
            f
          };
          task_handle.write().unwrap().set_total(total_progress);
          let stream = resp;
          let mut pending_progress = 0i64;
          let mut last_report = Instant::now();
          let copy_result = if let Some(lim) = limiter.clone() {
            Self::copy_segment_reader(
              lim.limit(stream.into_async_read()).compat(),
              &mut file,
              &task_handle,
              &mut pending_progress,
              &mut last_report,
              &download_group,
              LIMITED_DOWNLOAD_READ_TIMEOUT,
            )
            .await
          } else {
            Self::copy_segment_reader(
              stream.into_async_read().compat(),
              &mut file,
              &task_handle,
              &mut pending_progress,
              &mut last_report,
              &download_group,
              DOWNLOAD_READ_TIMEOUT,
            )
            .await
          };
          file.flush().await?;
          drop(file);

          if let Err(err) = copy_result {
            if task_handle.read().unwrap().status().is_cancelled() {
              tokio::fs::remove_file(&self.dest_path).await?;
              return Ok(());
            }
            if let Ok(mut tracker) = source_tracker.lock() {
              tracker.record_failure(&active_source, SourceFailureKind::Transfer);
            }
            last_error = Some(err);
          } else {
            if let Ok(report) = &copy_result
              && let Ok(mut tracker) = source_tracker.lock()
            {
              tracker.record_transfer(&active_source, report.bytes, report.elapsed);
            }
            let actual_size = tokio::fs::metadata(&self.dest_path)
              .await
              .map(|meta| meta.len() as i64)
              .unwrap_or(0);
            let expected_size_matches =
              total_progress < 0 || actual_size == effective_current + total_progress;
            if expected_size_matches {
              let validation =
                Self::validate_sha1_async(self.dest_path.clone(), param.sha1.clone()).await;
              match validation {
                Ok(()) => {
                  task_handle.write().unwrap().mark_completed();
                  return Ok(());
                }
                Err(err) => {
                  if let Ok(mut tracker) = source_tracker.lock() {
                    tracker.record_failure(&active_source, SourceFailureKind::Integrity);
                  }
                  last_error = Some(err);
                }
              }
            } else {
              if let Ok(mut tracker) = source_tracker.lock() {
                tracker.record_failure(&active_source, SourceFailureKind::Transfer);
              }
              last_error = Some(BGUMCLError(format!(
                "Download ended early: expected {}, got {}",
                effective_current + total_progress,
                actual_size
              )));
            }
          }

          resume_current = tokio::fs::metadata(&self.dest_path)
            .await
            .map(|meta| meta.len() as i64)
            .unwrap_or(0);
          task_handle.write().unwrap().set_current(resume_current);
          if attempt + 1 < total_attempts {
            tokio::time::sleep(retry_delay(attempt)).await;
          }
        }
        Err(last_error.unwrap_or_else(|| BGUMCLError("Download failed".to_string())))
      },
      handle,
    ))
  }

  pub async fn future(
    self,
    app_handle: AppHandle,
    limiter: Option<Limiter>,
    connection_semaphore: Arc<Semaphore>,
    request_gate: RequestGate,
    download_group: DownloadGroupStateHandle,
  ) -> BGUMCLResult<(
    impl Future<Output = BGUMCLResult<()>> + Send,
    Arc<RwLock<PTaskHandle>>,
  )> {
    Self::future_impl(
      self,
      app_handle,
      limiter,
      connection_semaphore,
      request_gate,
      download_group,
    )
    .await
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn v2_segment_count_is_size_bounded() {
    assert_eq!(DownloadTask::segment_count_for_size(7 * 1024 * 1024), 1);
    assert_eq!(DownloadTask::segment_count_for_size(8 * 1024 * 1024), 2);
    assert_eq!(DownloadTask::segment_count_for_size(32 * 1024 * 1024), 4);
    assert_eq!(DownloadTask::segment_count_for_size(64 * 1024 * 1024), 8);
  }

  #[test]
  fn group_stream_guard_tracks_live_connections() {
    let group = DownloadGroupState::new(4);
    {
      let _guard = group.enter_stream();
      assert_eq!(group.active_connections.load(Ordering::Relaxed), 1);
    }
    assert_eq!(group.active_connections.load(Ordering::Relaxed), 0);
  }

  #[test]
  fn v2_stream_budget_starts_bounded_and_can_grow_to_requested_ceiling() {
    let automatic = DownloadGroupState::new(128);
    assert_eq!(automatic.target_connections.load(Ordering::Relaxed), 16);
    assert_eq!(automatic.max_connections, 128);
    let manual = DownloadGroupState::new(12);
    assert_eq!(manual.target_connections.load(Ordering::Relaxed), 12);
    assert_eq!(manual.max_connections, 12);
  }

  #[test]
  fn range_reset_errors_are_classified_for_clean_resume() {
    assert!(is_range_reset_error(&BGUMCLError(
      "HTTP 416 Range Not Satisfiable".to_string()
    )));
    assert!(is_range_reset_error(&BGUMCLError(
      "Invalid resumed Content-Range".to_string()
    )));
    assert!(!is_range_reset_error(&BGUMCLError(
      "HTTP 500 Internal Server Error".to_string()
    )));
  }
}
