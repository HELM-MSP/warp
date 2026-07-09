//! # Helm Multi-Agent Mock
//!
//! An Axum-based mock server for Warp's multi-agent protobuf API. It accepts
//! `warp_multi_agent_api::Request` protobuf bodies and returns a stream of
//! `warp_multi_agent_api::ResponseEvent` responses, mirroring the real Warp
//! `/ai/multi-agent` endpoint.
//!
//! This crate exists to give the Helm OpenRouter adapter (and any future
//! adapters) a local, deterministic test harness that does not require network
//! access or Warp credentials.
//!
//! ## Example
//!
//! ```rust,no_run
//! use helm_multi_agent_mock::MultiAgentMockServer;
//!
//! #[tokio::main]
//! async fn main() {
//!     let server = MultiAgentMockServer::start().await.unwrap();
//!     println!("Mock server running at {}", server.base_url());
//!     // Drive requests against server.base_url() + "/ai/multi-agent"
//! }
//! ```

use std::net::SocketAddr;

use anyhow::{Context as _, Result};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use base64::Engine;
use futures::StreamExt;
use prost::Message;
use tokio::net::TcpListener;
use tracing::{error, info};

/// Mock server handle. Dropping it shuts down the server.
pub struct MultiAgentMockServer {
    base_url: String,
    #[allow(dead_code)]
    shutdown: tokio::sync::oneshot::Sender<()>,
}

impl MultiAgentMockServer {
    /// Starts the mock server on an ephemeral localhost port.
    pub async fn start() -> Result<Self> {
        let router = Self::make_router();
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let listener = TcpListener::bind(addr)
            .await
            .context("failed to bind mock server")?;
        let bound_addr = listener.local_addr()?;
        let base_url = format!("http://{bound_addr}");

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            let serve = axum::serve(listener, router);
            let serve = serve.with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            });
            if let Err(e) = serve.await {
                error!("mock server error: {e:#}");
            }
        });

        info!("Helm multi-agent mock server listening on {base_url}");
        Ok(Self {
            base_url,
            shutdown: shutdown_tx,
        })
    }

    /// Builds the Axum router used by the mock server.
    pub fn make_router() -> Router {
        Router::new()
            .route("/ai/multi-agent", post(handle_multi_agent))
            .route("/agent-mode-evals/multi-agent", post(handle_multi_agent))
            .route("/ai/passive-suggestions", post(handle_multi_agent))
            .with_state(MockState)
    }

    /// Returns the base URL of the running mock server.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

#[derive(Clone)]
struct MockState;

async fn handle_multi_agent(
    State(_state): State<MockState>,
    headers: HeaderMap,
    body: Body,
) -> Result<impl IntoResponse, MockError> {
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());

    if content_type != Some("application/x-protobuf") {
        return Err(MockError::BadRequest(
            "expected application/x-protobuf body".to_string(),
        ));
    }

    let bytes = axum::body::to_bytes(body, 64 * 1024 * 1024)
        .await
        .map_err(|e| MockError::BadRequest(format!("failed to read body: {e}")))?;

    let request = warp_multi_agent_api::Request::decode(bytes)
        .map_err(|e| MockError::BadRequest(format!("failed to decode protobuf: {e}")))?;

    info!(
        "mock server received multi-agent request with {} task(s)",
        request
            .task_context
            .as_ref()
            .map(|t| t.tasks.len())
            .unwrap_or(0)
    );

    let conversation_id = request
        .metadata
        .as_ref()
        .map(|m| m.conversation_id.clone())
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let request_id = uuid::Uuid::new_v4().to_string();
    let run_id = uuid::Uuid::new_v4().to_string();

    let stream = futures::stream::iter(vec![
        make_event(warp_multi_agent_api::response_event::Type::Init(
            warp_multi_agent_api::response_event::StreamInit {
                conversation_id: conversation_id.clone(),
                request_id: request_id.clone(),
                run_id: run_id.clone(),
            },
        )),
        make_event(warp_multi_agent_api::response_event::Type::ClientActions(
            warp_multi_agent_api::response_event::ClientActions {
                actions: vec![warp_multi_agent_api::ClientAction {
                    action: Some(
                        warp_multi_agent_api::client_action::Action::AddMessagesToTask(
                            warp_multi_agent_api::client_action::AddMessagesToTask {
                                task_id: conversation_id.clone(),
                                messages: vec![warp_multi_agent_api::Message {
                                    id: uuid::Uuid::new_v4().to_string(),
                                    task_id: conversation_id.clone(),
                                    request_id: request_id.clone(),
                                    timestamp: Some(prost_types::Timestamp::from(
                                        std::time::SystemTime::now(),
                                    )),
                                    server_message_data: String::new(),
                                    citations: Vec::new(),
                                    message: Some(
                                        warp_multi_agent_api::message::Message::AgentOutput(
                                            warp_multi_agent_api::message::AgentOutput {
                                                text: "Mock agent response from helm_multi_agent_mock.".to_string(),
                                            },
                                        ),
                                    ),
                                }],
                            },
                        ),
                    ),
                }],
            },
        )),
        make_event(warp_multi_agent_api::response_event::Type::Finished(
            warp_multi_agent_api::response_event::StreamFinished {
                reason: Some(warp_multi_agent_api::response_event::stream_finished::Reason::Done(
                    warp_multi_agent_api::response_event::stream_finished::Done {},
                )),
                token_usage: Vec::new(),
                should_refresh_model_config: false,
                request_cost: None,
                conversation_usage_metadata: None,
            },
        )),
    ])
    .map(|event| {
        let encoded = event.encode_to_vec();
        // The real Warp endpoint base64-encodes each protobuf event in SSE data.
        let data = base64::prelude::BASE64_URL_SAFE.encode(&encoded);
        Ok::<_, std::convert::Infallible>(format!("data: \"{data}\"\n\n"))
    });

    let body = Body::from_stream(stream);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "text/event-stream")
        .body(body)
        .unwrap())
}

fn make_event(
    event_type: warp_multi_agent_api::response_event::Type,
) -> warp_multi_agent_api::ResponseEvent {
    warp_multi_agent_api::ResponseEvent {
        r#type: Some(event_type),
    }
}

#[derive(Debug)]
enum MockError {
    BadRequest(String),
}

impl IntoResponse for MockError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            MockError::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
        };
        Response::builder()
            .status(status)
            .body(Body::from(message))
            .unwrap()
    }
}

impl From<anyhow::Error> for MockError {
    fn from(err: anyhow::Error) -> Self {
        MockError::BadRequest(err.to_string())
    }
}
