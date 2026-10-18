#[path = "../shredder.rs"]
mod shredder;

use std::time::Instant;

struct BenchmarkCase {
    name: &'static str,
    dimension: &'static str,
    input: &'static str,
    required_keywords: &'static [&'static [&'static str]], // Any keyword in sub-slice must match
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("================================================================================");
    println!("           REMAINDER EXTRACTION QUALITY BENCHMARK HARNESS (Llama 3.1 8B)        ");
    println!("================================================================================");

    let shredder = shredder::Shredder::new("llama3.1:8b");

    let test_cases = vec![
        BenchmarkCase {
            name: "Scope Isolation",
            dimension: "Scope",
            input: "Globally across all services, we use UTC timestamps for event emission. \
                    Within the billing microservice only, monetary calculations truncate to 4 decimal places.",
            required_keywords: &[
                &["utc", "timestamp"],
                &["billing", "4 decimal"],
            ],
        },
        BenchmarkCase {
            name: "Negation & Rejection",
            dimension: "Negation",
            input: "We evaluated MongoDB but explicitly rejected it due to lack of multi-document ACID transactions. \
                    We do NOT use document stores in production; all persistence is PostgreSQL.",
            required_keywords: &[
                &["reject", "not use", "no document", "mongodb"],
                &["postgresql", "postgres"],
            ],
        },
        BenchmarkCase {
            name: "Technical Rationale",
            dimension: "Rationale",
            input: "We migrated inter-service communication from JSON-RPC to gRPC because protocol buffers \
                    reduce payload serialization overhead by 68% and enforce strict typed contracts.",
            required_keywords: &[
                &["grpc"],
                &["68%", "serialization", "overhead", "typed", "contracts", "protocol buffers"],
            ],
        },
        BenchmarkCase {
            name: "Multiple Decisions",
            dimension: "Multi-Decision",
            input: "Architecture decisions for Q4: \
                    1. Enforce TLS 1.3 only for external traffic. \
                    2. Use ChaCha20-Poly1305 for devices lacking AES hardware acceleration. \
                    3. Mandate structured JSON logs with correlation IDs. \
                    4. Run all worker queues on NATS JetStream.",
            required_keywords: &[
                &["tls 1.3", "tls"],
                &["chacha20", "aes"],
                &["json", "correlation"],
                &["nats", "jetstream"],
            ],
        },
    ];

    let mut total_score = 0;
    let max_score = test_cases.len() * 100;

    for (i, tc) in test_cases.iter().enumerate() {
        println!("\n[{}/{}] Evaluating Dimension: {} ('{}')", i + 1, test_cases.len(), tc.dimension, tc.name);
        println!("Input: \"{}\"", tc.input);

        let t0 = Instant::now();
        let extraction = match shredder.extract_atomic(tc.input).await {
            Ok(res) => res,
            Err(e) => {
                println!("  [FAIL] Extraction error: {}", e);
                continue;
            }
        };
        let elapsed = t0.elapsed();

        println!("Output extracted in {:?}:", elapsed);
        let lines: Vec<&str> = extraction.lines().filter(|l| !l.trim().is_empty()).collect();
        for line in &lines {
            println!("    {}", line);
        }

        // Score based on keyword group matches
        let lower_output = extraction.to_lowercase();
        let mut matched_groups = 0;
        for group in tc.required_keywords {
            let matched = group.iter().any(|&kw| lower_output.contains(kw));
            if matched {
                matched_groups += 1;
            }
        }

        let case_score = (matched_groups * 100) / tc.required_keywords.len();
        total_score += case_score;

        println!("Dimension Score: {}/100 (Matched {}/{} critical semantic constraints)", 
                 case_score, matched_groups, tc.required_keywords.len());
    }

    let final_pct = (total_score * 100) / max_score;
    println!("\n================================================================================");
    println!("BENCHMARK SUMMARY: Overall Quality Score = {}% ({}/{})", final_pct, total_score, max_score);
    println!("================================================================================");

    Ok(())
}
