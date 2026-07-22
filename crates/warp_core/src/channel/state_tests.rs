use super::derive_http_origin_from_ws_url;
use crate::channel::{helm_warp, Channel};

#[test]
fn wss_becomes_https_and_strips_path() {
    let got = derive_http_origin_from_ws_url("wss://rtc.app.warp.dev/graphql/v2");
    assert_eq!(got.as_deref(), Some("https://rtc.app.warp.dev"));
}

#[test]
fn ws_becomes_http_and_preserves_port() {
    let got = derive_http_origin_from_ws_url("ws://localhost:8080/graphql/v2");
    assert_eq!(got.as_deref(), Some("http://localhost:8080"));
}

#[test]
fn unparseable_input_returns_none() {
    assert!(derive_http_origin_from_ws_url("not a url").is_none());
    assert!(derive_http_origin_from_ws_url("https://app.warp.dev").is_none());
}

#[test]
fn url_scheme_per_channel() {
    assert_eq!(Channel::Stable.url_scheme(), "warp");
    assert_eq!(Channel::Preview.url_scheme(), "warppreview");
    assert_eq!(Channel::Dev.url_scheme(), "warpdev");
    assert_eq!(Channel::Local.url_scheme(), "warplocal");
    assert_eq!(Channel::Oss.url_scheme(), "warposs");
    assert_eq!(Channel::Integration.url_scheme(), "warpintegration");
}

#[test]
fn url_schemes_local_claims_helm_warp_first() {
    // Channel::Local registers its primary scheme AND helm-warp, with the
    // channel scheme first so that the install/log order is deterministic.
    let schemes = Channel::Local.url_schemes();
    assert_eq!(schemes, &["warplocal", helm_warp::SCHEME]);
    assert_eq!(schemes[0], "warplocal");
    assert_eq!(schemes[1], "helm-warp");
}

#[test]
fn url_schemes_public_channels_register_only_primary() {
    // User-facing channels must keep their existing single-scheme Windows
    // protocol registration — a regression here would silently drop
    // warp:// / warppreview:// / warpdev:// / warposs:// URLs on Windows.
    // helm-warp is reserved for the internal Helm flow and must NOT be
    // claimed by any user-facing channel, otherwise a Stable user could
    // be routed into a local/dev build via the same scheme.
    let expectations: &[(Channel, &str)] = &[
        (Channel::Stable, "warp"),
        (Channel::Preview, "warppreview"),
        (Channel::Dev, "warpdev"),
        (Channel::Oss, "warposs"),
    ];
    for (channel, primary) in expectations {
        let schemes = channel.url_schemes();
        assert!(
            !schemes.contains(&helm_warp::SCHEME),
            "{channel:?} unexpectedly registered helm-warp: {schemes:?}"
        );
        assert_eq!(
            schemes,
            &[*primary],
            "{channel:?} should register exactly its primary scheme"
        );
    }
}

#[test]
fn url_schemes_integration_registers_primary_only() {
    // Integration tests use a dummy scheme; the runtime plumbing needs the
    // string to round-trip through url::Url::parse, but the test harness
    // never actually installs an OS handler. url_schemes() must still
    // return the primary scheme so callers can switch from url_scheme() to
    // url_schemes() without dropping the integration build's contract.
    assert_eq!(Channel::Integration.url_schemes(), &["warpintegration"]);
}

#[test]
fn url_schemes_exhaustive_pinning() {
    // Pin every channel's full result so a future refactor that drops a
    // primary scheme (or accidentally claims helm-warp on a public channel)
    // is caught here without needing to spot the individual test by name.
    let expectations: &[(Channel, &[&str])] = &[
        (Channel::Stable, &["warp"]),
        (Channel::Preview, &["warppreview"]),
        (Channel::Dev, &["warpdev"]),
        (Channel::Local, &["warplocal", helm_warp::SCHEME]),
        (Channel::Oss, &["warposs"]),
        (Channel::Integration, &["warpintegration"]),
    ];
    for (channel, expected) in expectations {
        assert_eq!(
            channel.url_schemes(),
            *expected,
            "{channel:?} url_schemes() drift"
        );
    }
}

#[test]
fn helm_warp_scheme_constant_matches_runtime() {
    // The packaged constant must match the runtime URL parser. If they ever
    // drift, the OS will route helm-warp:// URLs to the right app but the
    // parser will reject them.
    assert_eq!(helm_warp::SCHEME, "helm-warp");
}

