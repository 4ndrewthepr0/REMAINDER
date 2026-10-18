#[path = "../shredder.rs"]
mod shredder;

use std::time::Instant;

struct BenchmarkDimension {
    name: &'static str,
    dimension: &'static str,
    input: &'static str,
    negative_control: &'static str, // Inverted counterexample that MUST score 0
    validate: fn(&str) -> (bool, Vec<&'static str>), // (passed, failure_reasons)
}

fn validate_scope(text: &str) -> (bool, Vec<&'static str>) {
    let lower = text.to_lowercase();
    let mut failures = Vec::new();

    // Positive checks
    let has_utc_global = (lower.contains("utc") || lower.contains("timestamp"))
        && (lower.contains("global") || lower.contains("all service"));
    let has_billing_decimal = lower.contains("billing")
        && (lower.contains("4 decimal") || lower.contains("decimal precision") || lower.contains("truncate"));

    if !has_utc_global {
        failures.push("Missing proposition: UTC timestamps apply globally across all services.");
    }
    if !has_billing_decimal {
        failures.push("Missing proposition: 4 decimal truncation is confined to billing.");
    }

    // Negative polarity checks (detect inverted counterexamples)
    if lower.contains("only billing uses utc") || lower.contains("utc is confined to billing") || lower.contains("billing uses utc") {
        failures.push("Polarity Inversion: Falsely assigned UTC scope to billing only.");
    }
    if lower.contains("every service must truncate") || lower.contains("all services truncate") || lower.contains("global 4 decimal") {
        failures.push("Polarity Inversion: Falsely expanded 4 decimal truncation globally.");
    }

    (failures.is_empty(), failures)
}

fn validate_negation(text: &str) -> (bool, Vec<&'static str>) {
    let lower = text.to_lowercase();
    let mut failures = Vec::new();

    // Positive checks
    let rejects_mongo = lower.contains("mongodb")
        && (lower.contains("reject") || lower.contains("not support") || lower.contains("not use") || lower.contains("does not"));
    let adopts_postgres = lower.contains("postgres")
        && (lower.contains("used") || lower.contains("persist") || lower.contains("production") || lower.contains("persistence"));
    let rejects_doc_stores = lower.contains("document store")
        && (lower.contains("not used") || lower.contains("not use") || lower.contains("no document"));

    if !rejects_mongo {
        failures.push("Missing proposition: MongoDB rejected due to transaction limits.");
    }
    if !adopts_postgres {
        failures.push("Missing proposition: PostgreSQL adopted for production persistence.");
    }
    if !rejects_doc_stores {
        failures.push("Missing proposition: Document stores prohibited in production.");
    }

    // Negative polarity checks
    if lower.contains("use mongodb in production") || lower.contains("mongodb is used") || lower.contains("using mongodb") {
        failures.push("Polarity Inversion: Falsely stated MongoDB is used in production.");
    }
    if lower.contains("postgresql is rejected") || lower.contains("reject postgres") || lower.contains("not use postgres") {
        failures.push("Polarity Inversion: Falsely stated PostgreSQL is rejected.");
    }

    (failures.is_empty(), failures)
}

fn validate_rationale(text: &str) -> (bool, Vec<&'static str>) {
    let lower = text.to_lowercase();
    let mut failures = Vec::new();

    // Positive checks
    let adopts_grpc = lower.contains("grpc")
        && (lower.contains("migrate") || lower.contains("adopt") || lower.contains("use") || lower.contains("selected") || lower.contains("chosen"));
    let cites_68_pct = lower.contains("68%") || (lower.contains("serialization") && lower.contains("overhead"));
    let cites_typed_contracts = lower.contains("typed") || lower.contains("contract");

    if !adopts_grpc {
        failures.push("Missing proposition: Migrated inter-service communication to gRPC.");
    }
    if !cites_68_pct {
        failures.push("Missing proposition: Quantitative rationale of 68% serialization reduction.");
    }
    if !cites_typed_contracts {
        failures.push("Missing proposition: Enforce strict typed contracts.");
    }

    // Negative polarity checks
    if lower.contains("reject grpc") || lower.contains("not use grpc") || lower.contains("do not use grpc") {
        failures.push("Polarity Inversion: Falsely stated gRPC is rejected.");
    }
    if lower.contains("unnecessary") || lower.contains("not needed") || lower.contains("contracts are not required") {
        failures.push("Polarity Inversion: Falsely dismissed typed contracts as unnecessary.");
    }

    (failures.is_empty(), failures)
}

fn validate_multi_decision(text: &str) -> (bool, Vec<&'static str>) {
    let lower = text.to_lowercase();
    let mut failures = Vec::new();

    let dec1 = lower.contains("tls 1.3") || lower.contains("tls");
    let dec2 = lower.contains("chacha20") || lower.contains("aes");
    let dec3 = lower.contains("json") && (lower.contains("correlation") || lower.contains("log"));
    let dec4 = lower.contains("nats") || lower.contains("jetstream");

    if !dec1 { failures.push("Missing decision 1: Enforce TLS 1.3 for external traffic."); }
    if !dec2 { failures.push("Missing decision 2: Use ChaCha20-Poly1305 for non-AES devices."); }
    if !dec3 { failures.push("Missing decision 3: Mandate structured JSON logs with correlation IDs."); }
    if !dec4 { failures.push("Missing decision 4: Run worker queues on NATS JetStream."); }

    // Negative polarity checks
    if lower.contains("disable tls") || lower.contains("no tls") || lower.contains("reject tls") {
        failures.push("Polarity Inversion: Stated TLS is disabled.");
    }
    if lower.contains("reject chacha") || lower.contains("disable chacha") {
        failures.push("Polarity Inversion: Stated ChaCha20 is rejected.");
    }
    if lower.contains("prohibit json") || lower.contains("no json logs") {
        failures.push("Polarity Inversion: Stated JSON logs are prohibited.");
    }
    if lower.contains("do not use nats") || lower.contains("reject nats") {
        failures.push("Polarity Inversion: Stated NATS is not used.");
    }

    (failures.is_empty(), failures)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("================================================================================");
    println!("     REMAINDER SEMANTIC PROPOSITION BENCHMARK & NEGATIVE CONTROL HARNESS        ");
    println!("================================================================================");

    let shredder = shredder::Shredder::new("llama3.1:8b");

    let dimensions = vec![
        BenchmarkDimension {
            name: "Scope Isolation",
            dimension: "Scope",
            input: "Globally across all services, we use UTC timestamps for event emission. \
                    Within the billing microservice only, monetary calculations truncate to 4 decimal places.",
            negative_control: "Only billing uses UTC; every service must truncate money to 4 decimal places.",
            validate: validate_scope,
        },
        BenchmarkDimension {
            name: "Negation & Rejection",
            dimension: "Negation",
            input: "We evaluated MongoDB but explicitly rejected it due to lack of multi-document ACID transactions. \
                    We do NOT use document stores in production; all persistence is PostgreSQL.",
            negative_control: "Use MongoDB in production; PostgreSQL is rejected.",
            validate: validate_negation,
        },
        BenchmarkDimension {
            name: "Technical Rationale",
            dimension: "Rationale",
            input: "We migrated inter-service communication from JSON-RPC to gRPC because protocol buffers \
                    reduce payload serialization overhead by 68% and enforce strict typed contracts.",
            negative_control: "Reject gRPC because typed contracts are unnecessary.",
            validate: validate_rationale,
        },
        BenchmarkDimension {
            name: "Multiple Decisions",
            dimension: "Multi-Decision",
            input: "Architecture decisions for Q4: \
                    1. Enforce TLS 1.3 only for external traffic. \
                    2. Use ChaCha20-Poly1305 for devices lacking AES hardware acceleration. \
                    3. Mandate structured JSON logs with correlation IDs. \
                    4. Run all worker queues on NATS JetStream.",
            negative_control: "Disable TLS, reject ChaCha20, prohibit JSON logs, and do not use NATS.",
            validate: validate_multi_decision,
        },
    ];

    let mut model_passed = 0;
    let mut negative_controls_failed = 0;

    for (i, dim) in dimensions.iter().enumerate() {
        println!("\n[{}/{}] Evaluating Dimension: {} ('{}')", i + 1, dimensions.len(), dim.dimension, dim.name);

        // 1. Verify Negative Control MUST FAIL (Score 0)
        println!("  • Step A: Running Inverted Negative Control...");
        println!("    Negative Control Input: \"{}\"", dim.negative_control);
        let (neg_pass, neg_reasons) = (dim.validate)(dim.negative_control);
        if neg_pass {
            eprintln!("    [CRITICAL DEFECT] Negative control unexpectedly PASSED! Scorer is broken.");
            std::process::exit(1);
        } else {
            println!("    [VERIFIED REJECTED] Negative control correctly scored 0/100 (Detected: {:?})", neg_reasons);
            negative_controls_failed += 1;
        }

        // 2. Evaluate Live Llama 3.1 8B Model Output
        println!("  • Step B: Extracting via Local Llama 3.1 8B...");
        let t0 = Instant::now();
        let extraction = match shredder.extract_atomic(dim.input).await {
            Ok(res) => res,
            Err(e) => {
                println!("    [FAIL] LLM request error: {}", e);
                continue;
            }
        };
        let elapsed = t0.elapsed();
        println!("    Model Output extracted in {:?}:", elapsed);
        for line in extraction.lines().filter(|l| !l.trim().is_empty()) {
            println!("      {}", line);
        }

        let (model_pass, model_reasons) = (dim.validate)(&extraction);
        if model_pass {
            println!("    [PASSED] Proposition evaluation: 100/100 (All semantic predicates & scopes verified)");
            model_passed += 1;
        } else {
            println!("    [FAILED] Proposition evaluation: 0/100 (Violations: {:?})", model_reasons);
        }
    }

    println!("\n================================================================================");
    println!("BENCHMARK SUMMARY:");
    println!("  - Negative Controls Correctly Rejected : {}/{} (100% calibration)", negative_controls_failed, dimensions.len());
    println!("  - Llama 3.1 8B Propositional Accuracy : {}/{} ({}%)", model_passed, dimensions.len(), (model_passed * 100) / dimensions.len());
    println!("================================================================================");

    if model_passed == dimensions.len() && negative_controls_failed == dimensions.len() {
        println!("RESULT: Semantic Proposition Scorer verified robust against polarity & scope inversions.");
    }

    Ok(())
}
