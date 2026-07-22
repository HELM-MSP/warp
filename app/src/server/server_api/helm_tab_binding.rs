//! Per-tab Helm endpoint binding.
//!
//! ## Why this exists (hw-o8h)
//!
//! When a Warp tab is opened from a `helm-warp://connect?…` URL, the tab is
//! bound to a single endpoint for its entire lifetime. The binding is
//! captured once at tab-open time and frozen — subsequent changes to
//! `launch.json` on disk MUST NOT retarget the tab. To target a different
//! endpoint, the user opens a new tab.
//!
//! The runtime owner is `PaneGroup` (one PaneGroup == one tab). The binding
//! lives ON the PaneGroup, never in a process-global registry. The
//! `SessionContext` carries a snapshot of the owning PaneGroup's binding
//! for the duration of a single request, so the routing decision is
//! deterministic per request and per tab.
//!
//! ## Required fields
//!
//! All six identity/url/token fields must be non-blank for a binding to
//! construct. Anything missing fails closed with a typed
//! `BindingIncomplete` error — no binding is produced, no launch file is
//! written, and the tab-creation path surfaces a `EndpointUnresolved` to
//! the user. There is intentionally no "remote with partial identity"
//! mode (that would be a local tab in disguise and is the failure mode
//! hw-o8h is closing).
//!
//! ## Rebind / refresh
//!
//! * `freeze_remote` — the canonical constructor used at tab open.
//! * `try_rebind_remote` — refuses to overwrite an existing remote binding
//!   with a different endpoint; only allowed when the same endpoint sends
//!   a fresh token. A different endpoint requires a new tab.
//! * `try_refresh_token` — same-endpoint token rotation only.
//!
//! All three operations return `Result` so the caller surfaces typed
//! failures rather than silently keeping stale state.

use std::sync::Arc;

use parking_lot::RwLock;
use serde::Serialize;

/// Stable, cloneable endpoint identity used to compare bindings across
/// rebind/refresh calls. Owns its own Strings so callers (the refresh
/// loop in particular) can carry it without borrowing the parent binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointIdentity {
    pub endpoint_id: String,
    pub endpoint_friendly_label: String,
    pub endpoint_hostname: String,
    pub endpoint_os: String,
    pub helm_oz_url: String,
}

impl EndpointIdentity {
    /// Snapshot the five non-token identity fields from a binding.
    pub fn from_binding(b: &HelmEndpointBinding) -> Self {
        Self {
            endpoint_id: b.endpoint_id.clone(),
            endpoint_friendly_label: b.endpoint_friendly_label.clone(),
            endpoint_hostname: b.endpoint_hostname.clone(),
            endpoint_os: b.endpoint_os.clone(),
            helm_oz_url: b.helm_oz_url.clone(),
        }
    }
}

/// Endpoint identity + routing target frozen at tab-open time.
///
/// Fields are all required (non-blank) for the binding to construct. See
/// [`HelmTabBinding::freeze_remote`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HelmEndpointBinding {
    /// Bound endpoint id (e.g. `ep-abc123`).
    pub endpoint_id: String,
    /// Human-readable label (e.g. `operator-laptop`). Surfaces in the tab title.
    pub endpoint_friendly_label: String,
    /// Endpoint hostname (e.g. `operator-laptop.local`).
    pub endpoint_hostname: String,
    /// Endpoint OS (e.g. `macos`, `linux`, `windows`).
    pub endpoint_os: String,
    /// helm_oz base URL (no trailing path).
    pub helm_oz_url: String,
    /// Agent JWT bearer. Always required for a remote binding — there is
    /// no "remote without bearer" mode.
    pub agent_token: String,
}

impl HelmEndpointBinding {
    /// Construct from explicit fields. Production code should use
    /// [`HelmTabBinding::freeze_remote`] (which threads validation).
    #[cfg(test)]
    pub fn for_test(
        endpoint_id: &str,
        endpoint_friendly_label: &str,
        endpoint_hostname: &str,
        endpoint_os: &str,
        helm_oz_url: &str,
        agent_token: &str,
    ) -> Self {
        Self {
            endpoint_id: endpoint_id.to_string(),
            endpoint_friendly_label: endpoint_friendly_label.to_string(),
            endpoint_hostname: endpoint_hostname.to_string(),
            endpoint_os: endpoint_os.to_string(),
            helm_oz_url: helm_oz_url.to_string(),
            agent_token: agent_token.to_string(),
        }
    }

    /// Owned clone of the endpoint identity (no token). Useful for the
    /// refresh loop, which needs to carry the identity across an await
    /// point without borrowing the binding.
    pub fn endpoint_identity_owned(&self) -> EndpointIdentity {
        EndpointIdentity::from_binding(self)
    }

    /// Snapshot the identity inside an `Arc<HelmEndpointBinding>` — the
    /// refresh loop keeps this around across the refresh call so a
    /// mid-flight rebind on the same tab is visible when we get the
    /// response back.
    pub fn endpoint_identity_arc(binding: &Arc<HelmEndpointBinding>) -> EndpointIdentity {
        EndpointIdentity::from_binding(binding)
    }
}

/// Connection state of a Helm endpoint binding.
///
/// The state is tracked per-tab on the owning `HelmTabBinding` slot so the
/// refresh loop, request router, and tab UI all read the same value for the
/// same tab — there is no process-global counter.
///
/// * `Connected` — the most-recent refresh succeeded, OR no refresh has run
///   yet (a fresh binding is trusted at `freeze_remote` time; refresh has
///   not contradicted that trust).
/// * `Stale` — the refresh loop reached a stop condition: the Portal
///   returned a non-success status, a different endpoint, or a malformed
///   payload. The binding is still installed locally but the next JWT
///   rotation did not land; the agent executor must refuse to send the
///   next request until the operator reconnects (hw-ek5).
/// * `Disconnected` — the endpoint sent an explicit disconnect (for
///   example a `RemoteDisconnected` push from the Portal). No automatic
///   recovery — the tab stays disconnected until the user opens a new
///   tab from `helm-warp://connect?...`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HelmConnectionState {
    #[default]
    Connected,
    Stale,
    Disconnected,
}

impl HelmConnectionState {
    /// `true` iff the binding can carry traffic. `Stale` and `Disconnected`
    /// both refuse — see [`crate::ai::agent::api::generate_multi_agent_output`].
    pub fn is_connected(self) -> bool {
        matches!(self, HelmConnectionState::Connected)
    }

    /// Short label used in tab tooltips and the AI block header.
    pub fn label(self) -> &'static str {
        match self {
            HelmConnectionState::Connected => "connected",
            HelmConnectionState::Stale => "stale",
            HelmConnectionState::Disconnected => "disconnected",
        }
    }
}

/// Per-tab binding: holds zero or one `HelmEndpointBinding` behind a
/// parking_lot RwLock so the refresh loop can swap tokens atomically
/// without disturbing existing readers.
///
/// The slot itself is the storage; no process-global map is consulted at
/// request-routing time. Routing reads only what the owning PaneGroup
/// stores on this slot.
///
/// The slot also tracks the most recent [`HelmConnectionState`] observed
/// by the refresh loop. `freeze_remote` installs `Connected`; the refresh
/// loop updates this state at every step (see `crate::uri::helm_warp`).
#[derive(Debug, Default)]
pub struct HelmTabBinding {
    inner: RwLock<Option<Arc<HelmEndpointBinding>>>,
    connection_state: RwLock<HelmConnectionState>,
}

impl HelmTabBinding {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the current binding snapshot (cheap clone of the Arc).
    pub fn get(&self) -> Option<Arc<HelmEndpointBinding>> {
        self.inner.read().clone()
    }

    /// Returns `true` if a remote binding is installed on this tab.
    pub fn is_remote(&self) -> bool {
        self.inner.read().is_some()
    }

    /// Returns the most recent connection state observed by the refresh
    /// loop. Defaults to `Connected` for a fresh binding. When the tab is
    /// not remote (`is_remote() == false`) callers should not rely on
    /// this — the state is meaningful only when a binding is installed.
    pub fn connection_state(&self) -> HelmConnectionState {
        *self.connection_state.read()
    }

    /// Set the connection state explicitly. Used by the refresh loop in
    /// `crate::uri::helm_warp` and by the future explicit-disconnect path.
    pub fn set_connection_state(&self, state: HelmConnectionState) {
        *self.connection_state.write() = state;
    }

    /// Freeze a remote binding onto a tab. Called exactly once at
    /// tab-open time, immediately after the launch.json / exchange has
    /// produced a valid `HelmEndpointBinding`. Calling twice on the same
    /// tab returns [`BindingError::AlreadyFrozen`] so the caller surfaces
    /// the violation instead of silently overwriting.
    ///
    /// `freeze_remote` resets the connection state to `Connected` — at
    /// freeze time we have just minted a fresh JWT from the portal
    /// exchange and have not yet contradicted that trust.
    pub fn freeze_remote(&self, binding: HelmEndpointBinding) -> Result<(), BindingError> {
        let mut guard = self.inner.write();
        if guard.is_some() {
            return Err(BindingError::AlreadyFrozen);
        }
        *guard = Some(Arc::new(binding));
        drop(guard);
        // Reset connection state in a separate critical section so we don't
        // hold two locks at once (rustc's `clippy::await_holding_lock` would
        // never apply here, but the lock discipline is the same).
        *self.connection_state.write() = HelmConnectionState::Connected;
        Ok(())
    }

    /// Attempt to rebind to a different endpoint. Refused — a frozen
    /// binding cannot be replaced with one pointing at a different
    /// endpoint. To target a different endpoint, open a new tab.
    pub fn try_rebind_remote(&self, candidate: HelmEndpointBinding) -> Result<(), BindingError> {
        let guard = self.inner.read();
        let Some(existing) = guard.as_ref() else {
            return Err(BindingError::NotFrozen);
        };
        if existing.endpoint_identity_owned() != candidate.endpoint_identity_owned() {
            return Err(BindingError::EndpointMismatch {
                existing: existing.endpoint_id.clone(),
                attempted: candidate.endpoint_id.clone(),
            });
        }
        // Same endpoint: rotate the token (refreshing JWT is the only
        // legal mutation here, not identity).
        drop(guard);
        let mut guard = self.inner.write();
        *guard = Some(Arc::new(candidate));
        Ok(())
    }

    /// Same-endpoint token rotation. The endpoint identity must match the
    /// currently-frozen binding; a different endpoint returns
    /// [`BindingError::EndpointMismatch`].
    pub fn try_refresh_token(
        &self,
        endpoint_identity: &EndpointIdentity,
        fresh_token: &str,
    ) -> Result<(), BindingError> {
        if fresh_token.trim().is_empty() {
            return Err(BindingError::Incomplete(BindingIncomplete {
                missing: vec!["agent_token"],
            }));
        }
        let mut guard = self.inner.write();
        let Some(existing) = guard.as_ref() else {
            return Err(BindingError::NotFrozen);
        };
        if existing.endpoint_identity_owned() != *endpoint_identity {
            return Err(BindingError::EndpointMismatch {
                existing: existing.endpoint_id.clone(),
                attempted: endpoint_identity.endpoint_id.clone(),
            });
        }
        let mut next = (**existing).clone();
        next.agent_token = fresh_token.to_string();
        *guard = Some(Arc::new(next));
        drop(guard);
        // A successful same-endpoint JWT rotation confirms the endpoint is
        // still connected from our perspective; reset any prior Stale flag
        // the refresh loop may have parked here.
        *self.connection_state.write() = HelmConnectionState::Connected;
        Ok(())
    }

    /// Clear the binding (e.g. on tab close). Idempotent.
    pub fn clear(&self) {
        let mut guard = self.inner.write();
        *guard = None;
        drop(guard);
        *self.connection_state.write() = HelmConnectionState::Connected;
    }
}

/// Returned by `freeze_remote` / `try_rebind_remote` / `try_refresh_token`
/// when the operation cannot proceed safely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingError {
    /// Attempted to freeze a binding on a tab that already has one.
    /// Per-tab binding is immutable — open a new tab for a new endpoint.
    AlreadyFrozen,
    /// Attempted to rebind/refresh on a tab that has no frozen binding.
    NotFrozen,
    /// Attempted to rebind to a different endpoint. Refused — open a new
    /// tab.
    EndpointMismatch {
        existing: String,
        attempted: String,
    },
    /// Constructed binding was missing one or more required fields.
    Incomplete(BindingIncomplete),
}

impl std::fmt::Display for BindingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindingError::AlreadyFrozen => f.write_str("helm tab binding already frozen"),
            BindingError::NotFrozen => f.write_str("helm tab binding not frozen"),
            BindingError::EndpointMismatch { existing, attempted } => write!(
                f,
                "helm tab endpoint rebind refused (frozen={existing}, attempted={attempted})"
            ),
            BindingError::Incomplete(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BindingError {}

/// One-or-more required fields were blank/missing at construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingIncomplete {
    pub missing: Vec<&'static str>,
}

impl std::fmt::Display for BindingIncomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "helm tab binding incomplete (missing: {:?})", self.missing)
    }
}

impl std::error::Error for BindingIncomplete {}

/// Validate the six required fields and produce a `HelmEndpointBinding`,
/// or fail closed with [`BindingIncomplete`].
///
/// Used at tab-open time after the launch.json / exchange has produced
/// the candidate values. A missing required field means we will NOT write
/// the launch file and will NOT open the tab.
pub fn freeze_remote(
    endpoint_id: Option<&str>,
    endpoint_friendly_label: Option<&str>,
    endpoint_hostname: Option<&str>,
    endpoint_os: Option<&str>,
    helm_oz_url: Option<&str>,
    agent_token: Option<&str>,
) -> Result<HelmEndpointBinding, BindingIncomplete> {
    let mut missing = Vec::new();
    let endpoint_id = take(endpoint_id, "endpoint_id", &mut missing);
    let endpoint_friendly_label = take(
        endpoint_friendly_label,
        "endpoint_friendly_label",
        &mut missing,
    );
    let endpoint_hostname = take(endpoint_hostname, "endpoint_hostname", &mut missing);
    let endpoint_os = take(endpoint_os, "endpoint_os", &mut missing);
    let helm_oz_url = take(helm_oz_url, "helm_oz_url", &mut missing);
    let agent_token = take(agent_token, "agent_token", &mut missing);
    if !missing.is_empty() {
        return Err(BindingIncomplete { missing });
    }
    Ok(HelmEndpointBinding {
        endpoint_id,
        endpoint_friendly_label,
        endpoint_hostname,
        endpoint_os,
        helm_oz_url,
        agent_token,
    })
}

fn take(field: Option<&str>, name: &'static str, missing: &mut Vec<&'static str>) -> String {
    match field.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => {
            missing.push(name);
            String::new()
        }
    }
}

/// Format the helm tab title as `<friendly_label> · <hostname>` (hw-ek5).
///
/// Pure: produces the same string for the same inputs, regardless of
/// connection state, so it can be unit tested without a `HelmTabBinding`.
///
/// Returns `None` for a binding missing either input — callers should
/// not set a helm title without those fields.
pub fn format_helm_tab_title(
    friendly_label: &str,
    hostname: &str,
) -> Option<String> {
    let label = friendly_label.trim();
    let host = hostname.trim();
    if label.is_empty() || host.is_empty() {
        return None;
    }
    Some(format!("{label} · {host}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_args() -> (
        Option<&'static str>,
        Option<&'static str>,
        Option<&'static str>,
        Option<&'static str>,
        Option<&'static str>,
        Option<&'static str>,
    ) {
        (
            Some("ep-1"),
            Some("operator-laptop"),
            Some("laptop.local"),
            Some("macos"),
            Some("http://127.0.0.1:18080"),
            Some("jwt-A"),
        )
    }

    fn freeze_remote_from_tuple(
        fields: (
            Option<&str>,
            Option<&str>,
            Option<&str>,
            Option<&str>,
            Option<&str>,
            Option<&str>,
        ),
    ) -> Result<HelmEndpointBinding, BindingIncomplete> {
        freeze_remote(fields.0, fields.1, fields.2, fields.3, fields.4, fields.5)
    }

    #[test]
    fn freeze_remote_with_all_required_fields_succeeds() {
        let b = freeze_remote_from_tuple(full_args()).expect("complete args");
        assert_eq!(b.endpoint_id, "ep-1");
        assert_eq!(b.endpoint_friendly_label, "operator-laptop");
        assert_eq!(b.endpoint_hostname, "laptop.local");
        assert_eq!(b.endpoint_os, "macos");
        assert_eq!(b.helm_oz_url, "http://127.0.0.1:18080");
        assert_eq!(b.agent_token, "jwt-A");
    }

    #[test]
    fn freeze_remote_missing_agent_token_fails_closed() {
        let mut a = full_args();
        a.5 = None;
        let err = freeze_remote_from_tuple(a).unwrap_err();
        assert_eq!(err.missing, vec!["agent_token"]);
    }

    #[test]
    fn freeze_remote_blank_string_fails_closed() {
        let a = (
            Some("ep-1"),
            Some("   "),
            Some("laptop.local"),
            Some("macos"),
            Some("http://127.0.0.1:18080"),
            Some("jwt-A"),
        );
        let err = freeze_remote_from_tuple(a).unwrap_err();
        assert_eq!(err.missing, vec!["endpoint_friendly_label"]);
    }

    #[test]
    fn freeze_remote_lists_every_missing_field() {
        let err = freeze_remote(None, None, None, None, None, None).unwrap_err();
        assert_eq!(err.missing.len(), 6);
    }

    #[test]
    fn slot_starts_empty() {
        let slot = HelmTabBinding::new();
        assert!(!slot.is_remote());
        assert!(slot.get().is_none());
    }

    #[test]
    fn slot_freeze_remote_installs_binding_once() {
        let slot = HelmTabBinding::new();
        let b = freeze_remote_from_tuple(full_args()).unwrap();
        slot.freeze_remote(b.clone()).unwrap();
        assert!(slot.is_remote());
        assert_eq!(slot.get().unwrap().endpoint_id, "ep-1");

        // Second freeze refuses.
        let b2 = freeze_remote_from_tuple(full_args()).unwrap();
        let err = slot.freeze_remote(b2).unwrap_err();
        assert_eq!(err, BindingError::AlreadyFrozen);
    }

    #[test]
    fn slot_try_rebind_to_same_endpoint_rotates_token() {
        let slot = HelmTabBinding::new();
        slot.freeze_remote(freeze_remote_from_tuple(full_args()).unwrap()).unwrap();

        // Same endpoint, fresh token: allowed.
        let candidate = HelmEndpointBinding::for_test(
            "ep-1",
            "operator-laptop",
            "laptop.local",
            "macos",
            "http://127.0.0.1:18080",
            "jwt-REFRESHED",
        );
        slot.try_rebind_remote(candidate).unwrap();
        assert_eq!(slot.get().unwrap().agent_token, "jwt-REFRESHED");
    }

    #[test]
    fn slot_try_rebind_to_different_endpoint_refused() {
        let slot = HelmTabBinding::new();
        slot.freeze_remote(freeze_remote_from_tuple(full_args()).unwrap()).unwrap();

        // Different endpoint: refused.
        let candidate = HelmEndpointBinding::for_test(
            "ep-OTHER",
            "operator-laptop",
            "laptop.local",
            "macos",
            "http://127.0.0.1:18080",
            "jwt-OTHER",
        );
        let err = slot.try_rebind_remote(candidate).unwrap_err();
        assert_eq!(
            err,
            BindingError::EndpointMismatch {
                existing: "ep-1".into(),
                attempted: "ep-OTHER".into(),
            }
        );
        // Original binding still in place.
        assert_eq!(slot.get().unwrap().endpoint_id, "ep-1");
        assert_eq!(slot.get().unwrap().agent_token, "jwt-A");
    }

    fn identity_for_full_args() -> EndpointIdentity {
        EndpointIdentity::from_binding(&freeze_remote_from_tuple(full_args()).unwrap())
    }

    fn identity_other() -> EndpointIdentity {
        EndpointIdentity {
            endpoint_id: "ep-OTHER".to_string(),
            endpoint_friendly_label: "operator-laptop".to_string(),
            endpoint_hostname: "laptop.local".to_string(),
            endpoint_os: "macos".to_string(),
            helm_oz_url: "http://127.0.0.1:18080".to_string(),
        }
    }

    #[test]
    fn slot_refresh_token_same_endpoint_rotates() {
        let slot = HelmTabBinding::new();
        slot.freeze_remote(freeze_remote_from_tuple(full_args()).unwrap()).unwrap();

        slot.try_refresh_token(&identity_for_full_args(), "jwt-REFRESHED")
            .unwrap();
        assert_eq!(slot.get().unwrap().agent_token, "jwt-REFRESHED");
    }

    #[test]
    fn slot_refresh_token_different_endpoint_refused() {
        let slot = HelmTabBinding::new();
        slot.freeze_remote(freeze_remote_from_tuple(full_args()).unwrap()).unwrap();

        let err = slot
            .try_refresh_token(&identity_other(), "jwt-REFRESHED")
            .unwrap_err();
        assert!(matches!(err, BindingError::EndpointMismatch { .. }));
        assert_eq!(slot.get().unwrap().agent_token, "jwt-A");
    }

    #[test]
    fn slot_refresh_token_blank_refused() {
        let slot = HelmTabBinding::new();
        slot.freeze_remote(freeze_remote_from_tuple(full_args()).unwrap()).unwrap();

        let err = slot
            .try_refresh_token(&identity_for_full_args(), "  ")
            .unwrap_err();
        assert!(matches!(err, BindingError::Incomplete(_)));
    }

    #[test]
    fn slot_clear_is_idempotent() {
        let slot = HelmTabBinding::new();
        slot.clear();
        assert!(!slot.is_remote());
        slot.freeze_remote(freeze_remote_from_tuple(full_args()).unwrap()).unwrap();
        slot.clear();
        assert!(!slot.is_remote());
        slot.clear();
        assert!(!slot.is_remote());
    }

    #[test]
    fn connection_state_defaults_to_connected() {
        let slot = HelmTabBinding::new();
        assert_eq!(slot.connection_state(), HelmConnectionState::Connected);
    }

    #[test]
    fn connection_state_label_is_stable() {
        assert_eq!(HelmConnectionState::Connected.label(), "connected");
        assert_eq!(HelmConnectionState::Stale.label(), "stale");
        assert_eq!(HelmConnectionState::Disconnected.label(), "disconnected");
    }

    #[test]
    fn connection_state_is_connected_only_for_connected() {
        assert!(HelmConnectionState::Connected.is_connected());
        assert!(!HelmConnectionState::Stale.is_connected());
        assert!(!HelmConnectionState::Disconnected.is_connected());
    }

    #[test]
    fn freeze_remote_resets_state_to_connected() {
        let slot = HelmTabBinding::new();
        slot.set_connection_state(HelmConnectionState::Stale);
        assert_eq!(slot.connection_state(), HelmConnectionState::Stale);

        slot.freeze_remote(freeze_remote_from_tuple(full_args()).unwrap())
            .unwrap();
        // Freeze wins: the new binding has a fresh JWT and is trusted
        // (hw-ek5). The pre-existing Stale flag must be cleared.
        assert_eq!(slot.connection_state(), HelmConnectionState::Connected);
    }

    #[test]
    fn refresh_token_success_resets_state_to_connected() {
        let slot = HelmTabBinding::new();
        slot.freeze_remote(freeze_remote_from_tuple(full_args()).unwrap())
            .unwrap();
        slot.set_connection_state(HelmConnectionState::Stale);

        slot.try_refresh_token(&identity_for_full_args(), "jwt-NEW")
            .unwrap();

        // Same-endpoint token rotation succeeded — endpoint is back.
        assert_eq!(slot.connection_state(), HelmConnectionState::Connected);
    }

    #[test]
    fn refresh_token_failure_does_not_reset_state() {
        let slot = HelmTabBinding::new();
        slot.freeze_remote(freeze_remote_from_tuple(full_args()).unwrap())
            .unwrap();
        slot.set_connection_state(HelmConnectionState::Stale);

        // Refresh refused (different endpoint) — connection state stays
        // whatever the refresh loop set last.
        let err = slot
            .try_refresh_token(&identity_other(), "jwt-NEW")
            .unwrap_err();
        assert!(matches!(err, BindingError::EndpointMismatch { .. }));
        assert_eq!(slot.connection_state(), HelmConnectionState::Stale);
    }

    #[test]
    fn clear_resets_state_to_connected() {
        let slot = HelmTabBinding::new();
        slot.set_connection_state(HelmConnectionState::Disconnected);
        slot.clear();
        assert_eq!(slot.connection_state(), HelmConnectionState::Connected);
    }

    #[test]
    fn two_simultaneous_slots_have_independent_states() {
        // A/B isolation extends to connection state — refreshing A must
        // not flip B's state, and freezing on one slot does not touch the
        // other.
        let slot_a = HelmTabBinding::new();
        let slot_b = HelmTabBinding::new();
        slot_b.set_connection_state(HelmConnectionState::Stale);

        slot_a
            .freeze_remote(freeze_remote_from_tuple(full_args()).unwrap())
            .unwrap();
        assert_eq!(slot_a.connection_state(), HelmConnectionState::Connected);
        assert_eq!(
            slot_b.connection_state(),
            HelmConnectionState::Stale,
            "B's state untouched"
        );

        slot_a.set_connection_state(HelmConnectionState::Stale);
        assert_eq!(
            slot_b.connection_state(),
            HelmConnectionState::Stale,
            "B unchanged after A's transition"
        );
    }

    // ---- format_helm_tab_title (hw-ek5) ----

    #[test]
    fn format_helm_tab_title_combines_label_and_hostname() {
        let title = format_helm_tab_title("operator-laptop", "laptop.local")
            .expect("both inputs non-blank");
        assert_eq!(title, "operator-laptop · laptop.local");
    }

    #[test]
    fn format_helm_tab_title_trims_whitespace() {
        let title = format_helm_tab_title("  laptop  ", "  laptop.local  ")
            .expect("trims to non-empty");
        assert_eq!(title, "laptop · laptop.local");
    }

    #[test]
    fn format_helm_tab_title_returns_none_for_empty_label() {
        assert!(format_helm_tab_title("", "laptop.local").is_none());
        assert!(format_helm_tab_title("   ", "laptop.local").is_none());
    }

    #[test]
    fn format_helm_tab_title_returns_none_for_empty_hostname() {
        assert!(format_helm_tab_title("laptop", "").is_none());
        assert!(format_helm_tab_title("laptop", "   ").is_none());
    }

    #[test]
    fn format_helm_tab_title_returns_none_for_both_blank() {
        assert!(format_helm_tab_title("", "").is_none());
    }

    #[test]
    fn two_simultaneous_slots_have_independent_bindings() {
        // A/B isolation: two tabs, two different endpoints, neither
        // observes the other's state.
        let slot_a = HelmTabBinding::new();
        let slot_b = HelmTabBinding::new();

        let a = HelmEndpointBinding::for_test(
            "ep-A",
            "laptop-A",
            "laptop-a.local",
            "macos",
            "http://a.helm:18080",
            "jwt-A",
        );
        let b = HelmEndpointBinding::for_test(
            "ep-B",
            "laptop-B",
            "laptop-b.local",
            "linux",
            "http://b.helm:18080",
            "jwt-B",
        );
        slot_a.freeze_remote(a).unwrap();
        slot_b.freeze_remote(b).unwrap();

        assert_eq!(slot_a.get().unwrap().endpoint_id, "ep-A");
        assert_eq!(slot_a.get().unwrap().helm_oz_url, "http://a.helm:18080");
        assert_eq!(slot_b.get().unwrap().endpoint_id, "ep-B");
        assert_eq!(slot_b.get().unwrap().helm_oz_url, "http://b.helm:18080");

        // Refresh A doesn't touch B.
        let identity_a = EndpointIdentity::from_binding(&slot_a.get().unwrap());
        slot_a
            .try_refresh_token(&identity_a, "jwt-A-REFRESHED")
            .unwrap();
        assert_eq!(slot_a.get().unwrap().agent_token, "jwt-A-REFRESHED");
        assert_eq!(slot_b.get().unwrap().agent_token, "jwt-B", "B unchanged");
    }
}