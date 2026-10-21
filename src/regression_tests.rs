use super::*;

#[test]
fn watcher_dot_paths_reuse_existing_source_and_pending_key() -> anyhow::Result<()> {
    let mut engine = engine::RemainderEngine::new(":memory:")?;
    engine.refresh_source_facts("project/file.md", Some("Old"), vec![("TAG".into(), "Old".into())])?;
    // Simulate a legacy row and a watcher path with redundant components.
    engine.conn.execute("UPDATE sources SET file_path = 'project/./file.md'", [])?;
    engine.record_source_content("project/file.md", "New")?;
    let count: i64 = engine.conn.query_row("SELECT count(*) FROM sources", [], |r| r.get(0))?;
    assert_eq!(count, 1);
    assert!(engine.get_facts_by_tag("TAG", None)?.is_empty());
    let mut pending = HashMap::new();
    handle_watch_event(WatchEvent::Modify(PathBuf::from("project/./file.md")), &mut engine,
        &mut pending, &mut HashSet::new(), Duration::ZERO)?;
    assert!(pending.contains_key(&PathBuf::from("project/file.md")));
    Ok(())
}

#[test]
fn unicode_search_survives_excerpt_boundaries_and_case_expansion() -> anyhow::Result<()> {
    for raw in [format!("{}🚀 remaining text", "a".repeat(299)),
        format!("{} QUARTZFOX trailing", "İ".repeat(80)),
        format!("{}部署資料 QUARTZFOX", "🚀".repeat(110))] {
        let mut engine = engine::RemainderEngine::new(":memory:")?;
        engine.refresh_source_facts("unicode.md", Some(&raw), vec![("DOC".into(), "Document.".into())])?;
        assert!(!engine.search("DOC", None)?.is_empty());
        if raw.contains("QUARTZFOX") {
            assert!(engine.search("QUARTZFOX", None)?.iter().any(|hit| hit.fact_content.contains("QUARTZFOX")));
        }
        assert_eq!(engine.get_source_content("unicode.md")?.as_deref(), Some(raw.as_str()));
    }
    Ok(())
}

#[test]
fn changed_source_hides_old_facts_until_successful_refresh() -> anyhow::Result<()> {
    let mut engine = engine::RemainderEngine::new(":memory:")?;
    engine.refresh_source_facts("db.md", Some("Use MongoDB."), vec![("DATABASE".into(), "Use MongoDB.".into())])?;
    engine.conn.execute("UPDATE sources SET last_shredded = '2000-01-01'", [])?;
    engine.record_source_content("db.md", "Reject MongoDB. Use PostgreSQL.")?;
    assert!(engine.get_facts_by_tag("DATABASE", None)?.is_empty());
    let hits = engine.search("MongoDB", None)?;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].vault_tag, "SOURCE");
    assert!(hits[0].fact_content.contains("Reject MongoDB"));
    let (count, timestamp): (i64, String) = engine.conn.query_row(
        "SELECT (SELECT count(*) FROM atomics), last_shredded FROM sources", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
    assert_eq!(count, 1, "Old facts retained, not destroyed on extraction failure");
    assert_eq!(timestamp, "2000-01-01");
    engine.refresh_source_facts("db.md", Some("Reject MongoDB. Use PostgreSQL."), vec![("DATABASE".into(), "Use PostgreSQL.".into())])?;
    assert_eq!(engine.get_facts_by_tag("DATABASE", None)?, vec!["Use PostgreSQL."]);
    engine.record_source_content("db.md", "Reject MongoDB. Use PostgreSQL.")?;
    assert_eq!(engine.get_facts_by_tag("DATABASE", None)?.len(), 1);
    Ok(())
}

#[test]
fn migration_v4_preserves_rows_but_does_not_assert_unknown_freshness() -> anyhow::Result<()> {
    let mut engine = engine::RemainderEngine::new(":memory:")?;
    engine.refresh_source_facts("db.md", Some("Source."), vec![("TAG".into(), "Old fact.".into())])?;
    engine.conn.execute_batch("ALTER TABLE sources DROP COLUMN facts_current;
        ALTER TABLE sources DROP COLUMN observed_at; PRAGMA user_version = 4;")?;
    engine::RemainderEngine::run_migrations(&mut engine.conn)?;
    let count: i64 = engine.conn.query_row("SELECT count(*) FROM atomics", [], |r| r.get(0))?;
    assert_eq!(count, 1);
    assert!(engine.get_facts_by_tag("TAG", None)?.is_empty());
    assert_eq!(engine.get_source_content("db.md")?.as_deref(), Some("Source."));
    assert_eq!(engine.search("Source", None)?.len(), 1);
    Ok(())
}

#[test]
fn malformed_partial_extraction_cannot_replace_prior_facts() -> anyhow::Result<()> {
    let mut engine = engine::RemainderEngine::new(":memory:")?;
    let raw = "Use Postgres and TLS.";
    engine.refresh_source_facts("db.md", Some(raw), vec![("DB".into(), "Use Postgres.".into()), ("TLS".into(), "Use TLS.".into())])?;
    let outcome = process_inference_result(PathBuf::from("db.md"), compute_file_hash(raw),
        Ok("[DB] -> Use Postgres.\nmalformed second fact".into()), &mut engine,
        &mut HashMap::new(), &mut HashSet::new(), 1, Some(raw.into()))?;
    assert!(matches!(outcome, InferenceOutcome::ScheduledRetry { .. }));
    assert_eq!(engine.get_facts_by_tag("TLS", None)?, vec!["Use TLS."]);
    assert_eq!(parse_facts("[FLOW] -> a -> b")?[0].1, "a -> b");
    Ok(())
}

#[tokio::test]
async fn nonresponding_http_request_times_out_and_schedules_recovery() -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    let shredder = shredder::Shredder::with_timeout("test", &format!("http://{}", address), Duration::from_millis(100));
    let result = tokio::time::timeout(Duration::from_secs(3), shredder.extract_atomic("Use PostgreSQL.")).await?;
    server.abort();
    assert!(result.is_err());
    let mut engine = engine::RemainderEngine::new(":memory:")?;
    let path = PathBuf::from("db.md");
    let mut running = HashSet::from([path.clone()]);
    let mut pending = HashMap::new();
    let outcome = process_inference_result(path.clone(), compute_file_hash("Use PostgreSQL."), result,
        &mut engine, &mut pending, &mut running, 1, Some("Use PostgreSQL.".into()))?;
    assert!(matches!(outcome, InferenceOutcome::ScheduledRetry { attempt: 1, .. }));
    assert!(running.is_empty());
    assert!(pending.contains_key(&path));
    assert_eq!(engine.search("PostgreSQL", None)?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn startup_and_periodic_scan_recover_offline_edits_and_new_files() -> anyhow::Result<()> {
    let root = std::env::temp_dir().join(format!("remainder_recovery_{}", std::process::id()));
    std::fs::create_dir_all(root.join("node_modules"))?;
    let path = root.join("config.PY");
    std::fs::write(&path, "Use PostgreSQL.")?;
    std::fs::write(root.join("node_modules/ignored.ts"), "Ignored")?;
    let mut engine = engine::RemainderEngine::new(":memory:")?;
    let mut pending = HashMap::new();
    reconcile_project(&root, &mut engine, &mut pending, true).await?;
    assert_eq!(pending.len(), 1);
    assert!(pending.contains_key(&path));
    engine.refresh_source_facts(&path.to_string_lossy(), Some("Use PostgreSQL."), vec![("DB".into(), "Use PostgreSQL.".into())])?;
    pending.clear();
    reconcile_project(&root, &mut engine, &mut pending, true).await?;
    assert!(pending.is_empty(), "Current unchanged files need no inference");
    std::fs::write(&path, "Use SQLite.")?;
    reconcile_project(&root, &mut engine, &mut pending, false).await?;
    assert!(pending.contains_key(&path));
    assert!(engine.get_facts_by_tag("DB", None)?.is_empty());
    assert_eq!(engine.search("SQLite", None)?.len(), 1);
    pending.clear();
    reconcile_project(&root, &mut engine, &mut pending, false).await?;
    assert!(pending.is_empty(), "Exhausted retries are not restarted every periodic scan");
    reconcile_project(&root, &mut engine, &mut pending, true).await?;
    assert_eq!(pending.len(), 1, "Restart resumes unfinished extraction");
    std::fs::remove_file(&path)?;
    reconcile_project(&root, &mut engine, &mut pending, false).await?;
    assert!(engine.get_source_content(&path.to_string_lossy())?.is_none());
    std::fs::remove_dir_all(&root)?;
    Ok(())
}

#[test]
fn save_event_hides_old_facts_during_debounce() -> anyhow::Result<()> {
    let mut engine = engine::RemainderEngine::new(":memory:")?;
    engine.refresh_source_facts("db.md", Some("Old"), vec![("DB".into(), "Old".into())])?;
    handle_watch_event(WatchEvent::Modify(PathBuf::from("db.md")), &mut engine,
        &mut HashMap::new(), &mut HashSet::new(), Duration::from_secs(3))?;
    assert!(engine.get_facts_by_tag("DB", None)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn watch_path_argument_isolates_scan_to_requested_directory() -> anyhow::Result<()> {
    let base = std::env::temp_dir().join(format!("remainder_watch_iso_{}", std::process::id()));
    let dir_a = base.join("project_a");
    let dir_b = base.join("project_b");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&dir_a)?;
    std::fs::create_dir_all(&dir_b)?;

    let file_a = dir_a.join("service_a.rs");
    let file_b = dir_b.join("service_b.rs");
    std::fs::write(&file_a, "pub fn service_a() {}")?;
    std::fs::write(&file_b, "pub fn service_b() {}")?;

    let mut engine = engine::RemainderEngine::new(":memory:")?;
    let mut pending = HashMap::new();

    // Reconcile targeting requested dir_a specifically (mimicking `remainder watch <dir_a>`)
    reconcile_project(&dir_a, &mut engine, &mut pending, true).await?;

    // Only dir_a files should be scanned and staged
    assert!(pending.contains_key(&file_a));
    assert!(!pending.contains_key(&file_b));
    assert!(engine.source_state(&file_a.to_string_lossy())?.is_some());
    assert!(engine.source_state(&file_b.to_string_lossy())?.is_none());

    let collected = collect_project_files(&dir_a);
    assert_eq!(collected, vec![file_a]);
    let _ = std::fs::remove_dir_all(&base);
    Ok(())
}

#[test]
fn repair_detects_and_fixes_injected_foreign_key_violation() -> anyhow::Result<()> {
    let mut engine = engine::RemainderEngine::new(":memory:")?;
    engine.conn.execute("PRAGMA foreign_keys = OFF", [])?;
    engine.conn.execute(
        "INSERT INTO atomics (source_id, vault_tag, fact_content, hash) VALUES (777777, 'ORPHAN_TAG', 'Orphaned fact content', 'orphan_hash')",
        [],
    )?;
    engine.conn.execute("PRAGMA foreign_keys = ON", [])?;

    let report = engine.repair_bunker()?;
    // Must accurately reflect the violation and the repair
    assert_eq!(report.fk_violations_found, 1);
    assert_eq!(report.fk_violations_repaired, 1);
    assert_eq!(report.fk_violations_remaining, 0);
    assert!(!report.is_pristine(), "Must not claim pristine when violations existed");
    assert!(report.is_clean_after_repair(), "Must be clean after repair");
    assert!(report.messages.iter().any(|m| m.contains("Foreign key constraint violations detected: 1")));
    assert!(report.messages.iter().any(|m| m.contains("Repaired relational constraints: purged 1 orphaned atomic facts")));

    // Confirm that the orphaned row was pruned and no longer exists
    let remaining_orphans: i64 = engine.conn.query_row(
        "SELECT count(*) FROM atomics WHERE vault_tag = 'ORPHAN_TAG'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(remaining_orphans, 0);
    Ok(())
}

#[tokio::test]
async fn sweep_indexes_raw_source_and_queues_pending_extractions() -> anyhow::Result<()> {
    let temp = std::env::temp_dir().join(format!("remainder_sweep_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp)?;
    let doc_path = temp.join("arch.md");
    std::fs::write(&doc_path, "System Architecture: Event-driven actor model.")?;

    let mut engine = engine::RemainderEngine::new(":memory:")?;
    let mut pending = HashMap::new();

    reconcile_project(&temp, &mut engine, &mut pending, false).await?;

    // Raw source content is immediately recorded and searchable via FTS5
    assert_eq!(pending.len(), 1);
    assert!(pending.contains_key(&doc_path));
    let search_results = engine.search("Event-driven actor model", None)?;
    assert!(!search_results.is_empty());
    assert_eq!(search_results[0].vault_tag, "SOURCE");

    // Facts are not claimed current until LLM proposition extraction completes
    let state = engine.source_state(&doc_path.to_string_lossy())?;
    assert!(state.is_some());
    let (_, facts_current) = state.unwrap();
    assert!(!facts_current, "Source must not claim facts_current before LLM extraction");
    let _ = std::fs::remove_dir_all(&temp);
    Ok(())
}

#[tokio::test]
async fn guarded_commit_discards_stale_result_on_edit_during_extraction_and_processes_latest() -> anyhow::Result<()> {
    let mut engine = engine::RemainderEngine::new(":memory:")?;
    let mut pending = HashMap::new();
    let mut in_flight = HashSet::new();

    let temp = std::env::temp_dir().join(format!("remainder_race_edit_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp)?;
    let doc_path = temp.join("decision.md");

    // 1. Initial version dispatched to background LLM extraction
    let v1_text = "Use MongoDB for document storage.";
    std::fs::write(&doc_path, v1_text)?;
    let v1_hash = compute_file_hash(v1_text);
    in_flight.insert(doc_path.clone());

    // 2. While inference is running, file is edited on disk to version 2
    let v2_text = "Use PostgreSQL instead for relational integrity.";
    std::fs::write(&doc_path, v2_text)?;

    // 3. Worker finishes LLM inference for version 1
    let v1_shred_res = Ok("[DB] -> Use MongoDB for document storage.".to_string());
    let current_content = tokio::fs::read_to_string(&doc_path).await.ok();

    // 4. Guarded commit evaluates post-inference disk state
    let outcome1 = process_inference_result(
        doc_path.clone(),
        v1_hash,
        v1_shred_res,
        &mut engine,
        &mut pending,
        &mut in_flight,
        1,
        current_content,
    )?;

    // Must discard stale v1 result and schedule v2
    assert_eq!(outcome1, InferenceOutcome::DiscardedStaleChanged);
    assert!(in_flight.is_empty(), "In-flight must be cleared");
    assert!(pending.contains_key(&doc_path), "Latest version must be scheduled for extraction");

    // Stale facts must not be committed to the bunker
    let facts = engine.get_facts_by_tag("DB", None)?;
    assert!(facts.is_empty(), "Stale facts must not be committed");

    // Raw source content must be updated to v2 and searchable
    let raw = engine.get_source_content(&doc_path.to_string_lossy())?;
    assert_eq!(raw.as_deref(), Some(v2_text));

    // 5. Extraction processes the latest version (v2)
    let v2_hash = compute_file_hash(v2_text);
    in_flight.insert(doc_path.clone());
    let v2_shred_res = Ok("[DB] -> Use PostgreSQL instead for relational integrity.".to_string());
    let current_content_v2 = tokio::fs::read_to_string(&doc_path).await.ok();

    let outcome2 = process_inference_result(
        doc_path.clone(),
        v2_hash,
        v2_shred_res,
        &mut engine,
        &mut pending,
        &mut in_flight,
        1,
        current_content_v2,
    )?;

    assert_eq!(outcome2, InferenceOutcome::Committed(1));
    assert!(pending.is_empty());
    assert_eq!(engine.get_facts_by_tag("DB", None)?, vec!["Use PostgreSQL instead for relational integrity."]);

    let _ = std::fs::remove_dir_all(&temp);
    Ok(())
}

#[tokio::test]
async fn guarded_commit_discards_stale_result_on_file_deletion_during_extraction() -> anyhow::Result<()> {
    let mut engine = engine::RemainderEngine::new(":memory:")?;
    let mut pending = HashMap::new();
    let mut in_flight = HashSet::new();

    let temp = std::env::temp_dir().join(format!("remainder_race_del_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp)?;
    let doc_path = temp.join("deleted_secret.md");

    let text = "Confidential API token: sk-live-99999";
    std::fs::write(&doc_path, text)?;
    let hash = compute_file_hash(text);
    engine.record_source_content(&doc_path.to_string_lossy(), text)?;
    in_flight.insert(doc_path.clone());

    // File deleted on disk during LLM inference
    std::fs::remove_file(&doc_path)?;

    let shred_res = Ok("[SECRET] -> Confidential API token: sk-live-99999".to_string());
    let current_content = tokio::fs::read_to_string(&doc_path).await.ok();
    assert!(current_content.is_none());

    let outcome = process_inference_result(
        doc_path.clone(),
        hash,
        shred_res,
        &mut engine,
        &mut pending,
        &mut in_flight,
        1,
        current_content,
    )?;

    // Must discard result and purge source from bunker
    assert_eq!(outcome, InferenceOutcome::DiscardedStaleDeleted);
    assert!(!pending.contains_key(&doc_path));
    assert!(in_flight.is_empty());

    // No traces remaining in bunker
    assert!(engine.get_source_content(&doc_path.to_string_lossy())?.is_none());
    assert!(engine.search("sk-live-99999", None)?.is_empty());

    let _ = std::fs::remove_dir_all(&temp);
    Ok(())
}

#[tokio::test]
async fn guarded_commit_handles_file_rename_during_extraction() -> anyhow::Result<()> {
    let mut engine = engine::RemainderEngine::new(":memory:")?;
    let mut pending = HashMap::new();
    let mut in_flight = HashSet::new();

    let temp = std::env::temp_dir().join(format!("remainder_race_rename_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp)?;
    let old_path = temp.join("old_rpc.md");
    let new_path = temp.join("new_rpc.md");

    let text = "Use gRPC for inter-service RPC.";
    std::fs::write(&old_path, text)?;
    let old_hash = compute_file_hash(text);
    engine.record_source_content(&old_path.to_string_lossy(), text)?;
    in_flight.insert(old_path.clone());

    // File renamed during extraction
    std::fs::rename(&old_path, &new_path)?;

    // Watcher receives rename event
    handle_watch_event(
        WatchEvent::Rename { from: old_path.clone(), to: new_path.clone() },
        &mut engine,
        &mut pending,
        &mut in_flight,
        Duration::ZERO,
    )?;

    // LLM inference for old_path completes
    let old_shred_res = Ok("[PROTOCOL] -> Use gRPC for inter-service RPC.".to_string());
    let current_content_old = tokio::fs::read_to_string(&old_path).await.ok();
    assert!(current_content_old.is_none());

    let outcome_old = process_inference_result(
        old_path.clone(),
        old_hash,
        old_shred_res,
        &mut engine,
        &mut pending,
        &mut in_flight,
        1,
        current_content_old,
    )?;

    assert_eq!(outcome_old, InferenceOutcome::DiscardedStaleDeleted);
    assert!(engine.get_source_content(&old_path.to_string_lossy())?.is_none());

    // New path is queued in pending and extracts successfully
    assert!(pending.contains_key(&new_path));
    let new_hash = compute_file_hash(text);
    let new_shred_res = Ok("[PROTOCOL] -> Use gRPC for inter-service RPC.".to_string());
    let current_content_new = tokio::fs::read_to_string(&new_path).await.ok();

    let outcome_new = process_inference_result(
        new_path.clone(),
        new_hash,
        new_shred_res,
        &mut engine,
        &mut pending,
        &mut in_flight,
        1,
        current_content_new,
    )?;

    assert_eq!(outcome_new, InferenceOutcome::Committed(1));
    let hits = engine.search("gRPC", None)?;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].file_path, new_path.to_string_lossy());

    let _ = std::fs::remove_dir_all(&temp);
    Ok(())
}

#[tokio::test]
async fn sweep_extraction_recovers_from_concurrent_edit() -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;

    let temp = std::env::temp_dir().join(format!("remainder_sweep_race_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp)?;
    let doc_path = temp.join("config.md");
    std::fs::write(&doc_path, "Use Redis cluster.")?;

    let doc_for_server = doc_path.clone();
    let server = tokio::spawn(async move {
        // First request: receives "Use Redis cluster.", mutates file on disk, then returns Redis response
        if let Ok((mut stream, _)) = listener.accept().await {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await;
            // Mutate file on disk during inference!
            let _ = std::fs::write(&doc_for_server, "Use Dragonfly cluster instead.");
            let body = serde_json::json!({ "response": "[CACHE] -> Use Redis cluster." }).to_string();
            let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
            let _ = stream.write_all(resp.as_bytes()).await;
            let _ = stream.shutdown().await;
        }

        // Second request: receives "Use Dragonfly cluster instead.", returns Dragonfly response
        if let Ok((mut stream, _)) = listener.accept().await {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await;
            let body = serde_json::json!({ "response": "[CACHE] -> Use Dragonfly cluster." }).to_string();
            let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
            let _ = stream.write_all(resp.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });

    let shredder = shredder::Shredder::with_base_url("test-model", &format!("http://{}", address));
    let mut engine = engine::RemainderEngine::new(":memory:")?;

    let result = run_sweep_extraction(&mut engine, &shredder, vec![doc_path.clone()], 1).await?;
    server.abort();

    assert_eq!(result.failed_count, 0);
    assert_eq!(result.success_count, 1);
    assert_eq!(result.stale_discarded_count, 1, "Must have detected and discarded stale result");
    assert_eq!(result.committed_facts, 1);

    // Bunker must have committed the new Dragonfly fact, never the stale Redis fact
    assert_eq!(engine.get_facts_by_tag("CACHE", None)?, vec!["Use Dragonfly cluster."]);
    assert_eq!(engine.get_source_content(&doc_path.to_string_lossy())?.as_deref(), Some("Use Dragonfly cluster instead."));

    let _ = std::fs::remove_dir_all(&temp);
    Ok(())
}


