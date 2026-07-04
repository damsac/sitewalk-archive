//! Domain entities (spec §2, Rev 2 §3: Job is first-class; artifacts are a seam).
//! Plain serde data — these types cross the FFI boundary in Plan 07.

use serde::{Deserialize, Serialize};

use crate::error::CoreError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Active,
    Done,
    Archived,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Active => "active",
            JobStatus::Done => "done",
            JobStatus::Archived => "archived",
        }
    }

    pub fn parse(s: &str) -> Result<Self, CoreError> {
        match s {
            "active" => Ok(JobStatus::Active),
            "done" => Ok(JobStatus::Done),
            "archived" => Ok(JobStatus::Archived),
            other => Err(CoreError::Corrupt(format!("unknown job status: {other}"))),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Audio/transcript still coming in.
    Recording,
    /// Ended; queued for the processing pipeline (Plan 04). Offline-safe.
    AwaitingProcessing,
    /// Pipeline finished; summary and artifacts exist.
    Processed,
    /// Pipeline failed; retryable.
    Failed,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionStatus::Recording => "recording",
            SessionStatus::AwaitingProcessing => "awaiting_processing",
            SessionStatus::Processed => "processed",
            SessionStatus::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Result<Self, CoreError> {
        match s {
            "recording" => Ok(SessionStatus::Recording),
            "awaiting_processing" => Ok(SessionStatus::AwaitingProcessing),
            "processed" => Ok(SessionStatus::Processed),
            "failed" => Ok(SessionStatus::Failed),
            other => Err(CoreError::Corrupt(format!("unknown session status: {other}"))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub client: Option<String>,
    pub site: Option<String>,
    /// Unix seconds; None = unscheduled/backlog.
    pub scheduled_at: Option<u64>,
    pub status: JobStatus,
    pub created_at: u64,
    pub updated_at: u64,
    pub device_id: String,
}

#[derive(Clone, Debug, Default)]
pub struct NewJob {
    pub name: String,
    pub client: Option<String>,
    pub site: Option<String>,
    pub scheduled_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub job_id: Option<String>,
    /// Template key selecting extraction vocabulary + document layout
    /// (`landscape` | `property` | `inspection`). Persisted on the session
    /// (Plan 07 D4) so reprocessing stays template-consistent; `None` before
    /// `set_session_template` is called or for pre-migration sessions.
    pub template: Option<String>,
    pub status: SessionStatus,
    pub transcript: String,
    /// Filled by the processing pipeline (Plan 04); also feeds reflection activity.
    pub summary: Option<String>,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
    pub device_id: String,
}

/// Where a captured item came from. Drives the end-of-session swap
/// (`Store::finish_session_processed`): `live` items and *prior-run*
/// `authoritative` items are tombstoned when a new authoritative pass lands;
/// `manual` items are never swept by processing. Free of a migration for new
/// values would be nice, but the swap logic depends on the closed set — keep it
/// closed and parse defensively.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemSource {
    /// Written by a live in-session pass (Plan 05). Provisional; swept on the
    /// next successful process().
    Live,
    /// Written by an end-of-session processing run (Plan 04). The source of
    /// truth once its run finishes.
    Authoritative,
    /// User-entered (story 10 parity) or a direct `add_item`. Never swept by
    /// processing; only a full session delete removes it.
    Manual,
}

impl ItemSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemSource::Live => "live",
            ItemSource::Authoritative => "authoritative",
            ItemSource::Manual => "manual",
        }
    }
    pub fn parse(raw: &str) -> Result<Self, crate::error::CoreError> {
        match raw {
            "live" => Ok(ItemSource::Live),
            "authoritative" => Ok(ItemSource::Authoritative),
            "manual" => Ok(ItemSource::Manual),
            other => Err(crate::error::CoreError::Corrupt(format!(
                "unknown item source: {other}"
            ))),
        }
    }
}

/// A typed item extracted from (or manually added to) a session.
/// `kind` is a free string by design — conventions: "todo", "decision",
/// "note", "safety", "part", "price". New kinds must not require a migration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapturedItem {
    pub id: String,
    pub session_id: String,
    pub kind: String,
    pub text: String,
    pub source: ItemSource,
    pub done: bool,
    pub created_at: u64,
    pub updated_at: u64,
    pub device_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Contact {
    pub id: String,
    pub name: String,
    pub trade: Option<String>,
    pub phone: Option<String>,
    pub notes: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    pub device_id: String,
}

/// The artifact seam (Rev 2 §1): generated documents of any kind hang off a
/// session. `kind` is a free string ("report", "estimate", …); generators
/// register in Plan 04. `body` is markdown (or JSON for structured kinds).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    pub id: String,
    pub session_id: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub device_id: String,
}

/// One LLM call's cost record (R9). Append-only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LlmUsageRow {
    pub id: String,
    pub session_id: Option<String>,
    /// What the tokens bought: "processing" (extraction agent + summary call are
    /// folded into a single row per session by design), "reflection", or future
    /// pipeline phases. "summary" never appears as a standalone purpose.
    pub purpose: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub created_at: u64,
    pub device_id: String,
}

/// Transcript-free projection for lists and queue polling (Plan 03 review:
/// full `Session` structs carry 50-100KB transcripts; lists must not).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub job_id: Option<String>,
    pub status: SessionStatus,
    pub summary: Option<String>,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub transcript_chars: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_source_round_trips_through_str() {
        for s in [ItemSource::Live, ItemSource::Authoritative, ItemSource::Manual] {
            assert_eq!(ItemSource::parse(s.as_str()).unwrap(), s);
        }
        assert!(ItemSource::parse("bogus").is_err());
    }
}
