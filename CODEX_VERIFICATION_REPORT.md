# REMAINDER Industrial Hardening Report — Revision 7
**Target Repository:** `C:\Users\Sebastian\.gemini\antigravity-ide\scratch\remainder`  
**Author:** Antigravity Engineering Team  
**Evaluation Standard:** Independent Code Review Resolution (`COMPARISON.md`), Raw Source FTS5 Text Search, Passage Extraction, Automated Legacy Backfill, Cross-Branch Limit Enforcement, and Full Source Retrieval.

---

## 1. Summary of Latest Findings & Resolutions

Following Codex's in-memory reproduction and inspection of commit `7d61551`, three specific gaps were identified and resolved:

| Finding Raised by Codex | Technical Defect in Previous Code | Resolution Implemented | Verification Evidence |
|---|---|---|---|
| **1. Preserved details not searchable** | FTS5 index only covered `atomics` (tags and extracted facts). A detail present in source text but omitted by the LLM (e.g. `QUARTZFOX`) yielded 0 search hits, and excerpts were fixed to the first 300 characters. | Added `sources_fts` virtual table with triggers (`sources_ai`, `sources_ad`, `sources_au`). Implemented 3-branch search that queries `sources_fts` and uses `extract_passage` to center 300-char excerpts around query matches. Added `get_source_content` and MCP `get_source` tool. | **PASSED:** `test_source_detail_searchable_when_omitted_from_extracted_facts` reproduces `QUARTZFOX`: returns hit with `vault_tag == "SOURCE"` and passage containing `QUARTZFOX`. |
| **2. Existing sources not backfilled & failed extraction dropped raw text** | `sources` table had `raw_content` column, but pre-existing sources remained `NULL` (5 sources, 0 with raw text). Failed extraction also prevented raw text from being stored. | Added `backfill_existing_sources` called on `RemainderEngine::new` (reads disk files for any NULL sources). Added immediate `record_source_content` call before LLM dispatch and on inference retry. | **PASSED:** Live `remainder.db` check confirms 5/5 sources have `raw_content` populated. Verified by `test_backfill_existing_sources_populates_null_raw_content` and `test_production_failed_extraction_preserves_raw_source_content`. |
| **3. Result limit not enforced across branches** | Exact-tag branch and FTS branch each applied `limit` separately, returning more than the requested count (e.g. `limit=1` returned 2 results). | Merged results are ranked by BM25 score, deduplicated, and strictly truncated: `results.sort_by(...); results.truncate(max_results);`. | **PASSED:** `test_strict_limit_enforcement_across_branches` verifies `limit=1` returns exactly 1 hit and `limit=2` returns exactly 2 hits. Verified live: `remainder query "DATABASE" 1` returns 1 hit. |

---

## 2. Database Schema Upgrades (`user_version = 4`)

```sql
-- FTS5 full-text indexing over raw source text
CREATE VIRTUAL TABLE IF NOT EXISTS sources_fts USING fts5(
    file_path,
    raw_content,
    content='sources',
    content_rowid='id'
);

-- Real-time synchronization triggers on sources table
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
```

---

## 3. Search Engine Architecture & Passage Extraction ([src/engine.rs](file:///C:/Users/Sebastian/.gemini/antigravity-ide/scratch/remainder/src/engine.rs))

### Passage Extraction
```rust
pub fn extract_passage(raw: &str, query: &str, max_len: usize) -> String {
    // Locates query terms (>= 3 chars) in source text and centers a 300-char
    // window around the match, rather than taking the beginning of the file.
}
```

### 3-Branch Merged Search with Final Limit Truncation
```rust
pub fn search(&self, query: &str, limit: Option<usize>) -> anyhow::Result<Vec<SearchHit>> {
    let max_results = limit.unwrap_or(20);
    let mut results = Vec::new();
    let mut seen_atomic_ids = HashSet::new();
    let mut seen_source_ids = HashSet::new();

    // Branch 1: Direct tag matches (boosted to -100.0 BM25 rank)
    // Branch 2: Atomics FTS BM25 search (distilled propositions)
    // Branch 3: Sources FTS BM25 search (surfaces raw source details omitted by LLM)
    // Branch 4: Final limit enforcement:
    results.sort_by(|a, b| a.bm25_rank.partial_cmp(&b.bm25_rank).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(max_results);

    Ok(results)
}
```

---

## 4. Live System Verification

### A. Strict Result Limit Check
```powershell
$ remainder query "DATABASE" 1
[BUNKER] [DATABASE] -> Uses SQLite WAL mode with foreign_keys=ON and auto-checkpointing for corruption prevention.
         Source: C:\Users\Sebastian\.gemini\antigravity-ide\scratch\remainder\.\notes.txt
         Context: "DATABASE -> SQLite WAL mode enabled with foreign_keys=ON and auto-checkpointing for zero corruption."
# Exactly 1 hit returned (limit=1 strictly enforced)
```

### B. Full Source Retrieval Tool Check
```powershell
$ remainder source notes.txt
DATABASE -> SQLite WAL mode enabled with foreign_keys=ON and auto-checkpointing for zero corruption.
```

### C. Live Database Backfill Status
```powershell
$ node -e "const {DatabaseSync} = require('node:sqlite'); const db = new DatabaseSync('remainder.db'); console.log('user_version:', db.prepare('PRAGMA user_version').get()); console.log('sources:', db.prepare('SELECT id, file_path, length(raw_content) as raw_len FROM sources').all());"
user_version: [Object: null prototype] { user_version: 4 }
sources: [
  { id: 1, file_path: '...\\notes.txt', raw_len: 101 },
  { id: 3, file_path: '...\\src\\main.rs', raw_len: 45115 },
  { id: 4, file_path: '...\\src\\shredder.rs', raw_len: 2044 },
  { id: 5, file_path: '...\\LOG.md', raw_len: 146 },
  { id: 6, file_path: '...\\src\\engine.rs', raw_len: 36297 }
]
# All 5/5 sources backfilled with raw text
```

### D. MCP Server Tool Registration
Exposes both `query_bunker` (with `limit` parameter) and `get_source` (full-text document retrieval):
```json
{
  "tools": [
    {
      "name": "query_bunker",
      "description": "Retrieves technical facts and matching source passages from the local REMAINDER vault using natural-language search (BM25) or category tag lookup, with source file provenance and context excerpts.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "query": { "type": "string", "description": "Natural-language query or category tag" },
          "tag": { "type": "string", "description": "Optional category tag (backward compatibility)" },
          "limit": { "type": "integer", "description": "Maximum number of results to return (default 20)" }
        },
        "required": ["query"]
      }
    },
    {
      "name": "get_source",
      "description": "Retrieves the full preserved verbatim source text for a file path from the REMAINDER vault.",
      "inputSchema": {
        "type": "object",
        "properties": {
          "path": { "type": "string", "description": "Relative or absolute file path" }
        },
        "required": ["path"]
      }
    }
  ]
}
```

---

## 5. Automated Test Suite Results (14 Passed, 0 Failed)

```powershell
$ cargo test --bin remainder
running 14 tests
test tests::test_exponential_backoff_timing_and_schedule ... ok
test engine::tests::test_migration_error_rolls_back_and_preserves_original_table ... ok
test engine::tests::test_multi_fact_retrieval_preserves_decision_details ... ok
test engine::tests::test_source_detail_searchable_when_omitted_from_extracted_facts ... ok
test engine::tests::test_migration_preserves_null_hash_rows ... ok
test engine::tests::test_strict_limit_enforcement_across_branches ... ok
test tests::test_production_failed_extraction_preserves_raw_source_content ... ok
test engine::tests::test_backfill_existing_sources_populates_null_raw_content ... ok
test engine::tests::test_sqlite_fts5_natural_language_search_and_provenance ... ok
test tests::test_production_natural_language_bm25_search_and_provenance ... ok
test tests::test_production_directory_rename_handler_executes_rename_event ... ok
test engine::tests::test_migration_handles_v0_missing_source_id_column ... ok
test engine::tests::test_directory_removal_component_matching_prevents_wildcard_bleed ... ok
test tests::test_production_worker_http_stub_retry_pipeline ... ok

test result: ok. 14 passed; 0 failed; 0 ignored; finished in 0.03s
```
