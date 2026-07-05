use rusqlite::Row;

use crate::domain::{Session, SessionStatus, SessionSummary};
use crate::error::CoreError;
use crate::ids::new_id;
use crate::store::Store;

const SESSION_COLS: &str =
    "id, job_id, template, status, transcript, summary, started_at, ended_at, created_at, updated_at, device_id";

const SUMMARY_COLS: &str =
    "id, job_id, status, summary, started_at, ended_at, length(transcript) AS transcript_chars";

fn session_from_row(row: &Row) -> Result<Session, CoreError> {
    let status_raw: String = row.get("status").map_err(CoreError::Sqlite)?;
    Ok(Session {
        id: row.get("id").map_err(CoreError::Sqlite)?,
        job_id: row.get("job_id").map_err(CoreError::Sqlite)?,
        template: row.get("template").map_err(CoreError::Sqlite)?,
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

fn summary_from_row(row: &Row) -> Result<SessionSummary, CoreError> {
    let status_raw: String = row.get("status").map_err(CoreError::Sqlite)?;
    Ok(SessionSummary {
        id: row.get("id").map_err(CoreError::Sqlite)?,
        job_id: row.get("job_id").map_err(CoreError::Sqlite)?,
        status: SessionStatus::parse(&status_raw)?,
        summary: row.get("summary").map_err(CoreError::Sqlite)?,
        started_at: row.get::<_, i64>("started_at").map_err(CoreError::Sqlite)? as u64,
        ended_at: row
            .get::<_, Option<i64>>("ended_at")
            .map_err(CoreError::Sqlite)?
            .map(|v| v as u64),
        transcript_chars: row.get::<_, i64>("transcript_chars").map_err(CoreError::Sqlite)? as u64,
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
            template: None,
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

    /// `start_session` + `set_session_template` in ONE transaction (Plan 07
    /// review follow-up): the two writes were separate in the FFI `begin_walk`
    /// path, so a template failure after the insert leaked an unreachable
    /// Recording row with `template = NULL`. Here an error on either write
    /// rolls both back — a session exists with its template, or not at all.
    pub fn start_session_with_template(
        &self,
        job_id: Option<&str>,
        template: &str,
    ) -> Result<Session, CoreError> {
        let tx = self.conn.unchecked_transaction()?;
        // Both calls run bare statements on self.conn, joining the open
        // transaction; an early return drops `tx` and rolls everything back.
        let mut session = self.start_session(job_id)?;
        self.set_session_template(&session.id, template)?;
        tx.commit()?;
        session.template = Some(template.to_string());
        Ok(session)
    }

    /// Persists the template key (`landscape` | `property` | `inspection`)
    /// selecting extraction vocabulary + document layout (Plan 07 D4). Only
    /// valid while the session is still recording — reprocessing must stay
    /// template-consistent, so the key can't be changed after the fact.
    pub fn set_session_template(&self, id: &str, template: &str) -> Result<(), CoreError> {
        let session = self.get_session(id)?;
        if session.status != SessionStatus::Recording {
            return Err(CoreError::InvalidState(format!(
                "cannot set template on a {} session",
                session.status.as_str()
            )));
        }
        self.conn.execute(
            "UPDATE sessions SET template = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![template, self.now() as i64, id],
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

    /// Session library / UI listing without transcripts. Reverse-chron.
    pub fn list_session_summaries(&self) -> Result<Vec<SessionSummary>, CoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SUMMARY_COLS} FROM sessions WHERE deleted_at IS NULL
             ORDER BY started_at DESC, id DESC"
        ))?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(summary_from_row(row)?);
        }
        Ok(out)
    }

    /// Queue polling without transcripts (processing pull, zombie sweep).
    pub fn list_session_summaries_by_status(
        &self,
        status: SessionStatus,
    ) -> Result<Vec<SessionSummary>, CoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SUMMARY_COLS} FROM sessions WHERE status = ?1 AND deleted_at IS NULL
             ORDER BY started_at DESC, id DESC"
        ))?;
        let mut rows = stmt.query([status.as_str()])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(summary_from_row(row)?);
        }
        Ok(out)
    }

    /// The session-end call (Plan 03 review: dual-call contract). Ends the
    /// recording AND records the session for reflection cadence in one place
    /// so callers can't forget the bookkeeping half. Both writes commit in
    /// one transaction — a failure can't leave an AwaitingProcessing session
    /// with an under-counted reflection cadence.
    pub fn end_and_record_session(&self, id: &str) -> Result<Session, CoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let session = self.end_session(id)?;
        self.record_session_completed()?;
        tx.commit()?;
        Ok(session)
    }

    /// Pipeline success exit. In ONE transaction: swap out the old board, mark
    /// the session Processed, and log LLM cost. The swap tombstones every item
    /// for the session that is `source = live` or `source = authoritative` but
    /// was NOT created by this run (`run_item_ids`) — i.e. the prior live board
    /// and any authoritative leftovers from a failed prior run. `manual` items
    /// and this run's own items are never swept. A crash before this commit
    /// leaves the live board intact (nothing was swept); the next successful
    /// run sweeps the stragglers via the same rule.
    pub fn finish_session_processed(
        &self,
        session_id: &str,
        summary: &str,
        usage: &harness::Usage,
        run_item_ids: &[String],
    ) -> Result<Session, CoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let now = self.now() as i64;

        let mut sql = String::from(
            "UPDATE items SET deleted_at = ?1, updated_at = ?1
             WHERE session_id = ?2 AND deleted_at IS NULL
               AND source IN ('live', 'authoritative')",
        );
        // params: [now, session_id, run_item_ids...]
        let mut params: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(now), Box::new(session_id.to_string())];
        if !run_item_ids.is_empty() {
            let placeholders: Vec<String> =
                (0..run_item_ids.len()).map(|i| format!("?{}", i + 3)).collect();
            sql.push_str(&format!(" AND id NOT IN ({})", placeholders.join(", ")));
            for id in run_item_ids {
                params.push(Box::new(id.clone()));
            }
        }
        self.conn.execute(
            &sql,
            rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
        )?;

        let session = self.mark_session_processed(session_id, summary)?;
        self.record_llm_usage(Some(session_id), "processing", usage)?;
        tx.commit()?;
        Ok(session)
    }

    /// Pipeline failure exit: marks the session Failed AND records the LLM
    /// cost in one transaction (R9: cost is logged even on failure).
    pub fn finish_session_failed(
        &self,
        session_id: &str,
        usage: &harness::Usage,
    ) -> Result<(), CoreError> {
        let tx = self.conn.unchecked_transaction()?;
        self.mark_session_failed(session_id)?;
        self.record_llm_usage(Some(session_id), "processing", usage)?;
        tx.commit()?;
        Ok(())
    }

    /// Clears a session's AUTHORITATIVE outputs before a (re)processing attempt
    /// (Phase 0): tombstones items with `source='authoritative'` and ALL
    /// artifacts (artifacts are only ever written by processing, so every one is
    /// authoritative-equivalent). NEVER touches `source='live'` (the safety-net
    /// board that must survive a failed retry, Plan 06a) or `source='manual'`
    /// (the user's own). Bounds duplicate accumulation across repeated FAILED
    /// attempts to a single in-flight attempt's worth. Idempotent pure delete →
    /// crash-safe on retry. Returns rows tombstoned.
    pub fn clear_authoritative_outputs(&self, session_id: &str) -> Result<usize, CoreError> {
        let now = self.now() as i64;
        let tx = self.conn.unchecked_transaction()?;
        self.get_session(session_id)?; // NotFound if missing/tombstoned
        let items = self.conn.execute(
            "UPDATE items SET deleted_at = ?1, updated_at = ?1
             WHERE session_id = ?2 AND deleted_at IS NULL AND source = 'authoritative'",
            rusqlite::params![now, session_id],
        )?;
        let artifacts = self.conn.execute(
            "UPDATE artifacts SET deleted_at = ?1, updated_at = ?1
             WHERE session_id = ?2 AND deleted_at IS NULL",
            rusqlite::params![now, session_id],
        )?;
        tx.commit()?;
        Ok(items + artifacts)
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
    fn session_template_defaults_none_and_round_trips() {
        let s = store();
        let session = s.start_session(None).unwrap();
        assert_eq!(session.template, None);
        s.set_session_template(&session.id, "landscape").unwrap();
        assert_eq!(s.get_session(&session.id).unwrap().template.as_deref(), Some("landscape"));
    }

    #[test]
    fn start_session_with_template_creates_and_persists_atomically() {
        let s = store();
        let session = s.start_session_with_template(None, "landscape").unwrap();
        assert_eq!(session.status, SessionStatus::Recording);
        assert_eq!(session.template.as_deref(), Some("landscape"));
        // Persisted, not just on the returned struct.
        assert_eq!(
            s.get_session(&session.id).unwrap().template.as_deref(),
            Some("landscape")
        );
    }

    #[test]
    fn start_session_with_template_leaves_nothing_behind_on_failure() {
        let s = store();
        // start_session's job validation fails inside the transaction — no
        // session row (with or without a template) may survive.
        assert!(matches!(
            s.start_session_with_template(Some("no-such-job"), "landscape"),
            Err(CoreError::NotFound { entity: "job", .. })
        ));
        assert!(s
            .list_session_summaries_by_status(SessionStatus::Recording)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn set_template_rejects_non_recording() {
        let s = store();
        let session = s.start_session(None).unwrap();
        s.end_and_record_session(&session.id).unwrap();
        assert!(matches!(
            s.set_session_template(&session.id, "landscape"),
            Err(CoreError::InvalidState(_))
        ));
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

    #[test]
    fn summaries_are_light_and_reverse_chron() {
        let s = store().with_clock(Arc::new(|| 100));
        let a = s.start_session(None).unwrap();
        s.append_transcript(&a.id, "0123456789").unwrap();
        let s = s.with_clock(Arc::new(|| 200));
        let b = s.start_session(None).unwrap();

        let summaries = s.list_session_summaries().unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].id, b.id);
        assert_eq!(summaries[1].id, a.id);
        assert_eq!(summaries[1].transcript_chars, 10);
        assert!(summaries[1].summary.is_none());
    }

    #[test]
    fn summaries_by_status_filter() {
        let s = store();
        let a = s.start_session(None).unwrap();
        s.end_session(&a.id).unwrap();
        let _recording = s.start_session(None).unwrap();
        let queued = s.list_session_summaries_by_status(SessionStatus::AwaitingProcessing).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].id, a.id);
    }

    #[test]
    fn end_and_record_session_does_both() {
        let s = store();
        let session = s.start_session(None).unwrap();
        let ended = s.end_and_record_session(&session.id).unwrap();
        assert_eq!(ended.status, SessionStatus::AwaitingProcessing);
        assert_eq!(s.reflection_signals().unwrap().sessions_since_reflection, 1);
    }

    #[test]
    fn finish_processed_swaps_out_live_and_prior_authoritative_keeps_manual_and_this_run() {
        use crate::domain::ItemSource;
        let s = store();
        let session = s.start_session(None).unwrap();
        let sid = session.id.clone();
        // Board before this run: a live item, a prior-run authoritative item,
        // and a manual item.
        let live = s.add_item_with_source(&sid, "todo", "live", ItemSource::Live).unwrap();
        let prior = s.add_item_with_source(&sid, "todo", "prior auth", ItemSource::Authoritative).unwrap();
        let manual = s.add_item_with_source(&sid, "note", "manual", ItemSource::Manual).unwrap();
        // This run creates two authoritative items.
        let a1 = s.add_item_with_source(&sid, "todo", "new 1", ItemSource::Authoritative).unwrap();
        let a2 = s.add_item_with_source(&sid, "safety", "new 2", ItemSource::Authoritative).unwrap();

        s.end_session(&sid).unwrap();
        s.finish_session_processed(&sid, "done", &harness::Usage::default(), &[a1.id.clone(), a2.id.clone()]).unwrap();

        let ids: Vec<String> = s.list_items_for_session(&sid).unwrap().into_iter().map(|i| i.id).collect();
        assert!(ids.contains(&manual.id), "manual survives the swap");
        assert!(ids.contains(&a1.id) && ids.contains(&a2.id), "this-run authoritative survives");
        assert!(!ids.contains(&live.id), "live is swept");
        assert!(!ids.contains(&prior.id), "prior-run authoritative is swept");
        assert_eq!(ids.len(), 3);
        // status + cost still land in the same tx
        assert_eq!(s.get_session(&sid).unwrap().status, SessionStatus::Processed);
        assert_eq!(s.list_llm_usage_for_session(&sid).unwrap().len(), 1);
    }

    #[test]
    fn finish_processed_with_no_run_ids_sweeps_all_live_and_authoritative() {
        use crate::domain::ItemSource;
        let s = store();
        let session = s.start_session(None).unwrap();
        let sid = session.id.clone();
        s.add_item_with_source(&sid, "todo", "live", ItemSource::Live).unwrap();
        let manual = s.add_item_with_source(&sid, "note", "manual", ItemSource::Manual).unwrap();
        s.end_session(&sid).unwrap();
        s.finish_session_processed(&sid, "(empty session)", &harness::Usage::default(), &[]).unwrap();
        let ids: Vec<String> = s.list_items_for_session(&sid).unwrap().into_iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![manual.id], "only manual survives an empty-run swap");
    }

    #[test]
    fn finish_failed_leaves_the_live_board_intact() {
        use crate::domain::ItemSource;
        let s = store();
        let session = s.start_session(None).unwrap();
        let sid = session.id.clone();
        let live = s.add_item_with_source(&sid, "todo", "live", ItemSource::Live).unwrap();
        s.end_session(&sid).unwrap();
        s.finish_session_failed(&sid, &harness::Usage::default()).unwrap();
        let ids: Vec<String> = s.list_items_for_session(&sid).unwrap().into_iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![live.id], "a failed process must not sweep the live board (the whole fix)");
        assert_eq!(s.get_session(&sid).unwrap().status, SessionStatus::Failed);
    }

    #[test]
    fn clear_authoritative_outputs_spares_live_and_manual_and_sweeps_artifacts() {
        use crate::domain::ItemSource;
        let s = store();
        let session = s.start_session(None).unwrap();
        let sid = session.id.clone();
        let live = s.add_item_with_source(&sid, "todo", "live", ItemSource::Live).unwrap();
        let manual = s.add_item_with_source(&sid, "note", "manual", ItemSource::Manual).unwrap();
        s.add_item_with_source(&sid, "todo", "stale auth", ItemSource::Authoritative).unwrap();
        s.add_artifact(&sid, "report", "old", "body").unwrap();

        let cleared = s.clear_authoritative_outputs(&sid).unwrap();
        assert_eq!(cleared, 2, "one authoritative item + one artifact");
        let ids: Vec<String> = s.list_items_for_session(&sid).unwrap().into_iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![live.id, manual.id], "live and manual are spared");
        assert!(s.list_artifacts_for_session(&sid).unwrap().is_empty(), "all artifacts swept");
        // idempotent: nothing left to clear
        assert_eq!(s.clear_authoritative_outputs(&sid).unwrap(), 0);
        // missing session errors
        assert!(matches!(
            s.clear_authoritative_outputs("nope"),
            Err(CoreError::NotFound { entity: "session", .. })
        ));
    }

    #[test]
    fn processed_session_summary_carries_full_lifecycle_fields() {
        let s = store();
        let session = s.start_session(None).unwrap();
        s.append_transcript(&session.id, "walked the deck").unwrap();
        let s = s.with_clock(Arc::new(|| 2000));
        s.end_session(&session.id).unwrap();
        s.mark_session_processed(&session.id, "the summary").unwrap();

        let summaries = s.list_session_summaries().unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, session.id);
        assert_eq!(summaries[0].status, SessionStatus::Processed);
        assert_eq!(summaries[0].summary.as_deref(), Some("the summary"));
        assert_eq!(summaries[0].ended_at, Some(2000));
        assert_eq!(summaries[0].transcript_chars, 15);
    }
}
