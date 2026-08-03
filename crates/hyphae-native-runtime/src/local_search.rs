// SPDX-License-Identifier: Apache-2.0

use std::cmp::Ordering;

use hyphae_native_types::{Csn, ObjectId};
use thiserror::Error;

use crate::MatchHit;

/// Canonical fixed header width for one local lexical `MATCH` request.
pub const LOCAL_SEARCH_MATCH_HEADER_SIZE: usize = 28;
/// Canonical fixed header width for one local lexical result.
pub const LOCAL_SEARCH_RESULTS_HEADER_SIZE: usize = 16;
/// Maximum UTF-8 bytes admitted for one local lexical query.
pub const MAX_LOCAL_SEARCH_QUERY_BYTES: usize = 4_096;
/// Maximum result count admitted by one local lexical request.
pub const MAX_LOCAL_SEARCH_HITS: usize = 1_024;

const SEARCH_VERSION: u8 = 1;
const MATCH_OPCODE: u8 = 1;
const MATCH_RESULTS_TAG: u8 = 1;
const MATCH_HIT_HEADER_SIZE: usize = 12;

/// Borrowed canonical local lexical `MATCH` request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalSearchMatchRequest<'payload> {
    /// Stable search collection identity.
    pub index: ObjectId,
    /// UTF-8 lexical query, including a valid empty query.
    pub query: &'payload str,
    /// Strictly positive bounded result limit.
    pub limit: usize,
}

/// Borrowed canonical local lexical hit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LocalSearchMatchHit<'payload> {
    /// Stable binary document identity.
    pub document_id: &'payload [u8],
    /// Positive finite native BM25 score.
    pub score: f64,
}

/// Borrowed decoded local lexical result.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalSearchMatchResults<'payload> {
    /// All-engine root-set CSN used for the complete query.
    pub visible_csn: Csn,
    /// Canonically ordered lexical hits.
    pub hits: Vec<LocalSearchMatchHit<'payload>>,
}

/// Canonical local lexical payload failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum LocalSearchCodecError {
    /// The payload is shorter than its fixed header or one hit header.
    #[error("native local search payload is truncated")]
    Truncated,
    /// The payload version is unsupported.
    #[error("native local search payload version {0} is unsupported")]
    UnsupportedVersion(u8),
    /// Reserved payload bytes are nonzero.
    #[error("native local search payload reserved bytes are nonzero")]
    ReservedBytes,
    /// The search operation is unknown.
    #[error("native local search opcode {0} is unknown")]
    UnknownOpcode(u8),
    /// The search object identity is zero.
    #[error("native local search object identity is invalid")]
    InvalidObjectId,
    /// Query bytes are not valid UTF-8.
    #[error("native local search query is not UTF-8")]
    InvalidUtf8,
    /// The query exceeds the local CPU and memory guard.
    #[error("native local search query exceeds its canonical limit")]
    QueryTooLarge,
    /// The result limit is zero or above the local bound.
    #[error("native local search hit limit {0} is invalid")]
    InvalidLimit(usize),
    /// Declared and physical payload lengths differ.
    #[error("native local search payload length mismatch")]
    LengthMismatch,
    /// The complete encoded payload exceeds its configured frame bound.
    #[error("native local search payload exceeds the configured frame bound")]
    PayloadTooLarge,
    /// The lexical result tag is unknown.
    #[error("native local search result tag {0} is unknown")]
    UnknownResultTag(u8),
    /// The visible result CSN is zero.
    #[error("native local search result CSN is invalid")]
    InvalidCsn,
    /// The result contains more than the canonical maximum hit count.
    #[error("native local search result contains too many hits")]
    TooManyHits,
    /// A BM25 score is zero, negative, infinite, or NaN.
    #[error("native local search score is noncanonical")]
    NoncanonicalScore,
    /// Hits are duplicated or not in canonical score/document order.
    #[error("native local search hit order is noncanonical")]
    NoncanonicalHitOrder,
}

/// Encodes one canonical local lexical `MATCH` request.
///
/// # Errors
///
/// Returns a typed error for query, limit, identity, length, or frame-bound
/// violations before growing the reusable buffer.
pub fn encode_local_search_match<'buffer>(
    buffer: &'buffer mut Vec<u8>,
    index: ObjectId,
    query: &str,
    limit: usize,
    maximum_payload: usize,
) -> Result<&'buffer [u8], LocalSearchCodecError> {
    validate_query_length(query.len())?;
    validate_limit(limit)?;
    let encoded_length = LOCAL_SEARCH_MATCH_HEADER_SIZE
        .checked_add(query.len())
        .ok_or(LocalSearchCodecError::PayloadTooLarge)?;
    if encoded_length > maximum_payload {
        return Err(LocalSearchCodecError::PayloadTooLarge);
    }
    let query_length =
        u32::try_from(query.len()).map_err(|_| LocalSearchCodecError::QueryTooLarge)?;
    let limit = u32::try_from(limit).map_err(|_| LocalSearchCodecError::InvalidLimit(limit))?;
    buffer.resize(encoded_length, 0);
    buffer[..LOCAL_SEARCH_MATCH_HEADER_SIZE].fill(0);
    buffer[0] = SEARCH_VERSION;
    buffer[1] = MATCH_OPCODE;
    buffer[4..20].copy_from_slice(&index.get().to_le_bytes());
    buffer[20..24].copy_from_slice(&query_length.to_le_bytes());
    buffer[24..28].copy_from_slice(&limit.to_le_bytes());
    buffer[LOCAL_SEARCH_MATCH_HEADER_SIZE..].copy_from_slice(query.as_bytes());
    Ok(buffer)
}

/// Decodes one canonical local lexical `MATCH` request.
///
/// # Errors
///
/// Returns a typed error for every noncanonical version, opcode, reserved,
/// identity, UTF-8, query, limit, or length boundary.
pub fn decode_local_search_match(
    payload: &[u8],
) -> Result<LocalSearchMatchRequest<'_>, LocalSearchCodecError> {
    if payload.len() < LOCAL_SEARCH_MATCH_HEADER_SIZE {
        return Err(LocalSearchCodecError::Truncated);
    }
    validate_version_and_reserved(payload)?;
    if payload[1] != MATCH_OPCODE {
        return Err(LocalSearchCodecError::UnknownOpcode(payload[1]));
    }
    let index =
        ObjectId::new(read_u128(payload, 4)).map_err(|_| LocalSearchCodecError::InvalidObjectId)?;
    let query_length = read_u32(payload, 20)?;
    validate_query_length(query_length)?;
    let limit = read_u32(payload, 24)?;
    validate_limit(limit)?;
    require_payload_length(payload, LOCAL_SEARCH_MATCH_HEADER_SIZE, query_length)?;
    let query = std::str::from_utf8(&payload[LOCAL_SEARCH_MATCH_HEADER_SIZE..])
        .map_err(|_| LocalSearchCodecError::InvalidUtf8)?;
    Ok(LocalSearchMatchRequest {
        index,
        query,
        limit,
    })
}

/// Encodes one canonical local lexical result.
///
/// # Errors
///
/// Returns a typed error for count, score, order, length, or frame-bound
/// violations before growing the reusable buffer.
pub fn encode_local_search_match_results<'buffer>(
    buffer: &'buffer mut Vec<u8>,
    visible_csn: Csn,
    hits: &[MatchHit],
    maximum_payload: usize,
) -> Result<&'buffer [u8], LocalSearchCodecError> {
    validate_runtime_hits(hits)?;
    let hit_count = u32::try_from(hits.len()).map_err(|_| LocalSearchCodecError::TooManyHits)?;
    let encoded_length =
        hits.iter()
            .try_fold(LOCAL_SEARCH_RESULTS_HEADER_SIZE, |length, hit| {
                let _document_length = u32::try_from(hit.document_id.len())
                    .map_err(|_| LocalSearchCodecError::PayloadTooLarge)?;
                length
                    .checked_add(MATCH_HIT_HEADER_SIZE)
                    .and_then(|length| length.checked_add(hit.document_id.len()))
                    .ok_or(LocalSearchCodecError::PayloadTooLarge)
            })?;
    if encoded_length > maximum_payload {
        return Err(LocalSearchCodecError::PayloadTooLarge);
    }

    buffer.resize(encoded_length, 0);
    buffer[..LOCAL_SEARCH_RESULTS_HEADER_SIZE].fill(0);
    buffer[0] = SEARCH_VERSION;
    buffer[1] = MATCH_RESULTS_TAG;
    buffer[4..8].copy_from_slice(&hit_count.to_le_bytes());
    buffer[8..16].copy_from_slice(&visible_csn.get().to_le_bytes());
    let mut cursor = LOCAL_SEARCH_RESULTS_HEADER_SIZE;
    for hit in hits {
        let document_length = u32::try_from(hit.document_id.len())
            .map_err(|_| LocalSearchCodecError::PayloadTooLarge)?;
        buffer[cursor..cursor + 4].copy_from_slice(&document_length.to_le_bytes());
        buffer[cursor + 4..cursor + MATCH_HIT_HEADER_SIZE]
            .copy_from_slice(&hit.score.to_bits().to_le_bytes());
        cursor += MATCH_HIT_HEADER_SIZE;
        let document_end = cursor + hit.document_id.len();
        buffer[cursor..document_end].copy_from_slice(&hit.document_id);
        cursor = document_end;
    }
    Ok(buffer)
}

/// Decodes one canonical local lexical result.
///
/// # Errors
///
/// Returns a typed error for header, CSN, count, score, ordering, hit length,
/// or trailing-byte violations.
pub fn decode_local_search_match_results(
    payload: &[u8],
) -> Result<LocalSearchMatchResults<'_>, LocalSearchCodecError> {
    if payload.len() < LOCAL_SEARCH_RESULTS_HEADER_SIZE {
        return Err(LocalSearchCodecError::Truncated);
    }
    validate_version_and_reserved(payload)?;
    if payload[1] != MATCH_RESULTS_TAG {
        return Err(LocalSearchCodecError::UnknownResultTag(payload[1]));
    }
    let hit_count = read_u32(payload, 4)?;
    if hit_count > MAX_LOCAL_SEARCH_HITS {
        return Err(LocalSearchCodecError::TooManyHits);
    }
    let visible_csn =
        Csn::new(read_u64(payload, 8)).map_err(|_| LocalSearchCodecError::InvalidCsn)?;
    let mut cursor = LOCAL_SEARCH_RESULTS_HEADER_SIZE;
    let mut hits = Vec::with_capacity(hit_count);
    for _ in 0..hit_count {
        if payload.len().saturating_sub(cursor) < MATCH_HIT_HEADER_SIZE {
            return Err(LocalSearchCodecError::Truncated);
        }
        let document_length = read_u32(payload, cursor)?;
        let score = f64::from_bits(read_u64(payload, cursor + 4));
        validate_score(score)?;
        cursor = cursor
            .checked_add(MATCH_HIT_HEADER_SIZE)
            .ok_or(LocalSearchCodecError::PayloadTooLarge)?;
        let document_end = cursor
            .checked_add(document_length)
            .ok_or(LocalSearchCodecError::PayloadTooLarge)?;
        let document_id = payload
            .get(cursor..document_end)
            .ok_or(LocalSearchCodecError::LengthMismatch)?;
        hits.push(LocalSearchMatchHit { document_id, score });
        cursor = document_end;
    }
    if cursor != payload.len() {
        return Err(LocalSearchCodecError::LengthMismatch);
    }
    validate_decoded_hits(&hits)?;
    Ok(LocalSearchMatchResults { visible_csn, hits })
}

fn validate_runtime_hits(hits: &[MatchHit]) -> Result<(), LocalSearchCodecError> {
    if hits.len() > MAX_LOCAL_SEARCH_HITS {
        return Err(LocalSearchCodecError::TooManyHits);
    }
    for hit in hits {
        validate_score(hit.score)?;
    }
    if hits.windows(2).any(|pair| {
        compare_hits(
            pair[0].score,
            &pair[0].document_id,
            pair[1].score,
            &pair[1].document_id,
        ) != Ordering::Less
    }) {
        return Err(LocalSearchCodecError::NoncanonicalHitOrder);
    }
    Ok(())
}

fn validate_decoded_hits(hits: &[LocalSearchMatchHit<'_>]) -> Result<(), LocalSearchCodecError> {
    if hits.windows(2).any(|pair| {
        compare_hits(
            pair[0].score,
            pair[0].document_id,
            pair[1].score,
            pair[1].document_id,
        ) != Ordering::Less
    }) {
        return Err(LocalSearchCodecError::NoncanonicalHitOrder);
    }
    Ok(())
}

fn compare_hits(
    left_score: f64,
    left_document: &[u8],
    right_score: f64,
    right_document: &[u8],
) -> Ordering {
    right_score
        .total_cmp(&left_score)
        .then_with(|| left_document.cmp(right_document))
}

fn validate_score(score: f64) -> Result<(), LocalSearchCodecError> {
    if score.is_finite() && score > 0.0 {
        Ok(())
    } else {
        Err(LocalSearchCodecError::NoncanonicalScore)
    }
}

fn validate_query_length(length: usize) -> Result<(), LocalSearchCodecError> {
    if length <= MAX_LOCAL_SEARCH_QUERY_BYTES {
        Ok(())
    } else {
        Err(LocalSearchCodecError::QueryTooLarge)
    }
}

fn validate_limit(limit: usize) -> Result<(), LocalSearchCodecError> {
    if (1..=MAX_LOCAL_SEARCH_HITS).contains(&limit) {
        Ok(())
    } else {
        Err(LocalSearchCodecError::InvalidLimit(limit))
    }
}

fn validate_version_and_reserved(payload: &[u8]) -> Result<(), LocalSearchCodecError> {
    if payload[0] != SEARCH_VERSION {
        return Err(LocalSearchCodecError::UnsupportedVersion(payload[0]));
    }
    if payload[2..4] != [0, 0] {
        return Err(LocalSearchCodecError::ReservedBytes);
    }
    Ok(())
}

fn read_u32(payload: &[u8], offset: usize) -> Result<usize, LocalSearchCodecError> {
    let mut encoded = [0_u8; 4];
    encoded.copy_from_slice(&payload[offset..offset + 4]);
    usize::try_from(u32::from_le_bytes(encoded)).map_err(|_| LocalSearchCodecError::PayloadTooLarge)
}

fn read_u64(payload: &[u8], offset: usize) -> u64 {
    let mut encoded = [0_u8; 8];
    encoded.copy_from_slice(&payload[offset..offset + 8]);
    u64::from_le_bytes(encoded)
}

fn read_u128(payload: &[u8], offset: usize) -> u128 {
    let mut encoded = [0_u8; 16];
    encoded.copy_from_slice(&payload[offset..offset + 16]);
    u128::from_le_bytes(encoded)
}

fn require_payload_length(
    payload: &[u8],
    header_length: usize,
    body_length: usize,
) -> Result<(), LocalSearchCodecError> {
    let encoded_length = header_length
        .checked_add(body_length)
        .ok_or(LocalSearchCodecError::PayloadTooLarge)?;
    if payload.len() != encoded_length {
        return Err(LocalSearchCodecError::LengthMismatch);
    }
    Ok(())
}
