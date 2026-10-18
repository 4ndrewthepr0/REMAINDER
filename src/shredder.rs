use reqwest::Client;
use serde_json::json;

#[derive(Clone)]
pub struct Shredder {
    client: Client,
    model: String,
}

impl Shredder {
    pub fn new(model: &str) -> Self {
        Self {
            client: Client::new(),
            model: model.to_string(),
        }
    }

    /// Takes raw text and returns a distilled 'Atomic Fact' via Local LLM
    pub async fn extract_atomic(&self, input: &str) -> anyhow::Result<String> {
        let prompt = format!(
            "### Instruction: \
            Distill the following input into a single, dense technical fact or decision. \
            Format: [CATEGORY_TAG] -> DATA. No conversational fluff, no explanations. \
            \nInput: {}\n\n### Response:", 
            input
        );

        let resp = self.client.post("http://localhost:11434/api/generate")
            .json(&json!({
                "model": self.model,
                "prompt": prompt,
                "stream": false
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
