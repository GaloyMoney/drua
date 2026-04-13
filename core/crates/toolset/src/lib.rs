use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::instrument;

use llm::{ToolUseRequest, ToolUseRequestChannel, ToolUseResult};

/// Wraps a `tokio::task::JoinHandle` so that dropping the wrapper aborts the
/// task. Lets the toolset live as long as the owning service struct.
struct OwnedTaskHandle(Option<JoinHandle<()>>);

impl OwnedTaskHandle {
    fn new(inner: JoinHandle<()>) -> Self {
        Self(Some(inner))
    }
}

impl Drop for OwnedTaskHandle {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

/// Long-running service that drains `ToolUseRequest`s off a channel and
/// sends a single `ToolUseResult` (or `ToolUseError`) back per request.
///
/// `init` spawns the worker task immediately and returns the service plus the
/// sender half that producers (e.g. the `Agents` service) should be handed.
/// When the `Toolset` is dropped the worker task is aborted.
pub struct Toolset {
    _handle: OwnedTaskHandle,
}

impl Toolset {
    #[instrument(name = "domain.toolset.init", skip_all)]
    pub async fn init() -> (Self, ToolUseRequestChannel) {
        let (tx, rx) = mpsc::channel(64);
        let handle = tokio::spawn(Self::run(rx));
        (
            Self {
                _handle: OwnedTaskHandle::new(handle),
            },
            tx,
        )
    }

    async fn run(mut requests: mpsc::Receiver<ToolUseRequest>) {
        while let Some(request) = requests.recv().await {
            Self::dispatch(request);
        }
    }

    #[instrument(name = "domain.toolset.dispatch", skip_all)]
    fn dispatch(request: ToolUseRequest) {
        // Stub: respond immediately with a placeholder result. The real
        // implementation will route to upstream MCP servers / built-in tools.
        let result = ToolUseResult {
            tool_use_id: request.tool_use.id.clone(),
            content: format!("(stub) executed tool: {}", request.tool_use.name),
            is_error: false,
        };
        let _ = request.response_channel.send(Ok(result));
    }
}
