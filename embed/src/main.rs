// SPDX-License-Identifier: Apache-2.0

//! Attested local embedding and reranking.
//!
//! `hyphae-embed` runs a local BERT-family model over caller-supplied texts
//! and emits, next to every output, an `AttestedLocal` attestation envelope
//! byte-compatible with the engine's `HYATTS01` format: the BLAKE3 digest of
//! the exact model weights, of the canonical input, and of the canonical
//! little-endian output. The claim is replayable — rerunning the same
//! weights over the same input must reproduce the output digest, and the
//! replay-determinism evidence documents exactly that per attested target.
//!
//! The tool never downloads anything: the operator provides a model
//! directory holding `config.json`, `tokenizer.json`, and
//! `model.safetensors`.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig, DTYPE};
use tokenizers::Tokenizer;

const ATTESTATION_MAGIC: &[u8; 8] = b"HYATTS01";
const MAX_NAME_BYTES: usize = 256;
const MAX_TEXTS: usize = 256;
const MAX_TEXT_BYTES: usize = 64 * 1024;

fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().collect();
    let (command, rest) = match arguments.get(1).map(String::as_str) {
        Some(command @ ("embed" | "rerank")) => (command, &arguments[2..]),
        _ => bail!("usage: hyphae-embed <embed|rerank> --model-dir <DIR> [--query <TEXT>]"),
    };
    let mut model_dir: Option<PathBuf> = None;
    let mut query: Option<String> = None;
    let mut index = 0;
    while index < rest.len() {
        match rest[index].as_str() {
            "--model-dir" => {
                model_dir = Some(PathBuf::from(
                    rest.get(index + 1).context("--model-dir needs a value")?,
                ));
                index += 2;
            }
            "--query" => {
                query = Some(rest.get(index + 1).context("--query needs a value")?.clone());
                index += 2;
            }
            other => bail!("unknown argument: {other}"),
        }
    }
    let model_dir = model_dir.context("--model-dir is required")?;

    // Texts arrive as one JSON array on stdin so inputs are canonical bytes.
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let texts: Vec<String> = serde_json::from_str(&input).context("stdin must be a JSON array")?;
    if texts.is_empty() || texts.len() > MAX_TEXTS {
        bail!("between 1 and {MAX_TEXTS} texts are required");
    }
    if texts.iter().any(|text| text.len() > MAX_TEXT_BYTES) {
        bail!("a text exceeds {MAX_TEXT_BYTES} bytes");
    }

    let model = AttestedModel::load(&model_dir)?;
    match command {
        "embed" => {
            let (vectors, attestation) = model.embed(&texts)?;
            print_output(&serde_json::json!({
                "schema": "hyphae-embed-output-v1",
                "target": model.target,
                "dimensions": vectors.first().map_or(0, Vec::len),
                "vectors": vectors,
                "attestation_hex": hex(&attestation),
            }))
        }
        "rerank" => {
            let query = query.context("--query is required for rerank")?;
            let (scores, attestation) = model.rerank(&query, &texts)?;
            print_output(&serde_json::json!({
                "schema": "hyphae-rerank-output-v1",
                "target": model.target,
                "scores": scores,
                "attestation_hex": hex(&attestation),
            }))
        }
        _ => unreachable!(),
    }
}

fn print_output(value: &serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

struct AttestedModel {
    target: String,
    weights_digest: [u8; 32],
    tokenizer: Tokenizer,
    model: BertModel,
    device: Device,
}

impl AttestedModel {
    fn load(model_dir: &Path) -> Result<Self> {
        let config_bytes = std::fs::read(model_dir.join("config.json"))
            .context("config.json is missing from the model directory")?;
        let weights_path = model_dir.join("model.safetensors");
        let weights_bytes = std::fs::read(&weights_path)
            .context("model.safetensors is missing from the model directory")?;
        let weights_digest = *blake3::hash(&weights_bytes).as_bytes();
        drop(weights_bytes);
        let tokenizer = Tokenizer::from_file(model_dir.join("tokenizer.json"))
            .map_err(|error| anyhow::anyhow!("tokenizer.json failed to load: {error}"))?;
        let config: BertConfig = serde_json::from_slice(&config_bytes)?;
        let target = model_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "model".to_owned());
        if target.is_empty() || target.len() > MAX_NAME_BYTES {
            bail!("model directory name is unbounded");
        }
        // CPU execution keeps the replay-determinism claim host-independent.
        let device = Device::Cpu;
        let builder = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], DTYPE, &device)?
        };
        let model = BertModel::load(builder, &config)?;
        Ok(Self {
            target,
            weights_digest,
            tokenizer,
            model,
            device,
        })
    }

    /// Mean-pooled, L2-normalized sentence embeddings.
    fn embed(&self, texts: &[String]) -> Result<(Vec<Vec<f32>>, Vec<u8>)> {
        let mut vectors = Vec::with_capacity(texts.len());
        for text in texts {
            vectors.push(self.embed_one(text)?);
        }
        let input_digest = canonical_input_digest("embed", &self.target, None, texts);
        let output_digest = vectors_digest(&vectors);
        let attestation = attested_local_envelope(
            &self.target,
            &self.weights_digest,
            &input_digest,
            &output_digest,
        )?;
        Ok((vectors, attestation))
    }

    /// Query relevance scores as the cosine similarity of pooled embeddings.
    fn rerank(&self, query: &str, texts: &[String]) -> Result<(Vec<f32>, Vec<u8>)> {
        let query_vector = self.embed_one(query)?;
        let mut scores = Vec::with_capacity(texts.len());
        for text in texts {
            let vector = self.embed_one(text)?;
            let score: f32 = query_vector
                .iter()
                .zip(&vector)
                .map(|(left, right)| left * right)
                .sum();
            scores.push(score);
        }
        let input_digest = canonical_input_digest("rerank", &self.target, Some(query), texts);
        let mut output_bytes = Vec::with_capacity(scores.len() * 4);
        for score in &scores {
            output_bytes.extend_from_slice(&score.to_le_bytes());
        }
        let output_digest = *blake3::hash(&output_bytes).as_bytes();
        let attestation = attested_local_envelope(
            &self.target,
            &self.weights_digest,
            &input_digest,
            &output_digest,
        )?;
        Ok((scores, attestation))
    }

    fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|error| anyhow::anyhow!("tokenization failed: {error}"))?;
        let ids = encoding.get_ids().to_vec();
        let type_ids = encoding.get_type_ids().to_vec();
        let width = ids.len();
        let ids = Tensor::from_vec(ids, (1, width), &self.device)?;
        let type_ids = Tensor::from_vec(type_ids, (1, width), &self.device)?;
        let hidden = self.model.forward(&ids, &type_ids, None)?;
        // Mean pooling over the sequence, then L2 normalization.
        let pooled = (hidden.sum(1)? / width as f64)?;
        let pooled = pooled.to_dtype(DType::F32)?;
        let norm = pooled.sqr()?.sum_all()?.sqrt()?;
        let normalized = pooled.broadcast_div(&norm)?;
        Ok(normalized.squeeze(0)?.to_vec1::<f32>()?)
    }
}

/// Canonical input framing: operation, target, optional query, and texts as
/// length-framed UTF-8, so the digest never depends on JSON details.
fn canonical_input_digest(
    operation: &str,
    target: &str,
    query: Option<&str>,
    texts: &[String],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-embed-input-v1");
    hasher.update(&(operation.len() as u64).to_le_bytes());
    hasher.update(operation.as_bytes());
    hasher.update(&(target.len() as u64).to_le_bytes());
    hasher.update(target.as_bytes());
    match query {
        None => {
            hasher.update(&[0]);
        }
        Some(query) => {
            hasher.update(&[1]);
            hasher.update(&(query.len() as u64).to_le_bytes());
            hasher.update(query.as_bytes());
        }
    }
    hasher.update(&(texts.len() as u64).to_le_bytes());
    for text in texts {
        hasher.update(&(text.len() as u64).to_le_bytes());
        hasher.update(text.as_bytes());
    }
    *hasher.finalize().as_bytes()
}

fn vectors_digest(vectors: &[Vec<f32>]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-embed-output-v1");
    hasher.update(&(vectors.len() as u64).to_le_bytes());
    for vector in vectors {
        hasher.update(&(vector.len() as u64).to_le_bytes());
        for value in vector {
            hasher.update(&value.to_le_bytes());
        }
    }
    *hasher.finalize().as_bytes()
}

/// The `HYATTS01` `AttestedLocal` envelope, byte-compatible with the engine.
fn attested_local_envelope(
    target: &str,
    weights_digest: &[u8; 32],
    input_digest: &[u8; 32],
    output_digest: &[u8; 32],
) -> Result<Vec<u8>> {
    if target.is_empty() || target.len() > MAX_NAME_BYTES {
        bail!("attestation name is unbounded");
    }
    let mut encoded = Vec::with_capacity(9 + 2 + target.len() + 96);
    encoded.extend_from_slice(ATTESTATION_MAGIC);
    encoded.push(1);
    encoded.extend_from_slice(&(target.len() as u16).to_le_bytes());
    encoded.extend_from_slice(target.as_bytes());
    encoded.extend_from_slice(weights_digest);
    encoded.extend_from_slice(input_digest);
    encoded.extend_from_slice(output_digest);
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attested_local_envelope_matches_the_cross_language_layout() -> Result<()> {
        let weights = *blake3::hash(b"weights").as_bytes();
        let input = *blake3::hash(b"input").as_bytes();
        let output = *blake3::hash(b"output").as_bytes();
        let encoded = attested_local_envelope("bge-small-en-v1.5", &weights, &input, &output)?;
        let mut expected = Vec::new();
        expected.extend_from_slice(b"HYATTS01\x01");
        expected.extend_from_slice(&17_u16.to_le_bytes());
        expected.extend_from_slice(b"bge-small-en-v1.5");
        expected.extend_from_slice(&weights);
        expected.extend_from_slice(&input);
        expected.extend_from_slice(&output);
        assert_eq!(encoded, expected);
        Ok(())
    }

    #[test]
    fn canonical_input_digest_binds_every_component() {
        let texts = vec!["alpha".to_owned(), "beta".to_owned()];
        let base = canonical_input_digest("embed", "target", None, &texts);
        assert_ne!(
            base,
            canonical_input_digest("rerank", "target", None, &texts)
        );
        assert_ne!(base, canonical_input_digest("embed", "other", None, &texts));
        assert_ne!(
            base,
            canonical_input_digest("embed", "target", Some("query"), &texts)
        );
        assert_ne!(
            base,
            canonical_input_digest("embed", "target", None, &texts[..1].to_vec())
        );
    }
}
