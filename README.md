# REMAINDER

> **The Industrial Memory Bunker for AI Agents.**  
> Zero Python. Zero virtualenvs. Sub-second retrieval. Native MCP.

[![Rust](https://img.shields.io/badge/language-Rust-orange.svg)](https://www.rust-lang.org/)
[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](https://www.gnu.org/licenses/gpl-3.0)
[![MCP](https://img.shields.io/badge/protocol-Model_Context_Protocol-purple.svg)](https://modelcontextprotocol.io/)
[![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20Linux%20%7C%20macOS-lightgrey.svg)]()

**REMAINDER** is a high-performance, local-first memory layer and proposition vault built from the ground up in Rust. It is engineered for developers and autonomous agents who need deterministic, ultra-fast long-term memory without relying on 300MB+ Python runtimes, bloated dependency trees, or fragile vector databases.

---

## Why REMAINDER?

* ⚡ **Native Ingestion**: Full passage coverage with batched MiniLM inference and prepared SQLite inserts. ONNX Runtime provides the accelerated native backend; Tract remains available as a fallback.
* 🎯 **Measured Performance**: The selected November 12 development configuration scored 20/50 versus MemPalace's 17/50, with median indexing 9.97 versus 19.77 seconds and warm search 30.69 versus 41.43 ms. See [method, provenance and limitations](PERFORMANCE_2026-11-12.md); unseen-task superiority is not established.
* 🛡️ **Zero Ghost Leakage**: 100% clean deletion tracking. When you delete or modify a source file, REMAINDER's shredder automatically purges stale chunks and indexes.
* 🔍 **Granular Hybrid Retrieval**: FTS5 passage-level BM25 search plus optional local MiniLM embeddings, combined with reciprocal rank fusion and MMR diversification.
* 🔌 **Native Stdio MCP Server**: Plugs instantly into **Claude Desktop**, **Cursor**, **Windsurf**, and any MCP-compatible client.
* 🪶 **No Python Runtime**: The Windows installer provisions the native executable, MiniLM model, and ONNX Runtime. A standalone Tract backend is also available.

---

## Feature Comparison

| Feature | MemPalace | Python / LangChain RAG | **REMAINDER** |
|:---|:---:|:---:|:---:|
| **Language** | Python | Python | **Rust (Native)** |
| **Runtime Dependency** | Python 3.10+, pip | Python, venv, torch | **Native executable; optional ONNX Runtime library** |
| **Storage Engine** | ChromaDB / LanceDB | Vector store + metadata | **Embedded SQLite + FTS5** |
| **Deletions & Synced State** | Prone to orphan vectors | Prone to drift | **100% Synchronized Shredder** |
| **MCP Protocol** | Wrapper script | Requires wrapper | **Native `remainder mcp` command** |

---

## Quickstart

### 1. Download & Install

**One-line Windows Install (PowerShell):**
```powershell
powershell -ExecutionPolicy Bypass -File install.ps1
```

The Windows x64 installer builds current source, installs MiniLM and ONNX Runtime into `%LOCALAPPDATA%\REMAINDER`, and copies license notices with those assets. It reuses a cached MiniLM model when available; otherwise model and runtime downloads are verified against pinned SHA-256 hashes. Use `-SkipModel` or `-SkipRuntime` for an offline/custom setup, or `-BinarySource <exe>` to install an already built release artifact. `remainder stats` shows the selected backend and stored embedding coverage.

Or build from source with Cargo:
```bash
cargo build --release
```

Place `remainder.exe` (or `remainder` on Linux/macOS) in your `PATH`.

### 2. Initialize Your Project (3-Step Setup)

Navigate to any project directory:
```bash
cd ~/projects/my-app

# Step 1: Initialize vault & generate MCP configuration
remainder init .

# Step 2: Ingest ("mine") project codebase into FTS5
remainder mine .

# (Optional: Deep extraction with local Ollama LLM)
remainder mine . --extract
```

### 3. Connect to Your AI Tool (Claude, Cursor, Windsurf)

`remainder init` automatically prints your customized MCP config. Simply add it to `claude_desktop_config.json` or Cursor settings:

```json
{
  "mcpServers": {
    "remainder": {
      "command": "remainder.exe",
      "args": ["mcp"],
      "env": {
        "REMAINDER_DB": "C:\\path\\to\\your\\project\\remainder.db",
        "REMAINDER_MCP_SYNC_ROOT": "C:\\path\\to\\your\\project"
      }
    }
  }
}
```

### 4. Background Ghost Watcher Daemon
Keep memory synchronized with file system changes in real time:
```bash
remainder watch .
```

The MCP server checks already indexed files before tool calls, at most once every 30 seconds. Set `REMAINDER_MCP_SYNC_ROOT` to discover new files in a folder as well. Changes update searchable source passages immediately; extracted atomic facts remain marked stale until `remainder sweep <folder> --extract` or `remainder watch <folder>` completes local Ollama extraction. `watch` is still useful for continuous synchronization between queries.

Dense search locates MiniLM in `REMAINDER_MODEL_DIR`, the REMAINDER application data folder, or the legacy Chroma cache. Without a model, search uses SQLite full-text retrieval and `stats` reports that mode. All source chunks can receive embeddings; there is no 300-chunk limit. Dense search scores serialized vectors and materializes a bounded candidate list before fusion and diversification; it remains an exact scan, so its cost grows with vault size.

`REMAINDER_EMBED_BACKEND=auto` prefers native ONNX Runtime and falls back to Tract. Set it to `ort` to require the accelerated backend, or `tract` to use the standalone backend. `REMAINDER_ORT_DYLIB_PATH` overrides the runtime path; `REMAINDER_EMBED_THREADS` controls CPU threads (default 4). Keyword search filters filler words by default while semantic search retains the full original question. Set `REMAINDER_QUERY_FOCUS=0` for unfiltered retrieval, or `1` to focus the semantic query too.

After upgrading an older vault, run `remainder reindex` to rebuild full passage and embedding coverage from its preserved source text. Saved facts, timestamps, and freshness state are preserved; the command does not require the original files to remain at their old paths.

`REMAINDER_CHUNK_MODE=character` preserves the compatible character-window index (default). The experimental `token` mode creates overlapping 224-token windows and checks the model's actual token limit. Token windows improved indexing speed but reduced accuracy in the development comparison, so they are opt-in. Switching chunk modes requires `remainder reindex`.

Rank fusion can be configured with `REMAINDER_RRF_K`, `REMAINDER_DENSE_WEIGHT`, and `REMAINDER_MMR_PENALTY`. The selected development preset uses `1`, `adaptive`, and `0`, respectively, with character windows and standard 1,000-character excerpts. Adaptive fusion uses semantic weight 2 for queries containing numbers, quoted text, code identifiers, CamelCase or acronyms, and 0.5 for other prose. Numeric weight overrides remain available. The routing direction was selected on the development questions; this does not establish generalization to unseen questions. Benchmarks must record the settings and chunk policy.

`REMAINDER_EXCERPT_CHARS` sets the displayed surrounding-evidence limit (default 1,000 characters; accepted range 64–16,000). The experimental `REMAINDER_EVIDENCE_MODE=deduplicated` omits the separate source-passage copy only when the displayed excerpt contains that complete passage after whitespace normalization. Incomplete excerpts and atomic facts retain their separate fact text. This mode remains opt-in pending accuracy evaluation.

---

## Native MCP Integration

REMAINDER speaks the [Model Context Protocol (MCP)](https://modelcontextprotocol.io/) natively over `stdio`.

### Available MCP Tools

* `query_bunker(query, limit)`: Search atomic facts and source passages with BM25 and optional MiniLM hybrid ranking.
* `get_source(path)`: Retrieve the exact, unmodified raw source text for a tracked file.
* `save_memory(fact, tag)`: Persist an atomic proposition or architectural decision directly to the bunker from within chat.

---

## CLI Reference

```
COMMANDS:
    (default)            Start the Ghost Watcher daemon in current directory
    init [PATH]          Initialize bunker in PATH, index files, and print AI/MCP configurations
    mine [PATH]          Alias for 'sweep' (ingest and index project files into bunker)
    sweep [PATH] [--extract] One-shot bulk scan and index (pass --extract for LLM fact extraction)
    watch [PATH]         Start the Ghost Watcher daemon monitoring PATH (default: .)
    query <QUERY> [LIMIT] Search atomic facts and source passages (BM25 + Tag lookup)
    source <PATH>        Retrieve preserved verbatim raw source content for PATH
    stats                Display vault diagnostics, fact freshness, and storage health
    repair               Run SQLite integrity checks and rebuild FTS5 search indices
    reindex              Rebuild passage/vector indexes from preserved source text
    mcp                  Run Model Context Protocol (MCP) server over Stdio
    mcp list             List available MCP tools as JSON
    mcp call <TOOL> ...  Execute an MCP tool directly
```

---

## Architecture

REMAINDER uses a three-tier storage engine:
1. **Passage Shredder & Chunker**: Content-aware sliding-window chunker (750 chars, 200 overlap) with paragraph and sentence boundary snapping.
2. **SQLite FTS5 + optional MiniLM**: Passage-level full-text search and dense vectors, fused with reciprocal rank fusion and diversified with MMR.
3. **Ghost Watcher and MCP refresh**: Watch mode tracks file changes continuously; MCP mode checks tracked files on tool calls and can discover new files under its configured sync root.

---

## License

GNU General Public License v3.0 (GPL-3.0). See [LICENSE](LICENSE) for details. Built by **4ndrewthepr0**.

