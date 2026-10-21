use reqwest::Client;
use serde_json::json;
use std::time::Duration;

#[derive(Clone)]
pub struct Shredder {
    client: Client,
    model: String,
    base_url: String,
}

impl Shredder {
    #[allow(dead_code)]
    pub fn new(model: &str) -> Self {
        Self::with_base_url(model, "http://localhost:11434")
    }

    pub fn with_base_url(model: &str, base_url: &str) -> Self {
        Self::with_timeout(model, base_url, Duration::from_secs(120))
    }

    pub fn with_timeout(model: &str, base_url: &str, timeout: Duration) -> Self {
        Self {
            client: Client::builder().connect_timeout(Duration::from_secs(5))
                .timeout(timeout).build().expect("HTTP client configuration"),
            model: model.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// Takes raw text and extracts all distilled 'Atomic Facts' via Local LLM
    pub async fn extract_atomic(&self, input: &str) -> anyhow::Result<String> {
        let prompt = format!(
            "You are a strict data extraction engine for a developer knowledge vault.\n\
            Analyze the following document and extract all distinct technical facts, architectural decisions, and operational constraints.\n\
            Output each fact on its own line in the exact format:\n\
            [TAG] -> Complete standalone proposition sentence.\n\n\
            Examples:\n\
            [DATABASE] -> The production database is PostgreSQL 16.\n\
            [CACHE] -> The session cache is Dragonfly with a 20-minute expiration.\n\
            [BACKUP] -> Restore drills run every month against an isolated environment.\n\
            [ROLLBACK] -> The rollback authorization codeword is MARIGOLDANCHOR.\n\n\
            Rules:\n\
            1. Output ONLY lines matching: [TAG] -> PROPOSITION\n\
            2. Do NOT include markdown code blocks, numbers, bullet markers, introductions, or pleasantries.\n\
            3. Every fact must be a complete, self-contained statement.\n\
            4. Preserve exact version numbers, codewords, durations, and metrics verbatim.\n\n\
            Input:\n{}\n\n\
            Propositions:\n", 
            input
        );

        let url = format!("{}/api/generate", self.base_url);
        let resp = self.client.post(&url)
            .json(&json!({
                "model": self.model,
                "prompt": prompt,
                "stream": false,
                "options": {
                    "temperature": 0.0
                }
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            anyhow::bail!("Ollama HTTP request failed with status: {}", resp.status());
        }

        let res = resp.json::<serde_json::Value>().await?;

        let response_text = res
            .get("response")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Ollama response missing 'response' field: {:?}", res))?
            .trim();

        if response_text.is_empty() {
            anyhow::bail!("Ollama returned empty response");
        }

        Ok(response_text.to_string())
    }
}
