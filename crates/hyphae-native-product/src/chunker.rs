// SPDX-License-Identifier: Apache-2.0

//! Deterministic, dependency-free document chunking with digest-bound
//! identity.
//!
//! Every chunk is identified by a BLAKE3 digest over the source document's
//! digest, the chunker configuration's digest, and the chunk's exact byte
//! range — so identical inputs produce identical chunk identities on every
//! host, and any retrieved chunk is provably traceable to exact source
//! bytes. Boundaries always land on UTF-8 character boundaries; sentence
//! mode additionally prefers sentence terminators. Bounded and fail-closed:
//! oversized configurations and documents are rejected, never truncated.

/// Maximum source-document bytes accepted by one chunking call.
pub const MAX_CHUNK_SOURCE_BYTES: usize = 16 * 1024 * 1024;
/// Maximum chunk size in bytes.
pub const MAX_CHUNK_BYTES: usize = 64 * 1024;
/// Minimum chunk size in bytes.
pub const MIN_CHUNK_BYTES: usize = 32;
/// Maximum chunks produced from one document.
pub const MAX_CHUNKS_PER_DOCUMENT: usize = 10_000;

const CHUNK_IDENTITY_DOMAIN: &[u8] = b"hyphae-chunk-identity-v1";
const CHUNK_CONFIG_DOMAIN: &[u8] = b"hyphae-chunk-config-v1";

/// Deterministic chunking strategy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChunkerMode {
    /// Fixed-size windows with overlap, cut at UTF-8 boundaries.
    FixedBytes {
        /// Target window size in bytes.
        size: usize,
        /// Overlap carried into the next window, strictly below `size`.
        overlap: usize,
    },
    /// Sentence-packed windows: whole sentences accumulate up to the target
    /// and never beyond the maximum; a single sentence longer than the
    /// maximum falls back to fixed cuts.
    SentenceBounded {
        /// Preferred window size in bytes.
        target: usize,
        /// Hard window bound in bytes.
        maximum: usize,
    },
}

/// Complete chunker configuration with a stable identity digest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChunkerConfig {
    /// Boundary strategy.
    pub mode: ChunkerMode,
}

/// One produced chunk with digest-bound identity.
#[allow(clippy::struct_field_names)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Chunk {
    /// BLAKE3 over the identity domain, document digest, configuration
    /// digest, and the little-endian byte range.
    pub chunk_id: [u8; 32],
    /// Zero-based chunk ordinal in document order.
    pub ordinal: usize,
    /// Inclusive UTF-8 byte offset in the source document.
    pub byte_start: usize,
    /// Exclusive UTF-8 byte offset in the source document.
    pub byte_end: usize,
    /// The exact chunk text, byte-equal to the source range.
    pub text: String,
}

/// Fail-closed chunking rejection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChunkerError {
    /// A size, overlap, or target is outside its bounded range.
    InvalidConfiguration,
    /// The source document exceeds the bounded input size.
    SourceTooLarge,
    /// The document produces more than the bounded chunk count.
    TooManyChunks,
}

impl ChunkerConfig {
    /// Validates the configuration bounds.
    ///
    /// # Errors
    ///
    /// Returns `InvalidConfiguration` for any out-of-range parameter.
    pub const fn validate(&self) -> Result<(), ChunkerError> {
        match self.mode {
            ChunkerMode::FixedBytes { size, overlap } => {
                if size < MIN_CHUNK_BYTES || size > MAX_CHUNK_BYTES || overlap >= size {
                    return Err(ChunkerError::InvalidConfiguration);
                }
            }
            ChunkerMode::SentenceBounded { target, maximum } => {
                if target < MIN_CHUNK_BYTES || target > maximum || maximum > MAX_CHUNK_BYTES {
                    return Err(ChunkerError::InvalidConfiguration);
                }
            }
        }
        Ok(())
    }

    /// Returns the stable configuration digest bound into chunk identities.
    #[must_use]
    pub fn digest(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(CHUNK_CONFIG_DOMAIN);
        match self.mode {
            ChunkerMode::FixedBytes { size, overlap } => {
                hasher.update(&[1]);
                hasher.update(&(size as u64).to_le_bytes());
                hasher.update(&(overlap as u64).to_le_bytes());
            }
            ChunkerMode::SentenceBounded { target, maximum } => {
                hasher.update(&[2]);
                hasher.update(&(target as u64).to_le_bytes());
                hasher.update(&(maximum as u64).to_le_bytes());
            }
        }
        *hasher.finalize().as_bytes()
    }
}

/// Returns the stable document digest bound into chunk identities.
#[must_use]
pub fn document_digest(text: &str) -> [u8; 32] {
    *blake3::hash(text.as_bytes()).as_bytes()
}

/// Chunks one document deterministically.
///
/// # Errors
///
/// Returns a configuration, size, or cardinality error and never a partial
/// chunk list.
pub fn chunk_document(text: &str, config: ChunkerConfig) -> Result<Vec<Chunk>, ChunkerError> {
    config.validate()?;
    if text.len() > MAX_CHUNK_SOURCE_BYTES {
        return Err(ChunkerError::SourceTooLarge);
    }
    let ranges = match config.mode {
        ChunkerMode::FixedBytes { size, overlap } => fixed_ranges(text, size, overlap)?,
        ChunkerMode::SentenceBounded { target, maximum } => sentence_ranges(text, target, maximum)?,
    };
    let doc_digest = document_digest(text);
    let config_digest = config.digest();
    Ok(ranges
        .into_iter()
        .enumerate()
        .map(|(ordinal, (byte_start, byte_end))| Chunk {
            chunk_id: chunk_identity(&doc_digest, &config_digest, byte_start, byte_end),
            ordinal,
            byte_start,
            byte_end,
            text: text[byte_start..byte_end].to_owned(),
        })
        .collect())
}

/// Computes one chunk identity digest.
#[must_use]
pub fn chunk_identity(
    document_digest: &[u8; 32],
    config_digest: &[u8; 32],
    byte_start: usize,
    byte_end: usize,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(CHUNK_IDENTITY_DOMAIN);
    hasher.update(document_digest);
    hasher.update(config_digest);
    hasher.update(&(byte_start as u64).to_le_bytes());
    hasher.update(&(byte_end as u64).to_le_bytes());
    *hasher.finalize().as_bytes()
}

/// The largest UTF-8 boundary at or below `index`.
fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn push_range(ranges: &mut Vec<(usize, usize)>, range: (usize, usize)) -> Result<(), ChunkerError> {
    if ranges.len() >= MAX_CHUNKS_PER_DOCUMENT {
        return Err(ChunkerError::TooManyChunks);
    }
    ranges.push(range);
    Ok(())
}

fn fixed_ranges(
    text: &str,
    size: usize,
    overlap: usize,
) -> Result<Vec<(usize, usize)>, ChunkerError> {
    let mut ranges = Vec::new();
    if text.is_empty() {
        return Ok(ranges);
    }
    let mut start = 0_usize;
    loop {
        let end = floor_char_boundary(text, start + size);
        push_range(&mut ranges, (start, end))?;
        if end == text.len() {
            return Ok(ranges);
        }
        // The next window starts `overlap` bytes before this window's end,
        // aligned down to a character boundary, and always advances.
        let mut next = floor_char_boundary(text, end.saturating_sub(overlap));
        if next <= start {
            next = end;
        }
        start = next;
    }
}

/// Whether the character terminates a sentence.
const fn is_terminator(character: char) -> bool {
    matches!(character, '.' | '!' | '?' | '。' | '！' | '？')
}

/// Deterministic sentence segmentation: a sentence ends after a terminator
/// run followed by whitespace (or the end of input). The trailing
/// whitespace belongs to the sentence so ranges tile the document exactly.
fn sentence_ends(text: &str) -> Vec<usize> {
    let mut ends = Vec::new();
    let mut characters = text.char_indices().peekable();
    let mut after_terminator = false;
    while let Some((index, character)) = characters.next() {
        if is_terminator(character) {
            after_terminator = true;
            continue;
        }
        if after_terminator && character.is_whitespace() {
            // Consume the whitespace run.
            let mut end = index + character.len_utf8();
            while let Some((next_index, next)) = characters.peek().copied() {
                if next.is_whitespace() {
                    characters.next();
                    end = next_index + next.len_utf8();
                } else {
                    break;
                }
            }
            ends.push(end);
        }
        after_terminator = false;
    }
    if ends.last().copied() != Some(text.len()) && !text.is_empty() {
        ends.push(text.len());
    }
    ends
}

fn sentence_ranges(
    text: &str,
    target: usize,
    maximum: usize,
) -> Result<Vec<(usize, usize)>, ChunkerError> {
    let mut ranges = Vec::new();
    if text.is_empty() {
        return Ok(ranges);
    }
    let mut start = 0_usize;
    for end in sentence_ends(text) {
        // A single sentence beyond the hard bound falls back to fixed cuts.
        while end - start > maximum {
            let cut = floor_char_boundary(text, start + maximum);
            push_range(&mut ranges, (start, cut))?;
            start = cut;
        }
        if end - start >= target {
            push_range(&mut ranges, (start, end))?;
            start = end;
        }
    }
    if start < text.len() {
        push_range(&mut ranges, (start, text.len()))?;
    }
    Ok(ranges)
}

/// Builds ingest-ready chunk documents for one parent document: each chunk
/// carries the parent identity, its exact byte range, its ordinal, and its
/// digest identity as doc-values, so retrieval and proofs bind every chunk
/// to exact source bytes. Chunk object identities derive deterministically
/// from the chunk digest.
///
/// # Errors
///
/// Returns a chunking error, or `InvalidConfiguration` when a derived
/// object identity collides with zero (astronomically improbable and
/// fail-closed rather than remapped).
pub fn chunk_documents(
    parent_id: u128,
    text: &str,
    config: ChunkerConfig,
) -> Result<Vec<crate::ProductDocument>, ChunkerError> {
    let chunks = chunk_document(text, config)?;
    let mut documents = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let mut identity_bytes = [0_u8; 16];
        identity_bytes.copy_from_slice(&chunk.chunk_id[..16]);
        let object_id = u128::from_le_bytes(identity_bytes);
        let object_id =
            crate::ObjectId::new(object_id).map_err(|_| ChunkerError::InvalidConfiguration)?;
        let byte_start =
            i64::try_from(chunk.byte_start).map_err(|_| ChunkerError::SourceTooLarge)?;
        let byte_end = i64::try_from(chunk.byte_end).map_err(|_| ChunkerError::SourceTooLarge)?;
        let ordinal = i64::try_from(chunk.ordinal).map_err(|_| ChunkerError::TooManyChunks)?;
        documents.push(crate::ProductDocument {
            object_id,
            text: chunk.text,
            doc_values: [
                (
                    "parent".to_owned(),
                    crate::ProductDocValue::Bytes(parent_id.to_le_bytes().to_vec()),
                ),
                (
                    "chunk_id".to_owned(),
                    crate::ProductDocValue::Bytes(chunk.chunk_id.to_vec()),
                ),
                (
                    "byte_start".to_owned(),
                    crate::ProductDocValue::Integer(byte_start),
                ),
                (
                    "byte_end".to_owned(),
                    crate::ProductDocValue::Integer(byte_end),
                ),
                (
                    "chunk_ordinal".to_owned(),
                    crate::ProductDocValue::Integer(ordinal),
                ),
            ]
            .into_iter()
            .collect(),
            vectors: std::collections::BTreeMap::new(),
        });
    }
    Ok(documents)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(mode: ChunkerMode) -> ChunkerConfig {
        ChunkerConfig { mode }
    }

    #[test]
    fn fixed_chunks_tile_the_document_and_identities_are_stable() -> Result<(), ChunkerError> {
        let text = "abcdefghij".repeat(20);
        let chunks = chunk_document(
            &text,
            config(ChunkerMode::FixedBytes {
                size: 64,
                overlap: 16,
            }),
        )?;
        assert!(!chunks.is_empty());
        assert_eq!(chunks[0].byte_start, 0);
        assert_eq!(chunks.last().map(|chunk| chunk.byte_end), Some(text.len()));
        for window in chunks.windows(2) {
            // Overlapping windows: each starts before the previous end and
            // strictly after the previous start.
            assert!(window[1].byte_start < window[0].byte_end);
            assert!(window[1].byte_start > window[0].byte_start);
        }
        for chunk in &chunks {
            assert_eq!(
                chunk.text.as_bytes(),
                &text.as_bytes()[chunk.byte_start..chunk.byte_end]
            );
            assert_eq!(
                chunk.chunk_id,
                chunk_identity(
                    &document_digest(&text),
                    &config(ChunkerMode::FixedBytes {
                        size: 64,
                        overlap: 16,
                    })
                    .digest(),
                    chunk.byte_start,
                    chunk.byte_end,
                )
            );
        }
        let again = chunk_document(
            &text,
            config(ChunkerMode::FixedBytes {
                size: 64,
                overlap: 16,
            }),
        )?;
        assert_eq!(chunks, again);
        Ok(())
    }

    #[test]
    fn boundaries_never_split_multibyte_characters() -> Result<(), ChunkerError> {
        let text = "áéíóúñ加載中データ🦀".repeat(40);
        for (size, overlap) in [(32, 0), (33, 7), (64, 31), (37, 11)] {
            let chunks = chunk_document(&text, config(ChunkerMode::FixedBytes { size, overlap }))?;
            for chunk in &chunks {
                assert!(text.is_char_boundary(chunk.byte_start));
                assert!(text.is_char_boundary(chunk.byte_end));
                assert!(!chunk.text.is_empty());
            }
            assert_eq!(chunks.last().map(|chunk| chunk.byte_end), Some(text.len()));
        }
        Ok(())
    }

    #[test]
    fn sentence_mode_prefers_terminators_and_bounds_runaways() -> Result<(), ChunkerError> {
        let text = "One short sentence. Another follows here! A third? \
                    Then a very long sentence that just keeps going and going \
                    without any terminator at all until the end of the text";
        let chunks = chunk_document(
            text,
            config(ChunkerMode::SentenceBounded {
                target: 48,
                maximum: 80,
            }),
        )?;
        assert!(
            chunks
                .iter()
                .map(|chunk| chunk.byte_end - chunk.byte_start)
                .max()
                .unwrap_or(0)
                <= 80
        );
        // The first chunk ends exactly after a sentence terminator run.
        assert!(
            text[..chunks[0].byte_end]
                .trim_end()
                .ends_with(['.', '!', '?'])
        );
        // Ranges tile the document with no gaps.
        let mut cursor = 0;
        for chunk in &chunks {
            assert_eq!(chunk.byte_start, cursor);
            cursor = chunk.byte_end;
        }
        assert_eq!(cursor, text.len());
        Ok(())
    }

    #[test]
    fn identity_binds_document_configuration_and_range() {
        let document = document_digest("source");
        let other_document = document_digest("other");
        let config_a = config(ChunkerMode::FixedBytes {
            size: 64,
            overlap: 0,
        })
        .digest();
        let config_b = config(ChunkerMode::FixedBytes {
            size: 64,
            overlap: 8,
        })
        .digest();
        let base = chunk_identity(&document, &config_a, 0, 64);
        assert_ne!(base, chunk_identity(&other_document, &config_a, 0, 64));
        assert_ne!(base, chunk_identity(&document, &config_b, 0, 64));
        assert_ne!(base, chunk_identity(&document, &config_a, 0, 63));
        assert_ne!(base, chunk_identity(&document, &config_a, 1, 64));
    }

    #[test]
    fn invalid_shapes_fail_closed() {
        assert_eq!(
            config(ChunkerMode::FixedBytes {
                size: 16,
                overlap: 0
            })
            .validate(),
            Err(ChunkerError::InvalidConfiguration)
        );
        assert_eq!(
            config(ChunkerMode::FixedBytes {
                size: 64,
                overlap: 64
            })
            .validate(),
            Err(ChunkerError::InvalidConfiguration)
        );
        assert_eq!(
            config(ChunkerMode::SentenceBounded {
                target: 128,
                maximum: 64
            })
            .validate(),
            Err(ChunkerError::InvalidConfiguration)
        );
        let oversized = "a".repeat(MAX_CHUNK_SOURCE_BYTES + 1);
        assert_eq!(
            chunk_document(
                oversized.as_str(),
                config(ChunkerMode::FixedBytes {
                    size: 64,
                    overlap: 0
                })
            ),
            Err(ChunkerError::SourceTooLarge)
        );
    }
}
