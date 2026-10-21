use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use tract_onnx::prelude::*;
use tokenizers::Tokenizer;

pub type RunnableModelType = std::sync::Arc<TypedRunnableModel>;

pub struct Embedder {
    plan: RunnableModelType,
    tokenizer: Tokenizer,
    pub dimension: usize,
}

impl Embedder {
    pub fn try_new() -> Result<Self> {
        let (model_path, tokenizer_path) = Self::find_model_files()?;
        Self::new_from_paths(&model_path, &tokenizer_path)
    }

    pub fn new_from_paths(model_path: &Path, tokenizer_path: &Path) -> Result<Self> {
        let tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| anyhow::anyhow!("Failed to load tokenizer: {}", e))?;

        let model = tract_onnx::onnx()
            .model_for_path(model_path)
            .context("Failed to load ONNX model")?
            .into_optimized()
            .context("Failed to optimize ONNX model")?
            .into_runnable()
            .context("Failed to build runnable ONNX model")?;

        Ok(Self {
            plan: model,
            tokenizer,
            dimension: 384,
        })
    }

    pub fn find_model_files() -> Result<(PathBuf, PathBuf)> {
        if let Ok(dir) = std::env::var("REMAINDER_MODEL_DIR") {
            let p = PathBuf::from(dir);
            if p.join("model.onnx").exists() && p.join("tokenizer.json").exists() {
                return Ok((p.join("model.onnx"), p.join("tokenizer.json")));
            }
        }

        // Standard chroma cache location
        if let Ok(profile) = std::env::var("USERPROFILE") {
            let p = PathBuf::from(profile)
                .join(".cache")
                .join("chroma")
                .join("onnx_models")
                .join("all-MiniLM-L6-v2")
                .join("onnx");
            if p.join("model.onnx").exists() && p.join("tokenizer.json").exists() {
                return Ok((p.join("model.onnx"), p.join("tokenizer.json")));
            }
        }

        // Linux/macOS standard cache
        if let Ok(home) = std::env::var("HOME") {
            let p = PathBuf::from(home)
                .join(".cache")
                .join("chroma")
                .join("onnx_models")
                .join("all-MiniLM-L6-v2")
                .join("onnx");
            if p.join("model.onnx").exists() && p.join("tokenizer.json").exists() {
                return Ok((p.join("model.onnx"), p.join("tokenizer.json")));
            }
        }

        anyhow::bail!("MiniLM ONNX model files not found. Place model.onnx and tokenizer.json in ~/.cache/chroma/onnx_models/all-MiniLM-L6-v2/onnx/")
    }

    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let encoding = self.tokenizer.encode(text, true)
            .map_err(|e| anyhow::anyhow!("Tokenization failed: {}", e))?;

        let ids: Vec<i64> = encoding.get_ids().iter().map(|&id| id as i64).collect();
        let mask: Vec<i64> = encoding.get_attention_mask().iter().map(|&m| m as i64).collect();
        let type_ids: Vec<i64> = encoding.get_type_ids().iter().map(|&t| t as i64).collect();
        let seq_len = ids.len();

        let input_ids = tract_ndarray::Array2::from_shape_vec((1, seq_len), ids)?
            .into_tensor();
        let attention_mask = tract_ndarray::Array2::from_shape_vec((1, seq_len), mask.clone())?
            .into_tensor();
        let token_type_ids = tract_ndarray::Array2::from_shape_vec((1, seq_len), type_ids)?
            .into_tensor();

        // Run inference
        let outputs = self.plan.run(tvec!(input_ids.into(), attention_mask.into(), token_type_ids.into()))
            .context("ONNX inference failed")?;

        // outputs[0] is [1, seq_len, 384]
        let tensor = &outputs[0];
        let shape = tensor.shape();
        let hidden_dim = shape[2];

        let view = tensor.to_plain_array_view::<f32>()?;
        let data = view.as_slice().context("Non-contiguous tensor slice")?;

        // Mean pooling over attention mask
        let mut pooled = vec![0.0f32; hidden_dim];
        let mut mask_sum = 0.0f32;

        for i in 0..seq_len {
            if mask[i] > 0 {
                mask_sum += 1.0;
                for d in 0..hidden_dim {
                    pooled[d] += data[i * hidden_dim + d];
                }
            }
        }

        if mask_sum > 0.0 {
            for d in 0..hidden_dim {
                pooled[d] /= mask_sum;
            }
        }

        // L2 unit normalization
        let norm: f32 = pooled.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 1e-12 {
            for x in &mut pooled {
                *x /= norm;
            }
        }

        Ok(pooled)
    }
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_native_embedder_dimension_and_cosine_similarity() {
        if let Ok(embedder) = Embedder::try_new() {
            assert_eq!(embedder.dimension, 384);
            let v1 = embedder.embed("We use PostgreSQL 16 for database storage.").expect("v1 embedding");
            let v2 = embedder.embed("Relational data is stored in Postgres.").expect("v2 embedding");
            let v3 = embedder.embed("Bananas and tropical fruit cultivation.").expect("v3 embedding");

            assert_eq!(v1.len(), 384);
            assert_eq!(v2.len(), 384);
            assert_eq!(v3.len(), 384);

            let norm1: f32 = v1.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((norm1 - 1.0).abs() < 1e-4, "Must be unit normalized");

            let sim_sem = cosine_similarity(&v1, &v2);
            let sim_diff = cosine_similarity(&v1, &v3);

            println!("Semantic similarity (PostgreSQL vs Postgres): {}", sim_sem);
            println!("Unrelated similarity (PostgreSQL vs Fruit): {}", sim_diff);

            assert!(sim_sem > 0.50, "Similar concepts must have high cosine similarity: {}", sim_sem);
            assert!(sim_diff < 0.35, "Different concepts must have low cosine similarity: {}", sim_diff);
            assert!(sim_sem > sim_diff + 0.30, "Semantic match must significantly outscore unrelated text");
        }
    }
}

