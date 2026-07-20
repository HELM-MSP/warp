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

    /// The stable endpoint identity used for refresh/rebind matching.
    /// Two endpoints are the same iff their identity matches; token may
    /// differ across the same endpoint (JWT rotation).
    pub fn endpoint_identity(&self) -> (&str, &str, &str, &str, &str) {
        (
            &self.endpoint_id,
            &self.endpoint_friendly_label,
            &self.endpoint_hostname,
            &self.endpoint_os,
            &self.helm_oz_url,
        )
    }
}

/// Per-tab binding: holds zero or one `HelmEndpointBinding` behind a
/// parking_lot RwLock so the refresh loop can swap tokens atomically
/// without disturbing existing readers.
///
/// The slot itself is the storage; no process-global map is consulted at
/// request-routing time. Routing reads only what the owning PaneGroup
/// stores on this slot.
#[derive(Debug, Default)]
pub struct HelmTabBinding {
    inner: RwLock<Option<Arc<HelmEndpointBinding>>>,
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

    /// Freeze a remote binding onto a tab. Called exactly once at
    /// tab-open time, immediately after the launch.json / exchange has
    /// produced a valid `HelmEndpointBinding`. Calling twice on the same
    /// tab returns [`BindingError::AlreadyFrozen`] so the caller surfaces
    /// the violation instead of silently overwriting.
    pub fn freeze_remote(&self, binding: HelmEndpointBinding) -> Result<(), BindingError> {
        let mut guard = self.inner.write();
        if guard.is_some() {
            return Err(BindingError::AlreadyFrozen);
        }
        *guard = Some(Arc::new(binding));
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
        if existing.endpoint_identity() != candidate.endpoint_identity() {
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
        endpoint_identity: (&str, &str, &str, &str, &str),
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
        if existing.endpoint_identity() != endpoint_identity {
            return Err(BindingError::EndpointMismatch {
                existing: existing.endpoint_id.clone(),
                attempted: endpoint_identity.0.to_string(),
            });
        }
        let mut next = (**existing).clone();
        next.agent_token = fresh_token.to_string();
        *guard = Some(Arc::new(next));
        Ok(())
    }

    /// Clear the binding (e.g. on tab close). Idempotent.
    pub fn clear(&self) {
        let mut guard = self.inner.write();
        *guard = None;
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
    EndpointMismatch { existing: String, attempted: String },
    /// Constructed binding was missing one or more required fields.
    Incomplete(BindingIncomplete),
}

impl std::fmt::Display for BindingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindingError::AlreadyFrozen => f.write_str("helm tab binding already frozen"),
            BindingError::NotFrozen => f.write_str("helm tab binding not frozen"),
            BindingError::EndpointMismatch {
                existing,
                attempted,
            } => write!(
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
        write!(
            f,
            "helm tab binding incomplete (missing: {:?})",
            self.missing
        )
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
        let a: [Option<&str>; 6] = [None, None, None, None, None, None];
        let err = freeze_remote_from_tuple(a).unwrap_err();
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
        slot.freeze_remote(freeze_remote_from_tuple(full_args()).unwrap())
            .unwrap();

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
        slot.freeze_remote(freeze_remote_from_tuple(full_args()).unwrap())
            .unwrap();

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

    #[test]
    fn slot_refresh_token_same_endpoint_rotates() {
        let slot = HelmTabBinding::new();
        slot.freeze_remote(freeze_remote_from_tuple(full_args()).unwrap())
            .unwrap();

        slot.try_refresh_token(
            (
                "ep-1",
                "operator-laptop",
                "laptop.local",
                "macos",
                "http://127.0.0.1:18080",
            ),
            "jwt-REFRESHED",
        )
        .unwrap();
        assert_eq!(slot.get().unwrap().agent_token, "jwt-REFRESHED");
    }

    #[test]
    fn slot_refresh_token_different_endpoint_refused() {
        let slot = HelmTabBinding::new();
        slot.freeze_remote(freeze_remote_from_tuple(full_args()).unwrap())
            .unwrap();

        let err = slot
            .try_refresh_token(
                (
                    "ep-OTHER",
                    "operator-laptop",
                    "laptop.local",
                    "macos",
                    "http://127.0.0.1:18080",
                ),
                "jwt-REFRESHED",
            )
            .unwrap_err();
        assert!(matches!(err, BindingError::EndpointMismatch { .. }));
        assert_eq!(slot.get().unwrap().agent_token, "jwt-A");
    }

    #[test]
    fn slot_refresh_token_blank_refused() {
        let slot = HelmTabBinding::new();
        slot.freeze_remote(freeze_remote_from_tuple(full_args()).unwrap())
            .unwrap();

        let err = slot
            .try_refresh_token(
                (
                    "ep-1",
                    "operator-laptop",
                    "laptop.local",
                    "macos",
                    "http://127.0.0.1:18080",
                ),
                "  ",
            )
            .unwrap_err();
        assert!(matches!(err, BindingError::Incomplete(_)));
    }

    #[test]
    fn slot_clear_is_idempotent() {
        let slot = HelmTabBinding::new();
        slot.clear();
        assert!(!slot.is_remote());
        slot.freeze_remote(freeze_remote_from_tuple(full_args()).unwrap())
            .unwrap();
        slot.clear();
        assert!(!slot.is_remote());
        slot.clear();
        assert!(!slot.is_remote());
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
        slot_a.freeze_remote_from_tuple(a).unwrap();
        slot_b.freeze_remote(b).unwrap();

        assert_eq!(slot_a.get().unwrap().endpoint_id, "ep-A");
        assert_eq!(slot_a.get().unwrap().helm_oz_url, "http://a.helm:18080");
        assert_eq!(slot_b.get().unwrap().endpoint_id, "ep-B");
        assert_eq!(slot_b.get().unwrap().helm_oz_url, "http://b.helm:18080");

        // Refresh A doesn't touch B.
        slot_a
            .try_refresh_token(
                (
                    "ep-A",
                    "laptop-A",
                    "laptop-a.local",
                    "macos",
                    "http://a.helm:18080",
                ),
                "jwt-A-REFRESHED",
            )
            .unwrap();
        assert_eq!(slot_a.get().unwrap().agent_token, "jwt-A-REFRESHED");
        assert_eq!(slot_b.get().unwrap().agent_token, "jwt-B", "B unchanged");
    }
}
