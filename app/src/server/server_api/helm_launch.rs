//! Gap 4: Helm launch config bridge.
//!
//! Resolves where Warp should send `/ai/multi-agent` requests (and with which
//! bearer) from a structured launch config instead of bare env vars. This is
//! the verifiable substrate for the helm-warp launch flow: a `helm-warp://`
//! URL handler + Portal `/api/v1/launch/exchange` client (the producer, shipped
//! separately) writes `~/.config/helm/launch.json`; Warp reads it here.
//!
//! Precedence (`launch_target`):
//!   1. launch.json `{ helm_oz_url, agent_token }`
//!   2. `HELM_OZ_URL` + `HELM_OZ_BEARER` env (back-compat / dev)
//!   3. `None` → caller falls through to the inline OpenRouter adapter / hosted
//!
//! `~/.config/helm/` is the established Helm config dir (`openbrain.env`).

use std::path::PathBuf;

use serde::Deserialize;

/// Environment override for the launch config path.
const HELM_LAUNCH_CONFIG_ENV: &str = "HELM_LAUNCH_CONFIG";
/// Back-compat env: helm_oz URL (dev bridge, pre-launch.json).
const HELM_OZ_URL_ENV: &str = "HELM_OZ_URL";
/// Back-compat env: agent JWT bearer (dev bridge, pre-launch.json).
const HELM_OZ_BEARER_ENV: &str = "HELM_OZ_BEARER";

/// Default launch config location.
const DEFAULT_LAUNCH_CONFIG_REL: &str = ".config/helm/launch.json";

/// On-disk launch config written by the Portal launch flow (producer).
/// All fields optional so a partial/dev file still loads.
#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)] // schema fields: endpoint_id/endpoint_os/refresh_* are informational
pub struct HelmLaunchConfig {
    /// helm_oz base URL, e.g. `http://127.0.0.1:18080`.
    #[serde(default)]
    pub helm_oz_url: Option<String>,
    /// Agent JWT sent as `Authorization: Bearer` to helm_oz (carries endpoint_id).
    #[serde(default)]
    pub agent_token: Option<String>,
    /// Bound endpoint id (informational; helm_oz derives it from the JWT).
    #[serde(default)]
    pub endpoint_id: Option<String>,
    /// Endpoint OS hint (informational; helm_oz also reads HELM_OZ_ENDPOINT_OS).
    #[serde(default)]
    pub endpoint_os: Option<String>,
    /// Gap 5 refresh token for getting fresh agent JWTs (written by the
    /// helm-warp exchange consumer).
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Portal refresh endpoint URL.
    #[serde(default)]
    pub refresh_url: Option<String>,
}

/// Resolved helm_oz routing target: where to send the request + which bearer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchTarget {
    /// helm_oz base URL (no trailing path).
    pub helm_oz_url: String,
    /// Agent JWT bearer (`None` → local mode in helm_oz).
    pub agent_token: Option<String>,
}

/// Resolve the launch config path: `HELM_LAUNCH_CONFIG` override, else
/// `$HOME/.config/helm/launch.json`. Returns `None` when neither HOME nor the
/// override is set (cannot locate a default).
pub fn launch_config_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var(HELM_LAUNCH_CONFIG_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        return Some(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")?;
    let mut buf = PathBuf::from(home);
    buf.push(DEFAULT_LAUNCH_CONFIG_REL);
    Some(buf)
}

/// Read + parse the launch config from disk. Missing/unparseable file →
/// `None` (fall through to env). Malformed JSON is logged and treated as
/// absent so a corrupt file never blocks the env-var dev path.
pub fn read_launch_config() -> Option<HelmLaunchConfig> {
    let path = launch_config_path()?;
    let bytes = std::fs::read(&path).ok()?;
    match serde_json::from_slice::<HelmLaunchConfig>(&bytes) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            log::warn!(
                "helm: launch config at {} failed to parse ({}); ignoring",
                path.display(),
                e
            );
            None
        }
    }
}

/// Resolve the helm_oz routing target. Precedence: launch.json, then env.
/// Returns `None` when no helm_oz URL is configured anywhere (caller falls
/// through to the OpenRouter adapter / hosted endpoint).
pub fn launch_target() -> Option<LaunchTarget> {
    if let Some(cfg) = read_launch_config() {
        if let Some(url) = non_blank(cfg.helm_oz_url.as_deref()) {
            return Some(LaunchTarget {
                helm_oz_url: url.to_string(),
                agent_token: non_blank(cfg.agent_token.as_deref()).map(str::to_string),
            });
        }
    }
    // Env back-compat: HELM_OZ_URL (+ optional HELM_OZ_BEARER).
    let url = non_blank(std::env::var(HELM_OZ_URL_ENV).ok().as_deref())?.to_string();
    Some(LaunchTarget {
        helm_oz_url: url,
        agent_token: non_blank(std::env::var(HELM_OZ_BEARER_ENV).ok().as_deref())
            .map(str::to_string),
    })
}

/// Env-only helm_oz resolver for local/dev routing. Reads `HELM_OZ_URL` and
/// `HELM_OZ_BEARER`; **never** consults `launch.json`. Used for the
/// local/unbound tab path in `generate_multi_agent_output` so a local
/// tab does not get hijacked into a remote endpoint after any prior
/// launch (hw-o8h).
///
/// Local tabs MUST ignore `~/.config/helm/launch.json` — that file is
/// only meaningful for remote-bound tabs whose binding has been frozen
/// at open time. Reading it per-request for an unbound tab is exactly
/// the cross-talk the per-tab binding closes.
pub fn env_only_local_target() -> Option<LaunchTarget> {
    let url = non_blank(std::env::var(HELM_OZ_URL_ENV).ok().as_deref())?.to_string();
    Some(LaunchTarget {
        helm_oz_url: url,
        agent_token: non_blank(std::env::var(HELM_OZ_BEARER_ENV).ok().as_deref())
            .map(str::to_string),
    })
}

/// Treat `None` or a blank string as absent. Works for both `Option<&str>`
/// (env `as_deref()`) and the config's `Option<String>`.
fn non_blank(s: Option<&str>) -> Option<&str> {
    s.filter(|s| !s.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// Env vars are process-global and tests run in parallel by default; this
    /// lock serializes every test in this module so `set_var`/`remove_var`
    /// don't race. (Mirrors the wave4_integration pattern in helm-v2.)
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn launch_config_path_uses_override_when_set() {
        let _g = env_lock().lock().unwrap();
        std::env::set_var(HELM_LAUNCH_CONFIG_ENV, "/tmp/helm-launch-override.json");
        assert_eq!(
            launch_config_path(),
            Some(PathBuf::from("/tmp/helm-launch-override.json"))
        );
        std::env::remove_var(HELM_LAUNCH_CONFIG_ENV);
    }

    #[test]
    fn launch_config_path_defaults_under_home() {
        let _g = env_lock().lock().unwrap();
        std::env::remove_var(HELM_LAUNCH_CONFIG_ENV);
        std::env::set_var("HOME", "/tmp/helm-fake-home");
        let path = launch_config_path().expect("HOME set → Some path");
        assert!(path.ends_with(".config/helm/launch.json"));
        std::env::remove_var("HOME");
    }

    #[test]
    fn read_launch_config_parses_valid_file() {
        let _g = env_lock().lock().unwrap();
        std::env::set_var("HOME", "/tmp/helm-launch-valid");
        std::env::remove_var(HELM_LAUNCH_CONFIG_ENV);
        let dir = std::path::Path::new("/tmp/helm-launch-valid/.config/helm");
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("launch.json"),
            r#"{"helm_oz_url":"http://127.0.0.1:18080","agent_token":"jwt-A","endpoint_id":"ep-1"}"#,
        )
        .unwrap();
        let cfg = read_launch_config().expect("valid file parses");
        assert_eq!(cfg.helm_oz_url.as_deref(), Some("http://127.0.0.1:18080"));
        assert_eq!(cfg.agent_token.as_deref(), Some("jwt-A"));
        assert_eq!(cfg.endpoint_id.as_deref(), Some("ep-1"));
        std::fs::remove_dir_all("/tmp/helm-launch-valid").ok();
        std::env::remove_var("HOME");
    }

    #[test]
    fn read_launch_config_missing_file_is_none() {
        let _g = env_lock().lock().unwrap();
        std::env::set_var(HELM_LAUNCH_CONFIG_ENV, "/tmp/does-not-exist-launch.json");
        assert!(read_launch_config().is_none());
        std::env::remove_var(HELM_LAUNCH_CONFIG_ENV);
    }

    #[test]
    fn read_launch_config_malformed_json_is_none() {
        let _g = env_lock().lock().unwrap();
        std::env::set_var(HELM_LAUNCH_CONFIG_ENV, "/tmp/helm-bad.json");
        std::fs::write("/tmp/helm-bad.json", "not json {").unwrap();
        assert!(read_launch_config().is_none(), "malformed JSON must not panic");
        std::fs::remove_file("/tmp/helm-bad.json").ok();
        std::env::remove_var(HELM_LAUNCH_CONFIG_ENV);
    }

    #[test]
    fn launch_target_prefers_file_over_env() {
        let _g = env_lock().lock().unwrap();
        std::env::set_var("HOME", "/tmp/helm-prio-file");
        std::env::remove_var(HELM_LAUNCH_CONFIG_ENV);
        std::env::set_var(HELM_OZ_URL_ENV, "http://from-env");
        std::env::set_var(HELM_OZ_BEARER_ENV, "env-bearer");
        let dir = std::path::Path::new("/tmp/helm-prio-file/.config/helm");
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("launch.json"),
            r#"{"helm_oz_url":"http://from-file","agent_token":"file-bearer"}"#,
        )
        .unwrap();
        let target = launch_target().expect("file present → Some");
        assert_eq!(target.helm_oz_url, "http://from-file");
        assert_eq!(target.agent_token.as_deref(), Some("file-bearer"));
        std::fs::remove_dir_all("/tmp/helm-prio-file").ok();
        std::env::remove_var("HOME");
        std::env::remove_var(HELM_OZ_URL_ENV);
        std::env::remove_var(HELM_OZ_BEARER_ENV);
    }

    #[test]
    fn launch_target_falls_back_to_env_when_no_file() {
        let _g = env_lock().lock().unwrap();
        std::env::set_var(HELM_LAUNCH_CONFIG_ENV, "/tmp/no-such-launch.json");
        std::env::set_var(HELM_OZ_URL_ENV, "http://from-env");
        std::env::set_var(HELM_OZ_BEARER_ENV, "env-bearer");
        let target = launch_target().expect("env URL set → Some");
        assert_eq!(target.helm_oz_url, "http://from-env");
        assert_eq!(target.agent_token.as_deref(), Some("env-bearer"));
        std::env::remove_var(HELM_LAUNCH_CONFIG_ENV);
        std::env::remove_var(HELM_OZ_URL_ENV);
        std::env::remove_var(HELM_OZ_BEARER_ENV);
    }

    #[test]
    fn launch_target_none_when_unconfigured() {
        let _g = env_lock().lock().unwrap();
        std::env::set_var(HELM_LAUNCH_CONFIG_ENV, "/tmp/no-such-launch.json");
        std::env::remove_var(HELM_OZ_URL_ENV);
        assert!(launch_target().is_none(), "no file + no env URL → None");
        std::env::remove_var(HELM_LAUNCH_CONFIG_ENV);
    }

    #[test]
    fn launch_target_url_without_bearer_yields_local_mode_target() {
        let _g = env_lock().lock().unwrap();
        std::env::set_var(HELM_LAUNCH_CONFIG_ENV, "/tmp/no-such-launch.json");
        std::env::set_var(HELM_OZ_URL_ENV, "http://from-env");
        std::env::remove_var(HELM_OZ_BEARER_ENV);
        let target = launch_target().expect("env URL set → Some");
        assert_eq!(target.helm_oz_url, "http://from-env");
        assert!(target.agent_token.is_none(), "no bearer → local mode target");
        std::env::remove_var(HELM_LAUNCH_CONFIG_ENV);
        std::env::remove_var(HELM_OZ_URL_ENV);
    }

    // hw-o8h: env_only_local_target must NEVER read launch.json.
    // The local-tab routing path uses this to avoid cross-talk from a
    // prior remote launch.
    #[test]
    fn env_only_local_target_ignores_launch_json() {
        let _g = env_lock().lock().unwrap();
        // Set up a launch.json that would hijack `launch_target()`.
        std::env::set_var("HOME", "/tmp/helm-env-only-test");
        std::env::remove_var(HELM_LAUNCH_CONFIG_ENV);
        let dir = std::path::Path::new("/tmp/helm-env-only-test/.config/helm");
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("launch.json"),
            r#"{"helm_oz_url":"http://from-file","agent_token":"file-bearer"}"#,
        )
        .unwrap();
        // Set env to a different URL.
        std::env::set_var(HELM_OZ_URL_ENV, "http://from-env");
        std::env::set_var(HELM_OZ_BEARER_ENV, "env-bearer");
        let target =
            env_only_local_target().expect("env URL set → Some");
        assert_eq!(
            target.helm_oz_url, "http://from-env",
            "env_only_local_target must NOT read launch.json"
        );
        assert_eq!(target.agent_token.as_deref(), Some("env-bearer"));
        std::fs::remove_dir_all("/tmp/helm-env-only-test").ok();
        std::env::remove_var("HOME");
        std::env::remove_var(HELM_OZ_URL_ENV);
        std::env::remove_var(HELM_OZ_BEARER_ENV);
    }

    #[test]
    fn env_only_local_target_returns_none_without_env() {
        let _g = env_lock().lock().unwrap();
        std::env::set_var(HELM_LAUNCH_CONFIG_ENV, "/tmp/no-such-launch.json");
        std::env::remove_var(HELM_OZ_URL_ENV);
        std::env::remove_var(HELM_OZ_BEARER_ENV);
        assert!(
            env_only_local_target().is_none(),
            "no env URL → None (launch.json MUST NOT be consulted)"
        );
        std::env::remove_var(HELM_LAUNCH_CONFIG_ENV);
    }
}
