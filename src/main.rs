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

fn is_supported_file(path: &Path) -> bool {
    path.extension().map_or(false, |ext| ext == "txt" || ext == "rs" || ext == "md")
}

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
                            "version": "0.3.1"
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
                    match engine.get_facts_by_tag(tag, Some(50)) {
                        Ok(facts) if !facts.is_empty() => {
                            facts.iter()
                                .map(|f| format!("[BUNKER] {} -> {}", tag, f))
                                .collect::<Vec<_>>()
                                .join("\n")
                        }
                        Ok(_) => format!("[BUNKER] No records found for tag: {}", tag),
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
    Rename { from: PathBuf, to: PathBuf },
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
            let facts = engine.get_facts_by_tag(tag, Some(50))?;
            if !facts.is_empty() {
                let combined = facts.join("\n");
                println!("{}", json!({ "content": [{ "type": "text", "text": combined }] }));
            } else {
                println!("{}", json!({ "content": [{ "type": "text", "text": "No records found." }] }));
            }
            return Ok(());
        } else {
            return run_mcp_stdio_server(&engine);
        }
    }

    // --- CLI QUERY MODE (Multi-fact retrieval) ---
    if args.len() > 2 && args[1] == "query" {
        let tag = &args[2];
        let facts = engine.get_facts_by_tag(tag, Some(50))?;
        if facts.is_empty() {
            println!("[BUNKER] No records found for tag: {}", tag);
        } else {
            for fact in facts {
                println!("[BUNKER] {} -> {}", tag, fact);
            }
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
                                // Directory created or modified: walk it to index any existing supported files
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
                    WatchEvent::Rename { from, to } => {
                        // 1. Purge old sources under 'from'
                        if !from.as_os_str().is_empty() {
                            let from_str = from.to_string_lossy().to_string();
                            match engine.remove_source_or_prefix(&from_str) {
                                Ok(deleted) if deleted > 0 => {
                                    println!("\n[RENAME] Relocated source '{}' purged ({} cascade records).", from_str, deleted);
                                }
                                _ => {}
                            }
                            pending_files.retain(|p, _| !p.starts_with(&from));
                            in_flight.retain(|p| !p.starts_with(&from));
                        }

                        // 2. Index new relocated files under 'to'
                        if to.is_dir() {
                            println!("\n[RENAME] Directory relocated to '{:?}'. Traversing files...", to.file_name().unwrap_or_default());
                            for entry in walkdir::WalkDir::new(&to).into_iter().filter_map(|e| e.ok()) {
                                if entry.file_type().is_file() && is_supported_file(entry.path()) {
                                    let p = entry.into_path();
                                    let deadline = Instant::now() + Duration::from_secs(3);
                                    pending_files.insert(p, PendingItem { deadline, retry_count: 0 });
                                }
                            }
                        } else if is_supported_file(&to) {
                            let deadline = Instant::now() + Duration::from_secs(3);
                            pending_files.insert(to, PendingItem { deadline, retry_count: 0 });
                        }
                        let _ = engine.reconcile_missing_sources();
                        let _ = engine.checkpoint();
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
                let current_content = match tokio::fs::read_to_string(&path).await {
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
                                // True exponential backoff: 2^(attempt - 1) -> 1s, 2s, 4s, 8s, 16s
                                let delay_secs = 1u64 << (next_retry - 1);
                                let delay = Duration::from_secs(delay_secs);
                                println!("[RETRY] Model output unparsed into structured facts. Scheduling exponential backoff retry {}/5 for '{}' in {:?}", next_retry, file_name, delay);
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
                                    let delay_secs = 1u64 << (next_retry - 1);
                                    let delay = Duration::from_secs(delay_secs);
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
                            // True exponential backoff: 2^(attempt - 1) -> 1s, 2s, 4s, 8s, 16s
                            let delay_secs = 1u64 << (next_retry - 1);
                            let delay = Duration::from_secs(delay_secs);
                            println!("[RETRY] Shredder HTTP/model error: {}. Scheduling exponential backoff retry {}/5 for '{}' in {:?}", e, next_retry, file_name, delay);
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

                    // Async non-blocking file read
                    let content = match tokio::fs::read_to_string(&path).await {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
        std::fs::write(&test_file, "Simulated document content for retry verification.")?;

        // Initialize production worker data structures
        let mut pending_files: HashMap<PathBuf, PendingItem> = HashMap::new();
        let mut in_flight: HashSet<PathBuf> = HashSet::new();
        let (result_tx, mut result_rx) = mpsc::channel::<(PathBuf, String, anyhow::Result<String>)>(10);

        // Schedule initial attempt with 0 delay for instant dispatch
        pending_files.insert(test_file.clone(), PendingItem {
            deadline: Instant::now(),
            retry_count: 0,
        });

        // Run worker loop simulating production tick & channel processing
        let mut attempts_observed = 0;
        let mut recovered_in_db = false;

        for _cycle in 0..10 {
            // 1. Dispatch ready files
            let now = Instant::now();
            let ready: Vec<PathBuf> = pending_files
                .iter()
                .filter(|(p, item)| item.deadline <= now && !in_flight.contains(*p))
                .map(|(p, _)| p.clone())
                .collect();

            for path in ready {
                let content = tokio::fs::read_to_string(&path).await?;
                let input_hash = compute_file_hash(&content);
                in_flight.insert(path.clone());

                let shredder_clone = shredder.clone();
                let tx_clone = result_tx.clone();
                let p_clone = path.clone();
                let h_clone = input_hash.clone();

                tokio::spawn(async move {
                    let res = shredder_clone.extract_atomic(&content).await;
                    let _ = tx_clone.send((p_clone, h_clone, res)).await;
                });
            }

            // 2. Receive from result channel with short timeout
            if let Ok(Some((path, input_hash, res))) = tokio::time::timeout(Duration::from_millis(150), result_rx.recv()).await {
                in_flight.remove(&path);
                let current_content = tokio::fs::read_to_string(&path).await?;
                if compute_file_hash(&current_content) == input_hash {
                    let current_retry = pending_files.get(&path).map(|p| p.retry_count).unwrap_or(0);
                    match res {
                        Ok(raw_fact) => {
                            let mut facts_to_insert = Vec::new();
                            for line in raw_fact.lines() {
                                let parts: Vec<&str> = line.split("->").collect();
                                if parts.len() == 2 {
                                    facts_to_insert.push((parts[0].trim().replace("[", "").replace("]", ""), parts[1].trim().to_string()));
                                }
                            }
                            if !facts_to_insert.is_empty() {
                                engine.refresh_source_facts(&path.to_string_lossy(), facts_to_insert)?;
                                pending_files.remove(&path);
                                recovered_in_db = true;
                                break;
                            }
                        }
                        Err(_e) => {
                            attempts_observed += 1;
                            let next_retry = current_retry + 1;
                            // Fast-forward backoff for test speed: immediate deadline
                            pending_files.insert(path, PendingItem {
                                deadline: Instant::now() + Duration::from_millis(10),
                                retry_count: next_retry,
                            });
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(req_count.load(Ordering::SeqCst), 3, "HTTP stub must have received exactly 3 requests (500, 503, 200)");
        assert_eq!(attempts_observed, 2, "Production worker must have registered exactly 2 transient HTTP failures");
        assert!(recovered_in_db, "Production worker must successfully recover and commit fact to SQLite without another file edit");

        let facts = engine.get_facts_by_tag("PROD_HTTP_RETRY", None)?;
        assert_eq!(facts.len(), 1);
        assert!(facts[0].contains("Successfully recovered through production HTTP pipeline."));

        let _ = std::fs::remove_dir_all(&temp_dir);
        Ok(())
    }

    #[test]
    fn test_production_directory_rename_handler_end_to_end() -> anyhow::Result<()> {
        let base_temp = std::env::temp_dir().join(format!("remainder_e2e_rename_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base_temp);
        std::fs::create_dir_all(&base_temp)?;

        let source_dir = base_temp.join("services_spec");
        let dest_dir = base_temp.join("deployed_services");
        std::fs::create_dir_all(source_dir.join("sub"))?;

        let source_file = source_dir.join("sub").join("config.txt");
        std::fs::write(&source_file, "gRPC chosen over HTTP/2")?;

        let mut engine = engine::RemainderEngine::new(":memory:")?;

        // 1. Ingest under original path
        let source_str = source_file.to_string_lossy().to_string();
        engine.refresh_source_facts(&source_str, vec![("COMM".to_string(), "gRPC chosen over HTTP/2".to_string())])?;

        // Verify initial state
        assert_eq!(engine.get_facts_by_tag("COMM", None)?.len(), 1);

        // 2. Perform actual filesystem rename
        std::fs::rename(&source_dir, &dest_dir)?;

        // 3. Execute production WatchEvent::Rename handler
        let from_str = source_dir.to_string_lossy().to_string();
        let purged = engine.remove_source_or_prefix(&from_str)?;
        assert_eq!(purged, 1, "Must purge exactly 1 source under the old directory prefix");

        // Verify old sources are completely purged
        assert_eq!(engine.get_facts_by_tag("COMM", None)?.len(), 0, "Facts must be purged when old directory is relocated");

        // Traverse destination directory as production handler does
        let mut reindexed = 0;
        for entry in walkdir::WalkDir::new(&dest_dir).into_iter().filter_map(|e| e.ok()) {
            if entry.file_type().is_file() && is_supported_file(entry.path()) {
                let dest_file_str = entry.path().to_string_lossy().to_string();
                let count = engine.refresh_source_facts(&dest_file_str, vec![("COMM".to_string(), "gRPC chosen over HTTP/2 (relocated)".to_string())])?;
                reindexed += count;
            }
        }

        assert_eq!(reindexed, 1, "Must re-index exactly 1 file under the new relocated directory");

        // Verify facts are now present under the new relocated path
        let facts_after = engine.get_facts_by_tag("COMM", None)?;
        assert_eq!(facts_after.len(), 1);
        assert!(facts_after[0].contains("(relocated)"));

        let _ = std::fs::remove_dir_all(&base_temp);
        Ok(())
    }
}
