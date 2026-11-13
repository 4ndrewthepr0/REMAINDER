use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use tract_onnx::prelude::*;
use tokenizers::Tokenizer;

pub type RunnableModelType = std::sync::Arc<TypedRunnableModel>;

enum Backend {
    Tract(RunnableModelType),
    OnnxRuntime(std::sync::Mutex<ort::session::Session>),
}

pub struct Embedder {
    backend: Backend,
    tokenizer: Tokenizer,
    pub dimension: usize,
}

pub const SOURCE_CHUNK_TOKENS: usize = 224;
pub const SOURCE_CHUNK_OVERLAP: usize = 32;
pub const MINILM_SEQUENCE_LIMIT: usize = 256;

/// Split with the model's real WordPiece offsets, before any tokenizer
/// truncation. Source storage stays verbatim; only searchable passages collapse
/// whitespace. A second untruncated encoding checks the final passage budget.
fn token_chunks(tokenizer: &Tokenizer, text: &str) -> Result<Vec<String>> {
    if text.trim().is_empty() { return Ok(Vec::new()); }
    let mut counter = tokenizer.clone();
    counter.with_truncation(None).map_err(|error| anyhow::anyhow!("Disable source tokenizer truncation: {}", error))?;
    counter.with_padding(None);
    let encoded = counter.encode(text, false)
        .map_err(|error| anyhow::anyhow!("Tokenize source passages: {}", error))?;
    let offsets = encoded.get_offsets();
    if offsets.is_empty() { return Ok(Vec::new()); }
    let mut chunks = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut start = 0;
    while start < offsets.len() {
        let mut end = (start + SOURCE_CHUNK_TOKENS).min(offsets.len());
        // Prefer a paragraph/line boundary in the last 32 tokens, while keeping
        // useful context windows between 192 and 224 content tokens.
        if end < offsets.len() {
            for boundary in ((start + SOURCE_CHUNK_TOKENS - SOURCE_CHUNK_OVERLAP)..=end).rev() {
                if boundary == 0 || boundary >= offsets.len() { continue; }
                let before = offsets[boundary - 1].1;
                let after = offsets[boundary].0;
                if before <= after && text.get(before..after).map_or(false, |gap| gap.contains('\n')) {
                    end = boundary;
                    break;
                }
            }
        }
        let passage = loop {
            let begin_byte = offsets[start].0;
            let end_byte = offsets[end - 1].1;
            let raw = text.get(begin_byte..end_byte)
                .ok_or_else(|| anyhow::anyhow!("Tokenizer returned invalid Unicode source offsets {}..{}", begin_byte, end_byte))?;
            let normalized = raw.split_whitespace().collect::<Vec<_>>().join(" ");
            let checked = counter.encode(normalized.as_str(), true)
                .map_err(|error| anyhow::anyhow!("Verify source passage token budget: {}", error))?;
            if checked.len() <= MINILM_SEQUENCE_LIMIT { break normalized; }
            anyhow::ensure!(end > start + 1, "One source token exceeds the MiniLM sequence budget");
            // Subword boundaries can retokenize differently when sliced out of
            // a larger word. Shrink and recheck rather than silently truncating.
            end -= (checked.len() - MINILM_SEQUENCE_LIMIT).min(end - start - 1).max(1);
        };
        if !passage.is_empty() && seen.insert(passage.clone()) { chunks.push(passage); }
        if end == offsets.len() { break; }
        let next = end.saturating_sub(SOURCE_CHUNK_OVERLAP);
        start = if next > start { next } else { end };
    }
    Ok(chunks)
}

impl Embedder {
    pub fn chunk_source(&self, text: &str) -> Result<Vec<String>> {
        token_chunks(&self.tokenizer, text)
    }

    pub fn try_new() -> Result<Self> {
        let (model_path, tokenizer_path) = Self::find_model_files()?;
        Self::new_from_paths(&model_path, &tokenizer_path)
    }

    pub fn new_from_paths(model_path: &Path, tokenizer_path: &Path) -> Result<Self> {
        let requested = std::env::var("REMAINDER_EMBED_BACKEND").unwrap_or_else(|_| "auto".into());
        Self::new_with_backend(model_path, tokenizer_path, &requested)
    }

    fn new_with_backend(model_path: &Path, tokenizer_path: &Path, requested: &str) -> Result<Self> {
        let tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| anyhow::anyhow!("Failed to load tokenizer: {}", e))?;

        anyhow::ensure!(matches!(requested, "auto" | "ort" | "tract"),
            "REMAINDER_EMBED_BACKEND must be auto, ort, or tract");
        if requested != "tract" {
            let native = std::panic::catch_unwind(|| Self::ort_session(model_path, "REMAINDER_EMBED_THREADS"))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("ONNX Runtime library could not be initialized")));
            match native {
                Ok(session) => return Ok(Self { backend: Backend::OnnxRuntime(std::sync::Mutex::new(session)), tokenizer, dimension: 384 }),
                Err(error) if requested == "ort" => return Err(error.context("Requested ONNX Runtime backend unavailable")),
                Err(_) => {}, // Keep the standalone native Tract fallback.
            }
        }

        let model = tract_onnx::onnx()
            .model_for_path(model_path)
            .context("Failed to load ONNX model")?
            .into_optimized()
            .context("Failed to optimize ONNX model")?
            .into_runnable()
            .context("Failed to build runnable ONNX model")?;

        Ok(Self {
            backend: Backend::Tract(model),
            tokenizer,
            dimension: 384,
        })
    }

    pub fn backend_name(&self) -> &'static str {
        match self.backend { Backend::Tract(_) => "tract", Backend::OnnxRuntime(_) => "onnxruntime" }
    }

    pub(crate) fn ort_session(model_path: &Path, threads_env: &str) -> Result<ort::session::Session> {
        let library_name = if cfg!(target_os = "windows") { "onnxruntime.dll" }
            else if cfg!(target_os = "macos") { "libonnxruntime.dylib" } else { "libonnxruntime.so" };
        let mut paths = Vec::new();
        if let Some(path) = std::env::var_os("REMAINDER_ORT_DYLIB_PATH") {
            let path = PathBuf::from(path);
            anyhow::ensure!(path.is_file(), "Explicit ONNX Runtime library does not exist: {}", path.display());
            paths.push(path);
        } else {
            if let Ok(exe) = std::env::current_exe() {
                if let Some(parent) = exe.parent() { paths.push(parent.join(library_name)); }
            }
            if let Some(local) = std::env::var_os("LOCALAPPDATA") {
                paths.push(PathBuf::from(local).join("REMAINDER").join("runtime").join(library_name));
            }
        }
        let path = paths.into_iter().find(|path| path.is_file())
            .ok_or_else(|| anyhow::anyhow!("ONNX Runtime library not found; set REMAINDER_ORT_DYLIB_PATH or install the native runtime"))?;
        ort::init_from(path.to_string_lossy()).with_name("remainder").commit()?;
        let threads = std::env::var(threads_env).ok().and_then(|v| v.parse::<usize>().ok()).filter(|n| *n > 0).unwrap_or(4);
        Ok(ort::session::Session::builder()?
            .with_intra_threads(threads)?
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)?
            .commit_from_file(model_path)?)
    }

    pub fn find_model_files() -> Result<(PathBuf, PathBuf)> {
        if let Ok(dir) = std::env::var("REMAINDER_MODEL_DIR") {
            let p = PathBuf::from(dir);
            if p.join("model.onnx").exists() && p.join("tokenizer.json").exists() {
                return Ok((p.join("model.onnx"), p.join("tokenizer.json")));
            }
        }

        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let p = PathBuf::from(local).join("REMAINDER").join("models").join("all-MiniLM-L6-v2");
            if p.join("model.onnx").is_file() && p.join("tokenizer.json").is_file() {
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
        Ok(self.embed_batch(&[text])?.remove(0))
    }

    /// Evaluate several passages in one ONNX call. The attention mask keeps
    /// right padding out of mean pooling, so each result matches a single run.
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() { return Ok(Vec::new()); }
        let encodings = texts.iter().map(|text| self.tokenizer.encode(*text, true)
            .map_err(|e| anyhow::anyhow!("Tokenization failed: {}", e)))
            .collect::<Result<Vec<_>>>()?;
        let batch_size = encodings.len();
        let seq_len = encodings.iter().map(|encoding| encoding.len()).max().unwrap_or(0);
        let pad_id = self.tokenizer.get_padding().map(|padding| padding.pad_id as i64).unwrap_or(0);
        let mut ids = vec![pad_id; batch_size * seq_len];
        let mut mask = vec![0i64; batch_size * seq_len];
        let mut type_ids = vec![0i64; batch_size * seq_len];
        for (row, encoding) in encodings.iter().enumerate() {
            let offset = row * seq_len;
            for (column, &id) in encoding.get_ids().iter().enumerate() {
                ids[offset + column] = id as i64;
            }
            for (column, &attention) in encoding.get_attention_mask().iter().enumerate() {
                mask[offset + column] = attention as i64;
            }
            for (column, &kind) in encoding.get_type_ids().iter().enumerate() {
                type_ids[offset + column] = kind as i64;
            }
        }

        let (hidden_dim, data) = match &self.backend {
            Backend::Tract(plan) => {
                let input_ids = tract_ndarray::Array2::from_shape_vec((batch_size, seq_len), ids)?.into_tensor();
                let attention_mask = tract_ndarray::Array2::from_shape_vec((batch_size, seq_len), mask.clone())?.into_tensor();
                let token_type_ids = tract_ndarray::Array2::from_shape_vec((batch_size, seq_len), type_ids)?.into_tensor();
                let outputs = plan.run(tvec!(input_ids.into(), attention_mask.into(), token_type_ids.into()))
                    .context("ONNX batch inference failed")?;
                let tensor = &outputs[0];
                let shape = tensor.shape();
                anyhow::ensure!(shape.len() == 3 && shape[0] == batch_size && shape[1] == seq_len,
                    "Unexpected ONNX output shape: {:?}", shape);
                let view = tensor.to_plain_array_view::<f32>()?;
                (shape[2], view.as_slice().context("Non-contiguous tensor slice")?.to_vec())
            }
            Backend::OnnxRuntime(session) => {
                let mut session = session.lock().map_err(|_| anyhow::anyhow!("ONNX Runtime session lock poisoned"))?;
                let shape = [batch_size, seq_len];
                let outputs = session.run(ort::inputs![
                    "input_ids" => ort::value::Tensor::from_array((shape, ids))?,
                    "attention_mask" => ort::value::Tensor::from_array((shape, mask.clone()))?,
                    "token_type_ids" => ort::value::Tensor::from_array((shape, type_ids))?
                ])?;
                let (shape, data) = outputs[0].try_extract_tensor::<f32>()?;
                anyhow::ensure!(shape.len() == 3 && shape[0] == batch_size as i64 && shape[1] == seq_len as i64,
                    "Unexpected ONNX Runtime output shape: {:?}", shape);
                (shape[2] as usize, data.to_vec())
            }
        };
        let mut result = Vec::with_capacity(batch_size);
        for row in 0..batch_size {
            let mut pooled = vec![0.0f32; hidden_dim];
            let mut mask_sum = 0.0f32;
            for i in 0..seq_len {
                if mask[row * seq_len + i] > 0 {
                    mask_sum += 1.0;
                    let start = (row * seq_len + i) * hidden_dim;
                    for d in 0..hidden_dim {
                        pooled[d] += data[start + d];
                    }
                }
            }
            if mask_sum > 0.0 {
                for d in 0..hidden_dim { pooled[d] /= mask_sum; }
            }
            let norm: f32 = pooled.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 1e-12 {
                for x in &mut pooled { *x /= norm; }
            }
            result.push(pooled);
        }
        Ok(result)
    }
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Score a serialized embedding without allocating a float vector for rows
/// that are unlikely to enter the search shortlist.
pub fn cosine_similarity_bytes(query: &[f32], bytes: &[u8]) -> Option<f32> {
    if bytes.len() != query.len() * std::mem::size_of::<f32>() {
        return None;
    }
    Some(query.iter().zip(bytes.chunks_exact(4)).map(|(left, chunk)| {
        *left * f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
    }).sum())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_chunks_preserve_unicode_coverage_without_token_truncation() -> Result<()> {
        let (_, tokenizer_path) = match Embedder::find_model_files() {
            Ok(paths) => paths,
            Err(_) => return Ok(()),
        };
        let mut tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|error| anyhow::anyhow!("{}", error))?;
        tokenizer.with_truncation(None).map_err(|error| anyhow::anyhow!("{}", error))?;
        tokenizer.with_padding(None);
        let mut raw = "START_SENTINEL: source begins here.\n".to_string();
        for index in 0..120 {
            raw.push_str(&format!("section_{}: {}\n", index,
                "漢字かな变量={\"key\":[1,2,3]};call(arg); ".repeat(8)));
        }
        raw.push_str("FINAL_SENTINEL: source ends here.");
        let old_chunks = crate::engine::chunk_text_for_embedding(&raw);
        assert!(old_chunks.iter().any(|chunk| tokenizer.encode(chunk.as_str(), true).unwrap().len() > MINILM_SEQUENCE_LIMIT),
            "The fixture must exercise a source that character windows silently truncate");
        let chunks = token_chunks(&tokenizer, &raw)?;
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(tokenizer.encode(chunk.as_str(), true).unwrap().len() <= MINILM_SEQUENCE_LIMIT);
        }
        assert!(chunks.iter().any(|chunk| chunk.contains("START_SENTINEL")));
        assert!(chunks.iter().any(|chunk| chunk.contains("FINAL_SENTINEL")));
        for index in 0..120 {
            assert!(chunks.iter().any(|chunk| chunk.contains(&format!("section_{}:", index))),
                "Section {} disappeared across a token boundary", index);
        }
        assert!(chunks.iter().any(|chunk| chunk.contains("漢字かな变量")));
        assert!(token_chunks(&tokenizer, " \n\t ")?.is_empty());
        Ok(())
    }

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

    #[test]
    fn test_batch_embeddings_match_single_passage_embeddings() -> Result<()> {
        let embedder = match Embedder::try_new() {
            Ok(embedder) => embedder,
            Err(_) => return Ok(()),
        };
        let examples = [
            "PostgreSQL stores relational records.",
            "A substantially longer description of the database schema, migration history, and transaction isolation policy for this service.",
            "Vector retrieval uses cosine similarity.",
        ];
        let texts: Vec<&str> = (0..16).map(|index| examples[index % examples.len()]).collect();
        let batched = embedder.embed_batch(&texts)?;
        assert_eq!(batched.len(), texts.len());
        for (text, vector) in texts.iter().zip(batched.iter()) {
            let single = embedder.embed(text)?;
            assert!(cosine_similarity(&single, vector) > 0.99999,
                "Batching changed the semantic embedding for: {}", text);
        }
        Ok(())
    }

    #[test]
    fn serialized_cosine_matches_decoded_vector() {
        let query = [0.4f32, -0.7, 0.2];
        let stored = [0.6f32, 0.5, -0.3];
        let bytes: Vec<u8> = stored.iter().flat_map(|value| value.to_le_bytes()).collect();
        let direct = cosine_similarity_bytes(&query, &bytes).unwrap();
        assert!((direct - cosine_similarity(&query, &stored)).abs() < 1e-6);
        assert_eq!(cosine_similarity_bytes(&query, &bytes[..8]), None);
    }

    #[test]
    fn native_runtime_matches_tract_embeddings() -> Result<()> {
        let (model, tokenizer) = match Embedder::find_model_files() {
            Ok(paths) => paths,
            Err(_) => return Ok(()),
        };
        let native = match Embedder::new_with_backend(&model, &tokenizer, "ort") {
            Ok(embedder) => embedder,
            Err(_) => return Ok(()),
        };
        let tract = Embedder::new_with_backend(&model, &tokenizer, "tract")?;
        let passages = ["PostgreSQL stores relational records.", "A longer project decision about migration history and transaction isolation.", "Vector retrieval uses cosine similarity."];
        let native_vectors = native.embed_batch(&passages)?;
        let tract_vectors = tract.embed_batch(&passages)?;
        for (native, tract) in native_vectors.iter().zip(&tract_vectors) {
            assert!(cosine_similarity(native, tract) > 0.9999, "Runtime change altered MiniLM semantics");
        }
        Ok(())
    }
}

