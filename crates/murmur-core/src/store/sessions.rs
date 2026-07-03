use rusqlite::Row;

use crate::domain::{Session, SessionStatus};
use crate::error::CoreError;
use crate::ids::new_id;
use crate::store::Store;

const SESSION_COLS: &str =
    "id, job_id, status, transcript, summary, started_at, ended_at, created_at, updated_at, device_id";

fn session_from_row(row: &Row) -> Result<Session, CoreError> {
    let status_raw: String = row.get("status").map_err(CoreError::Sqlite)?;
    Ok(Session {
        id: row.get("id").map_err(CoreError::Sqlite)?,
        job_id: row.get("job_id").map_err(CoreError::Sqlite)?,
        status: SessionStatus::parse(&status_raw)?,
        transcript: row.get("transcript").map_err(CoreError::Sqlite)?,
        summary: row.get("summary").map_err(CoreError::Sqlite)?,
        started_at: row.get::<_, i64>("started_at").map_err(CoreError::Sqlite)? as u64,
        ended_at: row
            .get::<_, Option<i64>>("ended_at")
            .map_err(CoreError::Sqlite)?
            .map(|v| v as u64),
        created_at: row.get::<_, i64>("created_at").map_err(CoreError::Sqlite)? as u64,
        updated_at: row.get::<_, i64>("updated_at").map_err(CoreError::Sqlite)? as u64,
        device_id: row.get("device_id").map_err(CoreError::Sqlite)?,
    })
}

/// Escapes LIKE metacharacters so user text matches literally.
fn like_pattern(query: &str) -> String {
    let escaped = query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    format!("%{escaped}%")
}

impl Store {
    /// Starts a recording session. `job_id` is optional (R4: no pre-labeling
    /// required — the pipeline can link a job later from content).
    pub fn start_session(&self, job_id: Option<&str>) -> Result<Session, CoreError> {
        if let Some(jid) = job_id {
            self.get_job(jid)?; // validates existence + not tombstoned
        }
        let now = self.now();
        let session = Session {
            id: new_id(),
            job_id: job_id.map(str::to_string),
            status: SessionStatus::Recording,
            transcript: String::new(),
            summary: None,
            started_at: now,
            ended_at: None,
            created_at: now,
            updated_at: now,
            device_id: self.device_id.clone(),
        };
        self.conn.execute(
            "INSERT INTO sessions (id, job_id, status, transcript, started_at, created_at, updated_at, device_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                session.id,
                session.job_id,
                session.status.as_str(),
                session.transcript,
                session.started_at as i64,
                session.created_at as i64,
                session.updated_at as i64,
                session.device_id,
            ],
        )?;
        Ok(session)
    }

    /// Appends a transcript chunk. Transcript persists continuously (spec §6:
    /// a dead battery loses nothing) — call this per STT segment.
    pub fn append_transcript(&self, id: &str, chunk: &str) -> Result<(), CoreError> {
        let session = self.get_session(id)?;
        if session.status != SessionStatus::Recording {
            return Err(CoreError::InvalidState(format!(
                "cannot append transcript to a {} session",
                session.status.as_str()
            )));
        }
        self.conn.execute(
            "UPDATE sessions SET transcript = transcript || ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![chunk, self.now() as i64, id],
        )?;
        Ok(())
    }

    /// Ends recording; the session queues for processing (offline-safe).
    pub fn end_session(&self, id: &str) -> Result<Session, CoreError> {
        let session = self.get_session(id)?;
        if session.status != SessionStatus::Recording {
            return Err(CoreError::InvalidState(format!(
                "cannot end a {} session",
                session.status.as_str()
            )));
        }
        let now = self.now();
        self.conn.execute(
            "UPDATE sessions SET status = ?1, ended_at = ?2, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![SessionStatus::AwaitingProcessing.as_str(), now as i64, id],
        )?;
        self.get_session(id)
    }

    /// Pipeline success (Plan 04 calls this). Summary feeds the session
    /// library and reflection activity.
    pub fn mark_session_processed(&self, id: &str, summary: &str) -> Result<Session, CoreError> {
        self.transition_ended(id, SessionStatus::Processed, Some(summary))
    }

    /// Pipeline failure; retryable.
    pub fn mark_session_failed(&self, id: &str) -> Result<Session, CoreError> {
        self.transition_ended(id, SessionStatus::Failed, None)
    }

    fn transition_ended(
        &self,
        id: &str,
        to: SessionStatus,
        summary: Option<&str>,
    ) -> Result<Session, CoreError> {
        let session = self.get_session(id)?;
        // Allowlist: Processed/Failed are only reachable FROM AwaitingProcessing
        // (first attempt) or Failed (retry path). Processed is terminal.
        match session.status {
            SessionStatus::AwaitingProcessing | SessionStatus::Failed => {}
            SessionStatus::Recording => {
                return Err(CoreError::InvalidState(
                    "session is still recording".to_string(),
                ));
            }
            SessionStatus::Processed => {
                return Err(CoreError::InvalidState(
                    "session already processed".to_string(),
                ));
            }
        }
        match summary {
            Some(text) => self.conn.execute(
                "UPDATE sessions SET status = ?1, summary = ?2, updated_at = ?3 WHERE id = ?4",
                rusqlite::params![to.as_str(), text, self.now() as i64, id],
            )?,
            None => self.conn.execute(
                "UPDATE sessions SET status = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![to.as_str(), self.now() as i64, id],
            )?,
        };
        self.get_session(id)
    }

    pub fn get_session(&self, id: &str) -> Result<Session, CoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE id = ?1 AND deleted_at IS NULL"
        ))?;
        let mut rows = stmt.query([id])?;
        match rows.next()? {
            Some(row) => session_from_row(row),
            None => Err(CoreError::NotFound { entity: "session", id: id.to_string() }),
        }
    }

    /// The session library (story 9): reverse-chronological.
    pub fn list_sessions(&self) -> Result<Vec<Session>, CoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE deleted_at IS NULL
             ORDER BY started_at DESC, id DESC"
        ))?;
        let mut rows = stmt.query([])?;
        let mut sessions = Vec::new();
        while let Some(row) = rows.next()? {
            sessions.push(session_from_row(row)?);
        }
        Ok(sessions)
    }

    /// Sessions linked to one job, reverse-chronological (Plan 04: job detail
    /// screen).
    pub fn list_sessions_by_job(&self, job_id: &str) -> Result<Vec<Session>, CoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE job_id = ?1 AND deleted_at IS NULL
             ORDER BY started_at DESC, id DESC"
        ))?;
        let mut rows = stmt.query([job_id])?;
        let mut sessions = Vec::new();
        while let Some(row) = rows.next()? {
            sessions.push(session_from_row(row)?);
        }
        Ok(sessions)
    }

    /// Session-library text search (story 9) over transcripts and summaries,
    /// newest first. Plain LIKE — case-insensitive for ASCII only; an FTS5
    /// upgrade is the seam if real usage (~100+ sessions) demands it
    /// (research rec #7: wait for evidence).
    pub fn search_sessions(&self, query: &str) -> Result<Vec<Session>, CoreError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let pattern = like_pattern(query);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SESSION_COLS} FROM sessions
             WHERE deleted_at IS NULL
               AND (transcript LIKE ?1 ESCAPE '\\' OR summary LIKE ?1 ESCAPE '\\')
             ORDER BY started_at DESC, id DESC"
        ))?;
        let mut rows = stmt.query([&pattern])?;
        let mut sessions = Vec::new();
        while let Some(row) = rows.next()? {
            sessions.push(session_from_row(row)?);
        }
        Ok(sessions)
    }

    /// Sessions in one state, reverse-chronological. Plan 04's processing
    /// queue pulls `AwaitingProcessing` here; the app-open sweep uses
    /// `Recording` to find zombie sessions left by a crash.
    pub fn list_sessions_by_status(&self, status: SessionStatus) -> Result<Vec<Session>, CoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE status = ?1 AND deleted_at IS NULL
             ORDER BY started_at DESC, id DESC"
        ))?;
        let mut rows = stmt.query([status.as_str()])?;
        let mut sessions = Vec::new();
        while let Some(row) = rows.next()? {
            sessions.push(session_from_row(row)?);
        }
        Ok(sessions)
    }

    /// Tombstones a session AND cascades to its live items and artifacts in
    /// one transaction — deleting a session is a single logical delete
    /// operation (sync story: one op, never a half-cascaded state). Already
    /// tombstoned children are left untouched (their deleted_at is older).
    pub fn delete_session(&self, id: &str) -> Result<(), CoreError> {
        let now = self.now() as i64;
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE sessions SET deleted_at = ?1, updated_at = ?1 WHERE id = ?2 AND deleted_at IS NULL",
            rusqlite::params![now, id],
        )?;
        if changed == 0 {
            return Err(CoreError::NotFound { entity: "session", id: id.to_string() });
        }
        tx.execute(
            "UPDATE items SET deleted_at = ?1, updated_at = ?1 WHERE session_id = ?2 AND deleted_at IS NULL",
            rusqlite::params![now, id],
        )?;
        tx.execute(
            "UPDATE artifacts SET deleted_at = ?1, updated_at = ?1 WHERE session_id = ?2 AND deleted_at IS NULL",
            rusqlite::params![now, id],
        )?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::domain::{NewJob, SessionStatus};
    use crate::error::CoreError;
    use crate::store::Store;

    fn store() -> Store {
        Store::open_in_memory("device-a").unwrap().with_clock(Arc::new(|| 1000))
    }

    #[test]
    fn start_append_end_lifecycle() {
        let s = store();
        let session = s.start_session(None).unwrap();
        assert_eq!(session.status, SessionStatus::Recording);
        assert_eq!(session.started_at, 1000);
        assert!(session.ended_at.is_none());

        s.append_transcript(&session.id, "we need to fix the deck. ").unwrap();
        s.append_transcript(&session.id, "call Dev about the framing.").unwrap();

        let s = s.with_clock(Arc::new(|| 2000));
        let ended = s.end_session(&session.id).unwrap();
        assert_eq!(ended.status, SessionStatus::AwaitingProcessing);
        assert_eq!(ended.ended_at, Some(2000));
        assert_eq!(
            ended.transcript,
            "we need to fix the deck. call Dev about the framing."
        );
    }

    #[test]
    fn start_with_job_links_and_validates() {
        let s = store();
        let job = s
            .create_job(NewJob { name: "j".into(), ..Default::default() })
            .unwrap();
        let session = s.start_session(Some(&job.id)).unwrap();
        assert_eq!(session.job_id.as_deref(), Some(job.id.as_str()));
        assert!(matches!(
            s.start_session(Some("no-such-job")),
            Err(CoreError::NotFound { entity: "job", .. })
        ));
    }

    #[test]
    fn end_requires_recording_state() {
        let s = store();
        let session = s.start_session(None).unwrap();
        s.end_session(&session.id).unwrap();
        assert!(matches!(
            s.end_session(&session.id),
            Err(CoreError::InvalidState(_))
        ));
    }

    #[test]
    fn append_to_ended_session_is_invalid() {
        let s = store();
        let session = s.start_session(None).unwrap();
        s.end_session(&session.id).unwrap();
        assert!(matches!(
            s.append_transcript(&session.id, "late words"),
            Err(CoreError::InvalidState(_))
        ));
    }

    #[test]
    fn mark_processed_sets_summary() {
        let s = store();
        let session = s.start_session(None).unwrap();
        s.end_session(&session.id).unwrap();
        let done = s
            .mark_session_processed(&session.id, "Walked the deck; 2 todos.")
            .unwrap();
        assert_eq!(done.status, SessionStatus::Processed);
        assert_eq!(done.summary.as_deref(), Some("Walked the deck; 2 todos."));
    }

    #[test]
    fn mark_failed_is_retryable_state() {
        let s = store();
        let session = s.start_session(None).unwrap();
        s.end_session(&session.id).unwrap();
        let failed = s.mark_session_failed(&session.id).unwrap();
        assert_eq!(failed.status, SessionStatus::Failed);
    }

    #[test]
    fn processed_is_terminal() {
        let s = store();
        let session = s.start_session(None).unwrap();
        s.end_session(&session.id).unwrap();
        s.mark_session_processed(&session.id, "done.").unwrap();
        assert!(matches!(
            s.mark_session_processed(&session.id, "again?"),
            Err(CoreError::InvalidState(_))
        ));
        assert!(matches!(
            s.mark_session_failed(&session.id),
            Err(CoreError::InvalidState(_))
        ));
    }

    #[test]
    fn failed_session_can_retry_to_processed() {
        let s = store();
        let session = s.start_session(None).unwrap();
        s.end_session(&session.id).unwrap();
        s.mark_session_failed(&session.id).unwrap();
        let done = s.mark_session_processed(&session.id, "retry worked.").unwrap();
        assert_eq!(done.status, SessionStatus::Processed);
        assert_eq!(done.summary.as_deref(), Some("retry worked."));
    }

    #[test]
    fn list_sessions_by_job_filters() {
        let s = store();
        let job_a = s.create_job(NewJob { name: "a".into(), ..Default::default() }).unwrap();
        let job_b = s.create_job(NewJob { name: "b".into(), ..Default::default() }).unwrap();
        let sa1 = s.start_session(Some(&job_a.id)).unwrap();
        let sb1 = s.start_session(Some(&job_b.id)).unwrap();
        s.start_session(None).unwrap(); // unlinked — never listed by job
        let for_a: Vec<_> = s
            .list_sessions_by_job(&job_a.id)
            .unwrap()
            .into_iter()
            .map(|x| x.id)
            .collect();
        assert_eq!(for_a, vec![sa1.id]);
        let for_b: Vec<_> = s
            .list_sessions_by_job(&job_b.id)
            .unwrap()
            .into_iter()
            .map(|x| x.id)
            .collect();
        assert_eq!(for_b, vec![sb1.id]);
    }

    #[test]
    fn library_lists_reverse_chronological() {
        let s = store().with_clock(Arc::new(|| 100));
        let a = s.start_session(None).unwrap();
        let s = s.with_clock(Arc::new(|| 200));
        let b = s.start_session(None).unwrap();
        let ids: Vec<_> = s.list_sessions().unwrap().into_iter().map(|x| x.id).collect();
        assert_eq!(ids, vec![b.id, a.id]);
    }

    #[test]
    fn delete_session_is_a_tombstone() {
        let s = store();
        let session = s.start_session(None).unwrap();
        s.delete_session(&session.id).unwrap();
        assert!(matches!(s.get_session(&session.id), Err(CoreError::NotFound { .. })));
        assert!(s.list_sessions().unwrap().is_empty());
        // the row still exists (tombstone, not erase)
        let raw: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM sessions WHERE id = ?1", [&session.id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(raw, 1);
    }

    #[test]
    fn search_matches_transcript_and_summary() {
        let s = store();
        let a = s.start_session(None).unwrap();
        s.append_transcript(&a.id, "the french drain needs regrading").unwrap();
        s.end_session(&a.id).unwrap();

        let b = s.start_session(None).unwrap();
        s.end_session(&b.id).unwrap();
        s.mark_session_processed(&b.id, "Discussed drain pricing with Johnson").unwrap();

        let c = s.start_session(None).unwrap();
        s.append_transcript(&c.id, "unrelated roofing talk").unwrap();

        let hits: Vec<_> = s.search_sessions("drain").unwrap().into_iter().map(|x| x.id).collect();
        assert_eq!(hits.len(), 2);
        assert!(hits.contains(&a.id) && hits.contains(&b.id));
    }

    #[test]
    fn search_is_case_insensitive_for_ascii() {
        let s = store();
        let a = s.start_session(None).unwrap();
        s.append_transcript(&a.id, "French Drain").unwrap();
        assert_eq!(s.search_sessions("french").unwrap().len(), 1);
    }

    #[test]
    fn search_escapes_like_metacharacters() {
        let s = store();
        let a = s.start_session(None).unwrap();
        s.append_transcript(&a.id, "50% deposit due").unwrap();
        let b = s.start_session(None).unwrap();
        s.append_transcript(&b.id, "500 deposit due").unwrap();
        let hits = s.search_sessions("50%").unwrap();
        assert_eq!(hits.len(), 1, "% must match literally, not as wildcard");
        assert_eq!(hits[0].id, a.id);
    }

    #[test]
    fn search_escapes_underscore_and_backslash() {
        let s = store();
        let a = s.start_session(None).unwrap();
        s.append_transcript(&a.id, "15_min standup").unwrap();
        let b = s.start_session(None).unwrap();
        s.append_transcript(&b.id, "15xmin standup").unwrap();
        let hits = s.search_sessions("15_min").unwrap();
        assert_eq!(hits.len(), 1, "_ must match literally, not as single-char wildcard");
        assert_eq!(hits[0].id, a.id);
        let _ = b;

        let c = s.start_session(None).unwrap();
        s.append_transcript(&c.id, r"path C:\jobs\johnson").unwrap();
        let hits = s.search_sessions(r"\jobs").unwrap();
        assert_eq!(hits.len(), 1, "literal backslash matches only sessions containing one");
        assert_eq!(hits[0].id, c.id);
    }

    #[test]
    fn empty_or_blank_query_returns_nothing() {
        let s = store();
        let a = s.start_session(None).unwrap();
        s.append_transcript(&a.id, "anything").unwrap();
        assert!(s.search_sessions("").unwrap().is_empty());
        assert!(s.search_sessions("   ").unwrap().is_empty());
    }

    #[test]
    fn delete_session_cascades_to_items_and_artifacts() {
        let s = store();
        let session = s.start_session(None).unwrap();
        let item = s.add_item(&session.id, "todo", "order lumber").unwrap();
        let artifact = s.add_artifact(&session.id, "report", "walk", "body").unwrap();

        s.delete_session(&session.id).unwrap();

        assert!(s.list_open_todos().unwrap().is_empty());
        assert!(s.list_items_for_session(&session.id).unwrap().is_empty());
        assert!(s.list_artifacts_for_session(&session.id).unwrap().is_empty());
        // raw rows still exist (tombstones, not erasure)
        let raw_item: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM items WHERE id = ?1", [&item.id], |r| r.get(0))
            .unwrap();
        assert_eq!(raw_item, 1);
        let raw_artifact: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM artifacts WHERE id = ?1", [&artifact.id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(raw_artifact, 1);
    }

    #[test]
    fn list_sessions_by_status_filters() {
        let s = store();
        let recording = s.start_session(None).unwrap();
        let awaiting = s.start_session(None).unwrap();
        s.end_session(&awaiting.id).unwrap();
        let processed = s.start_session(None).unwrap();
        s.end_session(&processed.id).unwrap();
        s.mark_session_processed(&processed.id, "done.").unwrap();

        let ids = |status| -> Vec<String> {
            s.list_sessions_by_status(status)
                .unwrap()
                .into_iter()
                .map(|x| x.id)
                .collect()
        };
        assert_eq!(ids(SessionStatus::Recording), vec![recording.id]);
        assert_eq!(ids(SessionStatus::AwaitingProcessing), vec![awaiting.id]);
        assert_eq!(ids(SessionStatus::Processed), vec![processed.id]);
        assert!(ids(SessionStatus::Failed).is_empty());
    }
}
