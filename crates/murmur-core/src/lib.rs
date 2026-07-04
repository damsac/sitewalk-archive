pub mod coordinator;
pub mod domain;
pub mod error;
pub mod ids;
pub mod pipeline;
pub mod reflection;
pub mod store;

pub use coordinator::ReflectionCoordinator;
pub use domain::{
    Artifact, CapturedItem, Contact, Job, JobStatus, ItemSource, LlmUsageRow, NewJob, Session,
    SessionStatus, SessionSummary,
};
pub use error::CoreError;
pub use ids::new_id;
pub use pipeline::live::{LiveExtractOutcome, LiveExtractor};
pub use pipeline::{ProcessOutcome, SessionProcessor};
pub use pipeline::tools::{AddItemTool, UpsertContactTool, WriteReportTool};
pub use store::Store;
