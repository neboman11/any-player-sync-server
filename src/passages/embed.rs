//! Sentence embeddings for passage retrieval: multilingual-e5-small (BERT architecture, XLM-R
//! tokenizer) via candle on CPU. e5 expects "query: " / "passage: " prefixes, mean pooling over
//! the attention mask, and L2 normalization.

use std::path::Path;

use anyhow::Context;
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

#[allow(dead_code)] // exercised by tests; schema.rs hardcodes vector(384) directly
pub const DIMENSIONS: usize = 384;

#[derive(Clone, Copy)]
pub enum EmbedKind {
    Query,
    Passage,
}

pub trait Embed: Send + Sync {
    /// Returns one L2-normalized DIMENSIONS-long vector per input, in order.
    fn embed(&self, texts: &[String], kind: EmbedKind) -> anyhow::Result<Vec<Vec<f32>>>;
    fn count_tokens(&self, text: &str) -> usize;
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

pub struct BertEmbedder {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

impl BertEmbedder {
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let device = Device::Cpu;
        let config: Config = serde_json::from_str(
            &std::fs::read_to_string(dir.join("config.json")).context("config.json")?,
        )?;
        // SAFETY: the operator-provided model file is read-only and not modified while mapped.
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(
                &[dir.join("model.safetensors")],
                DType::F32,
                &device,
            )?
        };
        let model = BertModel::load(vb, &config)?;
        let mut tokenizer =
            Tokenizer::from_file(dir.join("tokenizer.json")).map_err(anyhow::Error::msg)?;
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            ..Default::default()
        }));
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: 512,
                ..Default::default()
            }))
            .map_err(anyhow::Error::msg)?;
        Ok(Self {
            model,
            tokenizer,
            device,
        })
    }
}

impl Embed for BertEmbedder {
    fn embed(&self, texts: &[String], kind: EmbedKind) -> anyhow::Result<Vec<Vec<f32>>> {
        let prefix = match kind {
            EmbedKind::Query => "query: ",
            EmbedKind::Passage => "passage: ",
        };
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(16) {
            let inputs: Vec<String> = batch.iter().map(|t| format!("{prefix}{t}")).collect();
            let encodings = self
                .tokenizer
                .encode_batch(inputs, true)
                .map_err(anyhow::Error::msg)?;
            let ids = encodings
                .iter()
                .map(|e| Tensor::new(e.get_ids(), &self.device))
                .collect::<Result<Vec<_>, _>>()?;
            let masks = encodings
                .iter()
                .map(|e| Tensor::new(e.get_attention_mask(), &self.device))
                .collect::<Result<Vec<_>, _>>()?;
            let ids = Tensor::stack(&ids, 0)?;
            let mask = Tensor::stack(&masks, 0)?;
            let hidden = self.model.forward(&ids, &ids.zeros_like()?, Some(&mask))?;
            let mask = mask.to_dtype(DType::F32)?.unsqueeze(2)?;
            let pooled = hidden
                .broadcast_mul(&mask)?
                .sum(1)?
                .broadcast_div(&mask.sum(1)?)?;
            let norm = pooled.sqr()?.sum_keepdim(1)?.sqrt()?;
            out.extend(pooled.broadcast_div(&norm)?.to_vec2::<f32>()?);
        }
        Ok(out)
    }

    fn count_tokens(&self, text: &str) -> usize {
        self.tokenizer
            .encode(text, false)
            .map(|e| e.get_ids().len())
            .unwrap_or_else(|_| text.len() / 4)
    }
}

/// Deterministic stand-in for tests: hashed bag of lowercase words (or characters for unspaced
/// text), normalized. Shared words give a high cosine; disjoint words give about 0.
#[cfg(test)]
pub struct HashEmbedder;

#[cfg(test)]
impl Embed for HashEmbedder {
    fn embed(&self, texts: &[String], _kind: EmbedKind) -> anyhow::Result<Vec<Vec<f32>>> {
        use std::hash::{Hash, Hasher};
        Ok(texts
            .iter()
            .map(|text| {
                let mut v = vec![0f32; DIMENSIONS];
                let lower = text.to_lowercase();
                let tokens: Vec<String> = if lower.contains(' ') {
                    lower.split_whitespace().map(str::to_string).collect()
                } else {
                    lower.chars().map(String::from).collect()
                };
                for token in tokens {
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    token
                        .trim_matches(|c: char| !c.is_alphanumeric())
                        .hash(&mut h);
                    v[(h.finish() % DIMENSIONS as u64) as usize] += 1.0;
                }
                let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
                v.iter().map(|x| x / n).collect()
            })
            .collect())
    }

    fn count_tokens(&self, text: &str) -> usize {
        let words = text.split_whitespace().count();
        if words > 1 {
            words
        } else {
            text.chars().count().div_ceil(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_embedder_scores_shared_words_higher() {
        let v = HashEmbedder
            .embed(
                &[
                    "recorded the song in london".into(),
                    "the song was recorded in london".into(),
                    "stock market prices fell".into(),
                ],
                EmbedKind::Passage,
            )
            .unwrap();
        assert!(cosine(&v[0], &v[1]) > cosine(&v[0], &v[2]));
        assert_eq!(v[0].len(), DIMENSIONS);
    }

    /// Runs only with DJ_EMBEDDING_MODEL_DIR pointing at multilingual-e5-small.
    #[test]
    fn e5_ranks_related_and_cross_lingual_passages() {
        let Ok(dir) = std::env::var("DJ_EMBEDDING_MODEL_DIR") else {
            return;
        };
        let e = BertEmbedder::load(Path::new(&dir)).expect("load e5");
        let q = e
            .embed(
                &["the story behind the song Silhouette by KANA-BOON".into()],
                EmbedKind::Query,
            )
            .unwrap();
        let p = e
            .embed(
                &[
                    "「シルエット」は、日本のロックバンドKANA-BOONの楽曲。アニメ『NARUTO』の主題歌に起用された。".into(),
                    "The quarterly earnings report showed revenue growth in the retail segment.".into(),
                ],
                EmbedKind::Passage,
            )
            .unwrap();
        assert_eq!(q[0].len(), DIMENSIONS);
        assert!(cosine(&q[0], &p[0]) > cosine(&q[0], &p[1]) + 0.05);
        assert!(e.count_tokens("hello world") >= 2);
    }
}
