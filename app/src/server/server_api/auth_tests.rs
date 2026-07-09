use crate::auth::credentials::{AuthToken, FirebaseToken, RefreshToken};
use crate::server::server_api::openrouter::HELM_OPENROUTER_API_KEY_ENV;
use crate::server::server_api::ServerApi;
use anyhow::Result;
use serial_test::serial;

#[test]
fn test_firebase_token_urls() -> Result<()> {
    let custom_token = FirebaseToken::Custom("ct".to_string());
    let refresh_token = FirebaseToken::Refresh(RefreshToken::new("rt".to_string()));

    assert_eq!(
        custom_token.access_token_url("api_key"),
        "https://identitytoolkit.googleapis.com/v1/accounts:signInWithCustomToken?key=api_key"
    );
    assert_eq!(
        refresh_token.access_token_url("api_key"),
        "https://securetoken.googleapis.com/v1/token?key=api_key"
    );

    assert_eq!(
        custom_token.access_token_request_body(),
        vec![("returnSecureToken", "true"), ("token", "ct")]
    );
    assert_eq!(
        refresh_token.access_token_request_body(),
        vec![("grant_type", "refresh_token"), ("refresh_token", "rt")],
    );

    assert_eq!(
        custom_token.proxy_url("https://staging.warp.dev", "api_key"),
        "https://staging.warp.dev/proxy/customToken?key=api_key"
    );
    assert_eq!(
        refresh_token.proxy_url("https://staging.warp.dev", "api_key"),
        "https://staging.warp.dev/proxy/token?key=api_key"
    );
    Ok(())
}

#[cfg(feature = "skip_login")]
#[test]
fn access_token_skip_login_rejects_bearer_token() {
    let (event_sender, _) = async_channel::unbounded();
    let server_api =
        ServerApi::new_for_test_with_bearer_token(Some("daemon-token".to_string()), event_sender);

    let error = futures::executor::block_on(server_api.access_token()).unwrap_err();

    assert_eq!(
        error.to_string(),
        "skip_login enabled; failing all authenticated requests"
    );
}

#[test]
#[serial]
fn access_token_openrouter_byok_returns_no_auth() {
    let (event_sender, _) = async_channel::unbounded();
    let server_api = ServerApi::new_for_test_with_bearer_token(None, event_sender);

    std::env::set_var(HELM_OPENROUTER_API_KEY_ENV, "sk-or-test-key");
    let token = futures::executor::block_on(server_api.access_token());
    std::env::remove_var(HELM_OPENROUTER_API_KEY_ENV);

    assert!(matches!(token, Ok(AuthToken::NoAuth)), "{token:?}");
}

#[test]
#[serial]
fn access_token_openrouter_empty_key_falls_back_to_missing_credentials() {
    let (event_sender, _) = async_channel::unbounded();
    let server_api = ServerApi::new_for_test_with_bearer_token(None, event_sender);

    std::env::set_var(HELM_OPENROUTER_API_KEY_ENV, "   ");
    let error = futures::executor::block_on(server_api.access_token()).unwrap_err();
    std::env::remove_var(HELM_OPENROUTER_API_KEY_ENV);

    assert_eq!(error.to_string(), "missing authentication credentials");
}
