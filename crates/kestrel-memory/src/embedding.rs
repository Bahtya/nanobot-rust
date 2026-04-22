//! Embedding generation trait and hash-based placeholder implementation.
//!
//! The [`EmbeddingGenerator`] trait abstracts over embedding backends so the
//! memory tools can generate vectors without knowing the concrete algorithm.
//! [`HashEmbedding`] provides a deterministic, zero-dependency placeholder
//! using random-projection hashing — good enough for development and testing,
//! and designed to be swapped out for a real model (e.g. OpenAI embeddings)
//! without changing downstream code.

use async_trait::async_trait;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::error::Result;

/// Trait for generating embedding vectors from text.
#[async_trait]
pub trait EmbeddingGenerator: Send + Sync {
    /// Generate an embedding vector for the given text.
    async fn generate(&self, text: &str) -> Result<Vec<f32>>;

    /// Return the dimension of generated embedding vectors.
    fn dimension(&self) -> usize;
}

/// Simple hash-based embedding generator using random-projection hashing.
///
/// Each word in the input is hashed to determine both the dimension index
/// and a sign (+1 / -1). The resulting sparse vector is L2-normalized. This
/// produces deterministic, fixed-dimension embeddings where texts sharing
/// words have higher cosine similarity — sufficient for development and as
/// a placeholder until a real embedding model is wired in.
pub struct HashEmbedding {
    dimension: usize,
}

impl HashEmbedding {
    /// Create a new hash embedding generator with the given vector dimension.
    pub fn new(dimension: usize) -> Self {
        Self { dimension }
    }

    /// Create with the default dimension of 256.
    pub fn default_dim() -> Self {
        Self::new(256)
    }

    /// Tokenize text into lowercase words.
    fn tokenize(text: &str) -> Vec<&str> {
        text.split(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Hash a string to a u64.
    fn hash_str(s: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        s.hash(&mut hasher);
        hasher.finish()
    }
}

#[async_trait]
impl EmbeddingGenerator for HashEmbedding {
    async fn generate(&self, text: &str) -> Result<Vec<f32>> {
        let tokens = Self::tokenize(text);
        if tokens.is_empty() {
            return Ok(vec![0.0; self.dimension]);
        }

        let mut vec = vec![0.0_f32; self.dimension];

        for token in &tokens {
            let lower = token.to_lowercase();
            let h = Self::hash_str(&lower);
            let idx = (h as usize) % self.dimension;
            // Use a second hash for the sign to reduce collision bias.
            let sign_h = h.wrapping_mul(0x9E3779B97F4A7C15);
            let sign: f32 = if sign_h % 2 == 0 { 1.0 } else { -1.0 };
            vec[idx] += sign;
        }

        // L2 normalize.
        let norm: f64 = vec.iter().map(|v| (*v as f64).powi(2)).sum::<f64>().sqrt();
        if norm > 0.0 {
            for v in &mut vec {
                *v = (*v as f64 / norm) as f32;
            }
        }

        Ok(vec)
    }

    fn dimension(&self) -> usize {
        self.dimension
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_generate_basic() {
        let gen = HashEmbedding::new(64);
        let vec = gen.generate("hello world").await.unwrap();
        assert_eq!(vec.len(), 64);
        // Should be L2-normalized.
        let norm: f64 = vec.iter().map(|v| (*v as f64).powi(2)).sum::<f64>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "norm = {norm}");
    }

    #[tokio::test]
    async fn test_generate_empty() {
        let gen = HashEmbedding::new(64);
        let vec = gen.generate("").await.unwrap();
        assert_eq!(vec.len(), 64);
        assert!(vec.iter().all(|v| *v == 0.0));
    }

    #[tokio::test]
    async fn test_deterministic() {
        let gen = HashEmbedding::new(64);
        let a = gen.generate("rust programming").await.unwrap();
        let b = gen.generate("rust programming").await.unwrap();
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn test_similar_texts_higher_similarity() {
        let gen = HashEmbedding::new(256);
        let a = gen.generate("the cat sat on the mat").await.unwrap();
        let b = gen
            .generate("the cat sat on the mat and slept")
            .await
            .unwrap();
        let c = gen
            .generate("quantum physics and differential equations")
            .await
            .unwrap();

        let sim_ab = cosine_similarity(&a, &b);
        let sim_ac = cosine_similarity(&a, &c);

        assert!(
            sim_ab > sim_ac,
            "similar texts should have higher cosine similarity: ab={sim_ab}, ac={sim_ac}"
        );
    }

    #[tokio::test]
    async fn test_dimension() {
        let gen = HashEmbedding::new(128);
        assert_eq!(gen.dimension(), 128);
        assert_eq!(gen.generate("test").await.unwrap().len(), 128);
    }

    #[tokio::test]
    async fn test_case_insensitive() {
        let gen = HashEmbedding::new(64);
        let a = gen.generate("Hello World").await.unwrap();
        let b = gen.generate("hello world").await.unwrap();
        assert_eq!(a, b);
    }

    fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
        if a.len() != b.len() || a.is_empty() {
            return 0.0;
        }
        let dot: f64 = a
            .iter()
            .zip(b.iter())
            .map(|(x, y)| (*x as f64) * (*y as f64))
            .sum();
        let na: f64 = a.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
        let nb: f64 = b.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
        if na == 0.0 || nb == 0.0 {
            return 0.0;
        }
        dot / (na * nb)
    }

    #[test]
    fn test_tokenize() {
        let tokens = HashEmbedding::tokenize("Hello, world! Foo-bar baz123");
        assert_eq!(tokens, vec!["Hello", "world", "Foo", "bar", "baz123"]);
    }

    #[test]
    fn test_tokenize_empty() {
        let tokens = HashEmbedding::tokenize("   !!! ... ");
        assert!(tokens.is_empty());
    }
}
