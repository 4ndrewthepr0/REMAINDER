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

* ⚡ **Ultra-Fast Ingestion**: Ingests and indexes massive context **~2x faster** than MemPalace (11.6s vs 21.6s cold ingest). Index 500k characters in under 2 seconds.
* 🎯 **Matched Benchmark Accuracy**: Matches MemPalace head-to-head (16/50 on frozen LongBench v2 audit) while running as a lean, compiled native binary.
* 🛡️ **Zero Ghost Leakage**: 100% clean deletion tracking. When you delete or modify a source file, REMAINDER's shredder automatically purges stale chunks and indexes.
* 🔍 **Granular Hybrid Retrieval**: FTS5 passage-level BM25 indexing paired with multi-term density scoring (`extract_passage_smart`), ensuring high-signal context without stopword drift.
* 🔌 **Native Stdio MCP Server**: Plugs instantly into **Claude Desktop**, **Cursor**, **Windsurf**, and any MCP-compatible client.
* 🪶 **Zero Python Runtime Overhead**: No virtual environments, no PyTorch, no huggingface-hub downloads, and no Python DLL conflicts. Just a single self-contained binary.

---

## Feature Comparison

| Feature | MemPalace | Python / LangChain RAG | **REMAINDER** |
|:---|:---:|:---:|:---:|
| **Language** | Python | Python | **Rust (Native)** |
| **Runtime Dependency** | Python 3.10+, pip | Python, venv, torch | **None (Single binary)** |
| **Ingestion Speed (Median)** | 21.6s | >30s | **11.7s (~2x faster)** |
| **Storage Engine** | ChromaDB / LanceDB | Vector store + metadata | **Embedded SQLite + FTS5** |
| **Deletions & Synced State** | Prone to orphan vectors | Prone to drift | **100% Synchronized Shredder** |
| **MCP Protocol** | Wrapper script | Requires wrapper | **Native `remainder mcp` command** |

---

## Quickstart

### 1. Download & Install
Download the compiled `remainder.exe` from the latest release, or build from source:
```bash
cargo build --release
```

### 2. Index Your Codebase or Project
Run a quick sweep of your project files:
```bash
# Fast passage-level indexing
remainder sweep .

# Optional: Deep extraction with local Ollama LLM
remainder sweep . --extract
```

### 3. Query Your Vault
```bash
remainder query "What is the token budget policy?" 5
```

### 4. Background Ghost Watcher Daemon
Keep memory synchronized with file system changes in real time:
```bash
remainder watch .
```

---

## Native MCP Integration

REMAINDER speaks the [Model Context Protocol (MCP)](https://modelcontextprotocol.io/) natively over `stdio`.

### Setup with Claude Desktop / Cursor / Windsurf

Add the following to your `claude_desktop_config.json` or Cursor MCP settings:

```json
{
  "mcpServers": {
    "remainder": {
      "command": "remainder.exe",
      "args": ["mcp"],
      "env": {
        "REMAINDER_DB": "C:\\path\\to\\your\\project\\remainder.db"
      }
    }
  }
}
```

### Available MCP Tools

* `query_memory(query, limit)`: Search atomic facts and relevant passages by keyword density & BM25 ranking.
* `get_source(path)`: Retrieve the exact, unmodified raw source text for a tracked file.
* `vault_stats()`: Check memory health, indexed file counts, chunk totals, and database integrity.

---

## CLI Reference

```
COMMANDS:
    watch [PATH]         Start the Ghost Watcher daemon monitoring PATH (default: .)
    query <QUERY> [LIMIT] Search atomic facts and source passages
    source <PATH>        Retrieve preserved verbatim raw source content
    stats                Display vault diagnostics and storage health
    sweep [PATH]         One-shot bulk scan and index (pass --extract for LLM fact extraction)
    repair               Run SQLite integrity checks and rebuild FTS5 search indices
    mcp                  Run Model Context Protocol (MCP) server over Stdio
    mcp list             List available MCP tools as JSON
    mcp call <TOOL> ...  Execute an MCP tool directly
```

---

## Architecture

REMAINDER uses a three-tier storage engine:
1. **Passage Shredder & Chunker**: Content-aware sliding-window chunker (750 chars, 200 overlap) with paragraph and sentence boundary snapping.
2. **SQLite FTS5 + BM25 Virtual Table**: Passage-level full text search with multi-term density scoring.
3. **Ghost Watcher**: File system monitor tracking creations, modifications, and deletions to ensure 0% deleted-file leakage.

---

## License

GNU General Public License v3.0 (GPL-3.0). See [LICENSE](LICENSE) for details. Built by **4ndrewthepr0**.

