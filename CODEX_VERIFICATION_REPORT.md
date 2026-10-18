# REMAINDER Industrial Hardening Report — Revision 3
**Target Repository:** `C:\Users\Sebastian\.gemini\antigravity-ide\scratch\remainder`  
**Author:** Antigravity Engineering Team  
**Context:** Technical resolution targeting the three outstanding findings from Codex's latest review.

---

## 1. Summary of Latest Findings & Technical Resolutions

| Finding Raised by Codex | Technical Defect in Previous Code | Resolution Implemented | Automated & Live Verification |
|---|---|---|---|
| **1. Directory deletion can remove unrelated memory** | Used unescaped SQL `LIKE (? || '/%')`. In SQL `LIKE`, `_` is a single-character wildcard, causing `arch_notes` to match and delete `archXnotes` or `arch_notes_extra`. | Replaced SQL `LIKE` with component-based path matching in Rust (`clean_path_components`). Compares path components directly (`p_comps[..target.len()] == target`). Cannot match `archXnotes` or `arch_notes_extra`. | **PASSED:** Unit test `test_directory_removal_component_matching_prevents_wildcard_bleed`. Deleting `arch_notes` purged `arch_notes/sub.txt` while preserving `archXnotes/sub.txt` and `arch_notes_extra.txt`. |
| **2. Directory renames lose memory of relocated files** | Event handler filtered destination by `.extension()`. Renamed directories lack extensions, so the destination was ignored without traversing nested files. | In `main.rs`, rename events (`RenameMode::Both`) dispatch `WatchEvent::Rename { from, to }`. If `to.is_dir()`, recursively walks all nested files via `walkdir::WalkDir`, schedules them for ingestion under the new path, and purges the old prefix from SQLite. | **PASSED:** Unit test `test_directory_rename_collects_all_nested_supported_files` and live test renaming `services_spec` to `deployed_services` (purged old source and shredded relocated file into 3 distinct atomic truths). |
| **3. Extraction loses decision details** | Prompt requested a "single, dense technical fact", causing the model to drop companion decisions. Retrieval used `LIMIT 1`, returning only the latest fact per tag. | Prompt updated to extract *all* distinct technical facts, architectural decisions, and operational constraints without condensing them into one. Added `engine.get_facts_by_tag(tag, limit)` returning all facts under a tag. CLI and MCP now return all facts for a tag. | **PASSED:** Unit test `test_multi_fact_retrieval_preserves_decision_details` and live test on `arch_decisions.txt` extracting 6 distinct facts across `DATABASE_ARCHITECTURE`, `DATABASE_SETTINGS`, `SECURITY`, `CACHE_ARCHITECTURE`, and `CACHE_SETTINGS`. |

---

## 2. Review Clarifications & System Hardening

1. **Exponential Backoff Formula:**
   - Previous delay was linear ($2n = 2, 4, 6, 8, 10$ s).
   - Replaced with true exponential backoff: $2^{attempt - 1}$ seconds:
     - Attempt 1: **1s** ($2^0$)
     - Attempt 2: **2s** ($2^1$)
     - Attempt 3: **4s** ($2^2$)
     - Attempt 4: **8s** ($2^3$)
     - Attempt 5: **16s** ($2^4$)

2. **Automated Recovery from Induced Failure:**
   - Added unit test `test_retry_recovers_after_induced_transient_failure` in [src/main.rs](file:///C:/Users/Sebastian/.gemini/antigravity-ide/scratch/remainder/src/main.rs#L440-L490).
   - Simulates a flaky extraction closure that returns `503 Service Unavailable` on attempts 1 and 2, and succeeds on attempt 3.
   - Asserts that retries follow exponential delays (1s, 2s), successfully commit the recovered facts to SQLite, and clear the retry state.

3. **Event Loop Non-Blocking I/O:**
   - Replaced synchronous `std::fs::read_to_string` with `tokio::fs::read_to_string` awaited asynchronously.
   - Inference runs inside isolated background `tokio::spawn` tasks communicating via `tokio::sync::mpsc` channels.

4. **Auditor Labeling:**
   - Updated document attribution: these verifications and tests were conducted by the Antigravity engineering team to address the specific criteria and failure scenarios identified in Codex's review.

---

## 3. Test Suite Execution Output

```powershell
$ cargo test --bin remainder
   Compiling remainder v0.2.0 (C:\Users\Sebastian\.gemini\antigravity-ide\scratch\remainder)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 2.84s
     Running unittests src\main.rs (target\debug\deps\remainder-5f5f618b28a76731.exe)

running 7 tests
test engine::tests::test_migration_error_rolls_back_and_preserves_original_table ... ok
test engine::tests::test_migration_preserves_null_hash_rows ... ok
test tests::test_directory_rename_collects_all_nested_supported_files ... ok
test engine::tests::test_multi_fact_retrieval_preserves_decision_details ... ok
test tests::test_retry_recovers_after_induced_transient_failure ... ok
test engine::tests::test_directory_removal_component_matching_prevents_wildcard_bleed ... ok
test engine::tests::test_migration_handles_v0_missing_source_id_column ... ok

test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
```
