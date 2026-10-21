# REMAINDER reliability update — 2026-10-18

Implemented over commit `88a73e9` and the user's existing uncommitted updates. Those changes were retained; no commit was created on their behalf.

## Delivered

- Unicode-safe passage extraction. Lowercase match offsets are mapped to original character positions; excerpt boundaries cannot split UTF-8 sequences.
- Source freshness tracking in schema 5. Changed sources immediately stop serving their previous extracted facts as current. New raw text remains searchable during inference failure. Old facts remain stored until a successful transactional replacement.
- Separate `observed_at` and `last_shredded` timestamps: merely recording a file no longer claims extraction completed.
- Conservative migration: legacy extracted facts are retained but hidden until re-extracted, because earlier schemas did not establish which source revision produced them. Original text remains searchable.
- Request deadline of 120 seconds and connection deadline of 5 seconds. A nonresponding server becomes a retryable error.
- At most two dispatched inference workers. Semaphore permits are held through completion; queued files remain pending instead of spawning unlimited requests.
- Startup scan and 30-second reconciliation discover existing files, offline changes, and missed events. Startup retries unfinished extraction; periodic scanning does not continually reset exhausted retries for unchanged content.
- Case-insensitive file-extension support for text, Rust, Markdown, Python, JS/JSX, TS/TSX, JSON, TOML, YAML, Go, Java, C/C++, C#, SQL, shell, PowerShell, HTML and CSS.
- Excludes `.git`, `target`, `node_modules`, `.venv`, `venv`, and `__pycache__` components. This is not a complete `.gitignore` implementation.
- Normalizes redundant path components in watch events and reuses older source records containing `.` components. This fixes duplicate source ownership found by the release-binary test.
- Strict extraction parsing: partially malformed responses schedule a retry instead of replacing a file's complete facts with only the subset that parsed. An arrow inside fact text remains valid.
- File changes during inference explicitly schedule current content, even if a watcher notification was missed.
- Optional `REMAINDER_DB`, `REMAINDER_MODEL`, and `REMAINDER_OLLAMA_URL` settings, enabling isolated projects and reproducible integration tests.

## Validation actually run

1. `cargo test --offline --locked --all-targets`: **69 passed, 0 failed**. Eight new regression tests cover Unicode boundaries, freshness, schema-4 migration, partial output, hung HTTP requests, scan recovery, debounce invalidation and redundant path identity.
2. `cargo build --offline --locked --release --bin remainder`: successful. The existing `Shredder::new` constructor is unused in the main binary and produces a harmless dead-code warning; it remains used by the benchmark binary.
3. `python scripts/verify_daemon.py`: nine checks passed against the actual release executable using a separate temporary database and local HTTP stub:
   - ingestion of six pre-existing `.PY` files;
   - observed HTTP concurrency peak exactly two;
   - stdio MCP initialization, tool listing and query round trip;
   - changed raw text preserved during HTTP failure;
   - stale proposition hidden and new source searchable;
   - successful retry without another user edit;
   - deletion propagation;
   - restart recovery of offline edits and a new TypeScript file;
   - database integrity and foreign-key checks.
4. The independent audit harness now reports both original Unicode panic reproductions as false, zero old-tag results after source replacement, searchable updated raw text, and strict result-limit enforcement.
5. Migrated a backup copy of the real database first. Integrity passed and all five source rows and five fact rows were preserved.
6. Installed the release binary and migrated the real database to version 5. Integrity check returned `ok`, foreign-key check returned no rows, and counts remained five sources and five facts.

The HTTP stub tests exercise the production executable and lifecycle; they do not measure Llama's semantic extraction accuracy. No claim of superiority over MemPalace follows from these tests alone.

## Installation and backups

Installed executable: `C:\Users\Sebastian\.local\bin\remainder.exe`.

Release and installed SHA-256:
`FCF1E3F6C81B12313E9533B4FD4F43B0A8D861A7314C5B45AF61EB36EB89E70F`.

Backup directory: `C:\Users\Sebastian\memory-comparison\backups\remainder-20261018-fixes`.

- `before.db`: consistent SQLite backup, including committed WAL data.
- `before.exe`: previous installed executable.
- `migration-check.db`: successfully migrated verification copy.

No REMAINDER daemon was running at deployment. No persistent daemon or autostart entry was added. Run `remainder` from the intended project directory to start capture; the first startup re-extracts legacy sources and discovers existing supported files. Queries and full-source retrieval work immediately with the installed executable.

For an explicit project database in PowerShell:

```powershell
$env:REMAINDER_DB = "$PWD\remainder.db"
remainder
```

The existing fallback database location is retained for compatibility. Explicit `REMAINDER_DB` is preferable when keeping multiple projects separate.

## Competitive scope

This release fixes demonstrated reliability defects and broadens capture. It does not add embedding-based semantic retrieval, a historical revision archive, or establish better answer quality than MemPalace. The next comparison must use the same developer documents, held-out questions, output token budget, stale-fact checks, and latency percentiles for both systems. The four-example phrase scorer is still only a smoke test.
