//! Helm-Warp URL scheme handler (`helm-warp://connect`).

use anyhow::{Context as _, Result};
use serde::Deserialize;
use url::Url;

use crate::server::server_api::helm_tab_binding::{
    self, EndpointIdentity, HelmConnectionState, HelmEndpointBinding, HelmTabBinding,
};
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
    /// Long-lived refresh token for /api/v1/launch/refresh (Gap 5).
    /// Optional — when absent the tab binds but has no refresh capability.
    #[serde(default)]
    refresh_token: Option<String>,
    /// Portal refresh endpoint URL (Gap 5).
    #[serde(default)]
    refresh_url: Option<String>,
}

/// The Portal `/api/v1/launch/refresh` response.
#[derive(Debug, Deserialize)]
pub struct RefreshResponse {
    pub agent_token: String,
    /// Endpoint identity echoed by the portal. The refresh loop compares
    /// this against the frozen binding — a mismatch means the portal is
    /// minting a token for a different endpoint and we must NOT apply it.
    #[serde(default)]
    pub endpoint_id: Option<String>,
}

/// Result of a successful helm-warp exchange: a frozen binding plus,
/// optionally, the refresh descriptor that wires the per-tab refresh loop.
///
/// `refresh_descriptor = None` means the exchange response did not include
/// both a refresh_token AND a refresh_url. The binding still installs; the
/// tab just won't get its JWT rotated — the operator will see a 401 after
/// the initial JWT TTL and can re-click from the Portal.
#[derive(Debug)]
pub struct HelmExchangeOutcome {
    pub binding: HelmEndpointBinding,
    pub refresh_descriptor: Option<HelmRefreshDescriptor>,
}

/// Refresh inputs carried alongside the immutable endpoint binding so the
/// per-tab refresh loop can rotate the JWT without ever consulting global
/// state. The descriptor lives only on the owning PaneGroup for the tab's
/// lifetime — it is never written to disk, never logged, and never read by
/// any other code path (hw-o8h).
#[derive(Debug, Clone)]
pub struct HelmRefreshDescriptor {
    /// Portal refresh endpoint URL.
    pub refresh_url: String,
    /// Long-lived refresh token used to mint fresh agent JWTs.
    pub refresh_token: String,
    /// Endpoint identity this descriptor is bound to. The refresh loop
    /// refuses to rotate the JWT if a refresh response names a different
    /// endpoint — that's the cross-endpoint retarget hw-o8h closes.
    pub endpoint_identity: EndpointIdentity,
}

impl HelmRefreshDescriptor {
    /// Construct from raw fields. Returns `None` when any required field is
    /// blank — fail closed at the boundary, never pass partial data to the
    /// background loop.
    pub fn new(
        refresh_url: Option<&str>,
        refresh_token: Option<&str>,
        endpoint_identity: EndpointIdentity,
    ) -> Option<Self> {
        let url = refresh_url.map(str::trim).filter(|s| !s.is_empty())?;
        let token = refresh_token.map(str::trim).filter(|s| !s.is_empty())?;
        Some(Self {
            refresh_url: url.to_string(),
            refresh_token: token.to_string(),
            endpoint_identity,
        })
    }
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
) -> Result<HelmExchangeOutcome> {
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
) -> Result<HelmExchangeOutcome> {
    if response.endpoint_id.trim() != expected_endpoint_id.trim() {
        anyhow::bail!(
            "exchange endpoint mismatch (requested={}, returned={})",
            expected_endpoint_id,
            response.endpoint_id
        );
    }
    let binding = helm_tab_binding::freeze_remote(
        Some(&response.endpoint_id),
        Some(&response.endpoint_friendly_label),
        Some(&response.endpoint_hostname),
        Some(&response.endpoint_os),
        Some(&response.helm_oz_url),
        Some(&response.agent_token),
    )
    .context("validating exchange response")?;

    let refresh_descriptor = HelmRefreshDescriptor::new(
        response.refresh_url.as_deref(),
        response.refresh_token.as_deref(),
        binding.endpoint_identity_owned(),
    );

    Ok(HelmExchangeOutcome {
        binding,
        refresh_descriptor,
    })
}

// =============================================================================
// Refresh helpers — pure, no I/O, no logging, no global state.
//
// The refresh loop reads `RefreshResponse` from the Portal and asks
// [`decide_refresh_action`] what to do. The function is split out so the
// decision logic (validate fields + check endpoint identity) can be unit
// tested without a network or executor, and so the background loop stays
// tiny.
// =============================================================================

/// Default refresh interval: 80% of the 300s agent-token TTL = 240s.
/// Public so tests and config can reference the same constant.
pub const REFRESH_INTERVAL_SECS: u64 = 240;

/// Network timeout for a single refresh call.
pub const REFRESH_HTTP_TIMEOUT_SECS: u64 = 15;

/// Decision returned by [`decide_refresh_action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshDecision {
    /// Same endpoint, all required fields present. Apply the new token.
    Rotate { fresh_agent_token: String },
    /// Response is missing one or more required fields. Fail closed.
    MalformedResponse { missing: Vec<&'static str> },
    /// Response named a different endpoint. Refuse and stop the loop.
    DifferentEndpoint {
        existing: String,
        attempted: Option<String>,
    },
}

/// Pure: classify a refresh response against the frozen endpoint identity.
///
/// `frozen` is the endpoint identity stored on the PaneGroup's binding at
/// freeze time. The refresh loop MUST refuse to apply a token from a
/// different endpoint — that's the cross-endpoint retarget hw-o8h closes.
pub fn decide_refresh_action(
    frozen: &EndpointIdentity,
    response_agent_token: Option<&str>,
    response_endpoint_id: Option<&str>,
) -> RefreshDecision {
    let mut missing: Vec<&'static str> = Vec::new();
    let token = response_agent_token
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if token.is_none() {
        missing.push("agent_token");
    }
    let endpoint_id = response_endpoint_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if endpoint_id.is_none() {
        missing.push("endpoint_id");
    }
    if !missing.is_empty() {
        return RefreshDecision::MalformedResponse { missing };
    }

    let response_endpoint = endpoint_id.expect("checked Some above");
    if response_endpoint != frozen.endpoint_id {
        return RefreshDecision::DifferentEndpoint {
            existing: frozen.endpoint_id.clone(),
            attempted: Some(response_endpoint),
        };
    }

    RefreshDecision::Rotate {
        fresh_agent_token: token.expect("checked Some above"),
    }
}

/// Pure: classify a refresh response, then attempt to apply the rotation
/// against the slot. Returns `true` iff the chain should continue (Rotate +
/// Ok). Any other outcome means the loop must stop.
///
/// This is the only place where `try_refresh_token` is called — keeping it
/// here means the slot-level same-endpoint guard is the single source of
/// truth (it enforces the contract regardless of which caller reaches in).
///
/// Side effect (hw-ek5): on a successful same-endpoint rotation the slot's
/// connection state is reset to `Connected` (via `try_refresh_token`).
/// Otherwise the loop should be considered stopped because the binding is
/// no longer trustworthy and the next refresh is unlikely to succeed.
pub fn apply_refresh_to_slot(
    slot: &HelmTabBinding,
    frozen: &EndpointIdentity,
    response: &RefreshResponse,
) -> bool {
    match decide_refresh_action(
        frozen,
        Some(&response.agent_token),
        response.endpoint_id.as_deref(),
    ) {
        RefreshDecision::Rotate { fresh_agent_token } => slot
            .try_refresh_token(frozen, &fresh_agent_token)
            .is_ok(),
        RefreshDecision::DifferentEndpoint { .. } => {
            // Mark stale and refuse: the portal is minting a token for a
            // different endpoint, which violates the immutable binding
            // contract. The loop will stop after this returns.
            slot.set_connection_state(HelmConnectionState::Stale);
            false
        }
        RefreshDecision::MalformedResponse { .. } => {
            slot.set_connection_state(HelmConnectionState::Stale);
            false
        }
    }
}

/// Mark the slot's connection state as [`HelmConnectionState::Stale`] and
/// return. Called from the background refresh chain whenever the loop
/// reaches a stop condition that doesn't involve a Portal response
/// (HTTP non-success body, parse failure, client build failure). The loop
/// continues to retry transport-only failures (`Http`) without marking
/// stale because those are network blips, not endpoint state.
pub fn mark_slot_stale(slot: &HelmTabBinding) {
    slot.set_connection_state(HelmConnectionState::Stale);
}

// =============================================================================
// Per-tab refresh loop — minimal, pane-group-keyed.
//
// Called once, from inside the `ViewContext<PaneGroup>` that just froze the
// binding. Each step:
//
//   1. Background future: sleep REFRESH_INTERVAL_SECS, then POST the
//      Portal refresh endpoint.
//   2. Main-thread callback: apply the result via [`apply_refresh_to_slot`]
//      on the PaneGroup's slot. If Rotated → spawn the next step. Else →
//      stop and let the chain die.
//
// The `ctx.spawn` is keyed to the PaneGroup view: when the user closes the
// tab, the view is dropped and the framework silently drops the pending
// callback (no panic, no leak). No abort handle is stored — the chain
// terminates naturally on tab close.
//
// No launch.json write, no global state, no logged tokens. The slot's
// same-endpoint guard is the single source of truth on identity.
// =============================================================================

/// One step of the refresh chain. Background future — sleeps then POSTs.
/// Returns the parsed body or a typed error.
async fn helm_refresh_step(
    descriptor: HelmRefreshDescriptor,
) -> Result<RefreshResponse, RefreshStepError> {
    tokio::time::sleep(std::time::Duration::from_secs(REFRESH_INTERVAL_SECS)).await;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(REFRESH_HTTP_TIMEOUT_SECS))
        .build()
        .map_err(RefreshStepError::ClientBuild)?;

    let resp = client
        .post(&descriptor.refresh_url)
        .json(&serde_json::json!({
            "refresh_token": descriptor.refresh_token,
        }))
        .send()
        .await
        .map_err(RefreshStepError::Http)?;

    if !resp.status().is_success() {
        return Err(RefreshStepError::HttpStatus(resp.status()));
    }
    resp.json::<RefreshResponse>()
        .await
        .map_err(RefreshStepError::Parse)
}

/// Categorized error for one refresh step. Distinguishes "retry next cycle"
/// (transport) from "stop" (HTTP non-success, parse failure).
#[derive(Debug)]
#[allow(dead_code)] // field values are diagnostic-only; loop only branches on the variant
pub enum RefreshStepError {
    ClientBuild(reqwest::Error),
    Http(reqwest::Error),
    HttpStatus(reqwest::StatusCode),
    Parse(reqwest::Error),
}

/// Begin the refresh chain on `pane_group`. The caller must have just
/// successfully frozen the binding AND produced a non-`None` `descriptor`.
/// Spawns the first step via `ctx.spawn`; each step chains itself on
/// success. Closes naturally when the PaneGroup view is dropped.
pub fn start_helm_refresh_loop(
    descriptor: HelmRefreshDescriptor,
    ctx: &mut warpui::ViewContext<crate::pane_group::PaneGroup>,
) {
    let endpoint_id = descriptor.endpoint_identity.endpoint_id.clone();
    let descriptor_for_step = descriptor.clone();
    let descriptor_for_chain = descriptor;
    ctx.spawn(
        helm_refresh_step(descriptor_for_step),
        move |pane_group, result, ctx| {
            match result {
                Ok(response) => {
                    let continued = apply_refresh_to_slot(
                        pane_group.helm_tab_binding_slot(),
                        &descriptor_for_chain.endpoint_identity,
                        &response,
                    );
                    if continued {
                        // Chain: schedule the next step.
                        start_helm_refresh_loop(
                            descriptor_for_chain,
                            ctx,
                        );
                    } else {
                        log::warn!(
                            "helm-warp: refresh loop stopped for endpoint {endpoint_id} (refused/invalid)"
                        );
                    }
                }
                Err(error) => {
                    use RefreshStepError::*;
                    match error {
                        Http(_) => {
                            // Transient transport blip — retry next cycle.
                            // Do not flip connection state; the loop will
                            // either succeed later (and stay Connected)
                            // or hit a non-transient failure (which will
                            // mark Stale in its own branch).
                            log::warn!(
                                "helm-warp: refresh HTTP failed for endpoint {endpoint_id}; retrying next cycle"
                            );
                            start_helm_refresh_loop(
                                descriptor_for_chain,
                                ctx,
                            );
                        }
                        HttpStatus(status) => {
                            mark_slot_stale(pane_group.helm_tab_binding_slot());
                            log::warn!(
                                "helm-warp: refresh returned {status} for endpoint {endpoint_id}; stopping loop (slot marked Stale)"
                            );
                        }
                        Parse(e) => {
                            mark_slot_stale(pane_group.helm_tab_binding_slot());
                            log::warn!(
                                "helm-warp: refresh response unparseable for endpoint {endpoint_id}: {e:?}; stopping loop (slot marked Stale)"
                            );
                        }
                        ClientBuild(e) => {
                            mark_slot_stale(pane_group.helm_tab_binding_slot());
                            log::warn!(
                                "helm-warp: refresh client build failed for endpoint {endpoint_id}: {e:?}; stopping loop (slot marked Stale)"
                            );
                        }
                    }
                }
            }
        },
    );
}

// =============================================================================
// Tests
// =============================================================================

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
            refresh_token: None,
            refresh_url: None,
        }
    }

    fn complete_response_with_refresh() -> ExchangeResponse {
        let mut r = complete_response();
        r.refresh_token = Some("rt-A".to_string());
        r.refresh_url = Some("https://portal/api/v1/launch/refresh".to_string());
        r
    }

    fn identity_a() -> EndpointIdentity {
        EndpointIdentity::from_binding(&HelmEndpointBinding::for_test(
            "ep-A",
            "Laptop A",
            "laptop-a.local",
            "macos",
            "https://oz.example",
            "jwt-A",
        ))
    }

    #[test]
    fn exchange_response_freezes_complete_binding() {
        let outcome = validate_exchange_response(complete_response(), "ep-A").unwrap();
        assert_eq!(outcome.binding.endpoint_id, "ep-A");
        assert_eq!(outcome.binding.endpoint_friendly_label, "Laptop A");
        assert_eq!(outcome.binding.agent_token, "jwt-A");
        assert!(
            outcome.refresh_descriptor.is_none(),
            "no refresh_token/refresh_url → no descriptor"
        );
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
    fn exchange_response_builds_refresh_descriptor_when_both_fields_present() {
        let outcome =
            validate_exchange_response(complete_response_with_refresh(), "ep-A").unwrap();
        let desc = outcome
            .refresh_descriptor
            .expect("both refresh fields present → Some");
        assert_eq!(desc.refresh_token, "rt-A");
        assert_eq!(desc.refresh_url, "https://portal/api/v1/launch/refresh");
        assert_eq!(desc.endpoint_identity.endpoint_id, "ep-A");
    }

    #[test]
    fn exchange_response_skips_descriptor_when_only_one_refresh_field_present() {
        let mut r = complete_response();
        r.refresh_token = Some("rt-A".to_string());
        r.refresh_url = None;
        let outcome = validate_exchange_response(r, "ep-A").unwrap();
        assert!(
            outcome.refresh_descriptor.is_none(),
            "partial refresh fields must not produce a descriptor"
        );

        let mut r = complete_response();
        r.refresh_token = None;
        r.refresh_url = Some("https://portal/api/v1/launch/refresh".to_string());
        let outcome = validate_exchange_response(r, "ep-A").unwrap();
        assert!(outcome.refresh_descriptor.is_none());
    }

    #[test]
    fn exchange_response_skips_descriptor_when_refresh_fields_are_blank() {
        let mut r = complete_response();
        r.refresh_token = Some("   ".to_string());
        r.refresh_url = Some("https://portal/api/v1/launch/refresh".to_string());
        let outcome = validate_exchange_response(r, "ep-A").unwrap();
        assert!(outcome.refresh_descriptor.is_none(), "blank token → None");
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

    // ---- decide_refresh_action ----

    #[test]
    fn refresh_decision_same_endpoint_rotates() {
        let frozen = identity_a();
        let d = decide_refresh_action(&frozen, Some("jwt-NEW"), Some("ep-A"));
        assert_eq!(
            d,
            RefreshDecision::Rotate {
                fresh_agent_token: "jwt-NEW".into(),
            }
        );
    }

    #[test]
    fn refresh_decision_different_endpoint_refused() {
        let frozen = identity_a();
        let d = decide_refresh_action(&frozen, Some("jwt-NEW"), Some("ep-OTHER"));
        assert_eq!(
            d,
            RefreshDecision::DifferentEndpoint {
                existing: "ep-A".into(),
                attempted: Some("ep-OTHER".into()),
            }
        );
    }

    #[test]
    fn refresh_decision_missing_endpoint_id_refused() {
        let frozen = identity_a();
        let d = decide_refresh_action(&frozen, Some("jwt-NEW"), None);
        assert!(matches!(d, RefreshDecision::MalformedResponse { .. }));
        if let RefreshDecision::MalformedResponse { missing } = d {
            assert!(missing.contains(&"endpoint_id"));
        }
    }

    #[test]
    fn refresh_decision_missing_agent_token_refused() {
        let frozen = identity_a();
        let d = decide_refresh_action(&frozen, None, Some("ep-A"));
        assert!(matches!(d, RefreshDecision::MalformedResponse { .. }));
        if let RefreshDecision::MalformedResponse { missing } = d {
            assert!(missing.contains(&"agent_token"));
        }
    }

    #[test]
    fn refresh_decision_blank_agent_token_refused() {
        let frozen = identity_a();
        let d = decide_refresh_action(&frozen, Some("   "), Some("ep-A"));
        assert!(matches!(d, RefreshDecision::MalformedResponse { .. }));
    }

    #[test]
    fn refresh_decision_lists_every_missing_field() {
        let frozen = identity_a();
        let d = decide_refresh_action(&frozen, None, None);
        if let RefreshDecision::MalformedResponse { missing } = d {
            assert_eq!(missing.len(), 2);
        } else {
            panic!("expected MalformedResponse");
        }
    }

    // ---- mark_slot_stale ----

    #[test]
    fn mark_slot_stale_sets_state() {
        let slot = HelmTabBinding::new();
        assert_eq!(slot.connection_state(), HelmConnectionState::Connected);
        mark_slot_stale(&slot);
        assert_eq!(slot.connection_state(), HelmConnectionState::Stale);
    }

    #[test]
    fn apply_refresh_rotation_clears_prior_stale_state() {
        // The refresh loop may have previously observed a stale condition
        // and stopped; a fresh, valid refresh must reset state to
        // Connected so the next request is allowed through.
        let slot = HelmTabBinding::new();
        slot.freeze_remote(complete_response_binding()).unwrap();
        slot.set_connection_state(HelmConnectionState::Stale);
        assert_eq!(slot.connection_state(), HelmConnectionState::Stale);

        let resp = RefreshResponse {
            agent_token: "jwt-NEW".to_string(),
            endpoint_id: Some("ep-A".to_string()),
        };
        let continued = apply_refresh_to_slot(&slot, &identity_a(), &resp);
        assert!(continued);
        assert_eq!(slot.connection_state(), HelmConnectionState::Connected);
    }

    #[test]
    fn apply_refresh_different_endpoint_marks_stale() {
        let slot = HelmTabBinding::new();
        slot.freeze_remote(complete_response_binding()).unwrap();

        let resp = RefreshResponse {
            agent_token: "jwt-NEW".to_string(),
            endpoint_id: Some("ep-OTHER".to_string()),
        };
        let continued = apply_refresh_to_slot(&slot, &identity_a(), &resp);
        assert!(!continued, "refused by decide_refresh_action");
        assert_eq!(
            slot.connection_state(),
            HelmConnectionState::Stale,
            "DifferentEndpoint must mark the slot Stale so the next request is refused (hw-ek5)"
        );
        // The binding is untouched.
        assert_eq!(slot.get().unwrap().agent_token, "jwt-A");
    }

    #[test]
    fn apply_refresh_malformed_response_marks_stale() {
        let slot = HelmTabBinding::new();
        slot.freeze_remote(complete_response_binding()).unwrap();

        let resp = RefreshResponse {
            agent_token: "   ".to_string(),
            endpoint_id: Some("ep-A".to_string()),
        };
        let continued = apply_refresh_to_slot(&slot, &identity_a(), &resp);
        assert!(!continued);
        assert_eq!(slot.connection_state(), HelmConnectionState::Stale);
        assert_eq!(slot.get().unwrap().agent_token, "jwt-A", "untouched");
    }

    #[test]
    fn apply_refresh_missing_endpoint_id_marks_stale() {
        let slot = HelmTabBinding::new();
        slot.freeze_remote(complete_response_binding()).unwrap();

        let resp = RefreshResponse {
            agent_token: "jwt-NEW".to_string(),
            endpoint_id: None,
        };
        let continued = apply_refresh_to_slot(&slot, &identity_a(), &resp);
        assert!(!continued);
        assert_eq!(slot.connection_state(), HelmConnectionState::Stale);
    }

    // ---- apply_refresh_to_slot ----
    //
    // We don't need a real PaneGroup: the slot is the same primitive the
    // production code mutates. End-to-end "real" tests belong with the
    // pane_group module (and the integration test path); here we just
    // prove the chain decision is right.

    #[test]
    fn apply_refresh_rotates_when_same_endpoint_and_valid_response() {
        let slot = HelmTabBinding::new();
        slot.freeze_remote(complete_response_binding()).unwrap();
        let resp = RefreshResponse {
            agent_token: "jwt-NEW".to_string(),
            endpoint_id: Some("ep-A".to_string()),
        };
        assert!(apply_refresh_to_slot(&slot, &identity_a(), &resp));
        assert_eq!(slot.get().unwrap().agent_token, "jwt-NEW");
    }

    #[test]
    fn apply_refresh_returns_false_and_does_not_mutate_on_different_endpoint() {
        let slot = HelmTabBinding::new();
        slot.freeze_remote(complete_response_binding()).unwrap();
        let resp = RefreshResponse {
            agent_token: "jwt-NEW".to_string(),
            endpoint_id: Some("ep-OTHER".to_string()),
        };
        assert!(!apply_refresh_to_slot(&slot, &identity_a(), &resp));
        assert_eq!(slot.get().unwrap().agent_token, "jwt-A", "untouched");
    }

    #[test]
    fn apply_refresh_returns_false_on_malformed_response() {
        let slot = HelmTabBinding::new();
        slot.freeze_remote(complete_response_binding()).unwrap();
        let resp = RefreshResponse {
            agent_token: "   ".to_string(),
            endpoint_id: Some("ep-A".to_string()),
        };
        assert!(!apply_refresh_to_slot(&slot, &identity_a(), &resp));
        assert_eq!(slot.get().unwrap().agent_token, "jwt-A", "untouched");
    }

    #[test]
    fn apply_refresh_returns_false_when_endpoint_id_missing_from_response() {
        let slot = HelmTabBinding::new();
        slot.freeze_remote(complete_response_binding()).unwrap();
        let resp = RefreshResponse {
            agent_token: "jwt-NEW".to_string(),
            endpoint_id: None,
        };
        assert!(!apply_refresh_to_slot(&slot, &identity_a(), &resp));
        assert_eq!(slot.get().unwrap().agent_token, "jwt-A", "untouched");
    }

    #[test]
    fn refresh_descriptor_new_rejects_blank_fields() {
        let id = identity_a();
        assert!(HelmRefreshDescriptor::new(Some("https://x"), None, id.clone()).is_none());
        assert!(HelmRefreshDescriptor::new(None, Some("rt"), id.clone()).is_none());
        assert!(HelmRefreshDescriptor::new(Some("   "), Some("rt"), id.clone()).is_none());
        assert!(HelmRefreshDescriptor::new(Some("https://x"), Some("  "), id.clone()).is_none());
    }

    #[test]
    fn refresh_descriptor_new_accepts_complete() {
        let id = identity_a();
        let d = HelmRefreshDescriptor::new(Some("https://x"), Some("rt"), id).unwrap();
        assert_eq!(d.refresh_url, "https://x");
        assert_eq!(d.refresh_token, "rt");
    }

    fn complete_response_binding() -> HelmEndpointBinding {
        HelmEndpointBinding::for_test(
            "ep-A",
            "Laptop A",
            "laptop-a.local",
            "macos",
            "https://oz.example",
            "jwt-A",
        )
    }
}
