//! Optional local cross-encoder; model assets and the native runtime are pinned
//! separately from the portable retrieval engine.
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tokenizers::{PaddingParams, Tokenizer, TruncationParams, TruncationStrategy};

pub struct CrossEncoder {
    session: Mutex<ort::session::Session>,
    tokenizer: Tokenizer,
}

impl CrossEncoder {
    pub fn model_name(&self) -> &'static str { "ms-marco-TinyBERT-L2-v2 (12 candidates, 128 pair tokens)" }

    pub fn try_new() -> Result<Self> {
        let directory = if let Some(path) = std::env::var_os("REMAINDER_RERANK_MODEL_DIR") {
            PathBuf::from(path)
        } else {
            PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA missing; set REMAINDER_RERANK_MODEL_DIR")?)
                .join("REMAINDER").join("models").join("ms-marco-TinyBERT-L2-v2")
        };
        Self::from_directory(&directory)
    }

    pub(crate) fn from_directory(directory: &Path) -> Result<Self> {
        let model = directory.join("onnx").join("model_quint8_avx2.onnx");
        let tokenizer_path = directory.join("tokenizer.json");
        for (path, expected) in [
            (&model, "a1ebdfe09dc81382cfabe64ddce946c1e75c9afaef0ec4e8d4f6d8274b23b48c"),
            (&tokenizer_path, "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66"),
        ] {
            let bytes = std::fs::read(path).with_context(|| format!("Reranker asset missing: {}", path.display()))?;
            anyhow::ensure!(hex::encode(Sha256::digest(&bytes)) == expected, "Reranker asset hash mismatch: {}", path.display());
        }
        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|error| anyhow::anyhow!("Reranker tokenizer: {}", error))?;
        tokenizer.with_truncation(Some(TruncationParams {
            max_length: 128, strategy: TruncationStrategy::LongestFirst, ..Default::default()
        })).map_err(|error| anyhow::anyhow!("Reranker truncation: {}", error))?;
        tokenizer.with_padding(Some(PaddingParams { pad_id: 0, pad_token: "[PAD]".into(), ..Default::default() }));
        let session = std::panic::catch_unwind(|| crate::embedder::Embedder::ort_session(&model, "REMAINDER_RERANK_THREADS"))
            .unwrap_or_else(|_| Err(anyhow::anyhow!("Reranker native runtime initialization failed")))?;
        Ok(Self { session: Mutex::new(session), tokenizer })
    }

    pub fn score(&self, query: &str, passages: &[&str]) -> Result<Vec<f32>> {
        self.score_pairs(passages.iter().map(|passage| (query, *passage)).collect())
    }

    pub fn rank(&self, query: &str, passages: &[&str], limit: usize) -> Result<Vec<usize>> {
        Ok(ranked_indices(&self.score(query, passages)?, limit))
    }

    fn score_pairs(&self, pairs: Vec<(&str, &str)>) -> Result<Vec<f32>> {
        if pairs.is_empty() { return Ok(Vec::new()); }
        let count = pairs.len();
        let encodings = self.tokenizer.encode_batch(pairs, true)
            .map_err(|error| anyhow::anyhow!("Reranker pair tokenization: {}", error))?;
        let sequence = encodings[0].len();
        let ids: Vec<i64> = encodings.iter().flat_map(|row| row.get_ids().iter().map(|id| *id as i64)).collect();
        let mask: Vec<i64> = encodings.iter().flat_map(|row| row.get_attention_mask().iter().map(|id| *id as i64)).collect();
        let types: Vec<i64> = encodings.iter().flat_map(|row| row.get_type_ids().iter().map(|id| *id as i64)).collect();
        let shape = [count, sequence];
        let mut session = self.session.lock().map_err(|_| anyhow::anyhow!("Reranker session lock poisoned"))?;
        let outputs = session.run(ort::inputs![
            "input_ids" => ort::value::Tensor::from_array((shape, ids))?,
            "attention_mask" => ort::value::Tensor::from_array((shape, mask))?,
            "token_type_ids" => ort::value::Tensor::from_array((shape, types))?
        ])?;
        let (_, logits) = outputs[0].try_extract_tensor::<f32>()?;
        anyhow::ensure!(logits.len() == count && logits.iter().all(|value| value.is_finite()), "Invalid reranker logits");
        Ok(logits.to_vec())
    }
}

fn ranked_indices(scores: &[f32], limit: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..scores.len()).collect();
    order.sort_by(|left, right| scores[*right].total_cmp(&scores[*left]).then_with(|| left.cmp(right)));
    order.truncate(limit);
    order
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_optional_assets_return_error() {
        assert!(CrossEncoder::from_directory(Path::new("missing-reranker-assets")).is_err());
    }

    #[test]
    fn ranking_limits_and_ties_preserve_native_order() {
        assert_eq!(ranked_indices(&[1.0, 3.0, 3.0, -1.0], 2), vec![1, 2]);
        assert!(ranked_indices(&[1.0], 0).is_empty());
    }

    #[test]
    fn native_pair_logits_match_pinned_python_fixture() -> Result<()> {
        let reranker = match CrossEncoder::try_new() { Ok(model) => model, Err(_) => return Ok(()) };
        let long = "The service stores UTF-8 text in SQLite, including emoji and non-Latin names. ".repeat(20);
        let scores = reranker.score_pairs(vec![
            ("Which endpoint accepts an uploaded image?", "The server handles POST /upload/image with multipart form data containing the image file."),
            ("Which endpoint accepts an uploaded image?", "The application settings page lets the user change the background color and font size."),
            ("How does the storage service preserve Unicode notes?", &long),
        ])?;
        for (actual, expected) in scores.iter().zip([-10.271573f32, -11.472789, -10.981366]) {
            assert!((actual - expected).abs() < 1e-4, "Native pair tokenization/logits differ from pinned Python fixture: {} vs {}", actual, expected);
        }
        Ok(())
    }
}
