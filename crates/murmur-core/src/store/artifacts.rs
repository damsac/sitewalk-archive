use rusqlite::Row;

use crate::domain::Artifact;
use crate::error::CoreError;
use crate::ids::new_id;
use crate::store::Store;

const ARTIFACT_COLS: &str =
    "id, session_id, kind, title, body, created_at, updated_at, device_id";

fn artifact_from_row(row: &Row) -> Result<Artifact, CoreError> {
    Ok(Artifact {
        id: row.get("id").map_err(CoreError::Sqlite)?,
        session_id: row.get("session_id").map_err(CoreError::Sqlite)?,
        kind: row.get("kind").map_err(CoreError::Sqlite)?,
        title: row.get("title").map_err(CoreError::Sqlite)?,
        body: row.get("body").map_err(CoreError::Sqlite)?,
        created_at: row.get::<_, i64>("created_at").map_err(CoreError::Sqlite)? as u64,
        updated_at: row.get::<_, i64>("updated_at").map_err(CoreError::Sqlite)? as u64,
        device_id: row.get("device_id").map_err(CoreError::Sqlite)?,
    })
}

impl Store {
    /// The artifact seam (Rev 2 §1): any generated document hangs off a
    /// session. Generators register in Plan 04; the store doesn't care what
    /// `kind` means.
    pub fn add_artifact(
        &self,
        session_id: &str,
        kind: &str,
        title: &str,
        body: &str,
    ) -> Result<Artifact, CoreError> {
        self.get_session(session_id)?;
        let now = self.now();
        let artifact = Artifact {
            id: new_id(),
            session_id: session_id.to_string(),
            kind: kind.to_string(),
            title: title.to_string(),
            body: body.to_string(),
            created_at: now,
            updated_at: now,
            device_id: self.device_id.clone(),
        };
        self.conn.execute(
            "INSERT INTO artifacts (id, session_id, kind, title, body, created_at, updated_at, device_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                artifact.id,
                artifact.session_id,
                artifact.kind,
                artifact.title,
                artifact.body,
                artifact.created_at as i64,
                artifact.updated_at as i64,
                artifact.device_id,
            ],
        )?;
        Ok(artifact)
    }

    /// Voice edits against artifacts ("make that fourteen hundred") land here
    /// via Plan 04 tools; manual edits use the same path (story 10).
    pub fn update_artifact_body(&self, id: &str, body: &str) -> Result<Artifact, CoreError> {
        let changed = self.conn.execute(
            "UPDATE artifacts SET body = ?1, updated_at = ?2 WHERE id = ?3 AND deleted_at IS NULL",
            rusqlite::params![body, self.now() as i64, id],
        )?;
        if changed == 0 {
            return Err(CoreError::NotFound { entity: "artifact", id: id.to_string() });
        }
        self.get_artifact(id)
    }

    pub fn get_artifact(&self, id: &str) -> Result<Artifact, CoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ARTIFACT_COLS} FROM artifacts WHERE id = ?1 AND deleted_at IS NULL"
        ))?;
        let mut rows = stmt.query([id])?;
        match rows.next()? {
            Some(row) => artifact_from_row(row),
            None => Err(CoreError::NotFound { entity: "artifact", id: id.to_string() }),
        }
    }

    pub fn list_artifacts_for_session(&self, session_id: &str) -> Result<Vec<Artifact>, CoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ARTIFACT_COLS} FROM artifacts
             WHERE session_id = ?1 AND deleted_at IS NULL ORDER BY id ASC"
        ))?;
        let mut rows = stmt.query([session_id])?;
        let mut artifacts = Vec::new();
        while let Some(row) = rows.next()? {
            artifacts.push(artifact_from_row(row)?);
        }
        Ok(artifacts)
    }

    /// The session's most-recent `document`-kind artifact, if any (Plan 07 D2).
    /// Scoped by `kind = 'document'` and newest-first so a caller reads *the*
    /// processing document rather than sweeping every artifact and taking the
    /// first hit — a future non-`document` artifact writer can't be misread as
    /// the document, and a re-process (which writes a fresh document after
    /// clearing the prior one) always resolves to the current one.
    pub fn latest_document_artifact(
        &self,
        session_id: &str,
    ) -> Result<Option<Artifact>, CoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ARTIFACT_COLS} FROM artifacts
             WHERE session_id = ?1 AND kind = 'document' AND deleted_at IS NULL
             ORDER BY id DESC LIMIT 1"
        ))?;
        let mut rows = stmt.query([session_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(artifact_from_row(row)?)),
            None => Ok(None),
        }
    }

    pub fn delete_artifact(&self, id: &str) -> Result<(), CoreError> {
        let now = self.now() as i64;
        let changed = self.conn.execute(
            "UPDATE artifacts SET deleted_at = ?1, updated_at = ?1 WHERE id = ?2 AND deleted_at IS NULL",
            rusqlite::params![now, id],
        )?;
        if changed == 0 {
            return Err(CoreError::NotFound { entity: "artifact", id: id.to_string() });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::error::CoreError;
    use crate::store::Store;

    fn store_with_session() -> (Store, String) {
        let s = Store::open_in_memory("device-a").unwrap().with_clock(Arc::new(|| 1000));
        let session = s.start_session(None).unwrap();
        (s, session.id)
    }

    #[test]
    fn add_and_list_for_session() {
        let (s, sid) = store_with_session();
        let report = s
            .add_artifact(&sid, "report", "Johnson walk", "## Summary\nDeck needs work.")
            .unwrap();
        assert_eq!(report.kind, "report");
        let listed = s.list_artifacts_for_session(&sid).unwrap();
        assert_eq!(listed, vec![report]);
    }

    #[test]
    fn add_to_missing_session_is_not_found() {
        let (s, _) = store_with_session();
        assert!(matches!(
            s.add_artifact("nope", "report", "t", "b"),
            Err(CoreError::NotFound { entity: "session", .. })
        ));
    }

    #[test]
    fn get_missing_artifact_is_not_found() {
        let (s, _) = store_with_session();
        assert!(matches!(
            s.get_artifact("nope"),
            Err(CoreError::NotFound { entity: "artifact", .. })
        ));
    }

    #[test]
    fn update_body_touches_updated_at() {
        let (s, sid) = store_with_session();
        let a = s.add_artifact(&sid, "report", "t", "v1").unwrap();
        let s = s.with_clock(Arc::new(|| 2000));
        let a2 = s.update_artifact_body(&a.id, "v2").unwrap();
        assert_eq!(a2.body, "v2");
        assert_eq!(a2.updated_at, 2000);
    }

    #[test]
    fn latest_document_artifact_ignores_other_kinds() {
        let (s, sid) = store_with_session();
        assert!(s.latest_document_artifact(&sid).unwrap().is_none());
        // A non-document artifact must never be misread as the document.
        s.add_artifact(&sid, "report", "r", "markdown").unwrap();
        assert!(s.latest_document_artifact(&sid).unwrap().is_none());
        let doc = s.add_artifact(&sid, "document", "doc #1", "{}").unwrap();
        let found = s.latest_document_artifact(&sid).unwrap().unwrap();
        assert_eq!(found.id, doc.id);
        assert_eq!(found.kind, "document");
    }

    #[test]
    fn delete_artifact_is_a_tombstone() {
        let (s, sid) = store_with_session();
        let a = s.add_artifact(&sid, "report", "t", "b").unwrap();
        s.delete_artifact(&a.id).unwrap();
        assert!(s.list_artifacts_for_session(&sid).unwrap().is_empty());
        assert!(matches!(s.delete_artifact(&a.id), Err(CoreError::NotFound { .. })));
    }
}
