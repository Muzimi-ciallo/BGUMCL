use reqwest_middleware::{ClientBuilder as ClientWithMiddlewareBuilder, ClientWithMiddleware};
use reqwest_retry::RetryTransientMiddleware;
use reqwest_retry::policies::ExponentialBackoff;
use reqwest_retry::{
  Retryable, RetryableStrategy, default_on_request_failure, default_on_request_success,
};
use std::sync::Mutex;
use std::time::Duration;
use tauri::http::StatusCode;
use tauri::{AppHandle, Manager};
use tauri_plugin_http::reqwest::header::HeaderMap;
use tauri_plugin_http::reqwest::{Client, ClientBuilder, Proxy};
use url::Url;

use crate::launcher_config::models::{LauncherConfig, ProxyType};

/// Builds a reqwest client with BGUMCL User-Agent and proxy support.
/// Defaults to 10s timeout.
///
/// # Arguments
///
/// * `app` - The Tauri AppHandle.
/// * `use_proxy` - Whether to use the proxy settings from the config.
///
/// TODO: support more custom config from reqwest::Config
/// FIXME: Seems like hyper will panic if this client is shared across threads.
///
/// # Returns
///
/// A reqwest::Client instance.
///
/// # Example
///
/// ```rust
/// let client = build_bgumcl_client(&app, true);
/// ```
pub fn build_bgumcl_client(app: &AppHandle, use_proxy: bool) -> Client {
  let mut builder = ClientBuilder::new()
    .timeout(Duration::from_secs(10))
    .tcp_keepalive(Duration::from_secs(10));

  if let Ok(config) = app.state::<Mutex<LauncherConfig>>().lock() {
    // According to the User-Agent requirements of mozilla and BMCLAPI, the User-Agent is set to start with ${NAME}/${VERSION}
    // https://github.com/MCLF-CN/docs/issues/2
    // https://developer.mozilla.org/zh-CN/docs/Web/HTTP/Reference/Headers/User-Agent
    if let Ok(header_value) = format!("BGUMCL/{}", &config.basic_info.launcher_version).parse() {
      let mut headers = HeaderMap::new();
      headers.insert("User-Agent", header_value);
      builder = builder.default_headers(headers);
    }

    if use_proxy && config.download.proxy.enabled {
      let proxy_cfg = &config.download.proxy;
      let proxy_url = match proxy_cfg.selected_type {
        ProxyType::Http => format!("http://{}:{}", proxy_cfg.host, proxy_cfg.port),
        ProxyType::Socks => format!("socks5h://{}:{}", proxy_cfg.host, proxy_cfg.port),
      };

      if let Ok(proxy) = Proxy::all(&proxy_url) {
        builder = builder.proxy(proxy);
      }
    }
  }

  builder.build().unwrap_or_else(|_| Client::new())
}

struct BGUMCLRetryableStrategy;

impl RetryableStrategy for BGUMCLRetryableStrategy {
  fn handle(
    &self,
    res: &Result<tauri_plugin_http::reqwest::Response, reqwest_middleware::Error>,
  ) -> Option<reqwest_retry::Retryable> {
    match res {
      // retry if 403
      Ok(success) if success.status() == StatusCode::FORBIDDEN => Some(Retryable::Transient),
      // otherwise do not retry a successful request
      Ok(success) => default_on_request_success(success),
      // but maybe retry a request failure
      Err(error) if matches!(error.status(), Some(StatusCode::FORBIDDEN)) => {
        Some(Retryable::Transient)
      }
      Err(error) if error.is_request() => Some(Retryable::Transient),
      Err(error) => default_on_request_failure(error),
    }
  }
}

pub fn with_retry(client: Client) -> ClientWithMiddleware {
  ClientWithMiddlewareBuilder::new(client)
    .with(RetryTransientMiddleware::new_with_policy_and_strategy(
      ExponentialBackoff::builder().build_with_total_retry_duration(Duration::from_secs(3600)),
      BGUMCLRetryableStrategy {},
    ))
    .build()
}

/// gh-proxy acceleration prefixes, tried in order. If the first one is
/// unreachable (e.g. v4.gh-proxy.org is blocked in some regions), the next
/// mirror (cdn.gh-proxy.org) or a direct connection is used automatically.
pub const GH_PROXY_PREFIXES: [&str; 2] = [
  "https://v4.gh-proxy.org/https://",
  "https://cdn.gh-proxy.org/https://",
];

/// Strip any known gh-proxy prefix, returning the remainder (scheme-less
/// GitHub path such as `github.com/owner/repo/...`). URLs without a known
/// prefix are returned unchanged.
pub fn strip_gh_proxy_prefix(url: &str) -> String {
  for prefix in GH_PROXY_PREFIXES {
    if let Some(rest) = url.strip_prefix(prefix) {
      return rest.to_string();
    }
  }
  url.to_string()
}

/// Map a GitHub account to its Gitee mirror account, for repos that are
/// mirrored to Gitee. Unknown accounts get `None`.
fn gitee_mirror_owner(github_owner: &str) -> Option<&'static str> {
  match github_owner {
    "Muzimi-ciallo" => Some("Muzimimiao"),
    _ => None,
  }
}

/// Convert a GitHub URL to the corresponding Gitee mirror URL, if the repo is
/// mirrored. Handles `github.com/...` and `raw.githubusercontent.com/...`.
pub fn github_to_gitee(url: &str) -> Option<String> {
  if let Some(rest) = url.strip_prefix("https://raw.githubusercontent.com/") {
    let mut parts = rest.splitn(3, '/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    let branch_path = parts.next()?;
    let g_owner = gitee_mirror_owner(owner)?;
    let mut bp = branch_path.splitn(2, '/');
    let branch = bp.next().unwrap_or("main");
    let path = bp.next().unwrap_or("");
    return Some(format!(
      "https://gitee.com/{}/{}/raw/{}/{}",
      g_owner, repo, branch, path
    ));
  }
  if let Some(rest) = url.strip_prefix("https://github.com/") {
    let mut parts = rest.splitn(3, '/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    let rest_path = parts.next().unwrap_or("");
    let g_owner = gitee_mirror_owner(owner)?;
    return Some(format!(
      "https://gitee.com/{}/{}/{}",
      g_owner, repo, rest_path
    ));
  }
  None
}

/// Build a list of candidate URLs to try for GitHub-related requests:
/// v4 proxy -> Gitee mirror -> cdn proxy -> direct connection. Gitee is put
/// early because it is reliably reachable in mainland China (e.g. Guangxi)
/// even when GitHub and gh-proxy are blocked. Non-GitHub URLs are returned
/// unchanged so unrelated downloads are not routed through a proxy.
pub fn gh_proxy_candidates(url: &str) -> Vec<String> {
  let is_github =
    url.contains("github.com") || url.contains("raw.githubusercontent.com");
  if !is_github {
    return vec![url.to_string()];
  }
  let stripped = strip_gh_proxy_prefix(url);
  let had_prefix = stripped != url;
  // Scheme-less GitHub path, e.g. `github.com/owner/repo/...`, which mirrors
  // expect right after `https://v4.gh-proxy.org/https://`.
  let scheme_less = if had_prefix {
    stripped
  } else {
    url.strip_prefix("https://")
      .or_else(|| url.strip_prefix("http://"))
      .unwrap_or(url)
      .to_string()
  };
  let direct = format!("https://{}", scheme_less);
  let mut out: Vec<String> = Vec::new();
  let v4 = format!("{}{}", GH_PROXY_PREFIXES[0], scheme_less);
  out.push(v4);
  if let Some(gitee) = github_to_gitee(&direct) {
    if !out.contains(&gitee) {
      out.push(gitee);
    }
  }
  let cdn = format!("{}{}", GH_PROXY_PREFIXES[1], scheme_less);
  if !out.contains(&cdn) {
    out.push(cdn);
  }
  if !out.contains(&direct) {
    out.push(direct);
  }
  // If the caller already used a specific mirror, keep that exact URL first.
  if had_prefix {
    if let Some(pos) = out.iter().position(|c| c == url) {
      out.swap(0, pos);
    }
  }
  out
}

/// Check whether the current IP is located in mainland China.
///
/// This function queries two Cloudflare trace endpoints in parallel.
/// If either endpoint reports `loc=CN`, the IP is considered to be in mainland China.
/// The detection result is cached into the launcher config.
///
/// # Arguments
///
/// * `app` - The Tauri AppHandle.
///
/// # Returns
///
/// * `Some(true)` if either endpoint reports mainland China.
/// * `Some(false)` if both endpoints report non-mainland China.
/// * `None` if both detection requests fail.
pub async fn is_china_mainland_ip(app: &AppHandle) -> Option<bool> {
  let client = app.state::<Client>();

  async fn fetch_and_extract_loc(client: &Client, url: &str) -> Option<String> {
    let resp = client.get(url).send().await.ok()?;
    let text = resp.text().await.ok()?;
    let loc_line = text.lines().find(|line| line.starts_with("loc="))?;
    let loc = loc_line.split('=').nth(1)?.trim();
    log::info!("Check location from {}, return {}", url, loc);
    Some(loc.to_string())
  }

  let (loc1, loc2) = tokio::join!(
    fetch_and_extract_loc(&client, "https://cloudflare.com/cdn-cgi/trace"),
    fetch_and_extract_loc(&client, "https://www.cloudflare-cn.com/cdn-cgi/trace")
  );
  let result = loc1.as_deref() == Some("CN") || loc2.as_deref() == Some("CN");

  let config_binding = app.state::<Mutex<LauncherConfig>>();
  match config_binding.lock() {
    Ok(mut config_state) => {
      let _ = config_state.partial_update(
        app,
        "basic_info.is_china_mainland_ip",
        &serde_json::to_string(&result).unwrap_or("false".to_string()),
      );
    }
    Err(_) => return Some(false),
  }

  Some(result)
}

/// Normalizes a URL string for semantic equality comparison, including:
/// - Lowercasing the scheme and the host (ref to RFC 3986, impl by Url::parse)
/// - Removing trailing slashes from paths (except for root `/`)
/// - Removing default ports (e.g. 80 for HTTP, 443 for HTTPS)
///
/// # Arguments
///
/// * `input` - The URL string to normalize.
///
/// # Returns
///
/// A normalized URL string suitable for direct string comparison.
/// If parsing fails, the original input string is returned unchanged.
pub fn normalize_url(input: &str) -> String {
  let url = match Url::parse(input) {
    Ok(url) if !url.cannot_be_a_base() && url.host_str().is_some() => url,
    _ => return input.to_string(),
  };

  // remove trailing slash except for root
  let mut path = url.path().to_string();
  if path != "/" {
    path = path.trim_end_matches('/').to_string();
  }

  // remove default port(e.g. 80 for HTTP, 443 for HTTPS)
  let port = match (url.port(), url.port_or_known_default()) {
    (Some(p), Some(default)) if p == default => None,
    (p, _) => p,
  };

  let mut normalized = match Url::parse(&format!("{}://{}", url.scheme(), url.host_str().unwrap()))
  {
    Ok(u) => u,
    Err(_) => return input.to_string(),
  };

  let _ = normalized.set_port(port);
  normalized.set_path(&path);
  normalized.set_query(url.query());
  normalized.set_fragment(url.fragment());

  normalized.to_string()
}
