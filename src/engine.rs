use rusqlite::{params, Connection};
use std::path::Path;

pub struct RemainderEngine {
    pub conn: Connection,
}

impl RemainderEngine {
    pub fn new(db_path: &str) -> anyhow::Result<Self> {
        let mut conn = Connection::open(db_path)?;
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA wal_autocheckpoint = 1000;"
        )?;

        Self::run_migrations(&mut conn)?;

        Ok(Self { conn })
    }

    pub fn run_migrations(conn: &mut Connection) -> anyhow::Result<()> {
        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;

        if version < 2 {
            let tx = conn.transaction()?;

            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS sources (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     file_path TEXT UNIQUE NOT NULL,
                     last_shredded DATETIME DEFAULT CURRENT_TIMESTAMP
                 );

                 CREATE TABLE IF NOT EXISTS atomics_v2 (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     source_id INTEGER NOT NULL,
                     vault_tag TEXT NOT NULL,
                     fact_content TEXT NOT NULL,
                     hash TEXT NOT NULL,
                     created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                     FOREIGN KEY(source_id) REFERENCES sources(id) ON DELETE CASCADE,
                     UNIQUE(source_id, hash)
                 );"
            )?;

            let table_exists: bool = tx.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='atomics'",
                [],
                |r| r.get::<_, i64>(0).map(|c| c > 0),
            )?;

            if table_exists {
                let old_count: i64 = tx.query_row("SELECT count(*) FROM atomics", [], |r| r.get(0))?;

                // Inspect available columns in the existing legacy table
                let columns: Vec<String> = {
                    let mut stmt = tx.prepare("PRAGMA table_info(atomics)")?;
                    let cols = stmt.query_map([], |row| row.get::<_, String>(1))?
                        .filter_map(|r| r.ok())
                        .collect();
                    cols
                };

                let has_source_id = columns.iter().any(|c| c == "source_id");
                let has_hash = columns.iter().any(|c| c == "hash");
                let has_created_at = columns.iter().any(|c| c == "created_at");

                // Ensure legacy source entries exist to satisfy foreign key constraints
                tx.execute(
                    "INSERT OR IGNORE INTO sources (id, file_path) VALUES (1, '__legacy__')",
                    [],
                )?;

                if has_source_id {
                    tx.execute(
                        "INSERT OR IGNORE INTO sources (id, file_path) 
                         SELECT DISTINCT source_id, '__legacy_source_' || source_id 
                         FROM atomics 
                         WHERE source_id IS NOT NULL AND source_id NOT IN (SELECT id FROM sources)",
                        [],
                    )?;
                }

                let source_expr = if has_source_id {
                    "COALESCE(source_id, 1)"
                } else {
                    "1"
                };

                // Fallback to random 16-byte hex if hash is NULL or empty
                let hash_expr = if has_hash {
                    "COALESCE(NULLIF(hash, ''), hex(randomblob(16)))"
                } else {
                    "hex(randomblob(16))"
                };

                let created_at_expr = if has_created_at {
                    "COALESCE(created_at, CURRENT_TIMESTAMP)"
                } else {
                    "CURRENT_TIMESTAMP"
                };

                let copy_sql = format!(
                    "INSERT INTO atomics_v2 (id, source_id, vault_tag, fact_content, hash, created_at)
                     SELECT id, {}, vault_tag, fact_content, {}, {} FROM atomics",
                    source_expr, hash_expr, created_at_expr
                );

                // Strictly propagate copy errors (no INSERT OR IGNORE)
                tx.execute(&copy_sql, [])?;

                // Invariant verification check: new count must match old count exactly
                let new_count: i64 = tx.query_row("SELECT count(*) FROM atomics_v2", [], |r| r.get(0))?;
                if new_count != old_count {
                    anyhow::bail!(
                        "Migration aborted: count mismatch (old table: {} facts, new table: {} facts). Rolling back.",
                        old_count, new_count
                    );
                }

                tx.execute_batch("DROP TABLE atomics;")?;
            }

            tx.execute_batch(
                "ALTER TABLE atomics_v2 RENAME TO atomics;
                 CREATE INDEX IF NOT EXISTS idx_atomics_tag ON atomics(vault_tag COLLATE NOCASE);
                 CREATE INDEX IF NOT EXISTS idx_atomics_source ON atomics(source_id);
                 PRAGMA user_version = 2;"
            )?;

            tx.commit()?;
        } else {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS sources (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     file_path TEXT UNIQUE NOT NULL,
                     last_shredded DATETIME DEFAULT CURRENT_TIMESTAMP
                 );

                 CREATE TABLE IF NOT EXISTS atomics (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     source_id INTEGER NOT NULL,
                     vault_tag TEXT NOT NULL,
                     fact_content TEXT NOT NULL,
                     hash TEXT NOT NULL,
                     created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                     FOREIGN KEY(source_id) REFERENCES sources(id) ON DELETE CASCADE,
                     UNIQUE(source_id, hash)
                 );

                 CREATE INDEX IF NOT EXISTS idx_atomics_tag ON atomics(vault_tag COLLATE NOCASE);
                 CREATE INDEX IF NOT EXISTS idx_atomics_source ON atomics(source_id);"
            )?;
        }

        Ok(())
    }

    pub fn get_latest_fact(&self, tag: &str) -> anyhow::Result<Option<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT fact_content FROM atomics 
             WHERE vault_tag = ?1 COLLATE NOCASE
             ORDER BY id DESC LIMIT 1"
        )?;
        
        let mut rows = stmt.query(params![tag])?;
        if let Some(row) = rows.next()? {
            let content: String = row.get(0)?;
            Ok(Some(content))
        } else {
            Ok(None)
        }
    }

    pub fn refresh_source_facts(&mut self, file_path: &str, facts: Vec<(String, String)>) -> anyhow::Result<usize> {
        use sha2::{Sha256, Digest};
        let tx = self.conn.transaction()?;

        // 1. Upsert source entry
        tx.execute(
            "INSERT INTO sources (file_path, last_shredded) VALUES (?1, CURRENT_TIMESTAMP)
             ON CONFLICT(file_path) DO UPDATE SET last_shredded = CURRENT_TIMESTAMP",
            params![file_path],
        )?;

        let source_id: i64 = tx.query_row(
            "SELECT id FROM sources WHERE file_path = ?1",
            params![file_path],
            |row| row.get(0),
        )?;

        // 2. Cascade purge old facts for THIS source only
        tx.execute("DELETE FROM atomics WHERE source_id = ?1", params![source_id])?;

        // 3. Insert fresh facts with composite UNIQUE(source_id, hash)
        let mut inserted = 0;
        for (tag, content) in facts {
            let mut hasher = Sha256::new();
            hasher.update(tag.as_bytes());
            hasher.update(b":");
            hasher.update(content.as_bytes());
            let hash_hex = hex::encode(hasher.finalize());

            let changes = tx.execute(
                "INSERT INTO atomics (source_id, vault_tag, fact_content, hash)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(source_id, hash) DO NOTHING",
                params![source_id, tag, content, hash_hex],
            )?;
            inserted += changes;
        }

        tx.commit()?;
        Ok(inserted)
    }

    pub fn remove_source_or_prefix(&mut self, path: &str) -> anyhow::Result<usize> {
        let normalized = path.replace("\\", "/");
        let slash_prefix = format!("{}/%", normalized.trim_end_matches('/'));
        let backslash_prefix = format!("{}\\%", path.trim_end_matches('\\'));

        let mut stmt = self.conn.prepare(
            "SELECT id FROM sources 
             WHERE file_path = ?1 
                OR file_path = ?2 
                OR file_path LIKE ?3 
                OR file_path LIKE ?4"
        )?;

        let source_ids: Vec<i64> = stmt
            .query_map(params![path, normalized, slash_prefix, backslash_prefix], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();

        let mut deleted = 0;
        for id in source_ids {
            deleted += self.conn.execute("DELETE FROM sources WHERE id = ?1", params![id])?;
        }

        Ok(deleted)
    }

    pub fn reconcile_missing_sources(&mut self) -> anyhow::Result<usize> {
        let mut stmt = self.conn.prepare("SELECT id, file_path FROM sources")?;
        let rows: Vec<(i64, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .filter_map(|r| r.ok())
            .collect();

        let mut purged = 0;
        for (id, file_path) in rows {
            if file_path == "__legacy__" || file_path.starts_with("__legacy_source_") {
                continue;
            }
            if !Path::new(&file_path).exists() {
                let count = self.conn.execute("DELETE FROM sources WHERE id = ?1", params![id])?;
                purged += count;
            }
        }
        Ok(purged)
    }

    pub fn checkpoint(&self) -> anyhow::Result<()> {
        self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_migration_preserves_null_hash_rows() -> anyhow::Result<()> {
        let mut conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA user_version = 1;
             CREATE TABLE sources (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 file_path TEXT UNIQUE NOT NULL
             );
             INSERT INTO sources (id, file_path) VALUES (10, 'notes.txt');
             CREATE TABLE atomics (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 source_id INTEGER,
                 vault_tag TEXT NOT NULL,
                 fact_content TEXT NOT NULL,
                 hash TEXT,
                 created_at DATETIME DEFAULT CURRENT_TIMESTAMP
             );
             INSERT INTO atomics (source_id, vault_tag, fact_content, hash) 
             VALUES (10, 'DATABASE', 'Uses WAL mode', NULL);"
        )?;

        let before_count: i64 = conn.query_row("SELECT count(*) FROM atomics", [], |r| r.get(0))?;
        assert_eq!(before_count, 1);

        RemainderEngine::run_migrations(&mut conn)?;

        let after_count: i64 = conn.query_row("SELECT count(*) FROM atomics", [], |r| r.get(0))?;
        let new_version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let non_null_hash: String = conn.query_row("SELECT hash FROM atomics WHERE vault_tag = 'DATABASE'", [], |r| r.get(0))?;

        assert_eq!(after_count, 1, "Fact must not be lost!");
        assert_eq!(new_version, 2, "user_version must be 2");
        assert!(!non_null_hash.is_empty(), "Hash must be populated and non-null");

        Ok(())
    }

    #[test]
    fn test_migration_handles_v0_missing_source_id_column() -> anyhow::Result<()> {
        let mut conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA user_version = 1;
             CREATE TABLE atomics (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 vault_tag TEXT NOT NULL,
                 fact_content TEXT NOT NULL,
                 hash TEXT UNIQUE,
                 created_at DATETIME DEFAULT CURRENT_TIMESTAMP
             );
             INSERT INTO atomics (vault_tag, fact_content, hash) 
             VALUES ('AUTH', 'JWT Bearer token', 'abc123hash');"
        )?;

        let before_count: i64 = conn.query_row("SELECT count(*) FROM atomics", [], |r| r.get(0))?;
        assert_eq!(before_count, 1);

        RemainderEngine::run_migrations(&mut conn)?;

        let after_count: i64 = conn.query_row("SELECT count(*) FROM atomics", [], |r| r.get(0))?;
        let new_version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;

        assert_eq!(after_count, 1, "Fact without source_id must be preserved");
        assert_eq!(new_version, 2);

        Ok(())
    }

    #[test]
    fn test_directory_removal_cascades_facts() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        engine.refresh_source_facts("C:/docs/notes.txt", vec![("DOCS".into(), "Fact 1".into())])?;
        engine.refresh_source_facts("C:/docs/sub/deep.txt", vec![("SUB".into(), "Fact 2".into())])?;
        engine.refresh_source_facts("C:/other/keep.txt", vec![("OTHER".into(), "Fact 3".into())])?;

        let count: i64 = engine.conn.query_row("SELECT count(*) FROM atomics", [], |r| r.get(0))?;
        assert_eq!(count, 3);

        // Delete directory "C:/docs"
        let deleted = engine.remove_source_or_prefix("C:/docs")?;
        assert_eq!(deleted, 2);

        let remaining: i64 = engine.conn.query_row("SELECT count(*) FROM atomics", [], |r| r.get(0))?;
        assert_eq!(remaining, 1);

        let kept: String = engine.conn.query_row("SELECT vault_tag FROM atomics", [], |r| r.get(0))?;
        assert_eq!(kept, "OTHER");

        Ok(())
    }

    #[test]
    fn test_migration_error_rolls_back_and_preserves_original_table() -> anyhow::Result<()> {
        let mut conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "PRAGMA user_version = 1;
             CREATE TABLE atomics (
                 id INTEGER PRIMARY KEY,
                 vault_tag TEXT NOT NULL
             );
             INSERT INTO atomics (vault_tag) VALUES ('ARCH');"
        )?;

        // Old table is missing 'fact_content' column entirely, so copy will fail
        let res = RemainderEngine::run_migrations(&mut conn);
        assert!(res.is_err(), "Migration must fail and return error when required columns cannot be copied");

        // Verify transaction rolled back: original table still exists and user_version is still 1
        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        assert_eq!(version, 1, "user_version must not be upgraded on failure");

        let count: i64 = conn.query_row("SELECT count(*) FROM atomics", [], |r| r.get(0))?;
        assert_eq!(count, 1, "Original table records must be preserved via rollback");

        Ok(())
    }
}
