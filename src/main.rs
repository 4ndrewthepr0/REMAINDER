mod engine;
mod shredder;
pub mod embedder;
#[cfg(test)]
mod regression_tests;

use std::path::{Path, PathBuf};
use std::env;
use std::io::{BufRead, Write};
use std::collections::{HashMap, HashSet};
use notify::{Watcher, RecursiveMode, Config, EventKind};
use tokio::sync::mpsc;
use tokio::time::{interval, Duration, Instant};
use serde_json::json;
use sha2::{Sha256, Digest};

pub fn is_supported_file(path: &Path) -> bool {
    if is_ignored_path(path) { return false; }
    path.extension().and_then(|ext| ext.to_str()).map_or(false, |ext| matches!(
        ext.to_ascii_lowercase().as_str(),
        "txt" | "rs" | "md" | "py" | "js" | "jsx" | "ts" | "tsx" | "json" |
        "toml" | "yaml" | "yml" | "go" | "java" | "c" | "h" | "cpp" | "hpp" |
        "cs" | "sql" | "sh" | "ps1" | "html" | "css"))
}

fn is_ignored_path(path: &Path) -> bool {
    path.components().any(|part| matches!(part.as_os_str().to_str(),
        Some(".git" | "target" | "node_modules" | ".venv" | "venv" | "__pycache__")))
}

fn collect_project_files(root: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(root).into_iter()
        .filter_entry(|e| !is_ignored_path(e.path()))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && is_supported_file(e.path()))
        .map(|e| e.into_path()).collect()
}

/// Reconcile existing files on startup and missed changes during operation.
/// Extraction retries exhausted in this run remain exhausted until an edit or restart.
async fn reconcile_project(root: &Path, engine: &mut engine::RemainderEngine,
    pending: &mut HashMap<PathBuf, PendingItem>, retry_stale: bool) -> anyhow::Result<()> {
    let root = root.to_path_buf();
    let files = tokio::task::spawn_blocking(move || collect_project_files(&root)).await?;
    for path in files {
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(content) => content,
            Err(error) => { eprintln!("[SCAN] {}: {}", path.display(), error); continue; }
        };
        let state = engine.source_state(&path.to_string_lossy())?;
        let changed = state.as_ref().map_or(true, |(raw, _)| raw.as_deref() != Some(content.as_str()));
        let stale = state.as_ref().map_or(true, |(_, current)| !current);
        if changed || (retry_stale && stale) {
            engine.record_source_content(&path.to_string_lossy(), &content)?;
            pending.entry(path).or_insert(PendingItem { deadline: Instant::now(), retry_count: 0 });
        }
    }
    engine.reconcile_missing_sources()?;
    Ok(())
}

pub fn get_db_path() -> PathBuf {
    if let Some(path) = env::var_os("REMAINDER_DB") { return PathBuf::from(path); }
    if Path::new("remainder.db").exists() {
        return PathBuf::from("remainder.db");
    }
    if let Ok(user_profile) = env::var("USERPROFILE") {
        let bunker_db = Path::new(&user_profile)
            .join(".gemini")
            .join("antigravity-ide")
            .join("scratch")
            .join("remainder")
            .join("remainder.db");
        if bunker_db.exists() {
            return bunker_db;
        }
    }
    PathBuf::from("remainder.db")
}

pub fn compute_file_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hex::encode(hasher.finalize())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    Modify(PathBuf),
    Remove(PathBuf),
    Rename { from: PathBuf, to: PathBuf },
}

#[derive(Debug, Clone)]
pub struct PendingItem {
    pub deadline: Instant,
    pub retry_count: u32,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum InferenceOutcome {
    Committed(usize),
    ScheduledRetry { attempt: u32, delay: Duration },
    ExhaustedRetries,
    DiscardedStaleChanged,
    DiscardedStaleDeleted,
}

impl InferenceOutcome {
    pub fn is_stale(&self) -> bool {
        matches!(self, InferenceOutcome::DiscardedStaleChanged | InferenceOutcome::DiscardedStaleDeleted)
    }
}

/// Computes deterministic exponential backoff: base_secs * 2^(attempt - 1)
pub fn compute_exponential_backoff(attempt: u32, base_secs: u64) -> Duration {
    if attempt == 0 {
        Duration::from_secs(0)
    } else {
        let shift = (attempt - 1).min(6);
        Duration::from_secs(base_secs * (1u64 << shift))
    }
}

/// Shared production watch event handler used by both daemon and integration tests
pub fn handle_watch_event(
    event: WatchEvent,
    engine: &mut engine::RemainderEngine,
    pending_files: &mut HashMap<PathBuf, PendingItem>,
    in_flight: &mut HashSet<PathBuf>,
    debounce_duration: Duration,
) -> anyhow::Result<usize> {
    let event = match event {
        WatchEvent::Modify(p) => WatchEvent::Modify(p.components().collect()),
        WatchEvent::Remove(p) => WatchEvent::Remove(p.components().collect()),
        WatchEvent::Rename { from, to } => WatchEvent::Rename {
            from: from.components().collect(), to: to.components().collect(),
        },
    };
    match event {
        WatchEvent::Remove(path) => {
            pending_files.retain(|p, _| !p.starts_with(&path));
            // Running tasks retain their slots until their result is consumed.
            let file_str = path.to_string_lossy().to_string();
            let file_name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            let deleted = engine.remove_source_or_prefix(&file_str)?;
            if deleted > 0 {
                println!("\n[DELETION] Removed '{}'. Purged {} cascade sources/facts.", file_name, deleted);
                let _ = engine.checkpoint();
            }
            let _ = engine.reconcile_missing_sources();
            Ok(deleted)
        }
        WatchEvent::Rename { from, to } => {
            let mut total_affected = 0;
            // 1. Purge old sources under 'from'
            if !from.as_os_str().is_empty() {
                let from_str = from.to_string_lossy().to_string();
                let deleted = engine.remove_source_or_prefix(&from_str)?;
                if deleted > 0 {
                    println!("\n[RENAME] Relocated source '{}' purged ({} cascade records).", from_str, deleted);
                }
                pending_files.retain(|p, _| !p.starts_with(&from));
                total_affected += deleted;
            }

            // 2. Index new relocated files under 'to'
            if to.is_dir() {
                println!("\n[RENAME] Relocated directory '{:?}' detected. Traversing files...", to.file_name().unwrap_or_default());
                for entry in walkdir::WalkDir::new(&to).into_iter().filter_map(|e| e.ok()) {
                    if entry.file_type().is_file() && is_supported_file(entry.path()) {
                        let p = entry.into_path();
                        let deadline = Instant::now() + debounce_duration;
                        pending_files.insert(p, PendingItem { deadline, retry_count: 0 });
                        total_affected += 1;
                    }
                }
            } else if is_supported_file(&to) {
                let deadline = Instant::now() + debounce_duration;
                pending_files.insert(to, PendingItem { deadline, retry_count: 0 });
                total_affected += 1;
            }

            let _ = engine.reconcile_missing_sources();
            let _ = engine.checkpoint();
            Ok(total_affected)
        }
        WatchEvent::Modify(path) => {
            let deadline = Instant::now() + debounce_duration;
            if let Some(file_name) = path.file_name() {
                if !pending_files.contains_key(&path) && !in_flight.contains(&path) {
                    println!("\n[DEBOUNCE] Activity detected on {:?}. Buffering for {:?} calm...", file_name, debounce_duration);
                }
            }
            engine.invalidate_source(&path.to_string_lossy())?;
            pending_files.insert(path, PendingItem { deadline, retry_count: 0 });
            Ok(1)
        }
    }
}

/// Shared production inference result processor used by both daemon and integration tests
pub fn process_inference_result(
    path: PathBuf,
    input_hash: String,
    res: anyhow::Result<String>,
    engine: &mut engine::RemainderEngine,
    pending_files: &mut HashMap<PathBuf, PendingItem>,
    in_flight: &mut HashSet<PathBuf>,
    base_retry_secs: u64,
    current_content: Option<String>,
) -> anyhow::Result<InferenceOutcome> {
    in_flight.remove(&path);
    let file_name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
    let file_str = path.to_string_lossy().to_string();

    let content = match current_content {
        Some(ref c) => c,
        None => {
            engine.remove_source_or_prefix(&file_str)?;
            pending_files.remove(&path);
            println!("[RACE GUARD] File '{}' disappeared during inference. Stale result discarded.", file_name);
            return Ok(InferenceOutcome::DiscardedStaleDeleted);
        }
    };

    if compute_file_hash(content) != input_hash {
        engine.record_source_content(&file_str, content)?;
        pending_files.insert(path.clone(), PendingItem {
            deadline: Instant::now() + Duration::from_secs(3), retry_count: 0,
        });
        println!("[RACE GUARD] File '{}' changed during LLM inference. Stale result discarded.", file_name);
        return Ok(InferenceOutcome::DiscardedStaleChanged);
    }

    // Persist raw source content immediately so it is indexed and searchable even if LLM extraction fails or retries
    engine.record_source_content(&file_str, content)?;

    let current_retry = pending_files.get(&path).map(|p| p.retry_count).unwrap_or(0);

    match res {
        Ok(raw_fact) => {
            // Reject partial/malformed output instead of replacing all prior facts
            // with whichever subset happened to parse.
            let facts_to_insert = parse_facts(&raw_fact).unwrap_or_default();

            if facts_to_insert.is_empty() {
                let next_retry = current_retry + 1;
                if next_retry <= 5 {
                    let delay = compute_exponential_backoff(next_retry, base_retry_secs);
                    println!("[RETRY] Model output unparsed into structured facts. Scheduling exponential backoff retry {}/5 for '{}' in {:?}", next_retry, file_name, delay);
                    pending_files.insert(path, PendingItem {
                        deadline: Instant::now() + delay,
                        retry_count: next_retry,
                    });
                    return Ok(InferenceOutcome::ScheduledRetry { attempt: next_retry, delay });
                } else {
                    println!("[FALLBACK] Max retry attempts (5/5) reached for '{}'. Engaging Raw Chunk Fallback to ensure zero data loss.", file_name);
                    let fallback_facts = extract_raw_chunks(&content, &file_name);
                    if !fallback_facts.is_empty() {
                        let count = engine.refresh_source_facts(&file_str, current_content.as_deref(), fallback_facts)?;
                        let _ = engine.checkpoint();
                        pending_files.remove(&path);
                        return Ok(InferenceOutcome::Committed(count));
                    }
                    println!("[RETRY] Max retry attempts (5/5) reached for '{}'. Existing vault state preserved.", file_name);
                    pending_files.remove(&path);
                    return Ok(InferenceOutcome::ExhaustedRetries);
                }
            }

            for (tag, data) in &facts_to_insert {
                println!("[BUNKER] Atomic Truth: [{}] -> {}", tag, data);
            }

            match engine.refresh_source_facts(&file_str, current_content.as_deref(), facts_to_insert) {
                Ok(count) => {
                    println!("[PROVENANCE] Updated {} facts for source '{}'. Raw source text preserved.", count, file_name);
                    let _ = engine.checkpoint();
                    pending_files.remove(&path);
                    Ok(InferenceOutcome::Committed(count))
                }
                Err(e) => {
                    println!("[ERROR] Transaction aborted. Database rolled back safely: {}", e);
                    let next_retry = current_retry + 1;
                    if next_retry <= 5 {
                        let delay = compute_exponential_backoff(next_retry, base_retry_secs);
                        pending_files.insert(path, PendingItem {
                            deadline: Instant::now() + delay,
                            retry_count: next_retry,
                        });
                        Ok(InferenceOutcome::ScheduledRetry { attempt: next_retry, delay })
                    } else {
                        pending_files.remove(&path);
                        Ok(InferenceOutcome::ExhaustedRetries)
                    }
                }
            }
        }
        Err(e) => {
            let next_retry = current_retry + 1;
            if next_retry <= 5 {
                let delay = compute_exponential_backoff(next_retry, base_retry_secs);
                println!("[RETRY] Shredder HTTP/model error: {}. Scheduling exponential backoff retry {}/5 for '{}' in {:?}", e, next_retry, file_name, delay);
                pending_files.insert(path, PendingItem {
                    deadline: Instant::now() + delay,
                    retry_count: next_retry,
                });
                Ok(InferenceOutcome::ScheduledRetry { attempt: next_retry, delay })
            } else {
                println!("[RETRY] Max retry attempts (5/5) reached for '{}'. Existing vault state preserved.", file_name);
                pending_files.remove(&path);
                Ok(InferenceOutcome::ExhaustedRetries)
            }
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepExtractionResult {
    pub success_count: usize,
    pub failed_count: usize,
    pub committed_facts: usize,
    pub stale_discarded_count: usize,
}

pub async fn run_sweep_extraction(
    engine: &mut engine::RemainderEngine,
    shredder: &shredder::Shredder,
    paths: Vec<PathBuf>,
    max_workers: usize,
) -> anyhow::Result<SweepExtractionResult> {
    let mut pending_files: HashMap<PathBuf, PendingItem> = HashMap::new();
    let mut in_flight: HashSet<PathBuf> = HashSet::new();
    let mut queue: std::collections::VecDeque<(PathBuf, u32)> = paths.into_iter().map(|p| (p, 0)).collect();
    let mut join_set: tokio::task::JoinSet<(PathBuf, u32, String, anyhow::Result<String>)> = tokio::task::JoinSet::new();

    let mut result = SweepExtractionResult::default();

    loop {
        // Replenish worker tasks up to max_workers
        while join_set.len() < max_workers {
            if let Some((next_path, attempt)) = queue.pop_front() {
                let file_str = next_path.to_string_lossy().to_string();
                let content = match tokio::fs::read_to_string(&next_path).await {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = process_inference_result(
                            next_path.clone(),
                            String::new(),
                            Err(anyhow::anyhow!("File unreadable: {}", e)),
                            engine,
                            &mut pending_files,
                            &mut in_flight,
                            1,
                            None,
                        );
                        result.stale_discarded_count += 1;
                        continue;
                    }
                };

                if content.trim().is_empty() {
                    let _ = engine.refresh_source_facts(&file_str, Some(""), Vec::new());
                    result.success_count += 1;
                    continue;
                }

                let input_hash = compute_file_hash(&content);
                let _ = engine.record_source_content(&file_str, &content);
                in_flight.insert(next_path.clone());

                let shredder_clone = shredder.clone();
                let path_for_task = next_path.clone();
                let hash_for_task = input_hash.clone();

                join_set.spawn(async move {
                    let shred_res = shredder_clone.extract_atomic(&content).await;
                    (path_for_task, attempt, hash_for_task, shred_res)
                });
            } else {
                break;
            }
        }

        if join_set.is_empty() {
            break;
        }

        if let Some(join_res) = join_set.join_next().await {
            let (path, _attempt, input_hash, shred_res) = match join_res {
                Ok(res) => res,
                Err(join_err) => {
                    eprintln!("[SWEEP ERROR] Worker task panicked: {}", join_err);
                    result.failed_count += 1;
                    continue;
                }
            };

            let file_name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            let current_content = tokio::fs::read_to_string(&path).await.ok();

            match process_inference_result(
                path.clone(),
                input_hash,
                shred_res,
                engine,
                &mut pending_files,
                &mut in_flight,
                1,
                current_content.clone(),
            ) {
                Ok(outcome) => match outcome {
                    InferenceOutcome::Committed(count) => {
                        println!("[SWEEP COMMIT] Committed {} fact(s) for '{}'.", count, file_name);
                        result.success_count += 1;
                        result.committed_facts += count;
                    }
                    InferenceOutcome::DiscardedStaleChanged => {
                        println!("[SWEEP RACE GUARD] File '{}' changed during extraction. Outdated result discarded. Processing latest version...", file_name);
                        result.stale_discarded_count += 1;
                        queue.push_back((path, 0));
                    }
                    InferenceOutcome::DiscardedStaleDeleted => {
                        println!("[SWEEP RACE GUARD] File '{}' disappeared during extraction. Stale result discarded and purged from bunker.", file_name);
                        result.stale_discarded_count += 1;
                    }
                    InferenceOutcome::ScheduledRetry { attempt: next_attempt, delay } => {
                        if next_attempt <= 2 {
                            println!("[SWEEP RETRY] Scheduling retry {}/2 for '{}' in {:?}", next_attempt, file_name, delay);
                            tokio::time::sleep(delay).await;
                            queue.push_back((path, next_attempt));
                        } else {
                            if let Some(ref text) = current_content {
                                println!("[SWEEP FALLBACK] Retry limit reached for '{}'. Engaging Raw Chunk Fallback.", file_name);
                                let fallback = extract_raw_chunks(text, &file_name);
                                if let Ok(c) = engine.refresh_source_facts(&path.to_string_lossy(), Some(text), fallback) {
                                    println!("[SWEEP COMMIT] Committed {} fallback fact(s) for '{}'.", c, file_name);
                                    result.success_count += 1;
                                    result.committed_facts += c;
                                    continue;
                                }
                            }
                            eprintln!("[SWEEP ERROR] Exceeded max retries (2/2) for '{}'.", file_name);
                            result.failed_count += 1;
                        }
                    }
                    InferenceOutcome::ExhaustedRetries => {
                        if let Some(ref text) = current_content {
                            println!("[SWEEP FALLBACK] Engaging Raw Chunk Fallback for '{}' to prevent data loss.", file_name);
                            let fallback = extract_raw_chunks(text, &file_name);
                            if let Ok(c) = engine.refresh_source_facts(&path.to_string_lossy(), Some(text), fallback) {
                                println!("[SWEEP COMMIT] Committed {} fallback fact(s) for '{}'.", c, file_name);
                                result.success_count += 1;
                                result.committed_facts += c;
                                continue;
                            }
                        }
                        eprintln!("[SWEEP ERROR] Exceeded max retries for '{}'.", file_name);
                        result.failed_count += 1;
                    }
                },
                Err(e) => {
                    eprintln!("[SWEEP ERROR] Error processing inference for '{}': {}", file_name, e);
                    result.failed_count += 1;
                }
            }
        }
    }

    let _ = engine.checkpoint();
    Ok(result)
}

pub fn extract_raw_chunks(content: &str, file_name: &str) -> Vec<(String, String)> {
    let mut chunks = Vec::new();
    let default_tag = match Path::new(file_name).file_stem().and_then(|s| s.to_str()) {
        Some(stem) => stem.to_uppercase(),
        None => "CHUNK".to_string(),
    };

    let paragraphs: Vec<&str> = content.split("\n\n").map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
    for p in paragraphs {
        let (tag, text) = if p.starts_with('#') {
            let heading_line = p.lines().next().unwrap_or(p);
            let heading_text = heading_line.trim_start_matches('#').trim();
            let body = p.strip_prefix(heading_line).unwrap_or(p).trim();
            let tag = if !heading_text.is_empty() {
                let first_word = heading_text.split_whitespace().next().unwrap_or(&default_tag);
                first_word.to_uppercase()
            } else {
                default_tag.clone()
            };
            let full_text = if !body.is_empty() {
                format!("{}. {}", heading_text, body)
            } else {
                heading_text.to_string()
            };
            (tag, full_text)
        } else {
            (default_tag.clone(), p.to_string())
        };

        let sentences: Vec<&str> = text.split(". ")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        if sentences.len() > 1 {
            for s in sentences {
                let clean_s = if s.ends_with('.') { s.to_string() } else { format!("{}.", s) };
                if clean_s.len() >= 15 {
                    chunks.push((tag.clone(), clean_s));
                }
            }
        } else if text.len() >= 15 {
            let clean_p = if text.ends_with('.') { text } else { format!("{}.", text) };
            chunks.push((tag, clean_p));
        }
    }

    if chunks.is_empty() && !content.trim().is_empty() {
        chunks.push((default_tag, content.trim().to_string()));
    }

    chunks
}

pub fn parse_facts(output: &str) -> anyhow::Result<Vec<(String, String)>> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    // 1. Try parsing JSON if formatted as JSON array
    let json_candidate = if let Some(stripped) = trimmed.strip_prefix("```json").or_else(|| trimmed.strip_prefix("```")) {
        stripped.trim().strip_suffix("```").unwrap_or(stripped).trim()
    } else {
        trimmed
    };

    if json_candidate.starts_with('[') && json_candidate.ends_with(']') {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_candidate) {
            if let Some(arr) = val.as_array() {
                let mut json_facts = Vec::new();
                let mut all_valid = true;
                for item in arr {
                    let tag = item.get("tag")
                        .or_else(|| item.get("category"))
                        .and_then(|v| v.as_str());
                    let fact = item.get("fact")
                        .or_else(|| item.get("content"))
                        .or_else(|| item.get("decision"))
                        .and_then(|v| v.as_str());
                    if let (Some(t), Some(f)) = (tag, fact) {
                        let t = t.trim().trim_start_matches('[').trim_end_matches(']');
                        let f = f.trim();
                        if !t.is_empty() && !f.is_empty() {
                            json_facts.push((t.to_string(), f.to_string()));
                            continue;
                        }
                    }
                    all_valid = false;
                    break;
                }
                if all_valid && !json_facts.is_empty() {
                    return Ok(json_facts);
                }
            }
        }
    }

    // 2. Line-by-line proposition parsing
    let mut facts = Vec::new();
    for raw_line in output.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("```") {
            continue;
        }

        let mut clean = line;
        if clean.starts_with("- ") || clean.starts_with("* ") || clean.starts_with("+ ") {
            clean = &clean[2..].trim();
        } else if let Some(idx) = clean.find(". ") {
            let prefix = &clean[..idx];
            if prefix.chars().all(|c| c.is_ascii_digit()) {
                clean = &clean[idx + 2..].trim();
            }
        } else if let Some(idx) = clean.find(") ") {
            let prefix = &clean[..idx];
            if prefix.chars().all(|c| c.is_ascii_digit()) {
                clean = &clean[idx + 2..].trim();
            }
        }

        let (tag_part, content_part) = if let Some((t, c)) = clean.split_once("->") {
            (t, c)
        } else if let Some((t, c)) = clean.split_once("=>") {
            (t, c)
        } else if let Some((t, c)) = clean.split_once(':') {
            (t, c)
        } else {
            anyhow::bail!("Missing fact delimiter in line: '{}'", line);
        };

        let tag = tag_part.trim().strip_prefix('[').and_then(|tag| tag.strip_suffix(']'))
            .filter(|tag| !tag.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("Invalid category tag in line: '{}'", line))?;
        anyhow::ensure!(!content_part.trim().is_empty(), "Empty fact in line: '{}'", line);

        facts.push((tag.trim().to_string(), content_part.trim().to_string()));
    }

    Ok(facts)
}

pub fn process_mcp_message(line: &str, engine: &engine::RemainderEngine) -> Option<serde_json::Value> {
    if line.trim().is_empty() {
        return None;
    }

    let req: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return None,
    };

    let id = req.get("id");
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

    match method {
        "initialize" => {
            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": "remainder-bunker",
                        "version": "0.4.0"
                    }
                }
            }))
        }
        "notifications/initialized" => None,
        "ping" => Some(json!({ "jsonrpc": "2.0", "id": id, "result": {} })),
        "tools/list" => {
            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
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
            }))
        }
        "tools/call" => {
            let params = req.get("params");
            let name = params.and_then(|p| p.get("name")).and_then(|n| n.as_str()).unwrap_or("");
            let query = params
                .and_then(|p| p.get("arguments"))
                .and_then(|a| a.get("query").or_else(|| a.get("tag")))
                .and_then(|t| t.as_str())
                .unwrap_or("");

            let text = if name == "get_source" {
                let path = params
                    .and_then(|p| p.get("arguments"))
                    .and_then(|a| a.get("path"))
                    .and_then(|p| p.as_str())
                    .unwrap_or("");
                match engine.get_source_content(path) {
                    Ok(Some(content)) => content,
                    Ok(None) => format!("[BUNKER] Source not found or has no recorded content: {}", path),
                    Err(e) => format!("Error retrieving source: {}", e),
                }
            } else if name == "query_bunker" {
                let limit = params
                    .and_then(|p| p.get("arguments"))
                    .and_then(|a| a.get("limit"))
                    .and_then(|l| l.as_u64())
                    .map(|l| l as usize);
                match engine.search(query, limit) {
                    Ok(hits) if !hits.is_empty() => {
                        hits.iter()
                            .map(|h| {
                                let mut s = format!("[BUNKER] [{}] -> {}", h.vault_tag, h.fact_content);
                                s.push_str(&format!("\n  Source: {}", h.file_path));
                                if let Some(ref exc) = h.raw_excerpt {
                                    let single_line = exc.lines().collect::<Vec<_>>().join(" ");
                                    let preview: String = single_line.chars().take(1000).collect();
                                    s.push_str(&format!("\n  Context: \"{}\"", preview));
                                }
                                s
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n")
                    }
                    Ok(_) => format!("[BUNKER] No records found for query: {}", query),
                    Err(e) => format!("Error querying bunker: {}", e),
                }
            } else {
                format!("Unknown tool: {}", name)
            };

            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": text }]
                }
            }))
        }
        _ => {
            if id.is_some() {
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "Method not found" }
                }))
            } else {
                None
            }
        }
    }
}

fn run_mcp_stdio_server(engine: &engine::RemainderEngine) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let reader = stdin.lock();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if let Some(resp) = process_mcp_message(&line, engine) {
            writeln!(stdout, "{}", serde_json::to_string(&resp)?)?;
            stdout.flush()?;
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let t_app_start = std::time::Instant::now();
    let args: Vec<String> = env::args().collect();
    let db_path = get_db_path();
    let db_path_str = db_path.to_string_lossy().to_string();
    let mut engine = engine::RemainderEngine::new(&db_path_str)?;
    let db_init_duration = t_app_start.elapsed();

    // --- CLI COMMAND DISPATCHER ---
    if args.len() > 1 {
        match args[1].as_str() {
            "-h" | "--help" | "help" => {
                println!(
"REMAINDER: Local-first Autonomous Knowledge Bunker & Proposition Vault
Version {}

USAGE:
    remainder [COMMAND] [OPTIONS]

COMMANDS:
    (default)            Start the Ghost Watcher daemon in current directory
    watch [PATH]         Start the Ghost Watcher daemon monitoring PATH (default: .)
    query <QUERY> [LIMIT] Search atomic facts and source passages (BM25 + Tag lookup)
    source <PATH>        Retrieve preserved verbatim raw source content for PATH
    stats                Display vault diagnostics, fact freshness, and storage health
    sweep [PATH] [--extract] One-shot bulk scan and index (pass --extract for LLM fact extraction)
    repair               Run SQLite integrity checks and rebuild FTS5 search indices
    mcp                  Run Model Context Protocol (MCP) server over Stdio
    mcp list             List available MCP tools as JSON
    mcp call <TOOL> ...  Execute an MCP tool directly

FLAGS:
    -h, --help           Print help information
    -V, --version        Print version information

ENVIRONMENT VARIABLES:
    REMAINDER_DB         Path to SQLite vault database (default: remainder.db)
    REMAINDER_MODEL      Ollama model name for proposition extraction (default: llama3.1:8b)
    REMAINDER_OLLAMA_URL Ollama HTTP API endpoint (default: http://localhost:11434)",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(());
            }
            "-V" | "--version" | "version" => {
                println!("remainder {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "stats" | "status" => {
                let stats = engine.get_stats()?;
                let db_size_bytes = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
                let db_size_kb = db_size_bytes as f64 / 1024.0;
                println!("============================================================");
                println!("REMAINDER BUNKER DIAGNOSTICS & VAULT STATUS");
                println!("============================================================");
                println!("  Database File      : {}", db_path.display());
                println!("  Database Size      : {:.1} KB ({} bytes)", db_size_kb, db_size_bytes);
                println!("  Schema Version     : v{}", stats.schema_version);
                println!("  B-tree Integrity   : {}", if stats.integrity_ok { "PASS (ok)" } else { "FAIL" });
                println!("  Tracked Sources    : {} total ({} current fresh)", stats.total_sources, stats.current_sources);
                println!("  Atomic Facts       : {} total ({} active fresh)", stats.total_facts, stats.current_facts);
                if !stats.top_tags.is_empty() {
                    println!("\n  Top Category Tags  :");
                    for (tag, count) in &stats.top_tags {
                        println!("    [{}] : {} facts", tag, count);
                    }
                }
                println!("============================================================");
                return Ok(());
            }
            "repair" | "check" => {
                println!("[REPAIR] Initiating comprehensive bunker integrity audit & index repair...");
                let report = engine.repair_bunker()?;
                for line in &report.messages {
                    println!("  - {}", line);
                }
                if !report.integrity_ok || report.fk_violations_remaining > 0 {
                    eprintln!("[REPAIR] Audit failed: unresolved integrity violations present.");
                    std::process::exit(1);
                } else if !report.is_pristine() {
                    println!("[REPAIR] Repair completed successfully: corrected {} foreign-key violation(s) and {} orphaned file(s). Bunker storage is pristine.", report.fk_violations_repaired, report.purged_sources);
                    return Ok(());
                } else {
                    println!("[REPAIR] All checks passed. Bunker storage is pristine.");
                    return Ok(());
                }
            }
            "sweep" => {
                let mut extract_mode = false;
                let mut target_path: Option<PathBuf> = None;
                for arg in args.iter().skip(2) {
                    if arg == "--extract" {
                        extract_mode = true;
                    } else if target_path.is_none() {
                        target_path = Some(PathBuf::from(arg));
                    }
                }
                let raw_target = target_path.unwrap_or_else(|| PathBuf::from("."));
                if !raw_target.exists() {
                    eprintln!("error: sweep target path '{}' does not exist.", raw_target.display());
                    std::process::exit(1);
                }
                if !raw_target.is_dir() {
                    eprintln!("error: sweep target path '{}' is not a directory.", raw_target.display());
                    std::process::exit(1);
                }
                let target = if raw_target.is_relative() {
                    if raw_target == Path::new(".") {
                        std::env::current_dir()?
                    } else {
                        std::env::current_dir()?.join(&raw_target)
                    }
                } else {
                    raw_target
                };

                println!("[SWEEP] Scanning '{}' for project files...", target.display());
                let mut pending = HashMap::new();
                reconcile_project(&target, &mut engine, &mut pending, extract_mode).await?;
                let _ = engine.checkpoint();

                let pending_count = pending.len();
                if extract_mode && pending_count > 0 {
                    println!("[SWEEP] Running one-shot proposition extraction for {} pending file(s)...", pending_count);
                    let model = env::var("REMAINDER_MODEL").unwrap_or_else(|_| "llama3.1:8b".into());
                    let url = env::var("REMAINDER_OLLAMA_URL").unwrap_or_else(|_| "http://localhost:11434".into());
                    let shredder = shredder::Shredder::with_base_url(&model, &url);

                    let result = run_sweep_extraction(&mut engine, &shredder, pending.into_keys().collect(), 2).await?;
                    if result.failed_count > 0 {
                        eprintln!("[SWEEP] FAILED: {} file(s) failed proposition extraction ({} succeeded, {} fact(s) committed).", result.failed_count, result.success_count, result.committed_facts);
                        std::process::exit(1);
                    } else {
                        println!("[SWEEP] Extraction complete. Successfully committed {} fact(s) across {} file(s).", result.committed_facts, result.success_count);
                    }
                } else if pending_count > 0 {
                    println!("[SWEEP] Reconciled and indexed source text into full-text search. {} file(s) pending proposition extraction (start 'remainder watch' daemon or run 'remainder sweep --extract' to complete LLM extraction).", pending_count);
                } else {
                    println!("[SWEEP] Sweep complete. All project files reconciled and up to date.");
                }
                return Ok(());
            }
            "query" => {
                if args.len() < 3 {
                    eprintln!("error: 'remainder query' requires a search query argument.\nUsage: remainder query <query> [limit]");
                    std::process::exit(1);
                }
                let query = &args[2];
                let limit = args.get(3).and_then(|s| s.parse::<usize>().ok()).or(Some(20));
                let hits = engine.search(query, limit)?;
                if hits.is_empty() {
                    println!("[BUNKER] No records found for query: {}", query);
                } else {
                    for hit in hits {
                        println!("[BUNKER] [{}] -> {}", hit.vault_tag, hit.fact_content);
                        println!("         Source: {}", hit.file_path);
                        if let Some(ref exc) = hit.raw_excerpt {
                            let single_line = exc.lines().collect::<Vec<_>>().join(" ");
                            let preview: String = single_line.chars().take(1000).collect();
                            println!("         Context: \"{}\"", preview);
                        }
                    }
                }
                return Ok(());
            }
            "source" => {
                if args.len() < 3 {
                    eprintln!("error: 'remainder source' requires a file path argument.\nUsage: remainder source <file-path>");
                    std::process::exit(1);
                }
                let path = &args[2];
                match engine.get_source_content(path)? {
                    Some(content) => println!("{}", content),
                    None => println!("[BUNKER] Source not found or has no recorded content: {}", path),
                }
                return Ok(());
            }
            "mcp" => {
                if args.len() > 2 && args[2] == "list" {
                    println!("{}", json!({
                        "tools": [
                            {
                                "name": "query_bunker",
                                "description": "Retrieves technical facts and matching source passages from the local REMAINDER vault using natural-language search (BM25) or category tag lookup, with source file provenance and context excerpts.",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "query": { "type": "string", "description": "Natural-language query or category tag" },
                                        "tag": { "type": "string", "description": "Optional category tag filter" },
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
                    }));
                    return Ok(());
                } else if args.len() > 3 && args[2] == "call" {
                    if args[3] == "source" || args[3] == "get_source" {
                        let path = args.get(4).map(|s| s.as_str()).unwrap_or("");
                        match engine.get_source_content(path)? {
                            Some(content) => println!("{}", json!({ "content": [{ "type": "text", "text": content }] })),
                            None => println!("{}", json!({ "content": [{ "type": "text", "text": format!("Source not found: {}", path) }] })),
                        }
                        return Ok(());
                    }

                    let query = &args[3];
                    let limit = args.get(4).and_then(|s| s.parse::<usize>().ok()).or(Some(20));
                    let hits = engine.search(query, limit)?;
                    if !hits.is_empty() {
                        let combined = hits.iter()
                            .map(|h| {
                                let mut s = format!("[{}] -> {}\n  Source: {}", h.vault_tag, h.fact_content, h.file_path);
                                if let Some(ref exc) = h.raw_excerpt {
                                    let single_line = exc.lines().collect::<Vec<_>>().join(" ");
                                    let preview: String = single_line.chars().take(1000).collect();
                                    s.push_str(&format!("\n  Context: \"{}\"", preview));
                                }
                                s
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        println!("{}", json!({ "content": [{ "type": "text", "text": combined }] }));
                    } else {
                        println!("{}", json!({ "content": [{ "type": "text", "text": "No records found." }] }));
                    }
                    return Ok(());
                } else {
                    return run_mcp_stdio_server(&engine);
                }
            }
            "watch" => {
                // Explicit subcommand to start ghost watcher daemon; continues to watcher setup
            }
            unknown => {
                eprintln!("error: unrecognized subcommand '{}'.\nRun 'remainder --help' for usage.", unknown);
                std::process::exit(1);
            }
        }
    }

    // --- GHOST WATCHER MODE ---
    let raw_watch_path = if args.len() > 1 && args[1] == "watch" {
        args.get(2).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
    } else {
        PathBuf::from(".")
    };

    if !raw_watch_path.exists() {
        eprintln!("error: watch target directory '{}' does not exist.", raw_watch_path.display());
        std::process::exit(1);
    }
    if !raw_watch_path.is_dir() {
        eprintln!("error: watch target path '{}' is not a directory.", raw_watch_path.display());
        std::process::exit(1);
    }

    let watch_path = if raw_watch_path.is_relative() {
        if raw_watch_path == Path::new(".") {
            std::env::current_dir()?
        } else {
            std::env::current_dir()?.join(&raw_watch_path)
        }
    } else {
        raw_watch_path
    };

    let model = env::var("REMAINDER_MODEL").unwrap_or_else(|_| "llama3.1:8b".into());
    let url = env::var("REMAINDER_OLLAMA_URL").unwrap_or_else(|_| "http://localhost:11434".into());
    let shredder = shredder::Shredder::with_base_url(&model, &url);

    let t_watch_start = std::time::Instant::now();
    let (tx, mut rx) = mpsc::channel::<WatchEvent>(200);
    let (std_tx, std_rx) = std::sync::mpsc::channel();
    let mut watcher = notify::RecommendedWatcher::new(std_tx, Config::default())?;
    watcher.watch(&watch_path, RecursiveMode::Recursive)?;
    let watch_init_duration = t_watch_start.elapsed();
    let total_init_duration = t_app_start.elapsed();

    println!("============================================================");
    println!("[STARTUP BENCHMARK]");
    println!("  - In-process SQLite & Schema Init : {:?}", db_init_duration);
    println!("  - Filesystem Watcher Registration : {:?}", watch_init_duration);
    println!("  - Total In-Process Ready Time     : {:?}", total_init_duration);
    println!("============================================================");
    println!("Bunker is watching '{}' (debounced + cascade deletion + async shredder). Ray Kroc is in the building.", watch_path.display());

    tokio::task::spawn_blocking(move || {
        for res in std_rx {
            if let Ok(event) = res {
                if event.paths.iter().all(|p| is_ignored_path(p)) {
                    continue;
                }

                match event.kind {
                    EventKind::Remove(_) => {
                        for path in event.paths {
                            let _ = tx.blocking_send(WatchEvent::Remove(path));
                        }
                    }
                    EventKind::Modify(notify::event::ModifyKind::Name(rename_mode)) => {
                        use notify::event::RenameMode;
                        match rename_mode {
                            RenameMode::Both => {
                                if event.paths.len() >= 2 {
                                    let _ = tx.blocking_send(WatchEvent::Rename {
                                        from: event.paths[0].clone(),
                                        to: event.paths[1].clone(),
                                    });
                                }
                            }
                            RenameMode::From => {
                                for path in event.paths {
                                    let _ = tx.blocking_send(WatchEvent::Remove(path));
                                }
                            }
                            RenameMode::To => {
                                for path in event.paths {
                                    if path.is_dir() {
                                        let _ = tx.blocking_send(WatchEvent::Rename {
                                            from: PathBuf::new(),
                                            to: path,
                                        });
                                    } else if is_supported_file(&path) {
                                        let _ = tx.blocking_send(WatchEvent::Modify(path));
                                    }
                                }
                            }
                            _ => {
                                for path in event.paths {
                                    if path.exists() {
                                        if path.is_dir() {
                                            let _ = tx.blocking_send(WatchEvent::Rename {
                                                from: PathBuf::new(),
                                                to: path,
                                            });
                                        } else if is_supported_file(&path) {
                                            let _ = tx.blocking_send(WatchEvent::Modify(path));
                                        }
                                    } else {
                                        let _ = tx.blocking_send(WatchEvent::Remove(path));
                                    }
                                }
                            }
                        }
                    }
                    EventKind::Modify(_) | EventKind::Create(_) => {
                        for path in event.paths {
                            if path.is_dir() {
                                for entry in walkdir::WalkDir::new(&path).into_iter().filter_map(|e| e.ok()) {
                                    if entry.file_type().is_file() && is_supported_file(entry.path()) {
                                        let _ = tx.blocking_send(WatchEvent::Modify(entry.into_path()));
                                    }
                                }
                            } else if is_supported_file(&path) {
                                let _ = tx.blocking_send(WatchEvent::Modify(path));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    });

    let mut pending_files: HashMap<PathBuf, PendingItem> = HashMap::new();
    let mut in_flight: HashSet<PathBuf> = HashSet::new();
    let (result_tx, mut result_rx) = mpsc::channel::<(PathBuf, String, anyhow::Result<String>)>(50);
    let mut ticker = interval(Duration::from_millis(500));
    let mut tick_counter: u64 = 0;
    let project_root = watch_path.clone();
    let workers = std::sync::Arc::new(tokio::sync::Semaphore::new(2));
    reconcile_project(&project_root, &mut engine, &mut pending_files, true).await?;

    loop {
        tokio::select! {
            Some(event) = rx.recv() => {
                if let Err(error) = handle_watch_event(event, &mut engine, &mut pending_files, &mut in_flight, Duration::from_secs(3)) {
                    eprintln!("[WATCH] {}", error);
                }
            }

            Some((path, input_hash, res)) = result_rx.recv() => {
                let current_content = tokio::fs::read_to_string(&path).await.ok();
                let retry_path = path.clone();
                if let Err(error) = process_inference_result(path, input_hash, res, &mut engine, &mut pending_files, &mut in_flight, 1, current_content) {
                    eprintln!("[INGEST] {}", error);
                    pending_files.insert(retry_path, PendingItem { deadline: Instant::now() + Duration::from_secs(5), retry_count: 0 });
                }
            }

            _ = ticker.tick() => {
                tick_counter += 1;
                if tick_counter % 10 == 0 {
                    let _ = engine.reconcile_missing_sources();
                }
                if tick_counter % 60 == 0 {
                    if let Err(error) = reconcile_project(&project_root, &mut engine, &mut pending_files, false).await {
                        eprintln!("[SCAN] {}", error);
                    }
                }

                let now = Instant::now();
                let ready_paths: Vec<PathBuf> = pending_files
                    .iter()
                    .filter(|(path, item)| item.deadline <= now && !in_flight.contains(*path))
                    .map(|(path, _)| path.clone())
                    .collect();

                for path in ready_paths {
                    let permit = match workers.clone().try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => break,
                    };
                    if !path.exists() {
                        pending_files.remove(&path);
                        let file_str = path.to_string_lossy().to_string();
                        let _ = engine.remove_source_or_prefix(&file_str);
                        continue;
                    }

                    let content = match tokio::fs::read_to_string(&path).await {
                        Ok(c) => c,
                        Err(_) => continue,
                    };

                    let file_name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                    let file_str = path.to_string_lossy().to_string();

                    if content.trim().is_empty() {
                        pending_files.remove(&path);
                        match engine.refresh_source_facts(&file_str, Some(""), Vec::new()) {
                            Ok(_) => {
                                println!("\n[GHOST] Source file '{}' emptied. All associated facts cleared.", file_name);
                                let _ = engine.checkpoint();
                            }
                            Err(e) => println!("Error clearing emptied file facts: {}", e),
                        }
                        continue;
                    }

                    let input_hash = compute_file_hash(&content);

                    // Immediately persist raw source content so it is indexed and searchable in sources_fts
                    // even before LLM inference completes or if inference encounters an error.
                    if let Err(error) = engine.record_source_content(&file_str, &content) {
                        eprintln!("[STORE] {}: {}", file_str, error);
                        continue;
                    }
                    in_flight.insert(path.clone());

                    println!("\n[GHOST] Ripe file ready: {}. Dispatching Llama 3.1 background inference...", file_name);
                    let shredder_clone = shredder.clone();
                    let tx_clone = result_tx.clone();
                    let p_clone = path.clone();
                    let c_clone = content.clone();
                    let h_clone = input_hash.clone();

                    tokio::spawn(async move {
                        let _permit = permit;
                        let shred_start = std::time::Instant::now();
                        let res = shredder_clone.extract_atomic(&c_clone).await;
                        if res.is_ok() {
                            println!("[INFERENCE] Shredded '{}' in: {:?}", p_clone.file_name().unwrap_or_default().to_string_lossy(), shred_start.elapsed());
                        }
                        let _ = tx_clone.send((p_clone, h_clone, res)).await;
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn test_exponential_backoff_timing_and_schedule() {
        assert_eq!(compute_exponential_backoff(0, 1), Duration::from_secs(0));
        assert_eq!(compute_exponential_backoff(1, 1), Duration::from_secs(1));  // 2^0 = 1s
        assert_eq!(compute_exponential_backoff(2, 1), Duration::from_secs(2));  // 2^1 = 2s
        assert_eq!(compute_exponential_backoff(3, 1), Duration::from_secs(4));  // 2^2 = 4s
        assert_eq!(compute_exponential_backoff(4, 1), Duration::from_secs(8));  // 2^3 = 8s
        assert_eq!(compute_exponential_backoff(5, 1), Duration::from_secs(16)); // 2^4 = 16s
        // Test base multiplier
        assert_eq!(compute_exponential_backoff(3, 2), Duration::from_secs(8));  // 2 * 2^2 = 8s
    }

    async fn spawn_mock_http_stub(req_count: Arc<AtomicU32>) -> anyhow::Result<std::net::SocketAddr> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;

        tokio::spawn(async move {
            loop {
                if let Ok((mut socket, _)) = listener.accept().await {
                    let count = req_count.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 4096];
                        let mut n = 0;
                        while n < buf.len() {
                            let bytes_read = match socket.read(&mut buf[n..]).await {
                                Ok(0) => break,
                                Ok(b) => b,
                                Err(_) => break,
                            };
                            n += bytes_read;
                            if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }

                        let attempt = count.fetch_add(1, Ordering::SeqCst) + 1;

                        let (status, body) = if attempt == 1 {
                            ("500 Internal Server Error", "{\"error\":\"transient 500\"}")
                        } else if attempt == 2 {
                            ("503 Service Unavailable", "{\"error\":\"transient 503\"}")
                        } else {
                            ("200 OK", "{\"response\": \"[PROD_HTTP_RETRY] -> Successfully recovered through production HTTP pipeline.\"}")
                        };

                        let response = format!(
                            "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            status, body.len(), body
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                        let _ = socket.shutdown().await;
                    });
                }
            }
        });

        Ok(addr)
    }

    #[tokio::test]
    async fn test_production_worker_http_stub_retry_pipeline() -> anyhow::Result<()> {
        let req_count = Arc::new(AtomicU32::new(0));
        let mock_addr = spawn_mock_http_stub(req_count.clone()).await?;

        // Instantiate Shredder pointed at the real TCP HTTP stub
        let shredder = shredder::Shredder::with_base_url("mock-llama", &format!("http://{}", mock_addr));
        let mut engine = engine::RemainderEngine::new(":memory:")?;

        let temp_dir = std::env::temp_dir().join(format!("remainder_http_stub_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir)?;
        let test_file = temp_dir.join("test_reco.txt");
        let content = "Simulated document content for retry verification.";
        std::fs::write(&test_file, content)?;
        let input_hash = compute_file_hash(content);

        // Initialize production worker data structures
        let mut pending_files: HashMap<PathBuf, PendingItem> = HashMap::new();
        let mut in_flight: HashSet<PathBuf> = HashSet::new();

        // Attempt 1: Call Shredder, receives HTTP 500, processes result with production process_inference_result
        let res1 = shredder.extract_atomic(content).await;
        assert!(res1.is_err(), "Attempt 1 must fail with HTTP 500");
        let outcome1 = process_inference_result(
            test_file.clone(),
            input_hash.clone(),
            res1,
            &mut engine,
            &mut pending_files,
            &mut in_flight,
            1, // 1s base backoff
            Some(content.to_string()),
        )?;
        assert_eq!(outcome1, InferenceOutcome::ScheduledRetry { attempt: 1, delay: Duration::from_secs(1) });
        assert_eq!(pending_files.get(&test_file).unwrap().retry_count, 1);

        // Attempt 2: Call Shredder, receives HTTP 503, processes result with production process_inference_result
        let res2 = shredder.extract_atomic(content).await;
        assert!(res2.is_err(), "Attempt 2 must fail with HTTP 503");
        let outcome2 = process_inference_result(
            test_file.clone(),
            input_hash.clone(),
            res2,
            &mut engine,
            &mut pending_files,
            &mut in_flight,
            1,
            Some(content.to_string()),
        )?;
        assert_eq!(outcome2, InferenceOutcome::ScheduledRetry { attempt: 2, delay: Duration::from_secs(2) });
        assert_eq!(pending_files.get(&test_file).unwrap().retry_count, 2);

        // Attempt 3: Call Shredder, receives HTTP 200, processes result with production process_inference_result
        let res3 = shredder.extract_atomic(content).await;
        assert!(res3.is_ok(), "Attempt 3 must succeed with HTTP 200");
        let outcome3 = process_inference_result(
            test_file.clone(),
            input_hash.clone(),
            res3,
            &mut engine,
            &mut pending_files,
            &mut in_flight,
            1,
            Some(content.to_string()),
        )?;
        assert_eq!(outcome3, InferenceOutcome::Committed(1));
        assert!(!pending_files.contains_key(&test_file), "Pending item must be cleared on successful commit");

        // Verify request counts and database persistence
        assert_eq!(req_count.load(Ordering::SeqCst), 3, "HTTP stub must have received exactly 3 requests (500, 503, 200)");
        let facts = engine.get_facts_by_tag("PROD_HTTP_RETRY", None)?;
        assert_eq!(facts.len(), 1);
        assert!(facts[0].contains("Successfully recovered through production HTTP pipeline."));

        let _ = std::fs::remove_dir_all(&temp_dir);
        Ok(())
    }

    #[test]
    fn test_production_directory_rename_handler_executes_rename_event() -> anyhow::Result<()> {
        let base_temp = std::env::temp_dir().join(format!("remainder_e2e_rename_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base_temp);
        std::fs::create_dir_all(&base_temp)?;

        let source_dir = base_temp.join("services_spec");
        let dest_dir = base_temp.join("deployed_services");
        std::fs::create_dir_all(source_dir.join("sub"))?;

        let source_file = source_dir.join("sub").join("config.txt");
        std::fs::write(&source_file, "gRPC chosen over HTTP/2")?;

        let mut engine = engine::RemainderEngine::new(":memory:")?;
        let mut pending_files = HashMap::new();
        let mut in_flight = HashSet::new();

        // 1. Ingest under original path
        let source_str = source_file.to_string_lossy().to_string();
        engine.refresh_source_facts(&source_str, Some("gRPC chosen over HTTP/2"), vec![("COMM".to_string(), "gRPC chosen over HTTP/2".to_string())])?;
        assert_eq!(engine.get_facts_by_tag("COMM", None)?.len(), 1);

        // 2. Perform actual filesystem rename
        std::fs::rename(&source_dir, &dest_dir)?;

        // 3. Dispatch directly through the production handle_watch_event function
        let affected = handle_watch_event(
            WatchEvent::Rename { from: source_dir, to: dest_dir.clone() },
            &mut engine,
            &mut pending_files,
            &mut in_flight,
            Duration::from_secs(3),
        )?;

        // Must purge old records and schedule new files
        assert!(affected >= 1, "Must process relocated sources");

        // Verify old sources are completely purged from database
        assert_eq!(engine.get_facts_by_tag("COMM", None)?.len(), 0, "Facts must be purged when old directory is relocated");

        // Verify the relocated file was scheduled in pending_files by the production handler
        let expected_relocated_file = dest_dir.join("sub").join("config.txt");
        assert!(pending_files.contains_key(&expected_relocated_file), "Production rename handler must schedule relocated nested files");

        let _ = std::fs::remove_dir_all(&base_temp);
        Ok(())
    }

    #[tokio::test]
    async fn test_production_natural_language_bm25_search_and_provenance() -> anyhow::Result<()> {
        let temp_dir = std::env::temp_dir().join(format!("remainder_nl_bm25_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir)?;

        let doc_path = temp_dir.join("db_adr.md");
        let raw_text = "Architecture Decision Record: Persistence Layer\n\
                        We adopt PostgreSQL 16 for all relational data with max_connections=25.\n\
                        MongoDB was rejected due to lack of multi-document ACID transactions.\n\
                        All migrations must run through goose with strict backward compatibility.";
        std::fs::write(&doc_path, raw_text)?;

        let mut engine = engine::RemainderEngine::new(":memory:")?;
        let mut pending_files = HashMap::new();
        let mut in_flight = HashSet::new();

        let input_hash = compute_file_hash(raw_text);
        let llm_response = Ok(
            "[DATABASE] -> Adopted PostgreSQL 16 with max_connections=25.\n\
             [ARCHITECTURE] -> Rejected MongoDB due to lack of multi-document ACID transactions."
                .to_string(),
        );

        // Process through production inference pipeline with full raw content
        let outcome = process_inference_result(
            doc_path.clone(),
            input_hash,
            llm_response,
            &mut engine,
            &mut pending_files,
            &mut in_flight,
            1,
            Some(raw_text.to_string()),
        )?;

        assert_eq!(outcome, InferenceOutcome::Committed(2), "Must commit both atomic propositions");

        // 1. Natural language query containing words from document not in the tag ('MongoDB' and 'transactions')
        let hits = engine.search("Why did we reject MongoDB for transactions?", None)?;
        assert!(!hits.is_empty(), "FTS5 BM25 search must find relevant facts from natural language query");
        assert_eq!(hits[0].vault_tag, "ARCHITECTURE");
        assert!(hits[0].fact_content.contains("Rejected MongoDB"));
        assert_eq!(hits[0].file_path, doc_path.to_string_lossy());
        assert!(hits[0].raw_excerpt.as_ref().unwrap().contains("MongoDB was rejected"));

        // 2. Exact tag lookup query ('DATABASE') gets highest priority boost (-100.0 rank)
        let tag_hits = engine.search("DATABASE", None)?;
        assert!(!tag_hits.is_empty());
        assert_eq!(tag_hits[0].vault_tag, "DATABASE");
        assert_eq!(tag_hits[0].bm25_rank, -100.0, "Exact tag matches receive priority boost");
        assert!(tag_hits[0].fact_content.contains("PostgreSQL 16"));

        let _ = std::fs::remove_dir_all(&temp_dir);
        Ok(())
    }

    #[tokio::test]
    async fn test_production_failed_extraction_preserves_raw_source_content() -> anyhow::Result<()> {
        let temp_dir = std::env::temp_dir().join(format!("remainder_failed_ext_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir)?;

        let doc_path = temp_dir.join("critical_spec.md");
        let raw_text = "Mission Critical Specification: System uses KRYPTON_ALPHA token.";
        std::fs::write(&doc_path, raw_text)?;

        let mut engine = engine::RemainderEngine::new(":memory:")?;
        let mut pending_files = HashMap::new();
        let mut in_flight = HashSet::new();

        let input_hash = compute_file_hash(raw_text);
        // Simulate LLM failure (e.g. Ollama 500 or timeout)
        let llm_err = Err(anyhow::anyhow!("Ollama connection refused"));

        let outcome = process_inference_result(
            doc_path.clone(),
            input_hash,
            llm_err,
            &mut engine,
            &mut pending_files,
            &mut in_flight,
            1,
            Some(raw_text.to_string()),
        )?;

        // Outcome is ScheduledRetry, but raw source must already be persisted and searchable!
        assert_eq!(outcome, InferenceOutcome::ScheduledRetry { attempt: 1, delay: Duration::from_secs(1) });

        // 1. Verify get_source_content retrieves the full raw content immediately
        let retrieved = engine.get_source_content(doc_path.to_str().unwrap())?;
        assert_eq!(retrieved.as_deref(), Some(raw_text));

        // 2. Verify search immediately finds KRYPTON_ALPHA from sources_fts
        let hits = engine.search("KRYPTON_ALPHA", None)?;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].vault_tag, "SOURCE");
        assert!(hits[0].fact_content.contains("KRYPTON_ALPHA"));

        let _ = std::fs::remove_dir_all(&temp_dir);
        Ok(())
    }

    #[test]
    fn test_mcp_initialize_handshake() -> anyhow::Result<()> {
        let engine = engine::RemainderEngine::new(":memory:")?;
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let resp = process_mcp_message(req, &engine).expect("Must return response for initialize");
        
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(resp["result"]["serverInfo"]["name"], "remainder-bunker");
        assert_eq!(resp["result"]["serverInfo"]["version"], "0.4.0");
        Ok(())
    }

    #[test]
    fn test_mcp_ping_returns_empty_result() -> anyhow::Result<()> {
        let engine = engine::RemainderEngine::new(":memory:")?;
        let req = r#"{"jsonrpc":"2.0","id":"ping-42","method":"ping"}"#;
        let resp = process_mcp_message(req, &engine).expect("Must return response for ping");
        
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], "ping-42");
        assert_eq!(resp["result"], json!({}));
        Ok(())
    }

    #[test]
    fn test_mcp_notifications_initialized_yields_none() -> anyhow::Result<()> {
        let engine = engine::RemainderEngine::new(":memory:")?;
        let req = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let resp = process_mcp_message(req, &engine);
        assert!(resp.is_none(), "Notifications must not yield a response envelope");
        Ok(())
    }

    #[test]
    fn test_mcp_tools_list_registers_query_bunker_and_get_source() -> anyhow::Result<()> {
        let engine = engine::RemainderEngine::new(":memory:")?;
        let req = r#"{"jsonrpc":"2.0","id":100,"method":"tools/list"}"#;
        let resp = process_mcp_message(req, &engine).expect("Must return tools list");

        let tools = resp["result"]["tools"].as_array().expect("Tools must be array");
        assert_eq!(tools.len(), 2, "Must register exactly query_bunker and get_source");

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"query_bunker"));
        assert!(names.contains(&"get_source"));
        Ok(())
    }

    #[test]
    fn test_mcp_tools_call_query_bunker_returns_formatted_facts() -> anyhow::Result<()> {
        let mut engine = engine::RemainderEngine::new(":memory:")?;
        engine.refresh_source_facts("arch.md", Some("Arch decisions"), vec![
            ("CACHE".into(), "Redis 7 with LRU eviction policy.".into()),
        ])?;

        let req = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"query_bunker","arguments":{"query":"CACHE"}}}"#;
        let resp = process_mcp_message(req, &engine).expect("Must process query_bunker call");

        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("[BUNKER] [CACHE] -> Redis 7 with LRU eviction policy."));
        assert!(text.contains("Source: arch.md"));
        Ok(())
    }

    #[test]
    fn test_mcp_tools_call_query_bunker_respects_limit() -> anyhow::Result<()> {
        let mut engine = engine::RemainderEngine::new(":memory:")?;
        engine.refresh_source_facts("s1.txt", Some("doc 1"), vec![("TAG".into(), "Fact 1".into())])?;
        engine.refresh_source_facts("s2.txt", Some("doc 2"), vec![("TAG".into(), "Fact 2".into())])?;

        let req = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"query_bunker","arguments":{"query":"TAG","limit":1}}}"#;
        let resp = process_mcp_message(req, &engine).expect("Must handle limit in query_bunker");

        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        // Since limit=1, there should be exactly one [BUNKER] tag
        let bunker_count = text.matches("[BUNKER]").count();
        assert_eq!(bunker_count, 1, "Limit=1 over MCP must return exactly 1 bunker hit");
        Ok(())
    }

    #[test]
    fn test_mcp_tools_call_query_bunker_no_records() -> anyhow::Result<()> {
        let engine = engine::RemainderEngine::new(":memory:")?;
        let req = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"query_bunker","arguments":{"query":"NON_EXISTENT"}}}"#;
        let resp = process_mcp_message(req, &engine).expect("Must handle empty query");

        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("[BUNKER] No records found for query: NON_EXISTENT"));
        Ok(())
    }

    #[test]
    fn test_mcp_tools_call_get_source_success() -> anyhow::Result<()> {
        let mut engine = engine::RemainderEngine::new(":memory:")?;
        let source_code = "pub fn execute() -> Result<(), Error> { Ok(()) }";
        engine.refresh_source_facts("lib.rs", Some(source_code), vec![])?;

        let req = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"get_source","arguments":{"path":"lib.rs"}}}"#;
        let resp = process_mcp_message(req, &engine).expect("Must handle get_source");

        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, source_code);
        Ok(())
    }

    #[test]
    fn test_mcp_tools_call_get_source_not_found() -> anyhow::Result<()> {
        let engine = engine::RemainderEngine::new(":memory:")?;
        let req = r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"get_source","arguments":{"path":"missing.rs"}}}"#;
        let resp = process_mcp_message(req, &engine).expect("Must handle missing source");

        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("[BUNKER] Source not found or has no recorded content: missing.rs"));
        Ok(())
    }

    #[test]
    fn test_mcp_tools_call_unknown_tool() -> anyhow::Result<()> {
        let engine = engine::RemainderEngine::new(":memory:")?;
        let req = r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"super_tool","arguments":{}}}"#;
        let resp = process_mcp_message(req, &engine).expect("Must return response for unknown tool");

        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, "Unknown tool: super_tool");
        Ok(())
    }

    #[test]
    fn test_mcp_unknown_method_returns_32601() -> anyhow::Result<()> {
        let engine = engine::RemainderEngine::new(":memory:")?;
        let req = r#"{"jsonrpc":"2.0","id":7,"method":"unsupported_method"}"#;
        let resp = process_mcp_message(req, &engine).expect("Must return error response for unknown method");

        assert_eq!(resp["error"]["code"], -32601);
        assert_eq!(resp["error"]["message"], "Method not found");
        Ok(())
    }

    #[test]
    fn test_mcp_malformed_json_and_empty_line_ignored() -> anyhow::Result<()> {
        let engine = engine::RemainderEngine::new(":memory:")?;
        assert!(process_mcp_message("", &engine).is_none());
        assert!(process_mcp_message("   \n", &engine).is_none());
        assert!(process_mcp_message("Not a valid json string {", &engine).is_none());
        Ok(())
    }

    #[test]
    fn test_pipeline_supported_file_extensions_exhaustive() {
        assert!(is_supported_file(Path::new("src/main.rs")));
        assert!(is_supported_file(Path::new("docs/notes.txt")));
        assert!(is_supported_file(Path::new("README.md")));
        assert!(!is_supported_file(Path::new("app.exe")));
        assert!(!is_supported_file(Path::new("data.db")));
        assert!(!is_supported_file(Path::new("image.png")));
        assert!(!is_supported_file(Path::new("no_ext")));
    }

    #[test]
    fn test_pipeline_compute_file_hash_avalanche_effect() {
        let h1 = compute_file_hash("abc");
        let h2 = compute_file_hash("abd");
        assert_ne!(h1, h2);
        assert_eq!(h1.len(), 64);
        assert_eq!(h2.len(), 64);
        // Determinism
        assert_eq!(h1, compute_file_hash("abc"));
    }

    #[test]
    fn test_pipeline_exponential_backoff_schedule_bounds() {
        assert_eq!(compute_exponential_backoff(0, 1), Duration::from_secs(0));
        assert_eq!(compute_exponential_backoff(1, 1), Duration::from_secs(1));
        assert_eq!(compute_exponential_backoff(2, 1), Duration::from_secs(2));
        assert_eq!(compute_exponential_backoff(3, 1), Duration::from_secs(4));
        assert_eq!(compute_exponential_backoff(4, 1), Duration::from_secs(8));
        assert_eq!(compute_exponential_backoff(5, 1), Duration::from_secs(16));
        // Shift cap check (min(6)) -> 2^6 = 64
        assert_eq!(compute_exponential_backoff(7, 1), Duration::from_secs(64));
        assert_eq!(compute_exponential_backoff(10, 1), Duration::from_secs(64));
    }

    #[test]
    fn test_pipeline_race_guard_detects_content_hash_mutation() -> anyhow::Result<()> {
        let mut engine = engine::RemainderEngine::new(":memory:")?;
        let mut pending = HashMap::new();
        let mut in_flight = HashSet::new();

        let path = PathBuf::from("race.txt");
        let initial_hash = compute_file_hash("Initial version");
        // File content changed in-flight to "Mutated version"
        let outcome = process_inference_result(
            path,
            initial_hash,
            Ok("[TAG] -> Fact".to_string()),
            &mut engine,
            &mut pending,
            &mut in_flight,
            1,
            Some("Mutated version".to_string()),
        )?;

        assert_eq!(outcome, InferenceOutcome::DiscardedStaleChanged, "Must discard stale inference result when file changed in-flight");
        Ok(())
    }

    #[test]
    fn test_pipeline_race_guard_detects_file_deletion_in_flight() -> anyhow::Result<()> {
        let mut engine = engine::RemainderEngine::new(":memory:")?;
        let mut pending = HashMap::new();
        let mut in_flight = HashSet::new();

        let path = PathBuf::from("deleted.txt");
        let initial_hash = compute_file_hash("Was here");
        // File deleted during LLM call -> current_content is None
        let outcome = process_inference_result(
            path,
            initial_hash,
            Ok("[TAG] -> Fact".to_string()),
            &mut engine,
            &mut pending,
            &mut in_flight,
            1,
            None,
        )?;

        assert_eq!(outcome, InferenceOutcome::DiscardedStaleDeleted, "Must discard result when file was deleted in-flight");
        Ok(())
    }
}
