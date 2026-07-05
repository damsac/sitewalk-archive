//! Per-`doc_kind` document-number minting (Plan 07 D5). Core mints the
//! integer; the Swift bridge renders the prefix (`EST-`, `MO-`, `IR-`).
//! Local bookkeeping — same posture as `reflection_state` (no
//! tombstone/sync fields; a counter is device-local in v1).

use crate::domain::Artifact;
use crate::error::CoreError;
use crate::store::Store;

impl Store {
    /// Read-or-insert-then-increment of the per-kind sequence. The caller MUST
    /// hold an open transaction on `self.conn` — this runs bare statements
    /// that join it, so the bump rolls back with whatever the caller aborts.
    fn bump_document_sequence(&self, doc_kind: &str) -> Result<u64, CoreError> {
        let current: Option<i64> = self
            .conn
            .query_row(
                "SELECT next FROM document_sequences WHERE doc_kind = ?1",
                [doc_kind],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        let minted = current.unwrap_or(0) + 1;
        self.conn.execute(
            "INSERT INTO document_sequences (doc_kind, next, device_id) VALUES (?1, ?2, ?3)
             ON CONFLICT(doc_kind) DO UPDATE SET next = ?2",
            rusqlite::params![doc_kind, minted, self.device_id],
        )?;
        Ok(minted as u64)
    }

    /// Returns the next document number for `doc_kind`, starting at 1 and
    /// incrementing monotonically per kind (independent sequences).
    /// Transactional read-or-insert-then-increment.
    pub fn mint_document_number(&self, doc_kind: &str) -> Result<u64, CoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let minted = self.bump_document_sequence(doc_kind)?;
        tx.commit()?;
        Ok(minted)
    }

    /// Mints (or reuses) the document number and writes the `document`-kind
    /// artifact in ONE transaction: a number is durably consumed if and only
    /// if the artifact lands (Plan 07 carry-note 1 follow-up — a failed write
    /// must not burn a sequence gap). `payload` arrives without `doc_number`;
    /// it is stamped here so the stored body and the sequence can never
    /// disagree. `existing_number` short-circuits the mint for an idempotent
    /// re-process (D5).
    pub fn mint_document_number_and_add_artifact(
        &self,
        session_id: &str,
        doc_kind: &str,
        existing_number: Option<u64>,
        mut payload: serde_json::Value,
    ) -> Result<Artifact, CoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let number = match existing_number {
            Some(n) => n,
            None => self.bump_document_sequence(doc_kind)?,
        };
        payload["doc_number"] = serde_json::json!(number);
        let body = serde_json::to_string(&payload)?;
        // add_artifact runs bare statements on self.conn, so it joins the open
        // transaction; an error drops `tx` and rolls the sequence bump back.
        let artifact =
            self.add_artifact(session_id, "document", &format!("{doc_kind} #{number}"), &body)?;
        tx.commit()?;
        Ok(artifact)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::store::Store;

    fn store() -> Store {
        Store::open_in_memory("device-a").unwrap().with_clock(Arc::new(|| 1000))
    }

    #[test]
    fn mint_is_monotonic_per_kind() {
        let s = store();
        assert_eq!(s.mint_document_number("estimate").unwrap(), 1);
        assert_eq!(s.mint_document_number("estimate").unwrap(), 2);
        assert_eq!(s.mint_document_number("report").unwrap(), 1); // independent sequence
        assert_eq!(s.mint_document_number("estimate").unwrap(), 3);
    }

    #[test]
    fn mint_and_add_writes_number_and_artifact_atomically() {
        let s = store();
        let session = s.start_session(None).unwrap();
        let art = s
            .mint_document_number_and_add_artifact(
                &session.id,
                "estimate",
                None,
                serde_json::json!({"lines": []}),
            )
            .unwrap();
        assert_eq!(art.title, "estimate #1");
        let v: serde_json::Value = serde_json::from_str(&art.body).unwrap();
        assert_eq!(v["doc_number"], 1);
        assert_eq!(s.mint_document_number("estimate").unwrap(), 2, "sequence advanced once");
    }

    #[test]
    fn mint_and_add_reuses_an_existing_number_without_bumping() {
        let s = store();
        let session = s.start_session(None).unwrap();
        let art = s
            .mint_document_number_and_add_artifact(
                &session.id,
                "estimate",
                Some(47),
                serde_json::json!({"lines": []}),
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&art.body).unwrap();
        assert_eq!(v["doc_number"], 47);
        assert_eq!(s.mint_document_number("estimate").unwrap(), 1, "reuse never bumps");
    }

    /// The rollback proof: `add_artifact` fails AFTER the sequence bump (the
    /// session doesn't exist), and the bump must roll back with it — a number
    /// is durably consumed if and only if the artifact lands.
    #[test]
    fn failed_artifact_write_rolls_the_mint_back() {
        let s = store();
        assert!(s
            .mint_document_number_and_add_artifact(
                "no-such-session",
                "estimate",
                None,
                serde_json::json!({"lines": []}),
            )
            .is_err());
        assert_eq!(
            s.mint_document_number("estimate").unwrap(),
            1,
            "the aborted write must not have consumed a number"
        );
    }
}
