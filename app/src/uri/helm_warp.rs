//! Helm-Warp URL scheme handler (`helm-warp://connect`).
//!
//! Gap 4b producer consumer. When an operator clicks an endpoint in Helm-Portal,
//! the Portal issues a short-lived exchange code and redirects the browser to
//! `helm-warp://connect?portal=<url>&endpoint_id=<id>&exchange=<code>`. macOS
//! delivers that URL to this app via the GURL Apple Event
//! (`warp_app_open_urls` → `on_open_urls` → `handle_incoming_uri`), which
//! dispatches it here.
//!
//! This handler exchanges the code for launch credentials
//! (`POST <portal>/api/v1/launch/exchange`) and writes the result to
//! `~/.config/helm/launch.json`. The next `/ai` agent-mode request then routes
//! to helm_oz bound to that endpoint (`launch_target()` reads launch.json
//! per-request, so no restart is needed).
//!
//! The exchange code alone authenticates the request (it is short-lived,
//! one-time-use, and bound to the endpoint/operator/org) — the fork has no
//! operator session cookie to send, by design.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::server::server_api::helm_launch::launch_config_path;
use crate::view_components::DismissibleToast;
use crate::workspace::ToastStack;
use warpui::SingletonEntity as _;

/// `helm-warp://` scheme.
pub const SCHEME: &str = "helm-warp";

/// Returns true if `url` is a `helm-warp://` URL this module should handle.
pub fn is_helm_warp_url(url: &Url) -> bool {
    url.scheme() == SCHEME
}

/// Top-level entry point invoked from `handle_incoming_uri` for any
/// `helm-warp://` URL. Shows an immediate "connecting" toast, then runs the
/// exchange + launch.json write on a background task (it does async HTTP).
/// Completion is logged; the visible signal that it worked is the next agent
/// answer rendering with endpoint data.
pub fn handle(url: &Url, ctx: &mut warpui::AppContext) {
    let parsed = match ParsedLaunchUrl::parse(url) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("helm-warp: invalid launch URL {url}: {e:?}");
            show_toast(ctx, format!("Invalid Helm launch link: {e}"), true);
            return;
        }
    };

    let endpoint_id = parsed.endpoint_id.clone();
    show_toast(ctx, format!("Connecting to endpoint {endpoint_id}…"), false);

    let portal = parsed.portal.clone();
    let exchange = parsed.exchange.clone();
    let endpoint_id = parsed.endpoint_id.clone();
    ctx.background_executor()
        .spawn(async move {
            match run_exchange(&portal, &exchange).await {
                Ok(launch) => log::info!(
                    "helm-warp: launch.json written for endpoint {} (helm_oz={}, os={:?})",
                    endpoint_id,
                    launch.helm_oz_url,
                    launch.endpoint_os
                ),
                Err(e) => log::warn!("helm-warp: exchange failed for endpoint {endpoint_id}: {e:#}"),
            }
        })
        .detach();
}

/// The Portal `/api/v1/launch/exchange` response. Mirrors helm-portal's
/// `ExchangeResponse` — just the fields launch.json needs.
#[derive(Debug, Deserialize)]
struct ExchangeResponse {
    helm_oz_url: String,
    agent_token: String,
    endpoint_id: String,
    #[serde(default)]
    endpoint_os: Option<String>,
}

/// On-disk launch.json shape written by this handler (matches the schema the
/// fork's `helm_launch::HelmLaunchConfig` reads).
#[derive(Debug, Serialize)]
struct HelmLaunchFile<'a> {
    helm_oz_url: &'a str,
    agent_token: &'a str,
    endpoint_id: &'a str,
    endpoint_os: Option<&'a str>,
}

/// `helm-warp://connect?portal=...&endpoint_id=...&exchange=...`.
struct ParsedLaunchUrl {
    portal: String,
    endpoint_id: String,
    exchange: String,
}

impl ParsedLaunchUrl {
    fn parse(url: &Url) -> Result<Self> {
        let host = url.host_str().context("helm-warp URL missing host")?;
        if host != "connect" {
            anyhow::bail!("unsupported helm-warp host: {host} (expected 'connect')");
        }
        let mut portal = None;
        let mut endpoint_id = None;
        let mut exchange = None;
        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "portal" => portal = Some(v.into_owned()),
                "endpoint_id" => endpoint_id = Some(v.into_owned()),
                "exchange" => exchange = Some(v.into_owned()),
                _ => {}
            }
        }
        Ok(Self {
            portal: portal.context("missing 'portal' query param")?,
            endpoint_id: endpoint_id.context("missing 'endpoint_id' query param")?,
            exchange: exchange.context("missing 'exchange' query param")?,
        })
    }
}

async fn run_exchange(portal: &str, exchange: &str) -> Result<ExchangeResponse> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let resp = client
        .post(format!(
            "{}/api/v1/launch/exchange",
            portal.trim_end_matches('/')
        ))
        .json(&serde_json::json!({ "exchange_code": exchange }))
        .send()
        .await
        .context("exchange HTTP request failed")?;
    let status = resp.status();
    let body = resp.bytes().await.context("reading exchange response")?;
    if !status.is_success() {
        anyhow::bail!(
            "exchange returned {}: {}",
            status,
            String::from_utf8_lossy(&body)
        );
    }
    let exchange_resp: ExchangeResponse =
        serde_json::from_slice(&body).context("parsing exchange response")?;
    write_launch_json(&exchange_resp)?;
    Ok(exchange_resp)
}

fn write_launch_json(launch: &ExchangeResponse) -> Result<()> {
    let path: PathBuf = launch_config_path().context("no launch.json path (HOME unset?)")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let pretty = serde_json::to_string_pretty(&HelmLaunchFile {
        helm_oz_url: &launch.helm_oz_url,
        agent_token: &launch.agent_token,
        endpoint_id: &launch.endpoint_id,
        endpoint_os: launch.endpoint_os.as_deref(),
    })?;
    std::fs::write(&path, pretty)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn show_toast(ctx: &mut warpui::AppContext, message: String, is_error: bool) {
    if let Some(window_id) = ctx.windows().frontmost_window_id() {
        let toast = if is_error {
            DismissibleToast::error(message)
        } else {
            DismissibleToast::success(message)
        };
        ToastStack::handle(ctx).update(ctx, |toast_stack, ctx| {
            toast_stack.add_ephemeral_toast(toast, window_id, ctx);
        });
    }
}
