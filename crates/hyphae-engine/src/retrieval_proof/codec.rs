// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use hyphae_core::{Q15Vector, SCORE_NANOS_SCALE, VectorSpaceName};
use hyphae_query::FieldPath;
use hyphae_retrieval::{
    ExactAbstention, ExactAbstentionReason, ExactRetrievalLimits, ExactRetrievalMatch,
    ExactRetrievalOutcome, ExactRetrievalRequest, HybridAbstention, HybridBranchAbsence,
    HybridExplanation, HybridMatch, HybridOutcome, HybridRequest, LexicalAbstention,
    LexicalAbstentionReason, LexicalFieldContribution, LexicalMatch, LexicalOutcome,
    LexicalRequest, LexicalTermContribution, MAX_LEXICAL_PATH_SEGMENT_BYTES,
    MAX_LEXICAL_PATH_SEGMENTS, MAX_LEXICAL_TOKEN_BYTES, fuse_hybrid, retrieve_exact, tokenize_v1,
};
use hyphae_storage::MAX_KEY_BYTES;

use super::{
    EXACT_RETRIEVAL_SEMANTICS_VERSION, ExactRetrievalProof, HYBRID_RETRIEVAL_SEMANTICS_VERSION,
    HybridRetrievalProof, LEXICAL_RETRIEVAL_SEMANTICS_VERSION, LexicalRetrievalProof,
    MAX_RETRIEVAL_PROOF_BYTES, RETRIEVAL_PROOF_FORMAT_VERSION, RetrievalProofAnchor,
    RetrievalProofError,
};

const MAGIC: [u8; 8] = *b"HYRPF001";
const HEADER_LENGTH: usize = 132;
const CHECKSUM_PREFIX_LENGTH: usize = 96;
const DIGEST_PREFIX_LENGTH: usize = 100;
const PROOF_DIGEST_OFFSET: usize = 100;
const DIGEST_DOMAIN: &[u8] = b"hyphae-retrieval-proof-v1";
const EXACT_OPERATION: u16 = 1;
const LEXICAL_OPERATION: u16 = 2;
const HYBRID_OPERATION: u16 = 3;
const MATCHES_OUTCOME: u8 = 1;
const ABSTAINED_OUTCOME: u8 = 2;
const ABSENT: u8 = 0;
const PRESENT: u8 = 1;
const MAX_PROOF_COLLECTION_ITEMS: usize = 1_000_000;

pub(crate) fn finalize_proof(
    anchor: RetrievalProofAnchor,
    request: ExactRetrievalRequest,
    outcome: ExactRetrievalOutcome,
) -> Result<ExactRetrievalProof, RetrievalProofError> {
    let mut proof = ExactRetrievalProof {
        anchor,
        semantics_version: EXACT_RETRIEVAL_SEMANTICS_VERSION,
        request,
        outcome,
        proof_digest: [0; 32],
    };
    let encoded = encode_proof(&proof)?;
    proof.proof_digest = copy_array(&encoded[PROOF_DIGEST_OFFSET..HEADER_LENGTH]);
    Ok(proof)
}

pub(crate) fn finalize_lexical_proof(
    anchor: RetrievalProofAnchor,
    request: LexicalRequest,
    outcome: LexicalOutcome,
) -> Result<LexicalRetrievalProof, RetrievalProofError> {
    let mut proof = LexicalRetrievalProof {
        anchor,
        semantics_version: LEXICAL_RETRIEVAL_SEMANTICS_VERSION,
        request,
        outcome,
        proof_digest: [0; 32],
    };
    let encoded = encode_lexical_proof(&proof)?;
    proof.proof_digest = copy_array(&encoded[PROOF_DIGEST_OFFSET..HEADER_LENGTH]);
    Ok(proof)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn finalize_hybrid_proof(
    anchor: RetrievalProofAnchor,
    lexical_request: LexicalRequest,
    lexical_outcome: LexicalOutcome,
    vector_request: ExactRetrievalRequest,
    vector_outcome: ExactRetrievalOutcome,
    fusion_request: HybridRequest,
    outcome: HybridOutcome,
) -> Result<HybridRetrievalProof, RetrievalProofError> {
    let mut proof = HybridRetrievalProof {
        anchor,
        semantics_version: HYBRID_RETRIEVAL_SEMANTICS_VERSION,
        lexical_request,
        lexical_outcome,
        vector_request,
        vector_outcome,
        fusion_request,
        outcome,
        proof_digest: [0; 32],
    };
    let encoded = encode_hybrid_proof(&proof)?;
    proof.proof_digest = copy_array(&encoded[PROOF_DIGEST_OFFSET..HEADER_LENGTH]);
    Ok(proof)
}

pub(crate) fn encode_proof(proof: &ExactRetrievalProof) -> Result<Vec<u8>, RetrievalProofError> {
    validate_anchor(&proof.anchor)?;
    validate_semantics(proof.semantics_version)?;
    validate_request(&proof.request)?;
    validate_outcome(&proof.request, &proof.outcome)?;

    let request = encode_request(&proof.request)?;
    let outcome = encode_outcome(&proof.outcome)?;
    let mut payload = Encoder::default();
    payload.length_u64(request.len())?;
    payload.extend(&request);
    payload.length_u64(outcome.len())?;
    payload.extend(&outcome);

    let file_length = HEADER_LENGTH
        .checked_add(payload.bytes.len())
        .ok_or(RetrievalProofError::LengthOverflow)?;
    let file_length_u64 =
        u64::try_from(file_length).map_err(|_| RetrievalProofError::LengthOverflow)?;
    if file_length_u64 > MAX_RETRIEVAL_PROOF_BYTES {
        return Err(RetrievalProofError::ProofLimitExceeded {
            actual: file_length_u64,
            maximum: MAX_RETRIEVAL_PROOF_BYTES,
        });
    }

    let mut header = [0_u8; HEADER_LENGTH];
    header[..8].copy_from_slice(&MAGIC);
    header[8..10].copy_from_slice(&RETRIEVAL_PROOF_FORMAT_VERSION.to_le_bytes());
    header[10..12].copy_from_slice(&0_u16.to_le_bytes());
    header[12..14].copy_from_slice(&EXACT_OPERATION.to_le_bytes());
    header[14..16].copy_from_slice(&proof.semantics_version.to_le_bytes());
    header[16..24].copy_from_slice(&proof.anchor.checkpoint_sequence.to_le_bytes());
    header[24..56].copy_from_slice(&proof.anchor.checkpoint_digest.unwrap_or([0; 32]));
    header[56..88].copy_from_slice(&proof.anchor.snapshot_digest);
    header[88..96].copy_from_slice(
        &u64::try_from(payload.bytes.len())
            .map_err(|_| RetrievalProofError::LengthOverflow)?
            .to_le_bytes(),
    );

    let checksum = crc32c::crc32c_append(
        crc32c::crc32c(&header[..CHECKSUM_PREFIX_LENGTH]),
        &payload.bytes,
    );
    header[96..100].copy_from_slice(&checksum.to_le_bytes());
    let mut hasher = blake3::Hasher::new();
    hasher.update(DIGEST_DOMAIN);
    hasher.update(&header[..DIGEST_PREFIX_LENGTH]);
    hasher.update(&payload.bytes);
    header[100..132].copy_from_slice(hasher.finalize().as_bytes());

    let mut encoded = Vec::with_capacity(file_length);
    encoded.extend_from_slice(&header);
    encoded.extend_from_slice(&payload.bytes);
    Ok(encoded)
}

pub(crate) fn decode_proof(encoded: &[u8]) -> Result<ExactRetrievalProof, RetrievalProofError> {
    let encoded_length =
        u64::try_from(encoded.len()).map_err(|_| RetrievalProofError::LengthOverflow)?;
    if encoded_length > MAX_RETRIEVAL_PROOF_BYTES {
        return Err(RetrievalProofError::ProofLimitExceeded {
            actual: encoded_length,
            maximum: MAX_RETRIEVAL_PROOF_BYTES,
        });
    }
    if encoded.len() < HEADER_LENGTH {
        return Err(invalid("truncated header"));
    }
    if encoded[..8] != MAGIC {
        return Err(invalid("bad magic"));
    }
    let format_version = u16::from_le_bytes(copy_array(&encoded[8..10]));
    if format_version != RETRIEVAL_PROOF_FORMAT_VERSION {
        return Err(RetrievalProofError::UnsupportedVersion {
            found: format_version,
            supported: RETRIEVAL_PROOF_FORMAT_VERSION,
        });
    }
    if u16::from_le_bytes(copy_array(&encoded[10..12])) != 0 {
        return Err(invalid("unsupported flags"));
    }
    let operation = u16::from_le_bytes(copy_array(&encoded[12..14]));
    if operation != EXACT_OPERATION {
        return Err(RetrievalProofError::UnsupportedOperation { found: operation });
    }
    let semantics_version = u16::from_le_bytes(copy_array(&encoded[14..16]));
    validate_semantics(semantics_version)?;

    let payload_length = usize::try_from(u64::from_le_bytes(copy_array(&encoded[88..96])))
        .map_err(|_| RetrievalProofError::LengthOverflow)?;
    let expected_length = HEADER_LENGTH
        .checked_add(payload_length)
        .ok_or(RetrievalProofError::LengthOverflow)?;
    if encoded.len() != expected_length {
        return Err(invalid("file length mismatch"));
    }
    let payload = &encoded[HEADER_LENGTH..];
    let expected_checksum = u32::from_le_bytes(copy_array(&encoded[96..100]));
    let actual_checksum =
        crc32c::crc32c_append(crc32c::crc32c(&encoded[..CHECKSUM_PREFIX_LENGTH]), payload);
    if actual_checksum != expected_checksum {
        return Err(RetrievalProofError::ChecksumMismatch);
    }
    let expected_digest: [u8; 32] = copy_array(&encoded[100..132]);
    let mut hasher = blake3::Hasher::new();
    hasher.update(DIGEST_DOMAIN);
    hasher.update(&encoded[..DIGEST_PREFIX_LENGTH]);
    hasher.update(payload);
    if *hasher.finalize().as_bytes() != expected_digest {
        return Err(RetrievalProofError::DigestMismatch);
    }

    let checkpoint_sequence = u64::from_le_bytes(copy_array(&encoded[16..24]));
    let raw_checkpoint_digest = copy_array(&encoded[24..56]);
    let checkpoint_digest = if checkpoint_sequence == 0 {
        if raw_checkpoint_digest != [0; 32] {
            return Err(invalid("empty checkpoint has a digest"));
        }
        None
    } else {
        Some(raw_checkpoint_digest)
    };
    let anchor = RetrievalProofAnchor {
        checkpoint_sequence,
        checkpoint_digest,
        snapshot_digest: copy_array(&encoded[56..88]),
    };
    validate_anchor(&anchor)?;

    let mut payload = Decoder::new(payload);
    let request_length = payload.length_u64()?;
    let request = decode_request(payload.take(request_length)?)?;
    let outcome_length = payload.length_u64()?;
    let outcome = decode_outcome(payload.take(outcome_length)?)?;
    payload.finish()?;
    validate_request(&request)?;
    validate_outcome(&request, &outcome)?;

    Ok(ExactRetrievalProof {
        anchor,
        semantics_version,
        request,
        outcome,
        proof_digest: expected_digest,
    })
}

pub(crate) fn encode_lexical_proof(
    proof: &LexicalRetrievalProof,
) -> Result<Vec<u8>, RetrievalProofError> {
    validate_anchor(&proof.anchor)?;
    validate_operation_semantics(proof.semantics_version, LEXICAL_RETRIEVAL_SEMANTICS_VERSION)?;
    validate_lexical_request(&proof.request)?;
    validate_lexical_outcome(&proof.request, &proof.outcome)?;
    encode_envelope(
        LEXICAL_OPERATION,
        proof.semantics_version,
        &proof.anchor,
        &encode_lexical_request(&proof.request)?,
        &encode_lexical_outcome(&proof.outcome)?,
    )
}

pub(crate) fn decode_lexical_proof(
    encoded: &[u8],
) -> Result<LexicalRetrievalProof, RetrievalProofError> {
    let decoded = decode_envelope(
        encoded,
        LEXICAL_OPERATION,
        LEXICAL_RETRIEVAL_SEMANTICS_VERSION,
    )?;
    let request = decode_lexical_request(decoded.request)?;
    let outcome = decode_lexical_outcome(decoded.outcome)?;
    validate_lexical_request(&request)?;
    validate_lexical_outcome(&request, &outcome)?;
    Ok(LexicalRetrievalProof {
        anchor: decoded.anchor,
        semantics_version: decoded.semantics_version,
        request,
        outcome,
        proof_digest: decoded.proof_digest,
    })
}

pub(crate) fn encode_hybrid_proof(
    proof: &HybridRetrievalProof,
) -> Result<Vec<u8>, RetrievalProofError> {
    validate_anchor(&proof.anchor)?;
    validate_operation_semantics(proof.semantics_version, HYBRID_RETRIEVAL_SEMANTICS_VERSION)?;
    validate_lexical_request(&proof.lexical_request)?;
    validate_lexical_outcome(&proof.lexical_request, &proof.lexical_outcome)?;
    validate_request(&proof.vector_request)?;
    validate_outcome(&proof.vector_request, &proof.vector_outcome)?;
    let fused = fuse_hybrid(
        &proof.lexical_outcome,
        &proof.vector_outcome,
        &proof.fusion_request,
    )?;
    if fused != proof.outcome {
        return Err(invalid("hybrid outcome does not match its branch outcomes"));
    }

    let mut request = Encoder::default();
    let lexical_request = encode_lexical_request(&proof.lexical_request)?;
    request.length_u64(lexical_request.len())?;
    request.extend(&lexical_request);
    let lexical_outcome = encode_lexical_outcome(&proof.lexical_outcome)?;
    request.length_u64(lexical_outcome.len())?;
    request.extend(&lexical_outcome);
    let vector_request = encode_request(&proof.vector_request)?;
    request.length_u64(vector_request.len())?;
    request.extend(&vector_request);
    let vector_outcome = encode_outcome(&proof.vector_outcome)?;
    request.length_u64(vector_outcome.len())?;
    request.extend(&vector_outcome);
    encode_hybrid_request(&mut request, &proof.fusion_request)?;

    encode_envelope(
        HYBRID_OPERATION,
        proof.semantics_version,
        &proof.anchor,
        &request.bytes,
        &encode_hybrid_outcome(&proof.outcome)?,
    )
}

pub(crate) fn decode_hybrid_proof(
    encoded: &[u8],
) -> Result<HybridRetrievalProof, RetrievalProofError> {
    let decoded = decode_envelope(
        encoded,
        HYBRID_OPERATION,
        HYBRID_RETRIEVAL_SEMANTICS_VERSION,
    )?;
    let mut request = Decoder::new(decoded.request);
    let lexical_request_length = request.length_u64()?;
    let lexical_request = decode_lexical_request(request.take(lexical_request_length)?)?;
    let lexical_outcome_length = request.length_u64()?;
    let lexical_outcome = decode_lexical_outcome(request.take(lexical_outcome_length)?)?;
    let vector_request_length = request.length_u64()?;
    let vector_request = decode_request(request.take(vector_request_length)?)?;
    let vector_outcome_length = request.length_u64()?;
    let vector_outcome = decode_outcome(request.take(vector_outcome_length)?)?;
    let fusion_request = decode_hybrid_request(&mut request)?;
    request.finish()?;
    let outcome = decode_hybrid_outcome(decoded.outcome)?;

    validate_lexical_request(&lexical_request)?;
    validate_lexical_outcome(&lexical_request, &lexical_outcome)?;
    validate_request(&vector_request)?;
    validate_outcome(&vector_request, &vector_outcome)?;
    let fused = fuse_hybrid(&lexical_outcome, &vector_outcome, &fusion_request)?;
    if fused != outcome {
        return Err(invalid("hybrid outcome does not match its branch outcomes"));
    }
    Ok(HybridRetrievalProof {
        anchor: decoded.anchor,
        semantics_version: decoded.semantics_version,
        lexical_request,
        lexical_outcome,
        vector_request,
        vector_outcome,
        fusion_request,
        outcome,
        proof_digest: decoded.proof_digest,
    })
}

fn encode_envelope(
    operation: u16,
    semantics_version: u16,
    anchor: &RetrievalProofAnchor,
    request: &[u8],
    outcome: &[u8],
) -> Result<Vec<u8>, RetrievalProofError> {
    let mut payload = Encoder::default();
    payload.length_u64(request.len())?;
    payload.extend(request);
    payload.length_u64(outcome.len())?;
    payload.extend(outcome);
    let file_length = HEADER_LENGTH
        .checked_add(payload.bytes.len())
        .ok_or(RetrievalProofError::LengthOverflow)?;
    let file_length_u64 =
        u64::try_from(file_length).map_err(|_| RetrievalProofError::LengthOverflow)?;
    if file_length_u64 > MAX_RETRIEVAL_PROOF_BYTES {
        return Err(RetrievalProofError::ProofLimitExceeded {
            actual: file_length_u64,
            maximum: MAX_RETRIEVAL_PROOF_BYTES,
        });
    }
    let mut header = [0_u8; HEADER_LENGTH];
    header[..8].copy_from_slice(&MAGIC);
    header[8..10].copy_from_slice(&RETRIEVAL_PROOF_FORMAT_VERSION.to_le_bytes());
    header[10..12].copy_from_slice(&0_u16.to_le_bytes());
    header[12..14].copy_from_slice(&operation.to_le_bytes());
    header[14..16].copy_from_slice(&semantics_version.to_le_bytes());
    header[16..24].copy_from_slice(&anchor.checkpoint_sequence.to_le_bytes());
    header[24..56].copy_from_slice(&anchor.checkpoint_digest.unwrap_or([0; 32]));
    header[56..88].copy_from_slice(&anchor.snapshot_digest);
    header[88..96].copy_from_slice(
        &u64::try_from(payload.bytes.len())
            .map_err(|_| RetrievalProofError::LengthOverflow)?
            .to_le_bytes(),
    );
    let checksum = crc32c::crc32c_append(
        crc32c::crc32c(&header[..CHECKSUM_PREFIX_LENGTH]),
        &payload.bytes,
    );
    header[96..100].copy_from_slice(&checksum.to_le_bytes());
    let mut hasher = blake3::Hasher::new();
    hasher.update(DIGEST_DOMAIN);
    hasher.update(&header[..DIGEST_PREFIX_LENGTH]);
    hasher.update(&payload.bytes);
    header[100..132].copy_from_slice(hasher.finalize().as_bytes());
    let mut encoded = Vec::with_capacity(file_length);
    encoded.extend_from_slice(&header);
    encoded.extend_from_slice(&payload.bytes);
    Ok(encoded)
}

struct DecodedEnvelope<'encoded> {
    anchor: RetrievalProofAnchor,
    semantics_version: u16,
    request: &'encoded [u8],
    outcome: &'encoded [u8],
    proof_digest: [u8; 32],
}

fn decode_envelope(
    encoded: &[u8],
    expected_operation: u16,
    supported_semantics: u16,
) -> Result<DecodedEnvelope<'_>, RetrievalProofError> {
    let encoded_length =
        u64::try_from(encoded.len()).map_err(|_| RetrievalProofError::LengthOverflow)?;
    if encoded_length > MAX_RETRIEVAL_PROOF_BYTES {
        return Err(RetrievalProofError::ProofLimitExceeded {
            actual: encoded_length,
            maximum: MAX_RETRIEVAL_PROOF_BYTES,
        });
    }
    if encoded.len() < HEADER_LENGTH {
        return Err(invalid("truncated header"));
    }
    if encoded[..8] != MAGIC {
        return Err(invalid("bad magic"));
    }
    let format_version = u16::from_le_bytes(copy_array(&encoded[8..10]));
    if format_version != RETRIEVAL_PROOF_FORMAT_VERSION {
        return Err(RetrievalProofError::UnsupportedVersion {
            found: format_version,
            supported: RETRIEVAL_PROOF_FORMAT_VERSION,
        });
    }
    if u16::from_le_bytes(copy_array(&encoded[10..12])) != 0 {
        return Err(invalid("unsupported flags"));
    }
    let operation = u16::from_le_bytes(copy_array(&encoded[12..14]));
    if operation != expected_operation {
        return Err(RetrievalProofError::UnsupportedOperation { found: operation });
    }
    let semantics_version = u16::from_le_bytes(copy_array(&encoded[14..16]));
    validate_operation_semantics(semantics_version, supported_semantics)?;
    let payload_length = usize::try_from(u64::from_le_bytes(copy_array(&encoded[88..96])))
        .map_err(|_| RetrievalProofError::LengthOverflow)?;
    let expected_length = HEADER_LENGTH
        .checked_add(payload_length)
        .ok_or(RetrievalProofError::LengthOverflow)?;
    if encoded.len() != expected_length {
        return Err(invalid("file length mismatch"));
    }
    let payload = &encoded[HEADER_LENGTH..];
    let expected_checksum = u32::from_le_bytes(copy_array(&encoded[96..100]));
    let actual_checksum =
        crc32c::crc32c_append(crc32c::crc32c(&encoded[..CHECKSUM_PREFIX_LENGTH]), payload);
    if actual_checksum != expected_checksum {
        return Err(RetrievalProofError::ChecksumMismatch);
    }
    let expected_digest = copy_array(&encoded[100..132]);
    let mut hasher = blake3::Hasher::new();
    hasher.update(DIGEST_DOMAIN);
    hasher.update(&encoded[..DIGEST_PREFIX_LENGTH]);
    hasher.update(payload);
    if *hasher.finalize().as_bytes() != expected_digest {
        return Err(RetrievalProofError::DigestMismatch);
    }
    let checkpoint_sequence = u64::from_le_bytes(copy_array(&encoded[16..24]));
    let raw_checkpoint_digest = copy_array(&encoded[24..56]);
    let checkpoint_digest = if checkpoint_sequence == 0 {
        if raw_checkpoint_digest != [0; 32] {
            return Err(invalid("empty checkpoint has a digest"));
        }
        None
    } else {
        Some(raw_checkpoint_digest)
    };
    let anchor = RetrievalProofAnchor {
        checkpoint_sequence,
        checkpoint_digest,
        snapshot_digest: copy_array(&encoded[56..88]),
    };
    validate_anchor(&anchor)?;
    let mut payload = Decoder::new(payload);
    let request_length = payload.length_u64()?;
    let request = payload.take(request_length)?;
    let outcome_length = payload.length_u64()?;
    let outcome = payload.take(outcome_length)?;
    payload.finish()?;
    Ok(DecodedEnvelope {
        anchor,
        semantics_version,
        request,
        outcome,
        proof_digest: expected_digest,
    })
}

fn encode_lexical_request(request: &LexicalRequest) -> Result<Vec<u8>, RetrievalProofError> {
    let mut encoded = Encoder::default();
    encoded.length_u16(request.index.as_str().len())?;
    encoded.extend(request.index.as_str().as_bytes());
    encoded.length_u32(request.query.len())?;
    encoded.extend(request.query.as_bytes());
    encoded.length_u64(request.limit)?;
    Ok(encoded.bytes)
}

fn decode_lexical_request(encoded: &[u8]) -> Result<LexicalRequest, RetrievalProofError> {
    let mut decoder = Decoder::new(encoded);
    let index_length = decoder.length_u16()?;
    let index = std::str::from_utf8(decoder.take(index_length)?)
        .map_err(|_| invalid("lexical-index name is not UTF-8"))?;
    let index = VectorSpaceName::new(index)?;
    let query_length = decoder.length_u32()?;
    let query = std::str::from_utf8(decoder.take(query_length)?)
        .map_err(|_| invalid("lexical query is not UTF-8"))?
        .to_owned();
    let limit = decoder.length_u64()?;
    decoder.finish()?;
    Ok(LexicalRequest {
        index,
        query,
        limit,
    })
}

fn encode_lexical_outcome(outcome: &LexicalOutcome) -> Result<Vec<u8>, RetrievalProofError> {
    let mut encoded = Encoder::default();
    match outcome {
        LexicalOutcome::Matches {
            matches,
            scanned_documents,
            matched_documents,
            query_tokens,
        } => {
            encoded.byte(MATCHES_OUTCOME);
            encoded.u64(*scanned_documents);
            encoded.u64(*matched_documents);
            encode_strings(&mut encoded, query_tokens)?;
            encoded.length_u64(matches.len())?;
            for matched in matches {
                encoded.length_u32(matched.key.len())?;
                encoded.extend(&matched.key);
                encoded.i64(matched.score_nanos);
                encoded.length_u64(matched.terms.len())?;
                for term in &matched.terms {
                    encoded.length_u32(term.token.len())?;
                    encoded.extend(term.token.as_bytes());
                    encoded.u64(term.document_frequency);
                    encoded.i64(term.score_nanos);
                    encoded.length_u64(term.fields.len())?;
                    for field in &term.fields {
                        encode_path(&mut encoded, &field.path)?;
                        encoded.u64(field.term_frequency);
                        encoded.u64(field.field_length);
                    }
                }
            }
        }
        LexicalOutcome::Abstained(abstention) => {
            encoded.byte(ABSTAINED_OUTCOME);
            encoded.byte(match abstention.reason {
                LexicalAbstentionReason::NoCandidates => 1,
            });
            encoded.u64(abstention.scanned_documents);
            encode_strings(&mut encoded, &abstention.query_tokens)?;
        }
    }
    Ok(encoded.bytes)
}

fn decode_lexical_outcome(encoded: &[u8]) -> Result<LexicalOutcome, RetrievalProofError> {
    let mut decoder = Decoder::new(encoded);
    let outcome = match decoder.byte()? {
        MATCHES_OUTCOME => {
            let scanned_documents = decoder.u64()?;
            let matched_documents = decoder.u64()?;
            let query_tokens = decode_strings(&mut decoder)?;
            let count = decoder.count_u64()?;
            let mut matches = Vec::with_capacity(count);
            for _ in 0..count {
                let key_length = decoder.length_u32()?;
                let key = decoder.take(key_length)?.to_vec();
                let score_nanos = decoder.i64()?;
                let term_count = decoder.count_u64()?;
                let mut terms = Vec::with_capacity(term_count);
                for _ in 0..term_count {
                    let token_length = decoder.length_u32()?;
                    let token = std::str::from_utf8(decoder.take(token_length)?)
                        .map_err(|_| invalid("lexical term is not UTF-8"))?
                        .to_owned();
                    let document_frequency = decoder.u64()?;
                    let score_nanos = decoder.i64()?;
                    let field_count = decoder.count_u64()?;
                    let mut fields = Vec::with_capacity(field_count);
                    for _ in 0..field_count {
                        fields.push(LexicalFieldContribution {
                            path: decode_path(&mut decoder)?,
                            term_frequency: decoder.u64()?,
                            field_length: decoder.u64()?,
                        });
                    }
                    terms.push(LexicalTermContribution {
                        token,
                        document_frequency,
                        score_nanos,
                        fields,
                    });
                }
                matches.push(LexicalMatch {
                    key,
                    score_nanos,
                    terms,
                });
            }
            LexicalOutcome::Matches {
                matches,
                scanned_documents,
                matched_documents,
                query_tokens,
            }
        }
        ABSTAINED_OUTCOME => {
            let reason = match decoder.byte()? {
                1 => LexicalAbstentionReason::NoCandidates,
                _ => return Err(invalid("unknown lexical abstention reason")),
            };
            LexicalOutcome::Abstained(LexicalAbstention {
                reason,
                scanned_documents: decoder.u64()?,
                query_tokens: decode_strings(&mut decoder)?,
            })
        }
        _ => return Err(invalid("unknown lexical outcome tag")),
    };
    decoder.finish()?;
    Ok(outcome)
}

fn encode_hybrid_request(
    encoded: &mut Encoder,
    request: &HybridRequest,
) -> Result<(), RetrievalProofError> {
    encoded.u32(request.lexical_weight);
    encoded.u32(request.vector_weight);
    encoded.length_u64(request.limit)
}

fn decode_hybrid_request(decoder: &mut Decoder<'_>) -> Result<HybridRequest, RetrievalProofError> {
    Ok(HybridRequest {
        lexical_weight: decoder.u32()?,
        vector_weight: decoder.u32()?,
        limit: decoder.length_u64()?,
    })
}

fn encode_hybrid_outcome(outcome: &HybridOutcome) -> Result<Vec<u8>, RetrievalProofError> {
    let mut encoded = Encoder::default();
    match outcome {
        HybridOutcome::Matches {
            matches,
            lexical_absence,
            vector_absence,
        } => {
            encoded.byte(MATCHES_OUTCOME);
            encode_optional_absence(&mut encoded, *lexical_absence);
            encode_optional_absence(&mut encoded, *vector_absence);
            encoded.length_u64(matches.len())?;
            for matched in matches {
                encoded.length_u32(matched.key.len())?;
                encoded.extend(&matched.key);
                encode_optional_u64(&mut encoded, matched.explanation.lexical_rank);
                encode_optional_i64(&mut encoded, matched.explanation.lexical_score_nanos);
                encode_optional_u64(&mut encoded, matched.explanation.vector_rank);
                encode_optional_i64(&mut encoded, matched.explanation.vector_score_nanos);
                encoded.u64(matched.explanation.lexical_contribution);
                encoded.u64(matched.explanation.vector_contribution);
                encoded.u64(matched.explanation.fusion_score);
                encoded.u64(matched.explanation.final_rank);
            }
        }
        HybridOutcome::Abstained(abstention) => {
            encoded.byte(ABSTAINED_OUTCOME);
            encoded.byte(encode_absence(abstention.lexical));
            encoded.byte(encode_absence(abstention.vector));
        }
    }
    Ok(encoded.bytes)
}

fn decode_hybrid_outcome(encoded: &[u8]) -> Result<HybridOutcome, RetrievalProofError> {
    let mut decoder = Decoder::new(encoded);
    let outcome = match decoder.byte()? {
        MATCHES_OUTCOME => {
            let lexical_absence = decode_optional_absence(&mut decoder)?;
            let vector_absence = decode_optional_absence(&mut decoder)?;
            let count = decoder.count_u64()?;
            let mut matches = Vec::with_capacity(count);
            for _ in 0..count {
                let key_length = decoder.length_u32()?;
                let key = decoder.take(key_length)?.to_vec();
                matches.push(HybridMatch {
                    key,
                    explanation: HybridExplanation {
                        lexical_rank: decode_optional_u64(&mut decoder)?,
                        lexical_score_nanos: decode_optional_i64(&mut decoder)?,
                        vector_rank: decode_optional_u64(&mut decoder)?,
                        vector_score_nanos: decode_optional_i64(&mut decoder)?,
                        lexical_contribution: decoder.u64()?,
                        vector_contribution: decoder.u64()?,
                        fusion_score: decoder.u64()?,
                        final_rank: decoder.u64()?,
                    },
                });
            }
            HybridOutcome::Matches {
                matches,
                lexical_absence,
                vector_absence,
            }
        }
        ABSTAINED_OUTCOME => HybridOutcome::Abstained(HybridAbstention {
            lexical: decode_absence(decoder.byte()?)?,
            vector: decode_absence(decoder.byte()?)?,
        }),
        _ => return Err(invalid("unknown hybrid outcome tag")),
    };
    decoder.finish()?;
    Ok(outcome)
}

fn encode_strings(encoded: &mut Encoder, strings: &[String]) -> Result<(), RetrievalProofError> {
    encoded.length_u64(strings.len())?;
    for value in strings {
        encoded.length_u32(value.len())?;
        encoded.extend(value.as_bytes());
    }
    Ok(())
}

fn decode_strings(decoder: &mut Decoder<'_>) -> Result<Vec<String>, RetrievalProofError> {
    let count = decoder.count_u64()?;
    let mut strings = Vec::with_capacity(count);
    for _ in 0..count {
        let length = decoder.length_u32()?;
        strings.push(
            std::str::from_utf8(decoder.take(length)?)
                .map_err(|_| invalid("lexical token is not UTF-8"))?
                .to_owned(),
        );
    }
    Ok(strings)
}

fn encode_path(encoded: &mut Encoder, path: &FieldPath) -> Result<(), RetrievalProofError> {
    encoded.length_u16(path.segments().len())?;
    for segment in path.segments() {
        encoded.length_u16(segment.len())?;
        encoded.extend(segment.as_bytes());
    }
    Ok(())
}

fn decode_path(decoder: &mut Decoder<'_>) -> Result<FieldPath, RetrievalProofError> {
    let count = decoder.length_u16()?;
    if count == 0 || count > MAX_LEXICAL_PATH_SEGMENTS {
        return Err(invalid("invalid lexical field path length"));
    }
    let mut segments = Vec::with_capacity(count);
    for _ in 0..count {
        let length = decoder.length_u16()?;
        if length == 0 || length > MAX_LEXICAL_PATH_SEGMENT_BYTES {
            return Err(invalid("invalid lexical field path segment"));
        }
        segments.push(
            std::str::from_utf8(decoder.take(length)?)
                .map_err(|_| invalid("lexical field path is not UTF-8"))?
                .to_owned(),
        );
    }
    Ok(FieldPath::new(segments))
}

fn encode_optional_absence(encoded: &mut Encoder, value: Option<HybridBranchAbsence>) {
    match value {
        None => encoded.byte(ABSENT),
        Some(value) => {
            encoded.byte(PRESENT);
            encoded.byte(encode_absence(value));
        }
    }
}

fn decode_optional_absence(
    decoder: &mut Decoder<'_>,
) -> Result<Option<HybridBranchAbsence>, RetrievalProofError> {
    match decoder.byte()? {
        ABSENT => Ok(None),
        PRESENT => Ok(Some(decode_absence(decoder.byte()?)?)),
        _ => Err(invalid("invalid optional hybrid absence tag")),
    }
}

fn encode_absence(value: HybridBranchAbsence) -> u8 {
    match value {
        HybridBranchAbsence::LexicalNoCandidates => 1,
        HybridBranchAbsence::VectorNoCandidates => 2,
        HybridBranchAbsence::VectorBelowThreshold => 3,
        HybridBranchAbsence::VectorAmbiguous => 4,
    }
}

fn decode_absence(value: u8) -> Result<HybridBranchAbsence, RetrievalProofError> {
    match value {
        1 => Ok(HybridBranchAbsence::LexicalNoCandidates),
        2 => Ok(HybridBranchAbsence::VectorNoCandidates),
        3 => Ok(HybridBranchAbsence::VectorBelowThreshold),
        4 => Ok(HybridBranchAbsence::VectorAmbiguous),
        _ => Err(invalid("unknown hybrid branch absence")),
    }
}

fn encode_optional_u64(encoded: &mut Encoder, value: Option<u64>) {
    match value {
        None => encoded.byte(ABSENT),
        Some(value) => {
            encoded.byte(PRESENT);
            encoded.u64(value);
        }
    }
}

fn decode_optional_u64(decoder: &mut Decoder<'_>) -> Result<Option<u64>, RetrievalProofError> {
    match decoder.byte()? {
        ABSENT => Ok(None),
        PRESENT => Ok(Some(decoder.u64()?)),
        _ => Err(invalid("invalid optional u64 tag")),
    }
}

fn encode_optional_i64(encoded: &mut Encoder, value: Option<i64>) {
    match value {
        None => encoded.byte(ABSENT),
        Some(value) => {
            encoded.byte(PRESENT);
            encoded.i64(value);
        }
    }
}

fn decode_optional_i64(decoder: &mut Decoder<'_>) -> Result<Option<i64>, RetrievalProofError> {
    match decoder.byte()? {
        ABSENT => Ok(None),
        PRESENT => Ok(Some(decoder.i64()?)),
        _ => Err(invalid("invalid optional i64 tag")),
    }
}

fn encode_request(request: &ExactRetrievalRequest) -> Result<Vec<u8>, RetrievalProofError> {
    let mut encoded = Encoder::default();
    encoded.length_u16(request.vector_space.as_str().len())?;
    encoded.extend(request.vector_space.as_str().as_bytes());
    encoded.u16(request.query.dimension());
    for value in request.query.as_slice() {
        encoded.i16(*value);
    }
    encoded.length_u64(request.limit)?;
    encoded.i64(request.minimum_score_nanos);
    encoded.u64(request.minimum_margin_nanos);
    Ok(encoded.bytes)
}

fn decode_request(encoded: &[u8]) -> Result<ExactRetrievalRequest, RetrievalProofError> {
    let mut decoder = Decoder::new(encoded);
    let name_length = decoder.length_u16()?;
    let name = std::str::from_utf8(decoder.take(name_length)?)
        .map_err(|_| invalid("vector-space name is not UTF-8"))?;
    let vector_space = VectorSpaceName::new(name)?;
    let dimension = decoder.u16()?;
    let mut values = Vec::with_capacity(usize::from(dimension));
    for _ in 0..dimension {
        values.push(decoder.i16()?);
    }
    let query = Q15Vector::new(values)?;
    if query.dimension() != dimension {
        return Err(invalid("query dimension mismatch"));
    }
    let limit = decoder.length_u64()?;
    let minimum_score_nanos = decoder.i64()?;
    let minimum_margin_nanos = decoder.u64()?;
    decoder.finish()?;
    Ok(ExactRetrievalRequest {
        vector_space,
        query,
        limit,
        minimum_score_nanos,
        minimum_margin_nanos,
    })
}

fn encode_outcome(outcome: &ExactRetrievalOutcome) -> Result<Vec<u8>, RetrievalProofError> {
    let mut encoded = Encoder::default();
    match outcome {
        ExactRetrievalOutcome::Matches {
            matches,
            scanned_candidates,
        } => {
            encoded.byte(MATCHES_OUTCOME);
            encoded.u64(*scanned_candidates);
            encoded.length_u64(matches.len())?;
            for matched in matches {
                encoded.length_u32(matched.key.len())?;
                encoded.extend(&matched.key);
                encoded.i64(matched.score_nanos);
            }
        }
        ExactRetrievalOutcome::Abstained(abstention) => {
            encoded.byte(ABSTAINED_OUTCOME);
            encoded.u64(abstention.scanned_candidates);
            encoded.byte(match abstention.reason {
                ExactAbstentionReason::NoCandidates => 1,
                ExactAbstentionReason::BelowThreshold => 2,
                ExactAbstentionReason::Ambiguous => 3,
            });
            encode_optional_score(&mut encoded, abstention.best_score_nanos);
            encode_optional_score(&mut encoded, abstention.runner_up_score_nanos);
        }
    }
    Ok(encoded.bytes)
}

fn decode_outcome(encoded: &[u8]) -> Result<ExactRetrievalOutcome, RetrievalProofError> {
    let mut decoder = Decoder::new(encoded);
    let tag = decoder.byte()?;
    let scanned_candidates = decoder.u64()?;
    let outcome = match tag {
        MATCHES_OUTCOME => {
            let count = decoder.length_u64()?;
            let mut matches = Vec::with_capacity(count);
            for _ in 0..count {
                let key_length = decoder.length_u32()?;
                let key = decoder.take(key_length)?.to_vec();
                let score_nanos = decoder.i64()?;
                matches.push(ExactRetrievalMatch { key, score_nanos });
            }
            ExactRetrievalOutcome::Matches {
                matches,
                scanned_candidates,
            }
        }
        ABSTAINED_OUTCOME => {
            let reason = match decoder.byte()? {
                1 => ExactAbstentionReason::NoCandidates,
                2 => ExactAbstentionReason::BelowThreshold,
                3 => ExactAbstentionReason::Ambiguous,
                _ => return Err(invalid("unknown abstention reason")),
            };
            ExactRetrievalOutcome::Abstained(ExactAbstention {
                reason,
                best_score_nanos: decode_optional_score(&mut decoder)?,
                runner_up_score_nanos: decode_optional_score(&mut decoder)?,
                scanned_candidates,
            })
        }
        _ => return Err(invalid("unknown outcome tag")),
    };
    decoder.finish()?;
    Ok(outcome)
}

fn encode_optional_score(encoded: &mut Encoder, score: Option<i64>) {
    match score {
        None => encoded.byte(ABSENT),
        Some(score) => {
            encoded.byte(PRESENT);
            encoded.i64(score);
        }
    }
}

fn decode_optional_score(decoder: &mut Decoder<'_>) -> Result<Option<i64>, RetrievalProofError> {
    match decoder.byte()? {
        ABSENT => Ok(None),
        PRESENT => Ok(Some(decoder.i64()?)),
        _ => Err(invalid("invalid optional score tag")),
    }
}

fn validate_anchor(anchor: &RetrievalProofAnchor) -> Result<(), RetrievalProofError> {
    if (anchor.checkpoint_sequence == 0) != anchor.checkpoint_digest.is_none() {
        return Err(invalid("noncanonical checkpoint identity"));
    }
    Ok(())
}

fn validate_semantics(version: u16) -> Result<(), RetrievalProofError> {
    validate_operation_semantics(version, EXACT_RETRIEVAL_SEMANTICS_VERSION)
}

fn validate_operation_semantics(version: u16, supported: u16) -> Result<(), RetrievalProofError> {
    if version != supported {
        return Err(RetrievalProofError::UnsupportedSemantics {
            found: version,
            supported,
        });
    }
    Ok(())
}

fn validate_lexical_request(request: &LexicalRequest) -> Result<(), RetrievalProofError> {
    let tokens = tokenize_v1(&request.query);
    if request.limit == 0 || tokens.is_empty() {
        return Err(invalid("invalid lexical request"));
    }
    Ok(())
}

fn validate_lexical_outcome(
    request: &LexicalRequest,
    outcome: &LexicalOutcome,
) -> Result<(), RetrievalProofError> {
    let expected_tokens = tokenize_v1(&request.query)
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let (matches, scanned_documents, matched_documents, query_tokens) = match outcome {
        LexicalOutcome::Matches {
            matches,
            scanned_documents,
            matched_documents,
            query_tokens,
        } => (
            Some(matches.as_slice()),
            *scanned_documents,
            Some(*matched_documents),
            query_tokens,
        ),
        LexicalOutcome::Abstained(abstention) => {
            if abstention.reason != LexicalAbstentionReason::NoCandidates {
                return Err(invalid("invalid lexical abstention"));
            }
            (
                None,
                abstention.scanned_documents,
                None,
                &abstention.query_tokens,
            )
        }
    };
    if query_tokens != &expected_tokens {
        return Err(invalid("noncanonical lexical query tokens"));
    }
    if let Some(matches) = matches {
        if matches.is_empty()
            || matches.len() > request.limit
            || matched_documents.unwrap_or(0) < u64::try_from(matches.len()).unwrap_or(u64::MAX)
            || matched_documents.unwrap_or(0) > scanned_documents
        {
            return Err(invalid("invalid lexical match counts"));
        }
        let mut previous: Option<&LexicalMatch> = None;
        for matched in matches {
            if matched.key.is_empty()
                || matched.key.len() > MAX_KEY_BYTES
                || matched.score_nanos <= 0
            {
                return Err(invalid("invalid lexical match"));
            }
            if let Some(previous) = previous
                && (previous.score_nanos < matched.score_nanos
                    || (previous.score_nanos == matched.score_nanos && previous.key >= matched.key))
            {
                return Err(invalid("lexical matches are not canonically ordered"));
            }
            let term_sum = matched
                .terms
                .iter()
                .try_fold(0_i64, |sum, term| sum.checked_add(term.score_nanos))
                .ok_or_else(|| invalid("lexical score overflow"))?;
            if term_sum != matched.score_nanos {
                return Err(invalid("lexical term scores do not sum to match score"));
            }
            let mut previous_token: Option<&str> = None;
            for term in &matched.terms {
                if term.token.is_empty()
                    || term.token.len() > MAX_LEXICAL_TOKEN_BYTES
                    || term.document_frequency == 0
                    || term.document_frequency > scanned_documents
                    || term.score_nanos <= 0
                    || previous_token.is_some_and(|previous| previous >= term.token.as_str())
                {
                    return Err(invalid("invalid lexical term contribution"));
                }
                previous_token = Some(&term.token);
                let mut previous_path: Option<&FieldPath> = None;
                for field in &term.fields {
                    if field.path.segments().is_empty()
                        || field.path.segments().len() > MAX_LEXICAL_PATH_SEGMENTS
                        || previous_path.is_some_and(|previous| previous >= &field.path)
                    {
                        return Err(invalid("invalid lexical field contribution"));
                    }
                    previous_path = Some(&field.path);
                }
            }
            previous = Some(matched);
        }
    }
    Ok(())
}

fn validate_request(request: &ExactRetrievalRequest) -> Result<(), RetrievalProofError> {
    let limits = ExactRetrievalLimits {
        max_candidates: u64::MAX,
        max_candidate_bytes: u64::MAX,
        max_returned: usize::MAX,
        timeout: Duration::from_secs(1),
    };
    let _outcome = retrieve_exact(&[], request, &limits)?;
    Ok(())
}

fn validate_outcome(
    request: &ExactRetrievalRequest,
    outcome: &ExactRetrievalOutcome,
) -> Result<(), RetrievalProofError> {
    match outcome {
        ExactRetrievalOutcome::Matches {
            matches,
            scanned_candidates,
        } => {
            if matches.is_empty()
                || matches.len() > request.limit
                || u64::try_from(matches.len()).unwrap_or(u64::MAX) > *scanned_candidates
            {
                return Err(invalid("invalid match count"));
            }
            let mut previous: Option<&ExactRetrievalMatch> = None;
            for matched in matches {
                if matched.key.is_empty()
                    || matched.key.len() > MAX_KEY_BYTES
                    || !(-SCORE_NANOS_SCALE..=SCORE_NANOS_SCALE).contains(&matched.score_nanos)
                {
                    return Err(invalid("invalid exact match"));
                }
                if let Some(previous) = previous
                    && (previous.score_nanos < matched.score_nanos
                        || (previous.score_nanos == matched.score_nanos
                            && previous.key >= matched.key))
                {
                    return Err(invalid("matches are not canonically ordered"));
                }
                previous = Some(matched);
            }
        }
        ExactRetrievalOutcome::Abstained(abstention) => {
            for score in [
                abstention.best_score_nanos,
                abstention.runner_up_score_nanos,
            ]
            .into_iter()
            .flatten()
            {
                if !(-SCORE_NANOS_SCALE..=SCORE_NANOS_SCALE).contains(&score) {
                    return Err(invalid("invalid abstention score"));
                }
            }
            match abstention.reason {
                ExactAbstentionReason::NoCandidates
                    if abstention.scanned_candidates == 0
                        && abstention.best_score_nanos.is_none()
                        && abstention.runner_up_score_nanos.is_none() => {}
                ExactAbstentionReason::BelowThreshold
                    if abstention.scanned_candidates > 0
                        && abstention.best_score_nanos.is_some() => {}
                ExactAbstentionReason::Ambiguous
                    if abstention.scanned_candidates > 1
                        && abstention.best_score_nanos.is_some()
                        && abstention.runner_up_score_nanos.is_some() => {}
                _ => return Err(invalid("noncanonical abstention evidence")),
            }
        }
    }
    Ok(())
}

#[derive(Default)]
struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    fn byte(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.extend(&value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.extend(&value.to_le_bytes());
    }

    fn i16(&mut self, value: i16) {
        self.extend(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.extend(&value.to_le_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.extend(&value.to_le_bytes());
    }

    fn extend(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    fn length_u16(&mut self, value: usize) -> Result<(), RetrievalProofError> {
        self.u16(u16::try_from(value).map_err(|_| RetrievalProofError::LengthOverflow)?);
        Ok(())
    }

    fn length_u32(&mut self, value: usize) -> Result<(), RetrievalProofError> {
        self.extend(
            &u32::try_from(value)
                .map_err(|_| RetrievalProofError::LengthOverflow)?
                .to_le_bytes(),
        );
        Ok(())
    }

    fn length_u64(&mut self, value: usize) -> Result<(), RetrievalProofError> {
        self.u64(u64::try_from(value).map_err(|_| RetrievalProofError::LengthOverflow)?);
        Ok(())
    }
}

struct Decoder<'encoded> {
    encoded: &'encoded [u8],
    offset: usize,
}

impl<'encoded> Decoder<'encoded> {
    fn new(encoded: &'encoded [u8]) -> Self {
        Self { encoded, offset: 0 }
    }

    fn take(&mut self, length: usize) -> Result<&'encoded [u8], RetrievalProofError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(RetrievalProofError::LengthOverflow)?;
        let value = self
            .encoded
            .get(self.offset..end)
            .ok_or_else(|| invalid("truncated payload"))?;
        self.offset = end;
        Ok(value)
    }

    fn byte(&mut self) -> Result<u8, RetrievalProofError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, RetrievalProofError> {
        Ok(u16::from_le_bytes(copy_array(self.take(2)?)))
    }

    fn u32(&mut self) -> Result<u32, RetrievalProofError> {
        Ok(u32::from_le_bytes(copy_array(self.take(4)?)))
    }

    fn i16(&mut self) -> Result<i16, RetrievalProofError> {
        Ok(i16::from_le_bytes(copy_array(self.take(2)?)))
    }

    fn u64(&mut self) -> Result<u64, RetrievalProofError> {
        Ok(u64::from_le_bytes(copy_array(self.take(8)?)))
    }

    fn i64(&mut self) -> Result<i64, RetrievalProofError> {
        Ok(i64::from_le_bytes(copy_array(self.take(8)?)))
    }

    fn length_u16(&mut self) -> Result<usize, RetrievalProofError> {
        Ok(usize::from(self.u16()?))
    }

    fn length_u32(&mut self) -> Result<usize, RetrievalProofError> {
        usize::try_from(u32::from_le_bytes(copy_array(self.take(4)?)))
            .map_err(|_| RetrievalProofError::LengthOverflow)
    }

    fn length_u64(&mut self) -> Result<usize, RetrievalProofError> {
        usize::try_from(self.u64()?).map_err(|_| RetrievalProofError::LengthOverflow)
    }

    fn count_u64(&mut self) -> Result<usize, RetrievalProofError> {
        let count = self.length_u64()?;
        if count > MAX_PROOF_COLLECTION_ITEMS {
            return Err(invalid("proof collection count exceeds hard bound"));
        }
        Ok(count)
    }

    fn finish(self) -> Result<(), RetrievalProofError> {
        if self.offset == self.encoded.len() {
            Ok(())
        } else {
            Err(invalid("trailing payload bytes"))
        }
    }
}

fn invalid(reason: &'static str) -> RetrievalProofError {
    RetrievalProofError::Invalid { reason }
}

fn copy_array<const LENGTH: usize>(bytes: &[u8]) -> [u8; LENGTH] {
    let mut copied = [0; LENGTH];
    copied.copy_from_slice(bytes);
    copied
}

#[cfg(test)]
mod tests {
    use hyphae_core::{Q15Vector, VectorSpaceName};
    use hyphae_retrieval::{ExactRetrievalMatch, ExactRetrievalOutcome, ExactRetrievalRequest};

    use super::{decode_proof, encode_proof, finalize_proof};
    use crate::RetrievalProofAnchor;

    #[test]
    fn exact_proof_codec_round_trips_and_detects_bit_flip() -> Result<(), Box<dyn std::error::Error>>
    {
        let proof = finalize_proof(
            RetrievalProofAnchor {
                checkpoint_sequence: 1,
                checkpoint_digest: Some([7; 32]),
                snapshot_digest: [9; 32],
            },
            ExactRetrievalRequest {
                vector_space: VectorSpaceName::new("semantic")?,
                query: Q15Vector::new(vec![32_767, 1])?,
                limit: 2,
                minimum_score_nanos: 0,
                minimum_margin_nanos: 7,
            },
            ExactRetrievalOutcome::Matches {
                matches: vec![ExactRetrievalMatch {
                    key: b"alpha".to_vec(),
                    score_nanos: 999_999_999,
                }],
                scanned_candidates: 2,
            },
        )?;
        let encoded = encode_proof(&proof)?;
        assert_eq!(decode_proof(&encoded)?, proof);

        let mut corrupted = encoded;
        let last = corrupted.len() - 1;
        corrupted[last] ^= 1;
        assert!(decode_proof(&corrupted).is_err());
        Ok(())
    }
}
