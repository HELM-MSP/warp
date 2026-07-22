mod config;
mod state;

use std::fmt;

pub use config::*;
pub use state::*;

/// URL scheme constants used by URL handlers shared across channels.
///
/// Defining the scheme constant here (next to `Channel`) lets packaging and
/// runtime code share a single source of truth — the runtime URL dispatcher
/// in `app/src/uri/helm_warp.rs` matches against this string, and the
/// platform-specific protocol registrars (macOS `Info.plist`, Windows
/// registry, Linux `.desktop`) need to know exactly which strings to claim.
pub mod helm_warp {
    /// The Helm-Warp deep-link scheme (`helm-warp://connect?...`).
    pub const SCHEME: &str = "helm-warp";
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// The official/first-party stable release.
    Stable,
    /// The official/first-party feature preview release.
    Preview,

    /// The internal-only nightly build.
    Dev,
    /// The internal-only HEAD build.
    Local,

    /// The open-source build of Warp.
    Oss,

    /// The integration test build.
    Integration,
}

impl Channel {
    /// Whether or not this channel is for internal use only
    pub fn is_dogfood(&self) -> bool {
        match self {
            Channel::Dev | Channel::Local => true,
            Channel::Stable | Channel::Preview | Channel::Integration | Channel::Oss => false,
        }
    }

    /// Whether this channel honors the `--server-root-url` / `--ws-server-url` /
    /// `--session-sharing-server-url` flags (and their `WARP_*` env-var equivalents).
    ///
    /// Release channels (`Stable`, `Preview`, `Oss`) ignore these overrides so shipped
    /// builds can't be redirected away from their baked-in server URLs. Internal-only channels
    /// (`Dev`, `Local`, `Integration`) continue to honor them for local development and testing.
    pub fn allows_server_url_overrides(&self) -> bool {
        match self {
            Channel::Dev | Channel::Local | Channel::Integration => true,
            Channel::Stable | Channel::Preview | Channel::Oss => false,
        }
    }

    /// Returns the CLI command name corresponding to this channel.
    pub fn cli_command_name(&self) -> &'static str {
        match self {
            Channel::Stable => "oz",
            Channel::Dev => "oz-dev",
            Channel::Preview => "oz-preview",
            Channel::Local => "oz-local",
            Channel::Integration => "oz-integration",
            Channel::Oss => "warp-oss",
        }
    }

    /// Returns the primary URL scheme (e.g. `warplocal`) for this channel.
    pub fn url_scheme(&self) -> &'static str {
        match self {
            Channel::Stable => "warp",
            Channel::Preview => "warppreview",
            Channel::Dev => "warpdev",
            Channel::Local => "warplocal",
            Channel::Oss => "warposs",
            // Dummy value--integration tests shouldn't support URL schemes.
            Channel::Integration => "warpintegration",
        }
    }

    /// Returns all URL schemes this channel wants OS-level handlers
    /// registered for. Always includes the channel's primary scheme (returned
    /// by [`Channel::url_scheme`]) so callers don't need to combine both
    /// pieces themselves. May additionally include cross-cutting schemes the
    /// runtime understands.
    ///
    /// Today, only the `Channel::Local` build ships the `helm-warp` deep-link
    /// alongside its primary scheme so that internal Portal-launched tabs can
    /// route into the developer/HEAD build. Public channels (`Stable`,
    /// `Preview`, `Oss`) deliberately do not claim helm-warp — that protocol
    /// is reserved for the internal Helm flow and must not be exposed
    /// outside of `Local`. `Channel::Integration` keeps its dummy scheme
    /// (`warpintegration`) for symmetry with the historical `url_scheme()`
    /// contract; the test harness never actually installs a Windows/
    /// macOS handler for it, but the runtime plumbing still needs the
    /// string to round-trip through `url::Url::parse`.
    ///
    /// The order is deterministic: the channel scheme is always first.
    pub fn url_schemes(&self) -> &'static [&'static str] {
        const LOCAL: &[&str] = &["warplocal", helm_warp::SCHEME];
        const STABLE: &[&str] = &["warp"];
        const PREVIEW: &[&str] = &["warppreview"];
        const DEV: &[&str] = &["warpdev"];
        const OSS: &[&str] = &["warposs"];
        const INTEGRATION: &[&str] = &["warpintegration"];
        match self {
            Channel::Local => LOCAL,
            Channel::Stable => STABLE,
            Channel::Preview => PREVIEW,
            Channel::Dev => DEV,
            Channel::Oss => OSS,
            Channel::Integration => INTEGRATION,
        }
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Channel::Stable => "stable",
            Channel::Preview => "preview",
            Channel::Dev => "dev",
            Channel::Integration => "integration",
            Channel::Local => "local",
            Channel::Oss => "warp-oss",
        })
    }
}
