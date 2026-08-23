use async_speed_limit::Limiter;
use futures::StreamExt;
use futures::stream::TryStreamExt;
use serde::{Deserialize, Serialize};
use sjmcl_types::error::{BGUMCLError, BGUMCLResult};
use std::collections::HashMap;
use std::error::Error;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager, Url};
use tauri_plugin_http::reqwest;
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

/// Browser-like User-Agent for file downloads. Some acceleration proxies
/// (e.g. gh-proxy) throttle non-browser User-Agents, which made large update
/// downloads extremely slow. Also use a long total timeout so big files
/// (launcher updates / modpacks) are not cut off by the default 10s timeout.
const DOWNLOAD_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// Large mirrored files use PCL-style HTTP Range segmentation. The segment
/// count is derived from the file size below, so small files do not create
/// excessive connections while large modpacks can use more parallel streams.
const SEGMENTED_DOWNLOAD_THRESHOLD: i64 = 8 * 1024 * 1024;
const MAX_DOWNLOAD_SEGMENTS: i64 = 4;
const SEGMENT_DOWNLOAD_RETRIES: usize = 4;
const RECOVERY_DOWNLOAD_ATTEMPTS: usize = 3;
const SOURCE_FAILURE_LIMIT: u8 = 2;
const DOWNLOAD_READ_TIMEOUT: Duration = Duration::from_secs(45);
const BMCLAPI_REQUEST_INTERVAL: Duration = Duration::from_millis(100);

type SourceHealth = Arc<Mutex<HashMap<String, u8>>>;
type PreferredSource = Arc<Mutex<Option<String>>>;
pub(crate) type RequestGate = Arc<AsyncMutex<Instant>>;

fn retry_delay(attempt: usize) -> Duration {
  Duration::from_millis(500u64.saturating_mul(1u64 << attempt.min(3)))
}

pub(crate) fn download_client() -> &'static reqwest::Client {
  static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
  CLIENT.get_or_init(|| {
    reqwest::Client::builder()
      .timeout(Duration::from_secs(600))
      .connect_timeout(Duration::from_secs(10))
      .tcp_keepalive(Duration::from_secs(30))
      .pool_max_idle_per_host(32)
      .user_agent(DOWNLOAD_USER_AGENT)
      .build()
      .unwrap_or_else(|_| reqwest::Client::new())
  })
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
    .pool_max_idle_per_host(32)
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

/// Only probe Range support for sources that have a known mainland mirror or
/// are already served by the mod mirrors. Random third-party URLs stay on the
/// normal streaming path because probing them adds latency and some servers
/// treat Range requests as errors.
fn supports_segmented_download(param: &DownloadParam) -> bool {
  // PCL keeps BMCLAPI, GitHub, and similar sources on the ordinary streaming
  // path. They are fast when used across many files, but aggressive Range
  // fan-out can trigger throttling or expose incomplete mirrors.
  is_curseforge_or_modrinth_url(&param.src)
}

pub struct DownloadTask {
  p_handle: PTaskHandle,
  param: DownloadParam,
  dest_path: PathBuf,
  report_interval: Duration,
}

impl DownloadTask {
  pub fn new(
    app_handle: AppHandle,
    task_id: u32,
    task_group: Option<String>,
    param: DownloadParam,
    report_interval: Duration,
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
          Duration::from_secs(1),
          TauriEventSink::new(app_handle.clone()),
        ),
      ),
      param: param.clone(),
      dest_path: cache_dir.clone().join(param.dest.clone()),
      report_interval,
    }
  }

  pub fn from_descriptor(
    app_handle: AppHandle,
    desc: PTaskDesc,
    report_interval: Duration,
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
          Duration::from_secs(1),
          TauriEventSink::new(app_handle.clone()),
        ),
      ),
      param: param.clone(),
      dest_path: cache_dir.clone().join(param.dest.clone()),
      report_interval,
    }
  }

  async fn send_request(
    app_handle: &AppHandle,
    current: i64,
    range_end: Option<i64>,
    candidate_offset: usize,
    param: &DownloadParam,
    source_health: &SourceHealth,
    preferred_source: &PreferredSource,
    request_gate: &RequestGate,
  ) -> BGUMCLResult<reqwest::Response> {
    let is_cf_mr = is_curseforge_or_modrinth_url(&param.src);

    // Build the list of candidate URLs to try, in order.
    let mut candidates: Vec<String> = Vec::new();
    if is_cf_mr {
      // PCL tries several equivalent CurseForge CDN addresses because one CDN
      // edge can be slow or unavailable in mainland China. Keep MCIM first,
      // then use the official CDN variants as fallbacks.
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
      for official in &official_candidates {
        if let Some(mirror) = crate::utils::web::mcim_mirror_url(official) {
          if !candidates.contains(&mirror) {
            candidates.push(mirror);
          }
        }
      }
      candidates.extend(official_candidates);
    } else if param.src.host_str().is_some_and(|host| {
      host == "github.com" || host == "raw.githubusercontent.com"
    }) {
      // GitHub-related downloads: Gitee mirror -> gh-proxy v4 -> cdn -> direct.
      candidates = crate::utils::web::gh_proxy_candidates(param.src.as_str());
    } else {
      // Mojang metadata, libraries, and assets: BMCLAPI mirrors first, then
      // the official URL as a fallback, matching PCL's source order.
      candidates = crate::utils::web::minecraft_download_candidates(param.src.as_str());
    }

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
    // this file on that source. This avoids scattering different ranges over
    // slow official CDNs when the mainland mirror is healthy. A later retry
    // can still rotate away from it by using a non-zero candidate offset.
    if let Some(preferred) = preferred_source.lock().ok().and_then(|value| value.clone())
      && let Some(index) = available_candidates
        .iter()
        .position(|candidate| candidate == &preferred)
    {
      available_candidates.swap(0, index);
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
      if is_curseforge_authenticated_url(&param.src) {
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

      match request.send().await {
        Ok(resp) => match resp.error_for_status() {
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
            log::info!("Download using candidate: {}", candidate);
            return Ok(resp);
          }
          Err(e) => {
            if let Ok(mut health) = source_health.lock() {
              let failures = health.entry(candidate.clone()).or_default();
              *failures = failures.saturating_add(1);
            }
            last_err = Some(format!("{:?}", e.source()));
          }
        },
        Err(e) => {
          if let Ok(mut health) = source_health.lock() {
            let failures = health.entry(candidate.clone()).or_default();
            *failures = failures.saturating_add(1);
          }
          last_err = Some(format!("{:?}", e.source()));
        }
      }
    }
    Err(BGUMCLError(
      last_err.unwrap_or_else(|| "Download request failed".to_string()),
    ))
  }

  async fn create_resp_stream(
    app_handle: &AppHandle,
    current: i64,
    candidate_offset: usize,
    param: &DownloadParam,
    source_health: &SourceHealth,
    preferred_source: &PreferredSource,
    request_gate: &RequestGate,
  ) -> BGUMCLResult<(
    impl Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send + use<>,
    i64,
    bool,
  )> {
    let resp = Self::send_request(
      app_handle,
      current,
      None,
      candidate_offset,
      param,
      source_health,
      preferred_source,
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
    Ok((
      resp
        .bytes_stream()
        .map_err(|err| std::io::Error::other(format!("download stream failed: {err}"))),
      total_progress,
      restart_from_zero,
    ))
  }

  /// Probe the total size using a one-byte Range request. This is used for
  /// CurseForge/Modrinth and known Mojang/BMCLAPI sources, where the extra
  /// request enables parallel downloading without affecting random
  /// third-party URLs.
  async fn probe_range_size(
    app_handle: &AppHandle,
    param: &DownloadParam,
    source_health: &SourceHealth,
    preferred_source: &PreferredSource,
    request_gate: &RequestGate,
  ) -> Option<i64> {
    let response = Self::send_request(
      app_handle,
      0,
      Some(0),
      0,
      param,
      source_health,
      preferred_source,
      request_gate,
    )
    .await
    .ok()?;
    let content_range = response.headers().get(CONTENT_RANGE)?.to_str().ok()?;
    let total = content_range.rsplit_once('/')?.1.parse::<i64>().ok()?;
    (total >= SEGMENTED_DOWNLOAD_THRESHOLD).then_some(total)
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

  async fn download_segment(
    app_handle: &AppHandle,
    param: &DownloadParam,
    part_path: &Path,
    segment_start: i64,
    segment_end: i64,
    source_offset: usize,
    task_handle: &Arc<RwLock<PTaskHandle>>,
    limiter: Option<Limiter>,
    connection_semaphore: &Arc<Semaphore>,
    source_health: &SourceHealth,
    preferred_source: &PreferredSource,
    request_gate: &RequestGate,
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
      let _connection_permit = connection_semaphore
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| BGUMCLError("Download connection pool closed".to_string()))?;
      let response =
        match Self::send_request(
          app_handle,
          request_start,
          Some(segment_end),
          source_offset + attempt,
          param,
          source_health,
          preferred_source,
          request_gate,
        )
        .await
        {
          Ok(response) => response,
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
      let copy_result = if let Some(limiter_instance) = limiter.clone() {
        Self::copy_segment_reader(
          limiter_instance.limit(reader).compat(),
          &mut file,
          task_handle,
          &mut pending_progress,
          &mut last_report,
        )
        .await
      } else {
        Self::copy_segment_reader(
          reader.compat(),
          &mut file,
          task_handle,
          &mut pending_progress,
          &mut last_report,
        )
        .await
      };
      file.flush().await?;
      if let Err(err) = copy_result {
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
  ) -> BGUMCLResult<()>
  where
    R: AsyncRead + Unpin,
  {
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
      loop {
        let status = task_handle.read().unwrap().status().clone();
        if status.is_cancelled() {
          return Err(BGUMCLError("Download cancelled".to_string()));
        }
        if !status.is_stopped() {
          break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
      }
      let size = tokio::time::timeout(DOWNLOAD_READ_TIMEOUT, reader.read(&mut buffer))
        .await
        .map_err(|_| BGUMCLError("Download source stalled for 45 seconds".to_string()))??;
      if size == 0 {
        break;
      }
      file.write_all(&buffer[..size]).await?;
      *pending_progress += size as i64;
      if last_report.elapsed() >= Duration::from_secs(1) {
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
    Ok(())
  }

  async fn download_segmented(
    app_handle: &AppHandle,
    param: &DownloadParam,
    dest_path: &Path,
    total_size: i64,
    task_handle: &Arc<RwLock<PTaskHandle>>,
    limiter: Option<Limiter>,
    connection_semaphore: &Arc<Semaphore>,
    source_health: &SourceHealth,
    preferred_source: &PreferredSource,
    request_gate: &RequestGate,
  ) -> BGUMCLResult<()> {
    // Adapt the number of ranges to the file size. This keeps small files
    // cheap while giving large modpack files enough independent connections
    // to avoid being limited by a single slow CDN stream.
    let segment_count = ((total_size + SEGMENTED_DOWNLOAD_THRESHOLD - 1)
      / SEGMENTED_DOWNLOAD_THRESHOLD)
      .clamp(2, MAX_DOWNLOAD_SEGMENTS);
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

    let jobs = (0..segment_count).map(|index| {
      let start = index * segment_size;
      let end = ((index + 1) * segment_size - 1).min(total_size - 1);
      let part_path = part_dir.join(format!("part-{index}"));
      let handle = task_handle.clone();
      let param = param.clone();
      let app_handle = app_handle.clone();
      let limiter = limiter.clone();
      async move {
        Self::download_segment(
          &app_handle,
          &param,
          &part_path,
          start,
          end,
          0,
          &handle,
          limiter,
          &connection_semaphore,
          &source_health,
          &preferred_source,
          &request_gate,
        )
        .await
      }
    });
    futures::stream::iter(jobs)
      .buffer_unordered(segment_count as usize)
      .try_collect::<Vec<_>>()
      .await?;

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
        let source_health: SourceHealth = Arc::new(Mutex::new(HashMap::new()));
        let preferred_source: PreferredSource = Arc::new(Mutex::new(None));
        let parts_exist = tokio::fs::try_exists(&Self::segment_directory(&self.dest_path))
          .await
          .unwrap_or(false);
        let segmented_total =
          if supports_segmented_download(&param) && (current == 0 || parts_exist) {
            Self::probe_range_size(
              &app_handle,
              &param,
              &source_health,
              &preferred_source,
              &request_gate,
            )
            .await
          } else {
            None
          };
        tokio::fs::create_dir_all(self.dest_path.parent().unwrap()).await?;
        {
          let mut task_handle = task_handle.write().unwrap();
          task_handle.set_total(segmented_total.unwrap_or(-1));
          task_handle.mark_started();
        }

        if let Some(total_size) = segmented_total {
          Self::download_segmented(
            &app_handle,
            &param,
            &self.dest_path,
            total_size,
            &task_handle,
            limiter.clone(),
            &connection_semaphore,
            &source_health,
            &preferred_source,
            &request_gate,
          )
          .await?;
          if let Some(truth) = param.sha1 {
            validate_sha1(self.dest_path.clone(), truth)?;
          }
          task_handle.write().unwrap().mark_completed();
          return Ok(());
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
            if let Ok(mut health) = source_health.lock() {
              health.clear();
            }
            if let Ok(mut preferred) = preferred_source.lock() {
              *preferred = None;
            }
            resume_current = 0;
            let _ = tokio::fs::remove_file(&self.dest_path).await;
            task_handle.write().unwrap().set_current(0);
            log::warn!(
              "Entering final single-stream recovery for {}",
              self.dest_path.display()
            );
          }
          let _connection_permit = connection_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| BGUMCLError("Download connection pool closed".to_string()))?;
          let (resp, total_progress, restart_from_zero) = match Self::create_resp_stream(
            &app_handle,
            resume_current,
            if in_recovery_pass {
              attempt - SEGMENT_DOWNLOAD_RETRIES
            } else {
              attempt
            },
            &param,
            &source_health,
            &preferred_source,
            &request_gate,
          )
          .await
          {
            Ok(value) => value,
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
            )
            .await
          } else {
            Self::copy_segment_reader(
              stream.into_async_read().compat(),
              &mut file,
              &task_handle,
              &mut pending_progress,
              &mut last_report,
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
            if let Some(source) = preferred_source.lock().ok().and_then(|value| value.clone())
              && let Ok(mut health) = source_health.lock()
            {
              let failures = health.entry(source).or_default();
              *failures = failures.saturating_add(1);
            }
            last_error = Some(err);
          } else {
            let actual_size = tokio::fs::metadata(&self.dest_path)
              .await
              .map(|meta| meta.len() as i64)
              .unwrap_or(0);
            let expected_size_matches = total_progress < 0
              || actual_size == effective_current + total_progress;
            if expected_size_matches {
              let validation = match &param.sha1 {
                Some(truth) => validate_sha1(self.dest_path.clone(), truth.clone()),
                None => Ok(()),
              };
              match validation {
                Ok(()) => {
                  task_handle.write().unwrap().mark_completed();
                  return Ok(());
                }
                Err(err) => {
                  if let Some(source) =
                    preferred_source.lock().ok().and_then(|value| value.clone())
                    && let Ok(mut health) = source_health.lock()
                  {
                    let failures = health.entry(source).or_default();
                    *failures = failures.saturating_add(1);
                  }
                  last_error = Some(err);
                }
              }
            } else {
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
  ) -> BGUMCLResult<(
    impl Future<Output = BGUMCLResult<()>> + Send,
    Arc<RwLock<PTaskHandle>>,
  )> {
    Self::future_impl(self, app_handle, limiter, connection_semaphore, request_gate).await
  }
}
