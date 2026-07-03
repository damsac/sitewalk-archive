use rusqlite::Row;

use crate::domain::CapturedItem;
use crate::error::CoreError;
use crate::ids::new_id;
use crate::store::Store;

const ITEM_COLS: &str =
    "id, session_id, kind, text, done, created_at, updated_at, device_id";

fn item_from_row(row: &Row) -> Result<CapturedItem, rusqlite::Error> {
    Ok(CapturedItem {
        id: row.get("id")?,
        session_id: row.get("session_id")?,
        kind: row.get("kind")?,
        text: row.get("text")?,
        done: row.get::<_, i64>("done")? != 0,
        created_at: row.get::<_, i64>("created_at")? as u64,
        updated_at: row.get::<_, i64>("updated_at")? as u64,
        device_id: row.get("device_id")?,
    })
}

impl Store {
    /// Adds an item to a session. Works for agent extraction (Plans 04/05)
    /// and manual entry alike (story 10: manual parity — nothing is agent-only).
    pub fn add_item(&self, session_id: &str, kind: &str, text: &str) -> Result<CapturedItem, CoreError> {
        self.get_session(session_id)?; // NotFound if missing/tombstoned
        let now = self.now();
        let item = CapturedItem {
            id: new_id(),
            session_id: session_id.to_string(),
            kind: kind.to_string(),
            text: text.to_string(),
            done: false,
            created_at: now,
            updated_at: now,
            device_id: self.device_id.clone(),
        };
        self.conn.execute(
            "INSERT INTO items (id, session_id, kind, text, done, created_at, updated_at, device_id)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?7)",
            rusqlite::params![
                item.id,
                item.session_id,
                item.kind,
                item.text,
                item.created_at as i64,
                item.updated_at as i64,
                item.device_id,
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
            Some(row) => item_from_row(row).map_err(CoreError::Sqlite),
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
            items.push(item_from_row(row).map_err(CoreError::Sqlite)?);
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
            items.push(item_from_row(row).map_err(CoreError::Sqlite)?);
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
    fn delete_item_is_a_tombstone() {
        let (s, sid) = store_with_session();
        let item = s.add_item(&sid, "todo", "x").unwrap();
        s.delete_item(&item.id).unwrap();
        assert!(s.list_items_for_session(&sid).unwrap().is_empty());
        assert!(matches!(s.delete_item(&item.id), Err(CoreError::NotFound { .. })));
    }
}
