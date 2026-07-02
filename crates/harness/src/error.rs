#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("provider error: {0}")]
    Provider(String),
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("tool '{name}' failed: {message}")]
    Tool { name: String, message: String },
    #[error("agent exceeded max turns ({0})")]
    MaxTurns(usize),
}
