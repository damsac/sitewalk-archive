use rusqlite::Connection;

use crate::error::CoreError;

/// One entry per schema version, applied in order. NEVER edit an existing
/// entry after it has shipped — append a new one.
pub(crate) const MIGRATIONS: &[&str] = &[
    // v1: initial schema (spec §9: timestamps + device id on every row, tombstones)
    r#"
    -- all *_at columns are unix epoch-seconds
    CREATE TABLE jobs (
        id           TEXT PRIMARY KEY,
        name         TEXT NOT NULL,
        client       TEXT,
        site         TEXT,
        scheduled_at INTEGER,
        status       TEXT NOT NULL,
        created_at   INTEGER NOT NULL,
        updated_at   INTEGER NOT NULL,
        device_id    TEXT NOT NULL,
        deleted_at   INTEGER
    );

    CREATE TABLE sessions (
        id         TEXT PRIMARY KEY,
        job_id     TEXT REFERENCES jobs(id),
        status     TEXT NOT NULL,
        transcript TEXT NOT NULL DEFAULT '',
        summary    TEXT,
        started_at INTEGER NOT NULL,
        ended_at   INTEGER,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        device_id  TEXT NOT NULL,
        deleted_at INTEGER
    );

    CREATE TABLE items (
        id         TEXT PRIMARY KEY,
        session_id TEXT NOT NULL REFERENCES sessions(id),
        kind       TEXT NOT NULL,
        text       TEXT NOT NULL,
        done       INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        device_id  TEXT NOT NULL,
        deleted_at INTEGER
    );

    CREATE TABLE contacts (
        id         TEXT PRIMARY KEY,
        name       TEXT NOT NULL,
        trade      TEXT,
        phone      TEXT,
        notes      TEXT,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        device_id  TEXT NOT NULL,
        deleted_at INTEGER
    );

    CREATE TABLE artifacts (
        id         TEXT PRIMARY KEY,
        session_id TEXT NOT NULL REFERENCES sessions(id),
        kind       TEXT NOT NULL,
        title      TEXT NOT NULL,
        body       TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        device_id  TEXT NOT NULL,
        deleted_at INTEGER
    );

    -- local-only bookkeeping: not synced, so no timestamps/device_id/tombstone
    CREATE TABLE reflection_state (
        id                INTEGER PRIMARY KEY CHECK (id = 1),
        signals           TEXT NOT NULL,
        last_reflected_at INTEGER NOT NULL DEFAULT 0
    );

    -- append-only cost log (R9: cost per session measured from day one).
    -- No tombstone: rows are never deleted, only summed.
    CREATE TABLE llm_usage (
        id            TEXT PRIMARY KEY,
        session_id    TEXT REFERENCES sessions(id),
        purpose       TEXT NOT NULL,
        input_tokens  INTEGER NOT NULL,
        output_tokens INTEGER NOT NULL,
        created_at    INTEGER NOT NULL,
        device_id     TEXT NOT NULL
    );
    CREATE INDEX idx_llm_usage_session ON llm_usage(session_id);

    CREATE INDEX idx_jobs_scheduled ON jobs(scheduled_at);
    CREATE INDEX idx_sessions_started ON sessions(started_at) WHERE deleted_at IS NULL;
    CREATE INDEX idx_sessions_job ON sessions(job_id) WHERE deleted_at IS NULL;
    CREATE INDEX idx_items_session ON items(session_id) WHERE deleted_at IS NULL;
    CREATE INDEX idx_artifacts_session ON artifacts(session_id) WHERE deleted_at IS NULL;
    "#,
    // v2: items.source (Plan 06a). Backfill existing rows as 'authoritative' —
    // pre-06a items were all written by the processing pipeline (Plan 04) or by
    // manual add_item; treating them as authoritative is the safe default (they
    // are never swept unless a *new* run supersedes them, exactly today's
    // behavior). SQLite ADD COLUMN with NOT NULL requires the DEFAULT.
    r#"
    ALTER TABLE items ADD COLUMN source TEXT NOT NULL DEFAULT 'authoritative';
    "#,
    // v3: sessions.template (Plan 07 D4) — nullable key selecting extraction
    // vocabulary + document layout ("landscape" | "property" | "inspection").
    // Persisted (not pass-through) so reprocessing stays template-consistent.
    r#"
    ALTER TABLE sessions ADD COLUMN template TEXT;
    "#,
];

pub(crate) fn migrate(conn: &Connection) -> Result<(), CoreError> {
    migrate_with(conn, MIGRATIONS)
}

/// Applies pending migrations from `migrations`. Each one is all-or-nothing:
/// the DDL and the `user_version` bump commit in a single transaction, so a
/// mid-batch failure rolls back cleanly instead of leaving partial tables
/// behind with a stale version.
fn migrate_with(conn: &Connection, migrations: &[&str]) -> Result<(), CoreError> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, sql) in migrations.iter().enumerate().skip(version as usize) {
        let result = conn.execute_batch(&format!(
            "BEGIN;\n{}\nPRAGMA user_version = {};\nCOMMIT;",
            sql,
            i + 1
        ));
        if let Err(e) = result {
            // A mid-batch failure leaves the explicit BEGIN open on the
            // connection; roll it back so the connection stays usable.
            if !conn.is_autocommit() {
                conn.execute_batch("ROLLBACK;")?;
            }
            return Err(e.into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_migration_rolls_back_cleanly() {
        let conn = Connection::open_in_memory().unwrap();
        let broken: &[&str] = &[MIGRATIONS[0], "CREATE TABLE broken (;"];
        let err = migrate_with(&conn, broken);
        assert!(err.is_err(), "broken migration must surface an error");

        // v1 committed; the broken v2 rolled back entirely.
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1);

        // Re-running with a fixed second migration succeeds.
        let fixed: &[&str] = &[MIGRATIONS[0], "CREATE TABLE fixed (id TEXT PRIMARY KEY);"];
        migrate_with(&conn, fixed).unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2);
    }
}
