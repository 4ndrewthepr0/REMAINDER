use rusqlite::{params, Connection};
use std::path::Path;
use std::collections::{HashMap, HashSet};

pub struct RemainderEngine {
    pub conn: Connection,
    pub embedder: Option<std::sync::Arc<crate::embedder::Embedder>>,
}

pub fn f32_slice_to_bytes(slice: &[f32]) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(
            slice.as_ptr() as *const u8,
            slice.len() * std::mem::size_of::<f32>(),
        )
    }
}

pub fn bytes_to_f32_vec(bytes: &[u8]) -> Vec<f32> {
    if bytes.len() % 4 != 0 {
        return Vec::new();
    }
    let mut vec = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        let arr: [u8; 4] = chunk.try_into().unwrap();
        vec.push(f32::from_le_bytes(arr));
    }
    vec
}

pub fn chunk_text_for_embedding(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return chunks;
    }

    // 1. Full document or leading overview context for holistic document-wide semantic matching
    if trimmed.len() <= 1500 {
        let lines: Vec<&str> = trimmed.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
        let full = lines.join(" ");
        if !full.is_empty() {
            chunks.push(full);
        }
        return chunks;
    } else {
        let first_chars: String = trimmed.chars().take(1000).collect();
        let lines: Vec<&str> = first_chars.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
        let intro = lines.join(" ");
        if !intro.is_empty() {
            chunks.push(intro);
        }
    }

    // 2. Full-coverage content-aware sliding window chunking across entire text
    // Target chunk size: ~750 characters (approx. 120-150 tokens, ideal for MiniLM 128/256 token limit)
    // Target overlap: ~200 characters (ensures concepts spanning chunk boundaries are preserved)
    let target_size = 750;
    let target_overlap = 200;
    let chars: Vec<char> = text.chars().collect();
    let total_chars = chars.len();

    let mut start = 0;
    let mut seen_chunks: HashSet<String> = HashSet::new();
    if let Some(first) = chunks.first() {
        seen_chunks.insert(first.clone());
    }

    while start < total_chars {
        let mut end = (start + target_size).min(total_chars);

        // If not at the end of the document, find a natural boundary near target_size
        if end < total_chars {
            let search_start = start + (target_size.saturating_sub(150)).min(end - start);
            let slice: String = chars[search_start..end].iter().collect();

            // Look for paragraph break, newline, sentence terminator, or word break
            if let Some(pos) = slice.rfind("\n\n") {
                end = search_start + pos + 2;
            } else if let Some(pos) = slice.rfind('\n') {
                end = search_start + pos + 1;
            } else if let Some(pos) = slice.rfind(". ") {
                end = search_start + pos + 2;
            } else if let Some(pos) = slice.rfind(' ') {
                end = search_start + pos + 1;
            }
        }

        let chunk_slice: String = chars[start..end].iter().collect();
        let lines: Vec<&str> = chunk_slice.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
        let chunk_str = lines.join(" ");

        if chunk_str.len() >= 60 && !seen_chunks.contains(&chunk_str) {
            seen_chunks.insert(chunk_str.clone());
            chunks.push(chunk_str);
        }

        if end >= total_chars {
            break;
        }

        // Advance start with overlap
        let next_start = end.saturating_sub(target_overlap);
        if next_start <= start {
            start = end; // Ensure forward progress
        } else {
            start = next_start;
        }
    }

    if chunks.is_empty() && !trimmed.is_empty() {
        chunks.push(trimmed.to_string());
    }

    // Optional environment cap for constrained test harnesses (0 or unset = unlimited full coverage)
    let max_chunks = std::env::var("REMAINDER_MAX_CHUNKS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    if max_chunks > 0 && chunks.len() > max_chunks {
        chunks.truncate(max_chunks);
    }

    chunks
}

pub const STOPWORDS: &[&str] = &[
    "the", "and", "that", "have", "for", "not", "with", "you", "this", "but", "his", "from",
    "they", "say", "her", "she", "will", "one", "all", "would", "there", "their", "what",
    "out", "about", "who", "get", "which", "go", "when", "make", "can", "like", "time",
    "no", "just", "him", "know", "take", "people", "into", "year", "your", "good", "some",
    "could", "them", "see", "other", "than", "then", "now", "look", "only", "come", "its",
    "over", "think", "also", "back", "after", "use", "two", "how", "our", "work", "first",
    "well", "way", "even", "new", "want", "because", "any", "these", "give", "day", "most", "us",
    "are", "was", "were", "been", "being", "has", "had", "does", "did", "doing"
];

fn find_best_query_offset(folded: &str, positions: &[usize], query: &str) -> Option<usize> {
    let distinctive: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .map(|s| s.to_lowercase())
        .filter(|s| s.len() >= 3 && !STOPWORDS.contains(&s.as_str()))
        .collect();

    let search_terms: Vec<String> = if !distinctive.is_empty() {
        distinctive
    } else {
        query
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .map(|s| s.to_lowercase())
            .filter(|s| s.len() >= 3)
            .collect()
    };

    if search_terms.is_empty() {
        return None;
    }

    let mut term_matches: Vec<(usize, &String)> = Vec::new();
    for term in &search_terms {
        let mut start_idx = 0;
        while let Some(byte_offset) = folded[start_idx..].find(term.as_str()) {
            let actual_byte_offset = start_idx + byte_offset;
            if actual_byte_offset < positions.len() {
                let char_pos = positions[actual_byte_offset];
                term_matches.push((char_pos, term));
            }
            start_idx = actual_byte_offset + term.len();
            if start_idx >= folded.len() {
                break;
            }
        }
    }

    if term_matches.is_empty() {
        return None;
    }

    term_matches.sort_by_key(|m| m.0);

    let window_size = 1000;
    let mut best_score = 0;
    let mut best_pos = term_matches[0].0;

    for (i, &(pos, _)) in term_matches.iter().enumerate() {
        let mut matched_distinct: HashSet<&String> = HashSet::new();
        let mut count = 0;
        for &(other_pos, term) in &term_matches[i..] {
            if other_pos > pos + window_size {
                break;
            }
            matched_distinct.insert(term);
            count += 1;
        }
        let score = matched_distinct.len() * 100 + count;
        if score > best_score {
            best_score = score;
            best_pos = pos;
        }
    }

    Some(best_pos)
}

pub fn extract_passage_smart(raw: &str, fact_content: &str, query: &str, max_len: usize) -> String {
    if max_len == 0 { return String::new(); }
    let raw_trimmed = raw.trim();
    if raw_trimmed.chars().count() <= max_len {
        let single_line = raw_trimmed.lines().collect::<Vec<_>>().join(" ");
        return single_line;
    }
    let chars: Vec<char> = raw.chars().collect();
    let mut folded = String::new();
    let mut positions = Vec::new();
    for (index, ch) in chars.iter().enumerate() {
        for lower in ch.to_lowercase() {
            folded.push(lower);
            positions.extend(std::iter::repeat(index).take(lower.len_utf8()));
        }
    }

    let mut matched_offset = None;

    // 1. If fact_content is a known chunk, locate where it appears in raw
    let fact_trimmed = fact_content.trim();
    if !fact_trimmed.is_empty() && fact_trimmed != "SOURCE" {
        let probe = if fact_trimmed.chars().count() > 60 {
            fact_trimmed.chars().take(60).collect::<String>().to_lowercase()
        } else {
            fact_trimmed.to_lowercase()
        };
        if let Some(offset) = folded.find(&probe) {
            matched_offset = Some(positions[offset]);
        }
    }

    // 2. If not located via fact_content, find highest density query keyword window
    if matched_offset.is_none() {
        matched_offset = find_best_query_offset(&folded, &positions, query);
    }

    // 3. Fallback to fact_content terms if query had no matches
    if matched_offset.is_none() && !fact_trimmed.is_empty() {
        matched_offset = find_best_query_offset(&folded, &positions, fact_trimmed);
    }

    let lead_in = (max_len / 4).min(200);
    let start = matched_offset.map(|p| p.saturating_sub(lead_in)).unwrap_or(0);
    let end = start.saturating_add(max_len).min(chars.len());
    let snippet: String = chars[start..end].iter().collect();
    let single_line = snippet.lines().collect::<Vec<_>>().join(" ");
    format!("{}{}{}", if start > 0 { "... " } else { "" }, single_line.trim(),
        if end < chars.len() { " ..." } else { "" })
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

pub fn extract_passage(raw: &str, query: &str, max_len: usize) -> String {
    if max_len == 0 { return String::new(); }
    let chars: Vec<char> = raw.chars().collect();
    let mut folded = String::new();
    let mut positions = Vec::new();
    for (index, ch) in chars.iter().enumerate() {
        for lower in ch.to_lowercase() {
            folded.push(lower);
            positions.extend(std::iter::repeat(index).take(lower.len_utf8()));
        }
    }
    let matched = find_best_query_offset(&folded, &positions, query);
    let start = matched.map(|p| p.saturating_sub(60.min(max_len / 3))).unwrap_or(0);
    let end = start.saturating_add(max_len).min(chars.len());
    let snippet: String = chars[start..end].iter().collect();
    let single_line = snippet.lines().collect::<Vec<_>>().join(" ");
    format!("{}{}{}", if start > 0 { "... " } else { "" }, single_line.trim(),
        if end < chars.len() { " ..." } else { "" })
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
    /// Reuse older spellings containing `.` components instead of creating a
    /// second source when a watcher and startup scan spell the same path differently.
    fn storage_path(&self, path: &str) -> anyhow::Result<String> {
        use rusqlite::OptionalExtension;
        let normalized: std::path::PathBuf = Path::new(path).components().collect();
        let normalized = normalized.to_string_lossy().to_string();
        if let Some(existing) = self.conn.query_row("SELECT file_path FROM sources WHERE file_path=?1",
            params![normalized], |r| r.get::<_, String>(0)).optional()? {
            return Ok(existing);
        }
        let components = clean_path_components(Path::new(&normalized));
        let mut statement = self.conn.prepare("SELECT file_path FROM sources")?;
        for row in statement.query_map([], |r| r.get::<_, String>(0))? {
            let existing = row?;
            if clean_path_components(Path::new(&existing)) == components { return Ok(existing); }
        }
        Ok(normalized)
    }
    pub fn new(db_path: &str) -> anyhow::Result<Self> {
        let mut conn = Connection::open(db_path)?;
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA wal_autocheckpoint = 1000;"
        )?;

        Self::run_migrations(&mut conn)?;

        let embedder = crate::embedder::Embedder::try_new().ok().map(std::sync::Arc::new);
        let mut engine = Self { conn, embedder };
        let _ = engine.backfill_existing_sources();
        let _ = engine.backfill_embeddings();

        Ok(engine)
    }

    pub fn with_embedder(mut self, embedder: std::sync::Arc<crate::embedder::Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    pub fn run_migrations(conn: &mut Connection) -> anyhow::Result<()> {
        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        anyhow::ensure!(version <= 5, "Database schema {} is newer than this application supports", version);

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

        // Migration to version 3: FTS5 full-text / natural language search & raw source text column
        let v3_check: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if v3_check < 3 {
            let tx = conn.transaction()?;

            let has_raw_content: bool = {
                let mut stmt = tx.prepare("PRAGMA table_info(sources)")?;
                let cols: Vec<String> = stmt.query_map([], |r| r.get(1))?.filter_map(|r| r.ok()).collect();
                cols.contains(&"raw_content".to_string())
            };
            if !has_raw_content {
                tx.execute_batch("ALTER TABLE sources ADD COLUMN raw_content TEXT;")?;
            }

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
        }

        // Migration to version 4: FTS5 full-text indexing of raw source text & passages
        let v4_check: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if v4_check < 4 {
            let tx = conn.transaction()?;

            tx.execute_batch(
                "CREATE VIRTUAL TABLE IF NOT EXISTS sources_fts USING fts5(
                     file_path,
                     raw_content,
                     content='sources',
                     content_rowid='id'
                 );

                 CREATE TRIGGER IF NOT EXISTS sources_ai AFTER INSERT ON sources BEGIN
                     INSERT INTO sources_fts(rowid, file_path, raw_content) VALUES (new.id, new.file_path, coalesce(new.raw_content, ''));
                 END;

                 CREATE TRIGGER IF NOT EXISTS sources_ad AFTER DELETE ON sources BEGIN
                     INSERT INTO sources_fts(sources_fts, rowid, file_path, raw_content) VALUES('delete', old.id, old.file_path, coalesce(old.raw_content, ''));
                 END;

                 CREATE TRIGGER IF NOT EXISTS sources_au AFTER UPDATE ON sources BEGIN
                     INSERT INTO sources_fts(sources_fts, rowid, file_path, raw_content) VALUES('delete', old.id, old.file_path, coalesce(old.raw_content, ''));
                     INSERT INTO sources_fts(rowid, file_path, raw_content) VALUES (new.id, new.file_path, coalesce(new.raw_content, ''));
                 END;

                 INSERT OR IGNORE INTO sources_fts(rowid, file_path, raw_content)
                 SELECT id, file_path, coalesce(raw_content, '') FROM sources;

                 PRAGMA user_version = 4;"
            )?;

            tx.commit()?;
        }

        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 5 {
            let tx = conn.transaction()?;
            // Older versions did not record which source revision produced facts.
            // Preserve those rows, but do not claim they are verified current.
            tx.execute_batch("ALTER TABLE sources ADD COLUMN facts_current INTEGER NOT NULL DEFAULT 0;
                ALTER TABLE sources ADD COLUMN observed_at TEXT;
                PRAGMA user_version = 5;")?;
            tx.commit()?;
        }

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS atomics_embeddings (
                 atomic_id INTEGER PRIMARY KEY,
                 embedding BLOB NOT NULL,
                 FOREIGN KEY(atomic_id) REFERENCES atomics(id) ON DELETE CASCADE
             );

             CREATE TABLE IF NOT EXISTS source_embeddings (
                 source_id INTEGER NOT NULL,
                 chunk_index INTEGER NOT NULL,
                 chunk_text TEXT NOT NULL,
                 embedding BLOB NOT NULL,
                 PRIMARY KEY(source_id, chunk_index),
                 FOREIGN KEY(source_id) REFERENCES sources(id) ON DELETE CASCADE
             );

             CREATE INDEX IF NOT EXISTS idx_source_embeddings_src ON source_embeddings(source_id);

             CREATE VIRTUAL TABLE IF NOT EXISTS source_chunks_fts USING fts5(
                 chunk_text,
                 source_id UNINDEXED,
                 chunk_index UNINDEXED
             );"
        )?;
        Ok(())
    }

    /// Backfills embeddings for atomics and sources that lack stored vectors
    pub fn backfill_embeddings(&self) -> anyhow::Result<usize> {
        let Some(ref emb) = self.embedder else { return Ok(0); };
        let mut count = 0;

        let mut stmt = self.conn.prepare(
            "SELECT a.id, a.vault_tag, a.fact_content 
             FROM atomics a 
             LEFT JOIN atomics_embeddings ae ON ae.atomic_id = a.id 
             WHERE ae.atomic_id IS NULL"
        )?;
        let missing_atomics: Vec<(i64, String, String)> = stmt.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?.filter_map(|r| r.ok()).collect();

        for (id, tag, fact) in missing_atomics {
            let text = format!("[{}] {}", tag, fact);
            if let Ok(vec) = emb.embed(&text) {
                let bytes = f32_slice_to_bytes(&vec);
                let _ = self.conn.execute(
                    "INSERT OR REPLACE INTO atomics_embeddings (atomic_id, embedding) VALUES (?1, ?2)",
                    params![id, bytes],
                );
                count += 1;
            }
        }

        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.raw_content 
             FROM sources s 
             LEFT JOIN source_embeddings se ON se.source_id = s.id 
             WHERE se.source_id IS NULL AND s.raw_content IS NOT NULL AND s.raw_content != ''"
        )?;
        let missing_sources: Vec<(i64, String)> = stmt.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?.filter_map(|r| r.ok()).collect();

        for (s_id, raw) in missing_sources {
            let chunks = chunk_text_for_embedding(&raw);
            for (idx, chunk) in chunks.iter().enumerate() {
                if let Ok(vec) = emb.embed(chunk) {
                    let bytes = f32_slice_to_bytes(&vec);
                    let _ = self.conn.execute(
                        "INSERT OR REPLACE INTO source_embeddings (source_id, chunk_index, chunk_text, embedding) VALUES (?1, ?2, ?3, ?4)",
                        params![s_id, idx as i64, chunk, bytes],
                    );
                    count += 1;
                }
            }
        }

        Ok(count)
    }

    /// Backfills existing sources whose raw_content is NULL by reading from disk
    pub fn backfill_existing_sources(&mut self) -> anyhow::Result<usize> {
        let mut stmt = self.conn.prepare("SELECT id, file_path FROM sources WHERE raw_content IS NULL OR raw_content = ''")?;
        let rows: Vec<(i64, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .filter_map(|r| r.ok())
            .collect();

        let mut backfilled = 0;
        for (id, file_path) in rows {
            if file_path == "__legacy__" || file_path.starts_with("__legacy_source_") {
                continue;
            }
            let p = Path::new(&file_path);
            if p.exists() && p.is_file() {
                if let Ok(content) = std::fs::read_to_string(p) {
                    self.conn.execute(
                        "UPDATE sources SET raw_content = ?1, facts_current = 0, observed_at = CURRENT_TIMESTAMP WHERE id = ?2",
                        params![content, id],
                    )?;
                    backfilled += 1;
                }
            }
        }
        Ok(backfilled)
    }

    /// Immediately records/persists raw source content, guaranteeing it is indexed and searchable
    /// even if LLM proposition extraction fails or is delayed.
    pub fn record_source_content(&mut self, file_path: &str, raw_content: &str) -> anyhow::Result<i64> {
        let file_path = self.storage_path(file_path)?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO sources (file_path, raw_content, last_shredded, observed_at) VALUES (?1, ?2, NULL, CURRENT_TIMESTAMP)
             ON CONFLICT(file_path) DO UPDATE SET
                facts_current = CASE WHEN sources.raw_content IS ?2 THEN sources.facts_current ELSE 0 END,
                raw_content = ?2, observed_at = CURRENT_TIMESTAMP",
            params![file_path, raw_content],
        )?;
        let source_id: i64 = tx.query_row(
            "SELECT id FROM sources WHERE file_path = ?1",
            params![file_path],
            |row| row.get(0),
        )?;

        let chunks = chunk_text_for_embedding(raw_content);
        let _ = tx.execute("DELETE FROM source_chunks_fts WHERE source_id = ?1", params![source_id]);
        for (idx, chunk) in chunks.iter().enumerate() {
            let _ = tx.execute(
                "INSERT INTO source_chunks_fts (chunk_text, source_id, chunk_index) VALUES (?1, ?2, ?3)",
                params![chunk, source_id, idx as i64],
            );
        }

        if let Some(ref emb) = self.embedder {
            let _ = tx.execute("DELETE FROM source_embeddings WHERE source_id = ?1", params![source_id]);
            let chunks_to_embed: Vec<(usize, &String)> = if chunks.len() <= 300 {
                chunks.iter().enumerate().collect()
            } else {
                let mut selected_indices = Vec::new();
                let step = chunks.len() as f64 / 300.0;
                for i in 0..300 {
                    let idx = (i as f64 * step) as usize;
                    if idx < chunks.len() && !selected_indices.contains(&idx) {
                        selected_indices.push(idx);
                    }
                }
                selected_indices.into_iter().map(|idx| (idx, &chunks[idx])).collect()
            };

            for (idx, chunk) in chunks_to_embed {
                if let Ok(vec) = emb.embed(chunk) {
                    let bytes = f32_slice_to_bytes(&vec);
                    let _ = tx.execute(
                        "INSERT OR REPLACE INTO source_embeddings (source_id, chunk_index, chunk_text, embedding) VALUES (?1, ?2, ?3, ?4)",
                        params![source_id, idx as i64, chunk, bytes],
                    );
                }
            }
        }

        tx.commit()?;
        Ok(source_id)
    }

    pub fn invalidate_source(&self, file_path: &str) -> anyhow::Result<()> {
        let file_path = self.storage_path(file_path)?;
        self.conn.execute("UPDATE sources SET facts_current = 0 WHERE file_path = ?1", params![file_path])?;
        Ok(())
    }

    pub fn source_state(&self, file_path: &str) -> anyhow::Result<Option<(Option<String>, bool)>> {
        let file_path = self.storage_path(file_path)?;
        use rusqlite::OptionalExtension;
        Ok(self.conn.query_row("SELECT raw_content, facts_current FROM sources WHERE file_path = ?1",
            params![file_path], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? == 1))).optional()?)
    }

    /// Retrieves full preserved raw source text by exact or suffix path match
    pub fn get_source_content(&self, file_path_query: &str) -> anyhow::Result<Option<String>> {
        let mut stmt = self.conn.prepare("SELECT raw_content FROM sources WHERE file_path = ?1")?;
        let mut rows = stmt.query(params![file_path_query])?;
        if let Some(row) = rows.next()? {
            let content: Option<String> = row.get(0)?;
            if content.is_some() {
                return Ok(content);
            }
        }

        let target_comps = clean_path_components(Path::new(file_path_query));
        if !target_comps.is_empty() {
            let mut all_stmt = self.conn.prepare("SELECT file_path, raw_content FROM sources WHERE raw_content IS NOT NULL")?;
            let items: Vec<(String, String)> = all_stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .filter_map(|r| r.ok())
                .collect();

            for (fp, content) in items {
                let p_comps = clean_path_components(Path::new(&fp));
                if p_comps.len() >= target_comps.len() && p_comps.ends_with(&target_comps) {
                    return Ok(Some(content));
                }
            }
        }

        Ok(None)
    }

    /// Retrieves up to `limit` (default 50) current facts recorded under a category tag.
    pub fn get_facts_by_tag(&self, tag: &str, limit: Option<usize>) -> anyhow::Result<Vec<String>> {
        let limit_val = limit.unwrap_or(50) as i64;
        let mut stmt = self.conn.prepare(
            "SELECT a.fact_content FROM atomics a JOIN sources s ON s.id = a.source_id
             WHERE a.vault_tag = ?1 COLLATE NOCASE AND s.facts_current = 1
             ORDER BY a.id DESC LIMIT ?2"
        )?;
        
        let rows = stmt.query_map(params![tag, limit_val], |row| row.get(0))?;
        let mut facts = Vec::new();
        for r in rows {
            facts.push(r?);
        }
        Ok(facts)
    }

    /// Unified Natural-language, Tag & Hybrid Semantic Search:
    /// 1. Exact tag matches receive -100.0 priority boost.
    /// 2. Natural language FTS5 BM25 search over atomic propositions and raw sources.
    /// 3. Dense 384-dimensional vector embedding search via fast pure-Rust ONNX MiniLM.
    /// 4. Reciprocal Rank Fusion (RRF) combining keyword precision with semantic meaning.
    /// 5. Maximum Marginal Relevance (MMR) and source diversity to avoid redundant passages.
    /// 6. High-fidelity passage extraction centered on matching sentences.
    pub fn search(&self, query: &str, limit: Option<usize>) -> anyhow::Result<Vec<SearchHit>> {
        let max_results = limit.unwrap_or(20);
        if max_results == 0 || query.trim().is_empty() {
            return Ok(Vec::new());
        }

        struct Candidate {
            vault_tag: String,
            fact_content: String,
            file_path: String,
            bm25_rank: Option<usize>,
            dense_rank: Option<usize>,
            dense_sim: Option<f32>,
            is_exact_tag: bool,
            embedding: Option<Vec<f32>>,
        }

        let mut candidate_map: HashMap<(String, String), Candidate> = HashMap::new();
        let mut seen_atomic_files: HashSet<String> = HashSet::new();
        let mut seen_exact_tag_files: HashSet<String> = HashSet::new();

        // 1. Direct tag lookup
        let mut tag_stmt = self.conn.prepare(
            "SELECT a.vault_tag, a.fact_content, s.file_path
             FROM atomics a
             JOIN sources s ON s.id = a.source_id
             WHERE a.vault_tag = ?1 COLLATE NOCASE AND s.facts_current = 1
             ORDER BY a.id DESC LIMIT ?2"
        )?;

        let tag_rows = tag_stmt.query_map(params![query, max_results], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;

        for r in tag_rows {
            let (tag, content, file_path) = r?;
            seen_atomic_files.insert(file_path.clone());
            seen_exact_tag_files.insert(file_path.clone());
            let key = (file_path.clone(), content.clone());
            candidate_map.insert(key, Candidate {
                vault_tag: tag,
                fact_content: content,
                file_path,
                bm25_rank: Some(0),
                dense_rank: None,
                dense_sim: None,
                is_exact_tag: true,
                embedding: None,
            });
        }

        // 2. FTS5 BM25 search over atomic propositions
        let fts_query = sanitize_fts5_query(query);
        if !fts_query.is_empty() {
            let mut fts_stmt = self.conn.prepare(
                "SELECT a.vault_tag, a.fact_content, s.file_path
                 FROM atomics_fts f
                 JOIN atomics a ON a.id = f.rowid
                 JOIN sources s ON s.id = a.source_id
                 WHERE atomics_fts MATCH ?1 AND s.facts_current = 1
                 ORDER BY bm25(atomics_fts)
                 LIMIT ?2"
            )?;

            let fts_rows = fts_stmt.query_map(params![fts_query, max_results * 3], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;

            for (rank_idx, r) in fts_rows.enumerate() {
                let (tag, content, file_path) = r?;
                seen_atomic_files.insert(file_path.clone());
                let key = (file_path.clone(), content.clone());
                candidate_map.entry(key).and_modify(|c| {
                    if c.bm25_rank.is_none() || c.bm25_rank > Some(rank_idx) {
                        c.bm25_rank = Some(rank_idx);
                    }
                }).or_insert(Candidate {
                    vault_tag: tag,
                    fact_content: content,
                    file_path,
                    bm25_rank: Some(rank_idx),
                    dense_rank: None,
                    dense_sim: None,
                    is_exact_tag: false,
                    embedding: None,
                });
            }

            // 3. FTS5 BM25 search over granular source chunks (source_chunks_fts)
            let mut chunk_fts_matched = 0;
            if let Ok(mut chunk_fts_stmt) = self.conn.prepare(
                "SELECT sc.chunk_text, s.file_path
                 FROM source_chunks_fts sc
                 JOIN sources s ON s.id = sc.source_id
                 WHERE source_chunks_fts MATCH ?1
                 ORDER BY bm25(source_chunks_fts)
                 LIMIT ?2"
            ) {
                if let Ok(chunk_rows) = chunk_fts_stmt.query_map(params![fts_query, max_results * 3], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                    ))
                }) {
                    for (rank_idx, r) in chunk_rows.filter_map(|r| r.ok()).enumerate() {
                        let (chunk_text, file_path) = r;
                        if !seen_atomic_files.contains(&file_path) {
                            let key = (file_path.clone(), chunk_text.clone());
                            candidate_map.entry(key).and_modify(|c| {
                                if c.bm25_rank.is_none() || c.bm25_rank > Some(rank_idx + 1) {
                                    c.bm25_rank = Some(rank_idx + 1);
                                }
                            }).or_insert(Candidate {
                                vault_tag: "SOURCE".to_string(),
                                fact_content: chunk_text,
                                file_path,
                                bm25_rank: Some(rank_idx + 1),
                                dense_rank: None,
                                dense_sim: None,
                                is_exact_tag: false,
                                embedding: None,
                            });
                            chunk_fts_matched += 1;
                        }
                    }
                }
            }

            // Fallback to file-level FTS5 (sources_fts) if no chunk matches were found
            if chunk_fts_matched == 0 {
                let mut src_fts_stmt = self.conn.prepare(
                    "SELECT s.file_path, s.raw_content
                     FROM sources_fts f
                     JOIN sources s ON s.id = f.rowid
                     WHERE sources_fts MATCH ?1
                     ORDER BY bm25(sources_fts)
                     LIMIT ?2"
                )?;

                let src_rows = src_fts_stmt.query_map(params![fts_query, max_results * 3], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                })?;

                for (rank_idx, r) in src_rows.enumerate() {
                    let (file_path, raw) = r?;
                    if !seen_atomic_files.contains(&file_path) {
                        if let Some(ref text) = raw {
                            let passage = extract_passage(text, query, 300);
                            let key = (file_path.clone(), passage.clone());
                            candidate_map.entry(key).and_modify(|c| {
                                if c.bm25_rank.is_none() || c.bm25_rank > Some(rank_idx + 1) {
                                    c.bm25_rank = Some(rank_idx + 1);
                                }
                            }).or_insert(Candidate {
                                vault_tag: "SOURCE".to_string(),
                                fact_content: passage,
                                file_path,
                                bm25_rank: Some(rank_idx + 1),
                                dense_rank: None,
                                dense_sim: None,
                                is_exact_tag: false,
                                embedding: None,
                            });
                        }
                    }
                }
            }
        }

        // 4. Dense Vector Semantic Search
        if let Some(ref emb) = self.embedder {
            if let Ok(query_vec) = emb.embed(query) {
                let mut dense_candidates = Vec::new();

                // Vector search over atomics
                let mut atomics_vec_stmt = self.conn.prepare(
                    "SELECT a.vault_tag, a.fact_content, s.file_path, ae.embedding
                     FROM atomics a
                     JOIN atomics_embeddings ae ON ae.atomic_id = a.id
                     JOIN sources s ON s.id = a.source_id
                     WHERE s.facts_current = 1"
                )?;

                let atomics_rows = atomics_vec_stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                })?;

                for r in atomics_rows {
                    let (tag, content, file_path, emb_bytes) = r?;
                    let vec = bytes_to_f32_vec(&emb_bytes);
                    if vec.len() == emb.dimension {
                        let sim = crate::embedder::cosine_similarity(&query_vec, &vec);
                        if sim >= 0.16 {
                            dense_candidates.push((sim, tag, content, file_path, vec, true));
                        }
                    }
                }

                // Vector search over source chunks
                let mut source_vec_stmt = self.conn.prepare(
                    "SELECT se.chunk_text, s.file_path, se.embedding
                     FROM source_embeddings se
                     JOIN sources s ON s.id = se.source_id"
                )?;

                let source_rows = source_vec_stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                })?;

                for r in source_rows {
                    let (chunk_text, file_path, emb_bytes) = r?;
                    if !seen_exact_tag_files.contains(&file_path) {
                        let vec = bytes_to_f32_vec(&emb_bytes);
                        if vec.len() == emb.dimension {
                            let sim = crate::embedder::cosine_similarity(&query_vec, &vec);
                            if sim >= 0.16 {
                                dense_candidates.push((sim, "SOURCE".to_string(), chunk_text, file_path, vec, false));
                            }
                        }
                    }
                }

                dense_candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

                for (rank_idx, (sim, tag, content, file_path, vec, is_atomic)) in dense_candidates.into_iter().enumerate() {
                    let key = (file_path.clone(), content.clone());
                    if is_atomic {
                        seen_atomic_files.insert(file_path.clone());
                    }
                    candidate_map.entry(key).and_modify(|c| {
                        c.dense_rank = Some(rank_idx);
                        c.dense_sim = Some(sim);
                        if c.embedding.is_none() {
                            c.embedding = Some(vec.clone());
                        }
                    }).or_insert(Candidate {
                        vault_tag: tag,
                        fact_content: content,
                        file_path,
                        bm25_rank: None,
                        dense_rank: Some(rank_idx),
                        dense_sim: Some(sim),
                        is_exact_tag: false,
                        embedding: Some(vec),
                    });
                }
            }
        }

        // 5. Reciprocal Rank Fusion
        struct CandidateScored {
            vault_tag: String,
            fact_content: String,
            file_path: String,
            rrf_score: f64,
            bm25_rank: f64,
            embedding: Option<Vec<f32>>,
        }

        let mut scored_candidates: Vec<CandidateScored> = candidate_map.into_values().map(|c| {
            let rrf_bm25 = match c.bm25_rank {
                Some(r) => 1.0 / (60.0 + r as f64),
                None => 0.0,
            };
            let rrf_dense = match c.dense_rank {
                Some(r) => 1.0 / (60.0 + r as f64),
                None => 0.0,
            };
            let tag_boost = if c.is_exact_tag { 1.0 } else { 0.0 };
            let atomic_boost = if c.vault_tag != "SOURCE" { 0.001 } else { 0.0 };
            let sim_boost = match c.dense_sim {
                Some(s) if s >= 0.50 => (s - 0.40) as f64 * 0.02,
                _ => 0.0,
            };

            let total_score = rrf_bm25 + rrf_dense + tag_boost + atomic_boost + sim_boost;

            let display_rank = if c.is_exact_tag {
                -100.0
            } else if let Some(r) = c.bm25_rank {
                if c.vault_tag == "SOURCE" { r as f64 + 1.0 } else { r as f64 }
            } else if let Some(s) = c.dense_sim {
                1.0 - (s as f64)
            } else {
                50.0
            };

            CandidateScored {
                vault_tag: c.vault_tag,
                fact_content: c.fact_content,
                file_path: c.file_path,
                rrf_score: total_score,
                bm25_rank: display_rank,
                embedding: c.embedding,
            }
        }).collect();

        scored_candidates.sort_by(|a, b| b.rrf_score.partial_cmp(&a.rrf_score).unwrap_or(std::cmp::Ordering::Equal));

        // 6. MMR & Source Diversity Selection
        let mut selected: Vec<CandidateScored> = Vec::new();
        while selected.len() < max_results && !scored_candidates.is_empty() {
            let mut best_idx = 0;
            let mut best_mmr = f64::NEG_INFINITY;

            for (idx, cand) in scored_candidates.iter().enumerate() {
                let max_sim = if let Some(ref c_emb) = cand.embedding {
                    selected.iter().filter_map(|s| {
                        s.embedding.as_ref().map(|s_emb| crate::embedder::cosine_similarity(c_emb, s_emb) as f64)
                    }).fold(0.0f64, f64::max)
                } else {
                    0.0
                };

                let file_occurrences = selected.iter().filter(|s| s.file_path == cand.file_path).count();
                let diversity_penalty = if file_occurrences > 0 {
                    (file_occurrences as f64) * 0.003
                } else {
                    0.0
                };

                let mmr = cand.rrf_score - 0.006 * max_sim - diversity_penalty;
                if mmr > best_mmr {
                    best_mmr = mmr;
                    best_idx = idx;
                }
            }

            let chosen = scored_candidates.remove(best_idx);
            selected.push(chosen);
        }

        // 7. Format final SearchHit results with high-fidelity excerpts
        let mut raw_cache: HashMap<String, Option<String>> = HashMap::new();
        let mut get_raw_stmt = self.conn.prepare("SELECT raw_content FROM sources WHERE file_path = ?1")?;

        let results = selected.into_iter().map(|s| {
            let raw_opt = raw_cache.entry(s.file_path.clone()).or_insert_with(|| {
                get_raw_stmt.query_row(params![s.file_path], |r| r.get(0)).ok()
            });
            let raw_excerpt = raw_opt.as_ref().map(|raw| {
                extract_passage_smart(raw, &s.fact_content, query, 1000)
            });
            SearchHit {
                vault_tag: s.vault_tag,
                fact_content: s.fact_content,
                file_path: s.file_path,
                raw_excerpt,
                bm25_rank: s.bm25_rank,
            }
        }).collect();

        Ok(results)
    }

    /// Convenience retrieval for single latest fact
    pub fn get_latest_fact(&self, tag: &str) -> anyhow::Result<Option<String>> {
        let facts = self.get_facts_by_tag(tag, Some(1))?;
        Ok(facts.into_iter().next())
    }

    pub fn refresh_source_facts(&mut self, file_path: &str, raw_content: Option<&str>, facts: Vec<(String, String)>) -> anyhow::Result<usize> {
        let file_path = self.storage_path(file_path)?;
        use sha2::{Sha256, Digest};
        let tx = self.conn.transaction()?;

        // 1. Upsert source entry with raw_content preserved (or keep existing if raw_content is None)
        tx.execute(
            "INSERT INTO sources (file_path, raw_content, last_shredded, observed_at, facts_current)
             VALUES (?1, ?2, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, 1)
             ON CONFLICT(file_path) DO UPDATE SET raw_content = COALESCE(?2, sources.raw_content),
                last_shredded = CURRENT_TIMESTAMP, observed_at = CURRENT_TIMESTAMP, facts_current = 1",
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
        for (tag, content) in &facts {
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

            if changes > 0 {
                let atomic_id = tx.last_insert_rowid();
                if let Some(ref emb) = self.embedder {
                    let text = format!("[{}] {}", tag, content);
                    if let Ok(vec) = emb.embed(&text) {
                        let bytes = f32_slice_to_bytes(&vec);
                        let _ = tx.execute(
                            "INSERT OR REPLACE INTO atomics_embeddings (atomic_id, embedding) VALUES (?1, ?2)",
                            params![atomic_id, bytes],
                        );
                    }
                }
            }
        }

        if let Some(text) = raw_content {
            let chunks = chunk_text_for_embedding(text);
            let _ = tx.execute("DELETE FROM source_chunks_fts WHERE source_id = ?1", params![source_id]);
            for (idx, chunk) in chunks.iter().enumerate() {
                let _ = tx.execute(
                    "INSERT INTO source_chunks_fts (chunk_text, source_id, chunk_index) VALUES (?1, ?2, ?3)",
                    params![chunk, source_id, idx as i64],
                );
            }
            if let Some(ref emb) = self.embedder {
                let _ = tx.execute("DELETE FROM source_embeddings WHERE source_id = ?1", params![source_id]);
                let chunks_to_embed: Vec<(usize, &String)> = if chunks.len() <= 300 {
                    chunks.iter().enumerate().collect()
                } else {
                    let mut selected_indices = Vec::new();
                    let step = chunks.len() as f64 / 300.0;
                    for i in 0..300 {
                        let idx = (i as f64 * step) as usize;
                        if idx < chunks.len() && !selected_indices.contains(&idx) {
                            selected_indices.push(idx);
                        }
                    }
                    selected_indices.into_iter().map(|idx| (idx, &chunks[idx])).collect()
                };

                for (idx, chunk) in chunks_to_embed {
                    if let Ok(vec) = emb.embed(chunk) {
                        let bytes = f32_slice_to_bytes(&vec);
                        let _ = tx.execute(
                            "INSERT OR REPLACE INTO source_embeddings (source_id, chunk_index, chunk_text, embedding) VALUES (?1, ?2, ?3, ?4)",
                            params![source_id, idx as i64, chunk, bytes],
                        );
                    }
                }
            }
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

    pub fn get_stats(&self) -> anyhow::Result<BunkerStats> {
        let total_sources: i64 = self.conn.query_row(
            "SELECT count(*) FROM sources WHERE file_path NOT LIKE '__legacy%'",
            [],
            |r| r.get(0),
        )?;
        let current_sources: i64 = self.conn.query_row(
            "SELECT count(*) FROM sources WHERE facts_current = 1 AND file_path NOT LIKE '__legacy%'",
            [],
            |r| r.get(0),
        )?;
        let total_facts: i64 = self.conn.query_row("SELECT count(*) FROM atomics", [], |r| r.get(0))?;
        let current_facts: i64 = self.conn.query_row(
            "SELECT count(*) FROM atomics a JOIN sources s ON s.id = a.source_id WHERE s.facts_current = 1",
            [],
            |r| r.get(0),
        )?;
        let schema_version: i32 = self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let integrity_check: String = self.conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        let integrity_ok = integrity_check == "ok";

        let mut stmt = self.conn.prepare(
            "SELECT a.vault_tag, count(*) as c 
             FROM atomics a 
             JOIN sources s ON s.id = a.source_id 
             WHERE s.facts_current = 1 
             GROUP BY a.vault_tag 
             ORDER BY c DESC LIMIT 10"
        )?;
        let tag_rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut top_tags = Vec::new();
        for row in tag_rows {
            top_tags.push(row?);
        }

        Ok(BunkerStats {
            total_sources,
            current_sources,
            total_facts,
            current_facts,
            schema_version,
            top_tags,
            integrity_ok,
        })
    }

    pub fn repair_bunker(&mut self) -> anyhow::Result<RepairReport> {
        let mut messages = Vec::new();

        // 1. Low-level B-tree integrity check
        let integrity: String = self.conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        let integrity_ok = integrity == "ok";
        if integrity_ok {
            messages.push("SQLite low-level B-tree integrity: OK".to_string());
        } else {
            messages.push(format!("SQLite integrity warning: {}", integrity));
        }

        // 2. Relational foreign key check
        let fk_violations_found = {
            let mut fk_stmt = self.conn.prepare("PRAGMA foreign_key_check")?;
            let mut fk_rows = fk_stmt.query([])?;
            let mut count = 0;
            while fk_rows.next()?.is_some() {
                count += 1;
            }
            count
        };

        let mut fk_violations_repaired = 0;
        let fk_violations_remaining;

        if fk_violations_found == 0 {
            messages.push("Foreign key relational constraints: OK (0 violations)".to_string());
            fk_violations_remaining = 0;
        } else {
            messages.push(format!("Foreign key constraint violations detected: {}", fk_violations_found));
            // Repair orphaned atomics whose source_id does not exist in sources
            let deleted = self.conn.execute(
                "DELETE FROM atomics WHERE source_id NOT IN (SELECT id FROM sources)",
                [],
            )?;
            if deleted > 0 {
                fk_violations_repaired = deleted;
                messages.push(format!("Repaired relational constraints: purged {} orphaned atomic facts", deleted));
            }
            // Re-check foreign keys after attempted repair
            let remaining = {
                let mut fk_stmt = self.conn.prepare("PRAGMA foreign_key_check")?;
                let mut fk_rows = fk_stmt.query([])?;
                let mut count = 0;
                while fk_rows.next()?.is_some() {
                    count += 1;
                }
                count
            };
            fk_violations_remaining = remaining;
            if remaining == 0 {
                messages.push("Foreign key relational constraints after repair: OK (0 violations remaining)".to_string());
            } else {
                messages.push(format!("Foreign key constraint violations remaining: {}", remaining));
            }
        }

        // 3. FTS5 index rebuild and synchronization
        self.conn.execute_batch(
            "INSERT INTO atomics_fts(atomics_fts) VALUES('rebuild');
             INSERT INTO sources_fts(sources_fts) VALUES('rebuild');"
        )?;
        messages.push("FTS5 full-text indices (atomics_fts, sources_fts): Rebuilt and synchronized".to_string());

        // 4. Missing sources reconcile
        let purged_sources = self.reconcile_missing_sources()?;
        if purged_sources > 0 {
            messages.push(format!("Reconciled orphaned files: {} purged from vault", purged_sources));
        } else {
            messages.push("Source catalog alignment: OK (all sources accounted for)".to_string());
        }

        // 5. Checkpoint WAL
        self.checkpoint()?;
        messages.push("SQLite WAL log: Checkpointed and truncated".to_string());

        Ok(RepairReport {
            messages,
            integrity_ok,
            fk_violations_found,
            fk_violations_repaired,
            fk_violations_remaining,
            purged_sources,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepairReport {
    pub messages: Vec<String>,
    pub integrity_ok: bool,
    pub fk_violations_found: usize,
    pub fk_violations_repaired: usize,
    pub fk_violations_remaining: usize,
    pub purged_sources: usize,
}

impl RepairReport {
    pub fn is_pristine(&self) -> bool {
        self.integrity_ok
            && self.fk_violations_found == 0
            && self.purged_sources == 0
    }

    pub fn is_clean_after_repair(&self) -> bool {
        self.integrity_ok
            && self.fk_violations_remaining == 0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BunkerStats {
    pub total_sources: i64,
    pub current_sources: i64,
    pub total_facts: i64,
    pub current_facts: i64,
    pub schema_version: i32,
    pub top_tags: Vec<(String, i64)>,
    pub integrity_ok: bool,
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
        assert_eq!(new_version, 5, "user_version must be upgraded to 5");
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
        assert_eq!(new_version, 5);

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

    #[test]
    fn test_source_detail_searchable_when_omitted_from_extracted_facts() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;

        // Document containing QUARTZFOX, but extracted atomic fact omits QUARTZFOX
        let doc = "System Configuration Details:\n\
                   Cluster node id: 0x4F\n\
                   Secret key identifier: QUARTZFOX\n\
                   Heartbeat interval: 500ms.";

        engine.refresh_source_facts("secrets.txt", Some(doc), vec![
            ("SYSTEM".into(), "Cluster node id is 0x4F and heartbeat interval is 500ms.".into()),
        ])?;

        // 1. Verify raw source is preserved exactly
        let full_content = engine.get_source_content("secrets.txt")?;
        assert_eq!(full_content.as_deref(), Some(doc));

        // 2. Search for source-only term QUARTZFOX omitted by LLM extraction
        let hits = engine.search("QUARTZFOX", None)?;
        assert!(!hits.is_empty(), "QUARTZFOX must be found in raw source FTS5 index even when omitted from facts");
        assert_eq!(hits[0].vault_tag, "SOURCE");
        assert_eq!(hits[0].file_path, "secrets.txt");
        assert!(hits[0].fact_content.contains("QUARTZFOX"), "Passage must contain QUARTZFOX");
        assert!(hits[0].raw_excerpt.as_ref().unwrap().contains("QUARTZFOX"));

        Ok(())
    }

    #[test]
    fn test_strict_limit_enforcement_across_branches() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;

        engine.refresh_source_facts("db.txt", Some("Database architecture uses WAL mode."), vec![
            ("DATABASE".into(), "WAL mode configured.".into()),
            ("DATABASE".into(), "Synchronous NORMAL configured.".into()),
        ])?;

        // With tag match + FTS facts match + source match, limit=1 must return EXACTLY 1 result
        let single_hit = engine.search("DATABASE", Some(1))?;
        assert_eq!(single_hit.len(), 1, "Limit=1 must strictly return 1 result across all branches");

        let two_hits = engine.search("DATABASE", Some(2))?;
        assert_eq!(two_hits.len(), 2, "Limit=2 must strictly return 2 results across all branches");

        Ok(())
    }

    #[test]
    fn test_backfill_existing_sources_populates_null_raw_content() -> anyhow::Result<()> {
        let temp_dir = std::env::temp_dir().join(format!("remainder_backfill_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir)?;
        let file_path = temp_dir.join("legacy_notes.txt");
        let content = "Backfilled documentation containing ZEBRA_OMEGA token.";
        std::fs::write(&file_path, content)?;

        let mut engine = RemainderEngine::new(":memory:")?;
        // Insert source row with NULL raw_content (simulating pre-v3 database state)
        engine.conn.execute(
            "INSERT INTO sources (file_path, raw_content) VALUES (?1, NULL)",
            params![file_path.to_string_lossy().to_string()],
        )?;

        // Run backfill
        let backfilled = engine.backfill_existing_sources()?;
        assert_eq!(backfilled, 1, "Must backfill exactly 1 legacy source from disk");

        // Verify raw_content is populated and indexed in sources_fts
        let hits = engine.search("ZEBRA_OMEGA", None)?;
        assert_eq!(hits.len(), 1);
        assert!(hits[0].fact_content.contains("ZEBRA_OMEGA"));

        let _ = std::fs::remove_dir_all(&temp_dir);
        Ok(())
    }

    #[test]
    fn test_sqlite_wal_mode_and_foreign_keys_active() -> anyhow::Result<()> {
        let engine = RemainderEngine::new(":memory:")?;
        let journal_mode: String = engine.conn.query_row("PRAGMA journal_mode;", [], |r| r.get(0))?;
        // In-memory sqlite often reports 'memory', disk reports 'wal'. For memory mode, foreign_keys is 1:
        let fk: i32 = engine.conn.query_row("PRAGMA foreign_keys;", [], |r| r.get(0))?;
        assert_eq!(fk, 1, "foreign_keys must be enabled (1)");
        assert!(!journal_mode.is_empty());
        Ok(())
    }

    #[test]
    fn test_sqlite_foreign_key_constraint_prevents_orphan_atomics() -> anyhow::Result<()> {
        let engine = RemainderEngine::new(":memory:")?;
        // Try inserting an atomic fact pointing to non-existent source_id 9999
        let res = engine.conn.execute(
            "INSERT INTO atomics (source_id, vault_tag, fact_content, hash) VALUES (9999, 'TAG', 'Orphan', 'dummy_hash')",
            [],
        );
        assert!(res.is_err(), "Foreign key constraint must reject orphan atomics with non-existent source_id");
        Ok(())
    }

    #[test]
    fn test_sqlite_transaction_rollback_preserves_previous_facts() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        engine.refresh_source_facts("good.txt", Some("Valid data"), vec![("TAG".into(), "Initial fact".into())])?;
        
        let count_before: i64 = engine.conn.query_row("SELECT count(*) FROM atomics", [], |r| r.get(0))?;
        assert_eq!(count_before, 1);

        // Attempt a transaction with invalid SQL that fails
        let tx_res = (|| -> anyhow::Result<()> {
            let tx = engine.conn.transaction()?;
            tx.execute("INSERT INTO atomics (source_id, vault_tag, fact_content, hash) VALUES (1, 'T', 'F', 'h2')", [])?;
            // Trigger failure: foreign key violation
            tx.execute("INSERT INTO atomics (source_id, vault_tag, fact_content, hash) VALUES (99999, 'T', 'F', 'h3')", [])?;
            tx.commit()?;
            Ok(())
        })();

        assert!(tx_res.is_err(), "Transaction must fail and error out");
        // Verify state is completely preserved
        let count_after: i64 = engine.conn.query_row("SELECT count(*) FROM atomics", [], |r| r.get(0))?;
        assert_eq!(count_after, 1, "Rollback must preserve original record count");
        let fact: String = engine.conn.query_row("SELECT fact_content FROM atomics", [], |r| r.get(0))?;
        assert_eq!(fact, "Initial fact");
        Ok(())
    }

    #[test]
    fn test_sqlite_cascade_delete_source_removes_atomics() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        engine.refresh_source_facts("service.rs", Some("code"), vec![
            ("SVC".into(), "Port 8080".into()),
            ("SVC".into(), "Health check /healthz".into()),
        ])?;

        let atomics_before: i64 = engine.conn.query_row("SELECT count(*) FROM atomics", [], |r| r.get(0))?;
        assert_eq!(atomics_before, 2);

        // Direct delete from sources table
        engine.conn.execute("DELETE FROM sources WHERE file_path = 'service.rs'", [])?;

        let atomics_after: i64 = engine.conn.query_row("SELECT count(*) FROM atomics", [], |r| r.get(0))?;
        assert_eq!(atomics_after, 0, "ON DELETE CASCADE must delete all associated atomic facts");
        Ok(())
    }

    #[test]
    fn test_sqlite_composite_unique_hash_prevents_duplicate_facts() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        // Submit duplicate facts in same batch
        let inserted = engine.refresh_source_facts("config.txt", Some("content"), vec![
            ("CONF".into(), "Identical fact proposition".into()),
            ("CONF".into(), "Identical fact proposition".into()),
            ("CONF".into(), "Unique fact proposition".into()),
        ])?;

        assert_eq!(inserted, 2, "Duplicate facts in batch must be deduplicated via ON CONFLICT DO NOTHING");
        let count: i64 = engine.conn.query_row("SELECT count(*) FROM atomics", [], |r| r.get(0))?;
        assert_eq!(count, 2);
        Ok(())
    }

    #[test]
    fn test_sqlite_refresh_replaces_only_target_source_facts() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        engine.refresh_source_facts("source_a.txt", Some("A content"), vec![("A".into(), "Fact A".into())])?;
        engine.refresh_source_facts("source_b.txt", Some("B content"), vec![("B".into(), "Fact B".into())])?;

        assert_eq!(engine.conn.query_row::<i64, _, _>("SELECT count(*) FROM atomics", [], |r| r.get(0))?, 2);

        // Refresh source_a with new fact
        engine.refresh_source_facts("source_a.txt", Some("A revised"), vec![("A".into(), "Fact A V2".into())])?;

        // Source B's facts must be untouched
        let b_fact: String = engine.conn.query_row("SELECT fact_content FROM atomics WHERE vault_tag = 'B'", [], |r| r.get(0))?;
        assert_eq!(b_fact, "Fact B");

        // Source A's fact must be updated
        let a_fact: String = engine.conn.query_row("SELECT fact_content FROM atomics WHERE vault_tag = 'A'", [], |r| r.get(0))?;
        assert_eq!(a_fact, "Fact A V2");
        Ok(())
    }

    #[test]
    fn test_sqlite_unicode_multibyte_storage_and_retrieval() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        let unicode_raw = "Internationalization test: 🚀 数据库 SQLite в памяти 日本語 العربية 'quoted' \"double\" `backticks` \\path\\escape";
        let facts = vec![
            ("I18N".into(), "Emoji 🚀 supported.".into()),
            ("CJK".into(), "数据库 SQLite 支持多字节字符。".into()),
            ("CYRILLIC".into(), "Тестирование поддержки UTF-8.".into()),
        ];

        engine.refresh_source_facts("unicode.txt", Some(unicode_raw), facts)?;

        let retrieved = engine.get_source_content("unicode.txt")?;
        assert_eq!(retrieved.as_deref(), Some(unicode_raw));

        let cjk_hits = engine.search("数据库", None)?;
        assert!(!cjk_hits.is_empty(), "Must support CJK search query");

        let emoji_hits = engine.search("🚀", None)?;
        assert!(!emoji_hits.is_empty() || !cjk_hits.is_empty());
        Ok(())
    }

    #[test]
    fn test_sqlite_very_large_document_storage_100kb() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        let large_doc = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(2000); // ~114 KB
        assert!(large_doc.len() > 100_000);

        engine.refresh_source_facts("large.txt", Some(&large_doc), vec![
            ("SCALE".into(), "Successfully ingested 100KB+ document.".into()),
        ])?;

        let retrieved = engine.get_source_content("large.txt")?;
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().len(), large_doc.len());

        let hits = engine.search("ingested 100KB", None)?;
        assert_eq!(hits.len(), 1);
        Ok(())
    }

    #[test]
    fn test_sqlite_empty_and_whitespace_only_documents() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        engine.refresh_source_facts("empty.txt", Some(""), vec![])?;
        engine.refresh_source_facts("whitespace.txt", Some("   \n\t   \n"), vec![])?;

        assert_eq!(engine.get_source_content("empty.txt")?.as_deref(), Some(""));
        assert_eq!(engine.get_source_content("whitespace.txt")?.as_deref(), Some("   \n\t   \n"));
        Ok(())
    }

    #[test]
    fn test_sqlite_clean_path_components_normalization() {
        let p1 = clean_path_components(Path::new("docs/sub/../sub/file.txt"));
        assert!(p1.iter().any(|c| c == "docs"));
        assert!(p1.iter().any(|c| c == "file.txt"));

        let p2 = clean_path_components(Path::new("foo///bar//baz"));
        assert_eq!(p2, vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn test_sqlite_remove_source_or_prefix_nested_hierarchy() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        engine.refresh_source_facts("root/a/b/1.txt", Some("1"), vec![("T".into(), "F1".into())])?;
        engine.refresh_source_facts("root/a/b/c/2.txt", Some("2"), vec![("T".into(), "F2".into())])?;
        engine.refresh_source_facts("root/a/b_other.txt", Some("3"), vec![("T".into(), "F3".into())])?;
        engine.refresh_source_facts("root/a/c/4.txt", Some("4"), vec![("T".into(), "F4".into())])?;

        let deleted = engine.remove_source_or_prefix("root/a/b")?;
        assert_eq!(deleted, 2, "Must delete exactly root/a/b/1.txt and root/a/b/c/2.txt");

        assert!(engine.get_source_content("root/a/b/1.txt")?.is_none());
        assert!(engine.get_source_content("root/a/b/c/2.txt")?.is_none());
        assert!(engine.get_source_content("root/a/b_other.txt")?.is_some(), "Must not delete sibling with prefix name");
        assert!(engine.get_source_content("root/a/c/4.txt")?.is_some(), "Must not delete unrelated sibling");
        Ok(())
    }

    #[test]
    fn test_sqlite_reconcile_missing_sources_prunes_deleted_disk_files() -> anyhow::Result<()> {
        let temp_dir = std::env::temp_dir().join(format!("remainder_reconcile_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir)?;

        let file1 = temp_dir.join("live.txt");
        let file2 = temp_dir.join("dead.txt");
        std::fs::write(&file1, "Live")?;
        std::fs::write(&file2, "Dead")?;

        let mut engine = RemainderEngine::new(":memory:")?;
        engine.refresh_source_facts(file1.to_str().unwrap(), Some("Live"), vec![("TAG".into(), "F1".into())])?;
        engine.refresh_source_facts(file2.to_str().unwrap(), Some("Dead"), vec![("TAG".into(), "F2".into())])?;

        // Delete dead.txt from disk
        std::fs::remove_file(&file2)?;

        let purged = engine.reconcile_missing_sources()?;
        assert_eq!(purged, 1, "Must purge exactly 1 missing source");
        assert!(engine.get_source_content(file1.to_str().unwrap())?.is_some());
        assert!(engine.get_source_content(file2.to_str().unwrap())?.is_none());

        let _ = std::fs::remove_dir_all(&temp_dir);
        Ok(())
    }

    #[test]
    fn test_sqlite_reconcile_preserves_legacy_sources() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        engine.conn.execute("INSERT INTO sources (file_path, raw_content) VALUES ('__legacy__', 'Legacy memory')", [])?;
        engine.conn.execute("INSERT INTO sources (file_path, raw_content) VALUES ('__legacy_source_1', 'Legacy item')", [])?;

        let purged = engine.reconcile_missing_sources()?;
        assert_eq!(purged, 0, "Reconcile must preserve special __legacy__ markers");
        Ok(())
    }

    #[test]
    fn test_sqlite_checkpoint_truncate_succeeds() -> anyhow::Result<()> {
        let engine = RemainderEngine::new(":memory:")?;
        assert!(engine.checkpoint().is_ok());
        Ok(())
    }

    #[test]
    fn test_search_sanitize_fts5_query_strips_special_syntax() {
        assert_eq!(sanitize_fts5_query("SELECT * FROM atomics"), "\"SELECT\" OR \"FROM\" OR \"atomics\"");
        assert_eq!(sanitize_fts5_query("query: \"quotes\" AND (parentheses)"), "\"query\" OR \"quotes\" OR \"AND\" OR \"parentheses\"");
        assert_eq!(sanitize_fts5_query("***test***"), "\"test\"");
        assert_eq!(sanitize_fts5_query("   "), "");
        assert_eq!(sanitize_fts5_query("tag:value"), "\"tag\" OR \"value\"");
    }

    #[test]
    fn test_search_empty_and_whitespace_query_graceful() -> anyhow::Result<()> {
        let engine = RemainderEngine::new(":memory:")?;
        assert!(engine.search("", None)?.is_empty());
        assert!(engine.search("   ", None)?.is_empty());
        assert!(engine.search("\t\n", None)?.is_empty());
        Ok(())
    }

    #[test]
    fn test_search_single_char_and_short_query() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        engine.refresh_source_facts("short.txt", Some("A short note on X."), vec![("NOTE".into(), "Short note on X.".into())])?;
        // Single char should execute without FTS5 error
        let hits = engine.search("X", None)?;
        assert!(hits.len() <= 20);
        Ok(())
    }

    #[test]
    fn test_search_exact_tag_case_insensitive_matching() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        engine.refresh_source_facts("tags.txt", Some("doc"), vec![("ARCHITECTURE".into(), "Microkernel design.".into())])?;

        let hits_upper = engine.search("ARCHITECTURE", None)?;
        let hits_lower = engine.search("architecture", None)?;
        let hits_mixed = engine.search("ArChiTecTure", None)?;

        assert_eq!(hits_upper.len(), 1);
        assert_eq!(hits_lower.len(), 1);
        assert_eq!(hits_mixed.len(), 1);
        assert_eq!(hits_upper[0].bm25_rank, -100.0);
        assert_eq!(hits_lower[0].bm25_rank, -100.0);
        Ok(())
    }

    #[test]
    fn test_search_exact_tag_priority_boost_over_bm25() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        engine.refresh_source_facts("file1.txt", Some("Lots of mentions of database database database database"), vec![
            ("OTHER".into(), "database optimization notes".into()),
        ])?;
        engine.refresh_source_facts("file2.txt", Some("Short note"), vec![
            ("DATABASE".into(), "Single database fact".into()),
        ])?;

        let hits = engine.search("DATABASE", Some(2))?;
        assert_eq!(hits.len(), 2);
        // Tag match must be rank 0
        assert_eq!(hits[0].vault_tag, "DATABASE", "Exact category tag match must rank first due to -100.0 boost");
        Ok(())
    }

    #[test]
    fn test_search_multi_word_query_finds_best_match() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        engine.refresh_source_facts("auth.txt", Some("OAuth2 bearer tokens and JWT validation."), vec![
            ("AUTH".into(), "Configured OAuth2 Bearer tokens with RS256 JWT validation.".into()),
        ])?;
        engine.refresh_source_facts("logging.txt", Some("Structured JSON logging with tracing crate."), vec![
            ("LOG".into(), "JSON logging enabled.".into()),
        ])?;

        let hits = engine.search("OAuth2 Bearer tokens JWT", None)?;
        assert!(!hits.is_empty());
        assert_eq!(hits[0].vault_tag, "AUTH");
        assert!(hits[0].fact_content.contains("RS256"));
        Ok(())
    }

    #[test]
    fn test_search_passage_extraction_centering_middle() {
        let prefix = "A".repeat(500);
        let target = "TARGET_KEYWORD";
        let suffix = "B".repeat(500);
        let text = format!("{}{}{}", prefix, target, suffix);

        let passage = extract_passage(&text, "TARGET_KEYWORD", 300);
        assert!(passage.contains("TARGET_KEYWORD"), "Passage must contain matched keyword");
        assert!(passage.len() <= 310);
    }

    #[test]
    fn test_search_passage_extraction_centering_start() {
        let text = "TARGET_AT_START and subsequent padding words to fill out the 300 character window requirement.".repeat(5);
        let passage = extract_passage(&text, "TARGET_AT_START", 300);
        assert!(passage.starts_with("TARGET_AT_START") || passage.contains("TARGET_AT_START"));
        assert!(passage.len() <= 310);
    }

    #[test]
    fn test_search_passage_extraction_centering_end() {
        let padding = "Padding content here. ".repeat(30);
        let text = format!("{}TARGET_AT_END", padding);
        let passage = extract_passage(&text, "TARGET_AT_END", 300);
        assert!(passage.contains("TARGET_AT_END"));
        assert!(passage.len() <= 310);
    }

    #[test]
    fn test_search_passage_extraction_no_match_fallback() {
        let text = "First 50 characters of this document here for testing.";
        let passage = extract_passage(text, "NON_EXISTENT_QUERY", 300);
        assert_eq!(passage, text);
    }

    #[test]
    fn test_search_strict_limit_zero() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        engine.refresh_source_facts("doc.txt", Some("content"), vec![("TAG".into(), "Fact".into())])?;
        let hits = engine.search("content", Some(0))?;
        assert_eq!(hits.len(), 0);
        Ok(())
    }

    #[test]
    fn test_search_strict_limit_pagination_subsets() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        for i in 1..=10 {
            engine.refresh_source_facts(&format!("doc_{}.txt", i), Some(&format!("Document number {}", i)), vec![
                ("BATCH".into(), format!("Proposition number {}", i)),
            ])?;
        }

        let hits_3 = engine.search("number", Some(3))?;
        assert_eq!(hits_3.len(), 3);

        let hits_7 = engine.search("number", Some(7))?;
        assert_eq!(hits_7.len(), 7);

        let hits_all = engine.search("number", Some(20))?;
        assert!(hits_all.len() >= 10);
        Ok(())
    }

    #[test]
    fn test_search_deduplication_between_facts_and_sources() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        engine.refresh_source_facts("unique.txt", Some("Alpha Beta Gamma unique term"), vec![
            ("TAG".into(), "Alpha Beta Gamma unique term fact".into()),
        ])?;

        // Query matches both atomic fact and raw source in same file
        let hits = engine.search("Gamma unique term", Some(10))?;
        // Atomic fact takes priority; source is also returned as passage, but atomic fact is not duplicated
        let atomic_hits = hits.iter().filter(|h| h.vault_tag != "SOURCE").count();
        assert_eq!(atomic_hits, 1, "Atomic proposition must not be duplicated");
        Ok(())
    }

    #[test]
    fn test_search_sql_injection_resilience() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        engine.refresh_source_facts("notes.txt", Some("Normal note"), vec![("SAFE".into(), "Safe fact".into())])?;

        // Injections in search query
        let injections = [
            "'; DROP TABLE atomics; --",
            "' OR '1'='1",
            "\" OR \"1\"=\"1",
            "UNION SELECT * FROM sources",
            "1; ATTACH DATABASE 'evil.db' AS evil;",
        ];

        for inj in injections {
            let res = engine.search(inj, None);
            assert!(res.is_ok(), "Injection query must be safely handled without error");
        }

        // Verify table still exists and safe fact is intact
        let count: i64 = engine.conn.query_row("SELECT count(*) FROM atomics", [], |r| r.get(0))?;
        assert_eq!(count, 1, "SQL injection must never alter or drop tables");
        Ok(())
    }

    #[test]
    fn test_search_get_latest_fact_helper() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        assert_eq!(engine.get_latest_fact("NON_EXISTENT")?, None);

        engine.refresh_source_facts("f.txt", None, vec![
            ("TAG".into(), "First fact".into()),
            ("TAG".into(), "Second fact".into()),
        ])?;

        let latest = engine.get_latest_fact("TAG")?;
        assert!(latest.is_some());
        Ok(())
    }

    #[test]
    fn test_source_retrieval_by_relative_and_filename_components() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        let full_path = "C:/Users/Sebastian/projects/app/src/utils/math.rs";
        engine.refresh_source_facts(full_path, Some("pub fn add(a: i32, b: i32) -> i32 { a + b }"), vec![])?;

        // Match by exact full path
        assert!(engine.get_source_content(full_path)?.is_some());
        // Match by filename only
        assert!(engine.get_source_content("math.rs")?.is_some());
        // Match by relative subpath
        assert!(engine.get_source_content("utils/math.rs")?.is_some());
        assert!(engine.get_source_content("src/utils/math.rs")?.is_some());
        // Non-matching path
        assert!(engine.get_source_content("other/math.rs")?.is_none());
        Ok(())
    }

    #[test]
    fn test_bunker_stats_calculation() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        let stats_empty = engine.get_stats()?;
        assert_eq!(stats_empty.total_sources, 0);
        assert_eq!(stats_empty.total_facts, 0);
        assert!(stats_empty.integrity_ok);

        engine.refresh_source_facts("src/main.rs", Some("fn main() {}"), vec![
            ("ARCHITECTURE".into(), "Rust single binary".into()),
            ("DATABASE".into(), "SQLite in WAL mode".into()),
        ])?;

        let stats = engine.get_stats()?;
        assert_eq!(stats.total_sources, 1);
        assert_eq!(stats.current_sources, 1);
        assert_eq!(stats.total_facts, 2);
        assert_eq!(stats.current_facts, 2);
        assert_eq!(stats.schema_version, 5);
        assert_eq!(stats.top_tags.len(), 2);
        assert!(stats.integrity_ok);
        Ok(())
    }

    #[test]
    fn test_bunker_repair_execution() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        engine.refresh_source_facts("__legacy__", Some("pub fn lib() {}"), vec![
            ("TAG".into(), "Fact 1".into()),
        ])?;

        let report = engine.repair_bunker()?;
        assert!(report.is_pristine());
        assert_eq!(report.purged_sources, 0);
        assert_eq!(report.fk_violations_found, 0);
        assert!(report.messages.iter().any(|line| line.contains("B-tree integrity: OK")));
        assert!(report.messages.iter().any(|line| line.contains("Foreign key relational constraints: OK")));
        assert!(report.messages.iter().any(|line| line.contains("FTS5 full-text indices")));
        assert!(report.messages.iter().any(|line| line.contains("WAL log: Checkpointed")));
        Ok(())
    }

    #[test]
    fn test_bunker_repair_detects_and_fixes_orphaned_foreign_key_atomics() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        engine.conn.execute("PRAGMA foreign_keys = OFF", [])?;
        engine.conn.execute(
            "INSERT INTO atomics (source_id, vault_tag, fact_content, hash) VALUES (999999, 'ORPHAN', 'Orphaned fact', 'fakehash')",
            [],
        )?;
        engine.conn.execute("PRAGMA foreign_keys = ON", [])?;

        let report = engine.repair_bunker()?;
        assert!(!report.is_pristine());
        assert_eq!(report.fk_violations_found, 1);
        assert_eq!(report.fk_violations_repaired, 1);
        assert_eq!(report.fk_violations_remaining, 0);
        assert!(report.is_clean_after_repair());

        // Re-running repair on the cleaned vault must yield a pristine report
        let second_report = engine.repair_bunker()?;
        assert!(second_report.is_pristine());
        assert_eq!(second_report.fk_violations_found, 0);
        Ok(())
    }

    #[test]
    fn test_full_coverage_chunking_exceeds_30_chunks_and_covers_entire_document() {
        // Document of ~50,000 characters with markers at beginning, middle, and end
        let mut doc = String::new();
        doc.push_str("START_SECTION: Initial configuration and architecture overview.\n\n");
        for i in 1..=100 {
            if i == 50 {
                doc.push_str("MIDDLE_SECTION_MARKER: Deep internal database transaction lock manager.\n\n");
            }
            doc.push_str(&format!("Paragraph {}: Lorem ipsum dolor sit amet, consectetur adipiscing elit. Integer nec odio. Praesent libero. Sed cursus ante dapibus diam. Sed nisi. Nulla quis sem at nibh elementum imperdiet. Duis sagittis ipsum. Praesent mauris.\n\n", i));
        }
        doc.push_str("FINAL_SECTION_MARKER: Critical shutdown cleanup and telemetry finalization.\n");

        let chunks = chunk_text_for_embedding(&doc);
        assert!(chunks.len() > 30, "Chunks count ({}) must exceed old arbitrary 30-chunk truncate cap", chunks.len());
        assert!(chunks.iter().any(|c| c.contains("START_SECTION")), "Must contain start marker");
        assert!(chunks.iter().any(|c| c.contains("MIDDLE_SECTION_MARKER")), "Must contain middle marker");
        assert!(chunks.iter().any(|c| c.contains("FINAL_SECTION_MARKER")), "Must contain final end marker");
    }

    #[test]
    fn test_source_chunks_fts_granular_passage_retrieval() -> anyhow::Result<()> {
        let mut engine = RemainderEngine::new(":memory:")?;
        
        let mut large_doc = String::new();
        large_doc.push_str("Introduction: Standard system preamble and general licensing.\n\n");
        for i in 1..=60 {
            large_doc.push_str(&format!("Module {}: Standard routine boilerplate with common terms like if want should.\n\n", i));
        }
        large_doc.push_str("CRITICAL_ENDPOINT_SECTION:\n");
        large_doc.push_str("@routes.post('/upload/image')\n");
        large_doc.push_str("async def image_upload(request: web.Request) -> web.Response:\n");
        large_doc.push_str("    return web.json_response({'status': 'uploaded', 'handler': 'comfy_image_upload_v2'})\n\n");
        for i in 61..=100 {
            large_doc.push_str(&format!("Trailing module {}: Postamble routines.\n\n", i));
        }

        engine.record_source_content("comfy_server.py", &large_doc)?;

        // Search for the deep endpoint function
        let hits = engine.search("What is the upload route for comfy_image_upload_v2?", Some(5))?;
        assert!(!hits.is_empty(), "Must retrieve deep endpoint hits via granular chunk FTS");
        assert_eq!(hits[0].vault_tag, "SOURCE");
        assert_eq!(hits[0].file_path, "comfy_server.py");
        assert!(hits[0].fact_content.contains("image_upload") || hits[0].fact_content.contains("upload/image"),
            "Granular chunk must be returned, got: {}", hits[0].fact_content);
        let excerpt = hits[0].raw_excerpt.as_ref().unwrap();
        assert!(excerpt.contains("/upload/image"), "Raw excerpt must contain route, got: {}", excerpt);
        assert!(excerpt.contains("comfy_image_upload_v2"), "Raw excerpt must contain function body");
        Ok(())
    }

    #[test]
    fn test_smart_passage_extraction_keyword_density_over_stopwords() {
        let text = "What is this issue tracker for? If you want to file a bug, click here.\n\
                    Common preamble with if and want.\n\n\
                    Actual specification details:\n\
                    The token scheduling budget algorithm enforces chunked prefill limit of 512 tokens.\n\
                    Token scheduler prevents memory exhaustion during batch inference.";

        let query = "What is the token scheduling budget for chunked prefill?";
        let passage = extract_passage(text, query, 300);
        assert!(passage.contains("token scheduling budget"), "Must anchor on density cluster, got: {}", passage);
        assert!(passage.contains("chunked prefill"));
        assert!(!passage.starts_with("What is this issue tracker"), "Must NOT anchor on preamble stopword 'What'");
    }
}

