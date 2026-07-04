use rusqlite::Row;

use crate::domain::{CapturedItem, ItemSource, SessionStatus};
use crate::error::CoreError;
use crate::ids::new_id;
use crate::store::Store;

const ITEM_COLS: &str =
    "id, session_id, kind, text, source, done, created_at, updated_at, device_id";

fn item_from_row(row: &Row) -> Result<CapturedItem, CoreError> {
    Ok(CapturedItem {
        id: row.get("id").map_err(CoreError::Sqlite)?,
        session_id: row.get("session_id").map_err(CoreError::Sqlite)?,
        kind: row.get("kind").map_err(CoreError::Sqlite)?,
        text: row.get("text").map_err(CoreError::Sqlite)?,
        source: {
            let raw: String = row.get("source").map_err(CoreError::Sqlite)?;
            ItemSource::parse(&raw)?
        },
        done: row.get::<_, i64>("done").map_err(CoreError::Sqlite)? != 0,
        created_at: row.get::<_, i64>("created_at").map_err(CoreError::Sqlite)? as u64,
        updated_at: row.get::<_, i64>("updated_at").map_err(CoreError::Sqlite)? as u64,
        device_id: row.get("device_id").map_err(CoreError::Sqlite)?,
    })
}

impl Store {
    /// Adds an item to a session. Works for agent extraction (Plans 04/05)
    /// and manual entry alike (story 10: manual parity — nothing is agent-only).
    /// A bare add is manual/parity: source=Manual.
    pub fn add_item(&self, session_id: &str, kind: &str, text: &str) -> Result<CapturedItem, CoreError> {
        self.add_item_with_source(session_id, kind, text, ItemSource::Manual)
    }

    /// Adds an item with an explicit source, ungated. The processing pipeline
    /// uses this for `authoritative` writes (it owns the session; no status
    /// gate applies during processing).
    pub fn add_item_with_source(
        &self,
        session_id: &str,
        kind: &str,
        text: &str,
        source: ItemSource,
    ) -> Result<CapturedItem, CoreError> {
        self.get_session(session_id)?; // NotFound if missing/tombstoned
        self.insert_item(session_id, kind, text, source)
    }

    /// Same as `add_item`, but only writes if the session's CURRENT status
    /// matches `required` — the status read and the insert happen against
    /// the same `&self` call with no intervening await, so as long as every
    /// caller shares one `Store` behind a single lock (as `AddItemTool` and
    /// `LiveExtractor` do), no window exists between the check and the
    /// write. Returns `Ok(None)` on a status mismatch (nothing written);
    /// `Ok(Some(item))` on success.
    pub fn add_item_if_status(
        &self,
        session_id: &str,
        kind: &str,
        text: &str,
        required: SessionStatus,
        source: ItemSource,
    ) -> Result<Option<CapturedItem>, CoreError> {
        let session = self.get_session(session_id)?; // NotFound if missing/tombstoned
        if session.status != required {
            return Ok(None);
        }
        self.insert_item(session_id, kind, text, source).map(Some)
    }

    fn insert_item(&self, session_id: &str, kind: &str, text: &str, source: ItemSource)
        -> Result<CapturedItem, CoreError>
    {
        let now = self.now();
        let item = CapturedItem {
            id: new_id(),
            session_id: session_id.to_string(),
            kind: kind.to_string(),
            text: text.to_string(),
            source,
            done: false,
            created_at: now,
            updated_at: now,
            device_id: self.device_id.clone(),
        };
        self.conn.execute(
            "INSERT INTO items (id, session_id, kind, text, source, done, created_at, updated_at, device_id)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7, ?8)",
            rusqlite::params![
                item.id, item.session_id, item.kind, item.text, item.source.as_str(),
                item.created_at as i64, item.updated_at as i64, item.device_id,
            ],
        )?;
        Ok(item)
    }

    pub fn set_item_done(&self, id: &str, done: bool) -> Result<CapturedItem, CoreError> {
        let changed = self.conn.execute(
            "UPDATE items SET done = ?1, updated_at = ?2 WHERE id = ?3 AND deleted_at IS NULL",
            rusqlite::params![done as i64, self.now() as i64, id],
        )?;
        if changed == 0 {
            return Err(CoreError::NotFound { entity: "item", id: id.to_string() });
        }
        self.get_item(id)
    }

    pub fn get_item(&self, id: &str) -> Result<CapturedItem, CoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ITEM_COLS} FROM items WHERE id = ?1 AND deleted_at IS NULL"
        ))?;
        let mut rows = stmt.query([id])?;
        match rows.next()? {
            Some(row) => item_from_row(row),
            None => Err(CoreError::NotFound { entity: "item", id: id.to_string() }),
        }
    }

    /// Items of one session in insertion order (UUIDv7 ids sort by creation).
    pub fn list_items_for_session(&self, session_id: &str) -> Result<Vec<CapturedItem>, CoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ITEM_COLS} FROM items
             WHERE session_id = ?1 AND deleted_at IS NULL ORDER BY id ASC"
        ))?;
        let mut rows = stmt.query([session_id])?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            items.push(item_from_row(row)?);
        }
        Ok(items)
    }

    /// The "morning glance" query (story 1): open todos across all sessions,
    /// oldest first.
    pub fn list_open_todos(&self) -> Result<Vec<CapturedItem>, CoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ITEM_COLS} FROM items
             WHERE kind = 'todo' AND done = 0 AND deleted_at IS NULL ORDER BY id ASC"
        ))?;
        let mut rows = stmt.query([])?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            items.push(item_from_row(row)?);
        }
        Ok(items)
    }

    pub fn delete_item(&self, id: &str) -> Result<(), CoreError> {
        let now = self.now() as i64;
        let changed = self.conn.execute(
            "UPDATE items SET deleted_at = ?1, updated_at = ?1 WHERE id = ?2 AND deleted_at IS NULL",
            rusqlite::params![now, id],
        )?;
        if changed == 0 {
            return Err(CoreError::NotFound { entity: "item", id: id.to_string() });
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
    fn add_and_list_in_insertion_order() {
        let (s, sid) = store_with_session();
        let a = s.add_item(&sid, "todo", "order lumber").unwrap();
        let b = s.add_item(&sid, "safety", "loose railing on deck").unwrap();
        assert_eq!(a.kind, "todo");
        assert!(!a.done);
        let items = s.list_items_for_session(&sid).unwrap();
        assert_eq!(items, vec![a, b]);
    }

    #[test]
    fn add_to_missing_session_is_not_found() {
        let (s, _) = store_with_session();
        assert!(matches!(
            s.add_item("nope", "todo", "x"),
            Err(CoreError::NotFound { entity: "session", .. })
        ));
    }

    #[test]
    fn get_missing_item_is_not_found() {
        let (s, _) = store_with_session();
        assert!(matches!(
            s.get_item("nope"),
            Err(CoreError::NotFound { entity: "item", .. })
        ));
    }

    #[test]
    fn done_toggle_round_trips() {
        let (s, sid) = store_with_session();
        let item = s.add_item(&sid, "todo", "order lumber").unwrap();
        let s = s.with_clock(Arc::new(|| 2000));
        let done = s.set_item_done(&item.id, true).unwrap();
        assert!(done.done);
        assert_eq!(done.updated_at, 2000);
        let undone = s.set_item_done(&item.id, false).unwrap();
        assert!(!undone.done);
    }

    #[test]
    fn open_todos_span_sessions_and_skip_done() {
        let (s, sid_a) = store_with_session();
        let sid_b = s.start_session(None).unwrap().id;
        let t1 = s.add_item(&sid_a, "todo", "one").unwrap();
        let t2 = s.add_item(&sid_b, "todo", "two").unwrap();
        s.add_item(&sid_b, "note", "not a todo").unwrap();
        s.set_item_done(&t1.id, true).unwrap();
        let open: Vec<_> = s.list_open_todos().unwrap().into_iter().map(|i| i.id).collect();
        assert_eq!(open, vec![t2.id]);
    }

    #[test]
    fn add_item_if_status_writes_when_status_matches() {
        use crate::domain::ItemSource;
        use crate::domain::SessionStatus;
        let (s, sid) = store_with_session();
        let item = s.add_item_if_status(&sid, "todo", "order lumber", SessionStatus::Recording, ItemSource::Live).unwrap();
        assert!(item.is_some());
        assert_eq!(s.list_items_for_session(&sid).unwrap().len(), 1);
    }

    #[test]
    fn add_item_if_status_no_ops_when_status_mismatches() {
        use crate::domain::ItemSource;
        use crate::domain::SessionStatus;
        let (s, sid) = store_with_session();
        s.end_and_record_session(&sid).unwrap(); // Recording -> AwaitingProcessing
        let item = s.add_item_if_status(&sid, "todo", "order lumber", SessionStatus::Recording, ItemSource::Live).unwrap();
        assert!(item.is_none());
        assert!(s.list_items_for_session(&sid).unwrap().is_empty());
    }

    #[test]
    fn add_item_defaults_to_manual_source() {
        use crate::domain::ItemSource;
        let (s, sid) = store_with_session();
        let item = s.add_item(&sid, "todo", "order lumber").unwrap();
        assert_eq!(item.source, ItemSource::Manual);
        // round-trips through the DB read
        assert_eq!(s.list_items_for_session(&sid).unwrap()[0].source, ItemSource::Manual);
    }

    #[test]
    fn add_item_with_source_persists_the_source() {
        use crate::domain::ItemSource;
        let (s, sid) = store_with_session();
        let live = s.add_item_with_source(&sid, "todo", "live one", ItemSource::Live).unwrap();
        let auth = s.add_item_with_source(&sid, "todo", "auth one", ItemSource::Authoritative).unwrap();
        assert_eq!(live.source, ItemSource::Live);
        assert_eq!(auth.source, ItemSource::Authoritative);
    }

    #[test]
    fn existing_rows_backfill_as_authoritative() {
        // simulate a pre-migration row: raw insert without a source column value
        let (s, sid) = store_with_session();
        s.conn.execute(
            "INSERT INTO items (id, session_id, kind, text, done, created_at, updated_at, device_id)
             VALUES ('legacy', ?1, 'todo', 'old', 0, 1, 1, 'device-a')",
            [&sid],
        ).unwrap();
        let legacy = s.list_items_for_session(&sid).unwrap()
            .into_iter().find(|i| i.id == "legacy").unwrap();
        assert_eq!(legacy.source, crate::domain::ItemSource::Authoritative,
            "the column DEFAULT backfills rows that predate the source column");
    }

    #[test]
    fn delete_item_is_a_tombstone() {
        let (s, sid) = store_with_session();
        let item = s.add_item(&sid, "todo", "x").unwrap();
        s.delete_item(&item.id).unwrap();
        assert!(s.list_items_for_session(&sid).unwrap().is_empty());
        assert!(matches!(s.delete_item(&item.id), Err(CoreError::NotFound { .. })));
    }
}
