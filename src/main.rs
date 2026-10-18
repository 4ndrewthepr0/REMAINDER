mod engine;
mod shredder;

use std::path::{Path, PathBuf};
use std::env;
use std::io::{BufRead, Write};
use std::collections::{HashMap, HashSet};
use notify::{Watcher, RecursiveMode, Config, EventKind};
use tokio::sync::mpsc;
use tokio::time::{interval, Duration, Instant};
use serde_json::json;
use sha2::{Sha256, Digest};

fn get_db_path() -> PathBuf {
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

fn compute_file_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hex::encode(hasher.finalize())
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
        if line.trim().is_empty() {
            continue;
        }

        let req: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = req.get("id");
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

        let response = match method {
            "initialize" => {
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": { "tools": {} },
                        "serverInfo": {
                            "name": "remainder-bunker",
                            "version": "0.3.0"
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
                        "tools": [{
                            "name": "query_bunker",
                            "description": "Retrieves historical technical facts from the local REMAINDER vault.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "tag": { "type": "string", "description": "Category (e.g. DATABASE, AUTH)" }
                                },
                                "required": ["tag"]
                            }
                        }]
                    }
                }))
            }
            "tools/call" => {
                let params = req.get("params");
                let name = params.and_then(|p| p.get("name")).and_then(|n| n.as_str()).unwrap_or("");
                let tag = params
                    .and_then(|p| p.get("arguments"))
                    .and_then(|a| a.get("tag"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");

                let text = if name == "query_bunker" {
                    match engine.get_latest_fact(tag) {
                        Ok(Some(fact)) => format!("[BUNKER] {} -> {}", tag, fact),
                        Ok(None) => format!("[BUNKER] No records found for tag: {}", tag),
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
        };

        if let Some(resp) = response {
            writeln!(stdout, "{}", serde_json::to_string(&resp)?)?;
            stdout.flush()?;
        }
    }
    Ok(())
}

enum WatchEvent {
    Modify(PathBuf),
    Remove(PathBuf),
}

struct PendingItem {
    deadline: Instant,
    retry_count: u32,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let t_app_start = std::time::Instant::now();
    let args: Vec<String> = env::args().collect();
    let db_path = get_db_path();
    let db_path_str = db_path.to_string_lossy().to_string();
    let mut engine = engine::RemainderEngine::new(&db_path_str)?;
    let db_init_duration = t_app_start.elapsed();

    // --- MCP BRIDGE MODE ---
    if args.len() > 1 && args[1] == "mcp" {
        if args.len() > 2 && args[2] == "list" {
            println!("{}", json!({
                "tools": [{
                    "name": "query_bunker",
                    "description": "Retrieves historical technical facts from the local REMAINDER vault.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "tag": { "type": "string", "description": "Category (e.g. DATABASE, AUTH)" }
                        },
                        "required": ["tag"]
                    }
                }]
            }));
            return Ok(());
        } else if args.len() > 3 && args[2] == "call" {
            let tag = &args[3];
            if let Ok(Some(fact)) = engine.get_latest_fact(tag) {
                println!("{}", json!({ "content": [{ "type": "text", "text": fact }] }));
            } else {
                println!("{}", json!({ "content": [{ "type": "text", "text": "No records found." }] }));
            }
            return Ok(());
        } else {
            return run_mcp_stdio_server(&engine);
        }
    }

    // --- CLI QUERY MODE ---
    if args.len() > 2 && args[1] == "query" {
        let tag = &args[2];
        match engine.get_latest_fact(tag)? {
            Some(fact) => println!("[BUNKER] {} -> {}", tag, fact),
            None => println!("[BUNKER] No records found for tag: {}", tag),
        }
        return Ok(());
    }

    // --- GHOST WATCHER MODE ---
    let shredder = shredder::Shredder::new("llama3.1:8b");

    let t_watch_start = std::time::Instant::now();
    let (tx, mut rx) = mpsc::channel::<WatchEvent>(200);
    let (std_tx, std_rx) = std::sync::mpsc::channel();
    let mut watcher = notify::RecommendedWatcher::new(std_tx, Config::default())?;
    watcher.watch(Path::new("."), RecursiveMode::Recursive)?;
    let watch_init_duration = t_watch_start.elapsed();
    let total_init_duration = t_app_start.elapsed();

    println!("============================================================");
    println!("[STARTUP BENCHMARK]");
    println!("  - In-process SQLite & Schema Init : {:?}", db_init_duration);
    println!("  - Filesystem Watcher Registration : {:?}", watch_init_duration);
    println!("  - Total In-Process Ready Time     : {:?}", total_init_duration);
    println!("============================================================");
    println!("Bunker is watching (debounced + cascade deletion + async shredder). Ray Kroc is in the building.");

    tokio::task::spawn_blocking(move || {
        for res in std_rx {
            if let Ok(event) = res {
                // Ignore build target directory
                if event.paths.iter().any(|p| p.components().any(|c| c.as_os_str() == "target")) {
                    continue;
                }

                match event.kind {
                    EventKind::Remove(_) => {
                        // Crucial: Do NOT filter removal by extension so directory removals are forwarded!
                        for path in event.paths {
                            let _ = tx.blocking_send(WatchEvent::Remove(path));
                        }
                    }
                    EventKind::Modify(notify::event::ModifyKind::Name(rename_mode)) => {
                        use notify::event::RenameMode;
                        match rename_mode {
                            RenameMode::Both => {
                                if event.paths.len() >= 2 {
                                    let _ = tx.blocking_send(WatchEvent::Remove(event.paths[0].clone()));
                                    let to_path = event.paths[1].clone();
                                    if to_path.extension().map_or(false, |ext| ext == "txt" || ext == "rs" || ext == "md") {
                                        let _ = tx.blocking_send(WatchEvent::Modify(to_path));
                                    }
                                }
                            }
                            RenameMode::From => {
                                for path in event.paths {
                                    let _ = tx.blocking_send(WatchEvent::Remove(path));
                                }
                            }
                            RenameMode::To => {
                                for path in event.paths {
                                    if path.extension().map_or(false, |ext| ext == "txt" || ext == "rs" || ext == "md") {
                                        let _ = tx.blocking_send(WatchEvent::Modify(path));
                                    }
                                }
                            }
                            _ => {
                                for path in event.paths {
                                    if path.exists() {
                                        if path.extension().map_or(false, |ext| ext == "txt" || ext == "rs" || ext == "md") {
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
                            if path.extension().map_or(false, |ext| ext == "txt" || ext == "rs" || ext == "md") {
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

    loop {
        tokio::select! {
            Some(event) = rx.recv() => {
                match event {
                    WatchEvent::Remove(path) => {
                        pending_files.remove(&path);
                        in_flight.remove(&path);
                        let file_str = path.to_string_lossy().to_string();
                        let file_name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                        match engine.remove_source_or_prefix(&file_str) {
                            Ok(deleted) if deleted > 0 => {
                                println!("\n[DELETION] Removed '{}'. Purged {} cascade sources/facts.", file_name, deleted);
                                let _ = engine.checkpoint();
                            }
                            _ => {}
                        }
                        let _ = engine.reconcile_missing_sources();
                    }
                    WatchEvent::Modify(path) => {
                        let deadline = Instant::now() + Duration::from_secs(3);
                        if let Some(file_name) = path.file_name() {
                            if !pending_files.contains_key(&path) && !in_flight.contains(&path) {
                                println!("\n[DEBOUNCE] Activity detected on {:?}. Buffering for 3s calm...", file_name);
                            }
                        }
                        let prev_retries = pending_files.get(&path).map(|p| p.retry_count).unwrap_or(0);
                        pending_files.insert(path, PendingItem { deadline, retry_count: prev_retries });
                    }
                }
            }

            Some((path, input_hash, res)) = result_rx.recv() => {
                in_flight.remove(&path);
                let file_name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                let file_str = path.to_string_lossy().to_string();

                // Race condition guard: verify file still exists with identical content
                let current_content = match std::fs::read_to_string(&path) {
                    Ok(c) => c,
                    Err(_) => {
                        println!("[RACE GUARD] File '{}' disappeared during inference. Stale result discarded.", file_name);
                        continue;
                    }
                };

                if compute_file_hash(&current_content) != input_hash {
                    println!("[RACE GUARD] File '{}' changed during LLM inference. Stale result discarded.", file_name);
                    continue;
                }

                let current_retry = pending_files.get(&path).map(|p| p.retry_count).unwrap_or(0);

                match res {
                    Ok(raw_fact) => {
                        let mut facts_to_insert = Vec::new();
                        for line in raw_fact.lines() {
                            let parts: Vec<&str> = line.split("->").collect();
                            if parts.len() == 2 {
                                let tag = parts[0].trim().replace("[", "").replace("]", "");
                                let data = parts[1].trim().to_string();
                                if !tag.is_empty() && !data.is_empty() {
                                    facts_to_insert.push((tag, data));
                                }
                            }
                        }

                        if facts_to_insert.is_empty() {
                            let parts: Vec<&str> = raw_fact.split("->").collect();
                            if parts.len() == 2 {
                                let tag = parts[0].trim().replace("[", "").replace("]", "");
                                let data = parts[1].trim().to_string();
                                if !tag.is_empty() && !data.is_empty() {
                                    facts_to_insert.push((tag, data));
                                }
                            }
                        }

                        if facts_to_insert.is_empty() {
                            let next_retry = current_retry + 1;
                            if next_retry <= 5 {
                                let delay = Duration::from_secs(2 * next_retry as u64);
                                println!("[RETRY] Model output unparsed into structured facts. Scheduling recovery retry {}/5 for '{}' in {:?}", next_retry, file_name, delay);
                                pending_files.insert(path, PendingItem {
                                    deadline: Instant::now() + delay,
                                    retry_count: next_retry,
                                });
                            } else {
                                println!("[RETRY] Max retry attempts (5/5) reached for '{}'. Existing vault state preserved.", file_name);
                                pending_files.remove(&path);
                            }
                            continue;
                        }

                        for (tag, data) in &facts_to_insert {
                            println!("[BUNKER] Atomic Truth: [{}] -> {}", tag, data);
                        }

                        match engine.refresh_source_facts(&file_str, facts_to_insert) {
                            Ok(count) => {
                                println!("[PROVENANCE] Updated {} facts for source '{}'. Stale facts purged.", count, file_name);
                                let _ = engine.checkpoint();
                                pending_files.remove(&path);
                            }
                            Err(e) => {
                                println!("[ERROR] Transaction aborted. Database rolled back safely: {}", e);
                                let next_retry = current_retry + 1;
                                if next_retry <= 5 {
                                    let delay = Duration::from_secs(2 * next_retry as u64);
                                    pending_files.insert(path, PendingItem {
                                        deadline: Instant::now() + delay,
                                        retry_count: next_retry,
                                    });
                                } else {
                                    pending_files.remove(&path);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let next_retry = current_retry + 1;
                        if next_retry <= 5 {
                            let delay = Duration::from_secs(2 * next_retry as u64);
                            println!("[RETRY] Extraction failed: {}. Scheduling recovery retry {}/5 for '{}' in {:?}", e, next_retry, file_name, delay);
                            pending_files.insert(path, PendingItem {
                                deadline: Instant::now() + delay,
                                retry_count: next_retry,
                            });
                        } else {
                            println!("[RETRY] Max retry attempts (5/5) reached for '{}'. Existing vault state preserved.", file_name);
                            pending_files.remove(&path);
                        }
                    }
                }
            }

            _ = ticker.tick() => {
                tick_counter += 1;
                // Periodically reconcile deleted/renamed paths every 5 seconds (10 ticks * 500ms)
                if tick_counter % 10 == 0 {
                    let _ = engine.reconcile_missing_sources();
                }

                let now = Instant::now();
                let ready_paths: Vec<PathBuf> = pending_files
                    .iter()
                    .filter(|(path, item)| item.deadline <= now && !in_flight.contains(*path))
                    .map(|(path, _)| path.clone())
                    .collect();

                for path in ready_paths {
                    if !path.exists() {
                        pending_files.remove(&path);
                        let file_str = path.to_string_lossy().to_string();
                        let _ = engine.remove_source_or_prefix(&file_str);
                        continue;
                    }

                    let content = match std::fs::read_to_string(&path) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };

                    let file_name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                    let file_str = path.to_string_lossy().to_string();

                    // Handle file emptying: clearing all text in an existing file purges its facts
                    if content.trim().is_empty() {
                        pending_files.remove(&path);
                        match engine.refresh_source_facts(&file_str, Vec::new()) {
                            Ok(_) => {
                                println!("\n[GHOST] Source file '{}' emptied. All associated facts cleared.", file_name);
                                let _ = engine.checkpoint();
                            }
                            Err(e) => println!("Error clearing emptied file facts: {}", e),
                        }
                        continue;
                    }

                    let input_hash = compute_file_hash(&content);
                    in_flight.insert(path.clone());

                    println!("\n[GHOST] Ripe file ready: {}. Dispatching Llama 3.1 background inference...", file_name);
                    let shredder_clone = shredder.clone();
                    let tx_clone = result_tx.clone();
                    let p_clone = path.clone();
                    let c_clone = content.clone();
                    let h_clone = input_hash.clone();

                    tokio::spawn(async move {
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
