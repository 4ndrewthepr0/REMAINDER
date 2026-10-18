use rusqlite::{params, Connection};
use std::path::Path;
use std::collections::HashSet;

pub struct RemainderEngine {
    pub conn: Connection,
}

fn clean_path_components(path: &Path) -> Vec<std::ffi::OsString> {
    path.components()
        .filter_map(|c| match c {
            std::path::Component::CurDir => None,
            _ => Some(c.as_os_str().to_os_string()),
        })
        .collect()
}

pub fn sanitize_fts5_query(query: &str) -> String {
    let words: Vec<&str> = query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        return String::new();
    }
    words.iter().map(|w| format!("\"{}\"", w)).collect::<Vec<_>>().join(" OR ")
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub vault_tag: String,
    pub fact_content: String,
    pub file_path: String,
    pub raw_excerpt: Option<String>,
    pub bm25_rank: f64,
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
                     raw_content TEXT,
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

                tx.execute(&copy_sql, [])?;

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
        }

        // Migration to version 3: FTS5 full-text / natural language search & raw source text preservation
        let current_version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if current_version < 3 {
            let tx = conn.transaction()?;

            // Check if raw_content exists on sources
            let has_raw_content: bool = {
                let mut stmt = tx.prepare("PRAGMA table_info(sources)")?;
                let cols: Vec<String> = stmt.query_map([], |r| r.get(1))?.filter_map(|r| r.ok()).collect();
                cols.contains(&"raw_content".to_string())
            };
            if !has_raw_content {
                tx.execute_batch("ALTER TABLE sources ADD COLUMN raw_content TEXT;")?;
            }

            // Create FTS5 virtual table for BM25 natural language search
            tx.execute_batch(
                "CREATE VIRTUAL TABLE IF NOT EXISTS atomics_fts USING fts5(
                     vault_tag,
                     fact_content,
                     content='atomics',
                     content_rowid='id'
                 );

                 CREATE TRIGGER IF NOT EXISTS atomics_ai AFTER INSERT ON atomics BEGIN
                     INSERT INTO atomics_fts(rowid, vault_tag, fact_content) VALUES (new.id, new.vault_tag, new.fact_content);
                 END;

                 CREATE TRIGGER IF NOT EXISTS atomics_ad AFTER DELETE ON atomics BEGIN
                     INSERT INTO atomics_fts(atomics_fts, rowid, vault_tag, fact_content) VALUES('delete', old.id, old.vault_tag, old.fact_content);
                 END;

                 CREATE TRIGGER IF NOT EXISTS atomics_au AFTER UPDATE ON atomics BEGIN
                     INSERT INTO atomics_fts(atomics_fts, rowid, vault_tag, fact_content) VALUES('delete', old.id, old.vault_tag, old.fact_content);
                     INSERT INTO atomics_fts(rowid, vault_tag, fact_content) VALUES (new.id, new.vault_tag, new.fact_content);
                 END;

                 INSERT OR IGNORE INTO atomics_fts(rowid, vault_tag, fact_content)
                 SELECT id, vault_tag, fact_content FROM atomics;

                 PRAGMA user_version = 3;"
            )?;

            tx.commit()?;
        } else {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS sources (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     file_path TEXT UNIQUE NOT NULL,
                     raw_content TEXT,
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
                 CREATE INDEX IF NOT EXISTS idx_atomics_source ON atomics(source_id);

                 CREATE VIRTUAL TABLE IF NOT EXISTS atomics_fts USING fts5(
                     vault_tag,
                     fact_content,
                     content='atomics',
                     content_rowid='id'
                 );"
            )?;
        }

        Ok(())
    }

    /// Retrieves up to `limit` (default 50) current facts recorded under a category tag.
    /// Note: source refreshes replace prior facts for that source to maintain clean provenance.
    pub fn get_facts_by_tag(&self, tag: &str, limit: Option<usize>) -> anyhow::Result<Vec<String>> {
        let limit_val = limit.unwrap_or(50) as i64;
        let mut stmt = self.conn.prepare(
            "SELECT fact_content FROM atomics 
             WHERE vault_tag = ?1 COLLATE NOCASE
             ORDER BY id DESC LIMIT ?2"
        )?;
        
        let rows = stmt.query_map(params![tag, limit_val], |row| row.get(0))?;
        let mut facts = Vec::new();
        for r in rows {
            facts.push(r?);
        }
        Ok(facts)
    }

    /// Natural-language & Tag search: runs BM25 lexical ranking across facts with exact tag boosting
    pub fn search(&self, query: &str, limit: Option<usize>) -> anyhow::Result<Vec<SearchHit>> {
        let max_results = limit.unwrap_or(20);
        let mut results = Vec::new();
        let mut seen_ids = HashSet::new();

        // 1. Direct tag lookup (for queries that match a category tag like 'DATABASE' or 'SECURITY')
        let mut tag_stmt = self.conn.prepare(
            "SELECT a.id, a.vault_tag, a.fact_content, s.file_path, s.raw_content
             FROM atomics a
             JOIN sources s ON s.id = a.source_id
             WHERE a.vault_tag = ?1 COLLATE NOCASE
             ORDER BY a.id DESC LIMIT ?2"
        )?;

        let tag_rows = tag_stmt.query_map(params![query, max_results], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;

        for r in tag_rows {
            let (id, tag, content, file_path, raw) = r?;
            seen_ids.insert(id);
            results.push(SearchHit {
                vault_tag: tag,
                fact_content: content,
                file_path,
                raw_excerpt: raw.map(|t| t.chars().take(300).collect()),
                bm25_rank: -100.0, // High priority boost for exact tag match
            });
        }

        // 2. Natural language FTS5 BM25 search
        let fts_query = sanitize_fts5_query(query);
        if !fts_query.is_empty() {
            let mut fts_stmt = self.conn.prepare(
                "SELECT a.id, a.vault_tag, a.fact_content, s.file_path, s.raw_content, bm25(atomics_fts) as rank
                 FROM atomics_fts f
                 JOIN atomics a ON a.id = f.rowid
                 JOIN sources s ON s.id = a.source_id
                 WHERE atomics_fts MATCH ?1
                 ORDER BY rank
                 LIMIT ?2"
            )?;

            let fts_rows = fts_stmt.query_map(params![fts_query, max_results], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, f64>(5)?,
                ))
            })?;

            for r in fts_rows {
                let (id, tag, content, file_path, raw, rank) = r?;
                if !seen_ids.contains(&id) {
                    seen_ids.insert(id);
                    results.push(SearchHit {
                        vault_tag: tag,
                        fact_content: content,
                        file_path,
                        raw_excerpt: raw.map(|t| t.chars().take(300).collect()),
                        bm25_rank: rank,
                    });
                }
            }
        }

        Ok(results)
    }

    /// Convenience retrieval for single latest fact
    pub fn get_latest_fact(&self, tag: &str) -> anyhow::Result<Option<String>> {
        let facts = self.get_facts_by_tag(tag, Some(1))?;
        Ok(facts.into_iter().next())
    }

    pub fn refresh_source_facts(&mut self, file_path: &str, raw_content: Option<&str>, facts: Vec<(String, String)>) -> anyhow::Result<usize> {
        use sha2::{Sha256, Digest};
        let tx = self.conn.transaction()?;

        // 1. Upsert source entry with raw_content preserved
        tx.execute(
            "INSERT INTO sources (file_path, raw_content, last_shredded) VALUES (?1, ?2, CURRENT_TIMESTAMP)
             ON CONFLICT(file_path) DO UPDATE SET raw_content = ?2, last_shredded = CURRENT_TIMESTAMP",
            params![file_path, raw_content],
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

    /// Removes a source or all nested sources in a directory using exact path component hierarchy.
    /// Immune to SQL LIKE wildcard collisions (e.g. 'arch_notes' will never match 'archXnotes' or 'arch_notes_extra').
    pub fn remove_source_or_prefix(&mut self, path_str: &str) -> anyhow::Result<usize> {
        let target_comps = clean_path_components(Path::new(path_str));
        if target_comps.is_empty() {
            return Ok(0);
        }

        let rows: Vec<(i64, String)> = {
            let mut stmt = self.conn.prepare("SELECT id, file_path FROM sources")?;
            let items = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .filter_map(|r| r.ok())
                .collect();
            items
        };

        let mut ids_to_delete = Vec::new();
        for (id, file_path) in rows {
            let p_comps = clean_path_components(Path::new(&file_path));
            if p_comps.len() >= target_comps.len() && p_comps[..target_comps.len()] == target_comps[..] {
                ids_to_delete.push(id);
            }
        }

        let mut deleted = 0;
        if !ids_to_delete.is_empty() {
            let tx = self.conn.transaction()?;
            for id in ids_to_delete {
                deleted += tx.execute("DELETE FROM sources WHERE id = ?1", params![id])?;
            }
            tx.commit()?;
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
    fn test_sqlite_fts5_natural_language_search_and_provenance() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;

        let doc1 = "We configured SQLite in WAL mode with synchronous=NORMAL for concurrent sub-millisecond writes.";
        let doc2 = "All inter-service RPC calls enforce Ed25519 digital signatures for zero-trust authorization.";

        engine.refresh_source_facts("db_config.txt", Some(doc1), vec![
            ("DATABASE".into(), "SQLite is configured in WAL mode with synchronous=NORMAL.".into()),
        ])?;

        engine.refresh_source_facts("rpc_security.txt", Some(doc2), vec![
            ("SECURITY".into(), "Ed25519 signatures are used for inter-service RPC authorization.".into()),
        ])?;

        // 1. Natural language query without specifying category tag
        let hits = engine.search("How do we authorize RPC calls?", None)?;
        assert!(!hits.is_empty(), "FTS5 BM25 search must find relevant facts from natural language questions");
        assert_eq!(hits[0].vault_tag, "SECURITY");
        assert!(hits[0].fact_content.contains("Ed25519"));
        assert_eq!(hits[0].file_path, "rpc_security.txt");
        assert!(hits[0].raw_excerpt.as_ref().unwrap().contains("zero-trust authorization"));

        // 2. Natural language query for database performance
        let db_hits = engine.search("concurrent writes and WAL mode", None)?;
        assert!(!db_hits.is_empty());
        assert_eq!(db_hits[0].vault_tag, "DATABASE");
        assert!(db_hits[0].fact_content.contains("WAL mode"));
        assert_eq!(db_hits[0].file_path, "db_config.txt");

        Ok(())
    }

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
        assert_eq!(new_version, 3, "user_version must be upgraded to 3");
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
        assert_eq!(new_version, 3);

        Ok(())
    }

    #[test]
    fn test_directory_removal_component_matching_prevents_wildcard_bleed() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        
        engine.refresh_source_facts("arch_notes/sub.txt", None, vec![("TARGET".into(), "Fact inside target".into())])?;
        engine.refresh_source_facts("archXnotes/sub.txt", None, vec![("UNRELATED_X".into(), "Must NOT be deleted".into())])?;
        engine.refresh_source_facts("arch_notes_extra.txt", None, vec![("UNRELATED_EXTRA".into(), "Must NOT be deleted".into())])?;

        let total_before: i64 = engine.conn.query_row("SELECT count(*) FROM atomics", [], |r| r.get(0))?;
        assert_eq!(total_before, 3);

        let deleted = engine.remove_source_or_prefix("arch_notes")?;
        assert_eq!(deleted, 1, "Only exact directory prefix must be deleted");

        let x_fact = engine.get_latest_fact("UNRELATED_X")?;
        assert_eq!(x_fact.as_deref(), Some("Must NOT be deleted"));

        let extra_fact = engine.get_latest_fact("UNRELATED_EXTRA")?;
        assert_eq!(extra_fact.as_deref(), Some("Must NOT be deleted"));

        let target_fact = engine.get_latest_fact("TARGET")?;
        assert_eq!(target_fact, None);

        Ok(())
    }

    #[test]
    fn test_multi_fact_retrieval_preserves_decision_details() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        
        let decisions = vec![
            ("DATABASE".to_string(), "Decided WAL mode for non-blocking concurrent reads.".to_string()),
            ("DATABASE".to_string(), "Configured synchronous=NORMAL for power-loss safety.".to_string()),
            ("DATABASE".to_string(), "Enforced foreign_keys=ON for cascade consistency.".to_string()),
        ];
        engine.refresh_source_facts("arch.txt", None, decisions)?;

        let all_db_facts = engine.get_facts_by_tag("DATABASE", None)?;
        assert_eq!(all_db_facts.len(), 3, "All decisions under tag must be retrieved");
        assert!(all_db_facts.iter().any(|f| f.contains("WAL mode")));
        assert!(all_db_facts.iter().any(|f| f.contains("synchronous=NORMAL")));
        assert!(all_db_facts.iter().any(|f| f.contains("foreign_keys=ON")));

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

        let res = RemainderEngine::run_migrations(&mut conn);
        assert!(res.is_err(), "Migration must fail and return error when required columns cannot be copied");

        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        assert_eq!(version, 1, "user_version must not be upgraded on failure");

        let count: i64 = conn.query_row("SELECT count(*) FROM atomics", [], |r| r.get(0))?;
        assert_eq!(count, 1, "Original table records must be preserved via rollback");

        Ok(())
    }
}
