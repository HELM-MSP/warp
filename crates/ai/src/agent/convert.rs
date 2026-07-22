#[derive(thiserror::Error, Debug)]
pub enum ConvertToAPITypeError {
    /// There is no API type for the given value.
    ///
    /// This means the value just be ignored during request construction.
    #[error("Ignoring value when constructing API type.")]
    Ignore,
    #[error("Conversion from type {0} is unimplemented.")]
    Unimplemented(String),
    /// Fail-closed typed error emitted before any local / hosted /
    /// OpenRouter / helm_oz route when the helm tab binding lookup could
    /// not locate the owning PaneGroup for the request's terminal view
    /// (hw-o8h). Distinct from [`Self::Ignore`] (which silently drops
    /// the request) and from [`Self::Other`] (which is a generic
    /// anyhow-wrapping fallback): the caller MUST surface this to the
    /// user and MUST NOT fall through to any non-bound route, because
    /// we have no authoritative answer about what tab the request is for.
    #[error("helm tab binding lookup could not locate the owning PaneGroup for the request's terminal view (hw-o8h fail-closed)")]
    HelmTabLookupFailed,
    /// Fail-closed typed error emitted before any local / hosted /
    /// OpenRouter / helm_oz route when the helm tab binding's connection
    /// state is not `Connected` (hw-ek5). The binding is still installed
    /// locally — operator must wait for the next refresh, or open a new
    /// tab via `helm-warp://connect?...`. The caller MUST surface this
    /// to the user and MUST NOT fall through to a non-bound route.
    #[error(
        "helm tab endpoint {endpoint_id} is in connection state '{state}' (must be 'connected' before agent requests can be sent; hw-ek5 fail-closed)"
    )]
    HelmTabNotConnected {
        endpoint_id: String,
        state: String,
    },
    #[error("Encountered error converting types for MultiAgentApi request: {0:?}")]
    Other(#[from] anyhow::Error),
}

/// Unexpected errors when trying to convert an [`api::message::ToolCall`] to an [`AIAgentAction`].
#[derive(Debug, thiserror::Error)]
pub enum ToolToAIAgentActionError {
    #[error("Missing tool")]
    MissingTool,
    #[error("Could not parse args for MCP tool call: {0}")]
    CallMCPToolArgsError(String),
    #[error("Error converting suggest prompt tool call: {0}")]
    SuggestPromptError(String),
    #[error("Required coordinates for computer use action were missing")]
    MissingComputerUseCoordinates,
    #[error("Required scroll distance for mouse wheel action was missing")]
    MissingComputerUseScrollDistance,
    #[error("Received missing computer use action type")]
    MissingComputerUseActionType,
    #[error("Wait duration must be non-negative")]
    InvalidComputerUseWaitDuration,
    #[error("Required key for KeyDown/KeyUp action was missing")]
    MissingComputerUseKey,
    #[error("Character key was empty")]
    InvalidComputerUseCharKey,
    #[error("Received unexpected tool")]
    UnexpectedTool,
    #[error("Missing required reference for read skill tool call")]
    MissingSkillReference,
    #[error("Missing required file reference for upload artifact tool call")]
    MissingUploadArtifactFileReference,
}
