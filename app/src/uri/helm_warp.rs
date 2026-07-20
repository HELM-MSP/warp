//! Helm-Warp URL scheme handler (`helm-warp://connect`).

use anyhow::{Context as _, Result};
use serde::Deserialize;
use url::Url;

use crate::server::server_api::helm_tab_binding::{self, HelmEndpointBinding};
use crate::workspace::{HelmExchangeCode, WorkspaceAction};

pub const SCHEME: &str = "helm-warp";

pub fn is_helm_warp_url(url: &Url) -> bool {
    url.scheme() == SCHEME
}

pub fn parse_action(url: &Url) -> Result<WorkspaceAction> {
    let parsed = ParsedLaunchUrl::parse(url)?;
    Ok(WorkspaceAction::ConnectHelmEndpoint {
        portal: parsed.portal,
        exchange: HelmExchangeCode::new(parsed.exchange),
        endpoint_id: parsed.endpoint_id,
    })
}

#[derive(Debug, Deserialize)]
struct ExchangeResponse {
    helm_oz_url: String,
    agent_token: String,
    endpoint_id: String,
    endpoint_friendly_label: String,
    endpoint_hostname: String,
    endpoint_os: String,
}

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
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "portal" => portal = Some(value.into_owned()),
                "endpoint_id" => endpoint_id = Some(value.into_owned()),
                "exchange" => exchange = Some(value.into_owned()),
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

pub async fn run_exchange(
    portal: String,
    exchange: HelmExchangeCode,
    expected_endpoint_id: String,
) -> Result<HelmEndpointBinding> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let response = client
        .post(format!(
            "{}/api/v1/launch/exchange",
            portal.trim_end_matches('/')
        ))
        .json(&serde_json::json!({ "exchange_code": exchange.expose() }))
        .send()
        .await
        .context("exchange HTTP request failed")?;
    let status = response.status();
    let body = response.bytes().await.context("reading exchange response")?;
    if !status.is_success() {
        anyhow::bail!("exchange returned {status}");
    }
    let response: ExchangeResponse =
        serde_json::from_slice(&body).context("parsing exchange response")?;
    validate_exchange_response(response, &expected_endpoint_id)
}

fn validate_exchange_response(
    response: ExchangeResponse,
    expected_endpoint_id: &str,
) -> Result<HelmEndpointBinding> {
    if response.endpoint_id.trim() != expected_endpoint_id.trim() {
        anyhow::bail!(
            "exchange endpoint mismatch (requested={}, returned={})",
            expected_endpoint_id,
            response.endpoint_id
        );
    }
    helm_tab_binding::freeze_remote(
        Some(&response.endpoint_id),
        Some(&response.endpoint_friendly_label),
        Some(&response.endpoint_hostname),
        Some(&response.endpoint_os),
        Some(&response.helm_oz_url),
        Some(&response.agent_token),
    )
    .context("validating exchange response")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete_response() -> ExchangeResponse {
        ExchangeResponse {
            helm_oz_url: "https://oz.example".to_string(),
            agent_token: "jwt-A".to_string(),
            endpoint_id: "ep-A".to_string(),
            endpoint_friendly_label: "Laptop A".to_string(),
            endpoint_hostname: "laptop-a.local".to_string(),
            endpoint_os: "macos".to_string(),
        }
    }

    #[test]
    fn exchange_response_freezes_complete_binding() {
        let binding = validate_exchange_response(complete_response(), "ep-A").unwrap();
        assert_eq!(binding.endpoint_id, "ep-A");
        assert_eq!(binding.endpoint_friendly_label, "Laptop A");
        assert_eq!(binding.agent_token, "jwt-A");
    }

    #[test]
    fn exchange_response_rejects_endpoint_mismatch() {
        let error = validate_exchange_response(complete_response(), "ep-B").unwrap_err();
        assert!(error.to_string().contains("endpoint mismatch"));
    }

    #[test]
    fn exchange_response_rejects_blank_required_field() {
        let mut response = complete_response();
        response.endpoint_hostname = "  ".to_string();
        let error = validate_exchange_response(response, "ep-A").unwrap_err();
        assert!(error.to_string().contains("validating exchange response"));
    }

    #[test]
    fn action_debug_redacts_exchange_code() {
        let url = Url::parse(
            "helm-warp://connect?portal=https%3A%2F%2Fportal.example&endpoint_id=ep-A&exchange=secret-code",
        )
        .unwrap();
        let action = parse_action(&url).unwrap();
        let debug = format!("{action:?}");
        assert!(!debug.contains("secret-code"));
        assert!(debug.contains("[REDACTED]"));
    }
}
