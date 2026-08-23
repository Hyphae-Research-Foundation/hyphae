// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;

use super::crypto::{blake3_parts, crc32c_parts};
use super::model::{
    AdmittedProofLimits, AnnFilterStrategy, AnnProofMetadata, ApproximationLabel, CanonicalBytes,
    CompletionStatus, HybridBranchBinding, HybridDuplicatePolicy, HybridFailurePolicy,
    HybridFusionMethod, HybridProofMetadata, MAX_NATIVE_PROOF_BYTES, NATIVE_PROOF_VERSION,
    NativeProof, NativeProofAnchor, NativeProofContent, NativeProofError, NativeProofKind,
    NativeWitnessReference, ProofCodecLimits, ProofObjectBinding, VectorMetric,
    canonical_bytes_digest, limit,
};

pub(crate) const PROOF_MAGIC: [u8; 8] = *b"HYNPRF02";
pub(crate) const HEADER_BYTES: usize = 64;
const PROOF_DIGEST_OFFSET: usize = 32;
const PROOF_DOMAIN: &[u8] = b"hyphae-native-proof-envelope-v2";
const ANCHOR_BYTES: usize = 120;

/// Encodes one validated proof into its canonical `HYNPRF02` representation.
///
/// # Errors
///
/// Returns an error when the model is invalid or a configured bound is exceeded.
pub fn encode_native_proof(
    proof: &NativeProof,
    limits: &ProofCodecLimits,
) -> Result<Vec<u8>, NativeProofError> {
    encode_content(&proof.content, limits)
}

/// Decodes and validates one complete canonical `HYNPRF02` artifact.
///
/// # Errors
///
/// Rejects truncation, trailing bytes, noncanonical values, integrity failures, and limits.
pub fn decode_native_proof(
    encoded: &[u8],
    limits: &ProofCodecLimits,
) -> Result<NativeProof, NativeProofError> {
    check_encoded_limit(
        encoded.len(),
        limits.max_proof_bytes.min(MAX_NATIVE_PROOF_BYTES),
        "proof bytes",
    )?;
    if encoded.len() < HEADER_BYTES {
        return Err(NativeProofError::Invalid("truncated proof header"));
    }
    if encoded[..8] != PROOF_MAGIC {
        return Err(NativeProofError::Invalid("bad proof magic"));
    }
    check_version(read_u16(&encoded[8..10]))?;
    if read_u16(&encoded[10..12]) != 0 || encoded[14..16] != [0, 0] {
        return Err(NativeProofError::Invalid("unsupported proof flags"));
    }
    let kind = decode_kind(encoded[12])?;
    let completion = decode_completion(encoded[13])?;
    let payload_length = read_length(&encoded[16..24])?;
    let expected_length = HEADER_BYTES
        .checked_add(payload_length)
        .ok_or(NativeProofError::LengthOverflow)?;
    if encoded.len() != expected_length {
        return Err(NativeProofError::Invalid("proof file length mismatch"));
    }
    if encoded[28..32] != [0; 4] {
        return Err(NativeProofError::Invalid("nonzero proof reserved bytes"));
    }
    let payload = &encoded[HEADER_BYTES..];
    verify_envelope(encoded, payload, PROOF_DOMAIN)?;

    let mut decoder = Decoder::new(payload);
    let anchor = decode_anchor(&mut decoder)?;
    let semantics_version = decoder.u16()?;
    let ordering_version = decoder.u16()?;
    let limits_model = AdmittedProofLimits {
        result_items: decoder.u64()?,
        candidate_items: decoder.u64()?,
        evidence_bytes: decoder.u64()?,
    };
    let witness = NativeWitnessReference {
        digest: decoder.array()?,
        file_bytes: decoder.u64()?,
    };
    let object_count = decoder.count(limits.max_objects, "proof objects")?;
    let mut objects = Vec::new();
    objects
        .try_reserve_exact(object_count)
        .map_err(|_| NativeProofError::LengthOverflow)?;
    for _ in 0..object_count {
        objects.push(ProofObjectBinding {
            object_id: decoder.u128()?,
            definition_digest: decoder.array()?,
        });
    }

    let mut decoded_section_bytes = 0_u64;
    let request = decoder.canonical_section(limits, &mut decoded_section_bytes)?;
    let result = decoder.canonical_section(limits, &mut decoded_section_bytes)?;
    let evidence = decoder.canonical_section(limits, &mut decoded_section_bytes)?;
    let (ann, hybrid) = match kind {
        NativeProofKind::Ann => (Some(decode_ann(&mut decoder)?), None),
        NativeProofKind::Hybrid => (
            None,
            Some(decode_hybrid(&mut decoder, limits.max_hybrid_branches)?),
        ),
        _ => (None, None),
    };
    decoder.finish()?;

    let content = NativeProofContent {
        kind,
        anchor,
        semantics_version,
        ordering_version,
        objects,
        request,
        result,
        evidence,
        limits: limits_model,
        completion,
        witness,
        ann,
        hybrid,
    };
    validate_content(&content, limits)?;
    let canonical = encode_content(&content, limits)?;
    if canonical != encoded {
        return Err(NativeProofError::Invalid("noncanonical proof encoding"));
    }
    Ok(NativeProof {
        content,
        proof_digest: copy_array(&encoded[PROOF_DIGEST_OFFSET..HEADER_BYTES]),
    })
}

pub(crate) fn finalize_proof(
    content: NativeProofContent,
    limits: &ProofCodecLimits,
) -> Result<NativeProof, NativeProofError> {
    let encoded = encode_content(&content, limits)?;
    Ok(NativeProof {
        content,
        proof_digest: copy_array(&encoded[PROOF_DIGEST_OFFSET..HEADER_BYTES]),
    })
}

fn encode_content(
    content: &NativeProofContent,
    limits: &ProofCodecLimits,
) -> Result<Vec<u8>, NativeProofError> {
    validate_content(content, limits)?;
    let mut payload = Encoder::default();
    encode_anchor(&mut payload, content.anchor);
    payload.u16(content.semantics_version);
    payload.u16(content.ordering_version);
    payload.u64(content.limits.result_items);
    payload.u64(content.limits.candidate_items);
    payload.u64(content.limits.evidence_bytes);
    payload.extend(&content.witness.digest);
    payload.u64(content.witness.file_bytes);
    payload.count(content.objects.len())?;
    for object in &content.objects {
        payload.u128(object.object_id);
        payload.extend(&object.definition_digest);
    }
    payload.canonical_section(&content.request)?;
    payload.canonical_section(&content.result)?;
    payload.canonical_section(&content.evidence)?;
    if let Some(ann) = &content.ann {
        encode_ann(&mut payload, ann);
    }
    if let Some(hybrid) = &content.hybrid {
        encode_hybrid(&mut payload, hybrid)?;
    }
    seal_envelope(
        PROOF_MAGIC,
        content.kind as u8,
        content.completion as u8,
        &payload.bytes,
        limits.max_proof_bytes.min(MAX_NATIVE_PROOF_BYTES),
        "proof bytes",
        PROOF_DOMAIN,
    )
}

fn validate_content(
    content: &NativeProofContent,
    limits: &ProofCodecLimits,
) -> Result<(), NativeProofError> {
    validate_anchor(content.anchor)?;
    if content.semantics_version == 0 || content.ordering_version == 0 {
        return Err(NativeProofError::Invalid(
            "proof semantics and ordering versions must be nonzero",
        ));
    }
    if content.witness.digest == [0; 32] || content.witness.file_bytes < HEADER_BYTES as u64 {
        return Err(NativeProofError::Invalid("invalid witness reference"));
    }
    if content.objects.len() > limits.max_objects {
        return Err(limit(
            "proof objects",
            content.objects.len(),
            limits.max_objects,
        ));
    }
    let mut prior = None;
    for object in &content.objects {
        if object.object_id == 0 || prior.is_some_and(|value| value >= object.object_id) {
            return Err(NativeProofError::Invalid(
                "proof objects are not sorted unique nonzero identities",
            ));
        }
        prior = Some(object.object_id);
    }
    let mut decoded = 0_u64;
    for section in [&content.request, &content.result, &content.evidence] {
        let length =
            u64::try_from(section.bytes.len()).map_err(|_| NativeProofError::LengthOverflow)?;
        if length > limits.max_section_bytes {
            return Err(limit(
                "canonical section bytes",
                length,
                limits.max_section_bytes,
            ));
        }
        decoded = decoded
            .checked_add(length)
            .ok_or(NativeProofError::LengthOverflow)?;
        if section.digest != canonical_bytes_digest(&section.bytes) {
            return Err(NativeProofError::DigestMismatch("canonical section"));
        }
    }
    if decoded > limits.max_decoded_bytes {
        return Err(limit(
            "proof decoded bytes",
            decoded,
            limits.max_decoded_bytes,
        ));
    }
    let evidence_length = u64::try_from(content.evidence.bytes.len())
        .map_err(|_| NativeProofError::LengthOverflow)?;
    if evidence_length > content.limits.evidence_bytes {
        return Err(NativeProofError::Invalid(
            "evidence exceeds the admitted proof limit",
        ));
    }
    match (content.kind, content.ann.as_ref(), content.hybrid.as_ref()) {
        (NativeProofKind::Ann, Some(ann), None) => validate_ann(ann, content.limits)?,
        (NativeProofKind::Hybrid, None, Some(hybrid)) => {
            validate_hybrid(hybrid, content.limits, limits.max_hybrid_branches)?;
        }
        (NativeProofKind::Ann, _, _) => {
            return Err(NativeProofError::Invalid(
                "ANN proof requires only ANN metadata",
            ));
        }
        (NativeProofKind::Hybrid, _, _) => {
            return Err(NativeProofError::Invalid(
                "hybrid proof requires only hybrid metadata",
            ));
        }
        (_, None, None) => {}
        _ => {
            return Err(NativeProofError::Invalid(
                "proof kind carries inapplicable metadata",
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_anchor(anchor: NativeProofAnchor) -> Result<(), NativeProofError> {
    if anchor.directory_lineage == [0; 24]
        || anchor.history_epoch == 0
        || anchor.catalog_version == 0
        || anchor.root_digest == [0; 32]
    {
        return Err(NativeProofError::Invalid("invalid native proof anchor"));
    }
    let empty_checkpoint = anchor.checkpoint_sequence == 0;
    if empty_checkpoint != (anchor.checkpoint_digest == [0; 32])
        || anchor.checkpoint_sequence > anchor.visible_csn
    {
        return Err(NativeProofError::Invalid(
            "noncanonical checkpoint identity",
        ));
    }
    Ok(())
}

fn validate_ann(
    ann: &AnnProofMetadata,
    limits: AdmittedProofLimits,
) -> Result<(), NativeProofError> {
    if ann.search_breadth == 0
        || ann.rerank_count > ann.candidate_count
        || ann.candidate_count > limits.candidate_items
    {
        return Err(NativeProofError::Invalid("invalid ANN execution counts"));
    }
    match (ann.approximation, ann.exact_oracle_digest) {
        (ApproximationLabel::Approximate, None)
        | (ApproximationLabel::ApproximateWithExactOracle, Some(_)) => Ok(()),
        _ => Err(NativeProofError::Invalid(
            "ANN approximation label and exact oracle disagree",
        )),
    }
}

fn validate_hybrid(
    hybrid: &HybridProofMetadata,
    limits: AdmittedProofLimits,
    maximum_branches: usize,
) -> Result<(), NativeProofError> {
    if hybrid.branches.len() < 2 {
        return Err(NativeProofError::Invalid(
            "hybrid proof requires at least two branches",
        ));
    }
    if hybrid.branches.len() > maximum_branches {
        return Err(limit(
            "hybrid branches",
            hybrid.branches.len(),
            maximum_branches,
        ));
    }
    let mut digests = BTreeSet::new();
    let mut candidate_sum = 0_u64;
    for branch in &hybrid.branches {
        if branch.proof_digest == [0; 32]
            || branch.weight_millionths == 0
            || branch.candidate_limit == 0
            || !digests.insert(branch.proof_digest)
        {
            return Err(NativeProofError::Invalid("invalid hybrid branch binding"));
        }
        candidate_sum = candidate_sum
            .checked_add(u64::from(branch.candidate_limit))
            .ok_or(NativeProofError::LengthOverflow)?;
    }
    if candidate_sum > limits.candidate_items {
        return Err(NativeProofError::Invalid(
            "hybrid branches exceed admitted candidates",
        ));
    }
    Ok(())
}

fn encode_ann(encoder: &mut Encoder, ann: &AnnProofMetadata) {
    encoder.byte(ann.metric as u8);
    encoder.extend(&ann.index_definition_digest);
    encoder.extend(&ann.graph_generation_digest);
    encoder.u32(ann.search_breadth);
    encoder.byte(ann.filter_strategy as u8);
    encoder.extend(&ann.eligible_set_digest);
    encoder.u64(ann.visited_count);
    encoder.u64(ann.candidate_count);
    encoder.u64(ann.rerank_count);
    encoder.byte(ann.approximation as u8);
    if let Some(digest) = ann.exact_oracle_digest {
        encoder.extend(&digest);
    }
}

fn decode_ann(decoder: &mut Decoder<'_>) -> Result<AnnProofMetadata, NativeProofError> {
    let metric = match decoder.byte()? {
        1 => VectorMetric::Cosine,
        2 => VectorMetric::NegativeDot,
        3 => VectorMetric::SquaredL2,
        _ => return Err(NativeProofError::Invalid("invalid ANN vector metric")),
    };
    let index_definition_digest = decoder.array()?;
    let graph_generation_digest = decoder.array()?;
    let search_breadth = decoder.u32()?;
    let filter_strategy = match decoder.byte()? {
        0 => AnnFilterStrategy::None,
        1 => AnnFilterStrategy::PreFilter,
        2 => AnnFilterStrategy::PostFilter,
        3 => AnnFilterStrategy::Iterative,
        4 => AnnFilterStrategy::ExactSeededPostFilter,
        _ => return Err(NativeProofError::Invalid("invalid ANN filter strategy")),
    };
    let eligible_set_digest = decoder.array()?;
    let visited_count = decoder.u64()?;
    let candidate_count = decoder.u64()?;
    let rerank_count = decoder.u64()?;
    let approximation = match decoder.byte()? {
        1 => ApproximationLabel::Approximate,
        2 => ApproximationLabel::ApproximateWithExactOracle,
        _ => return Err(NativeProofError::Invalid("invalid approximation label")),
    };
    let exact_oracle_digest = if approximation == ApproximationLabel::ApproximateWithExactOracle {
        Some(decoder.array()?)
    } else {
        None
    };
    Ok(AnnProofMetadata {
        metric,
        index_definition_digest,
        graph_generation_digest,
        search_breadth,
        filter_strategy,
        eligible_set_digest,
        visited_count,
        candidate_count,
        rerank_count,
        approximation,
        exact_oracle_digest,
    })
}

fn encode_hybrid(
    encoder: &mut Encoder,
    hybrid: &HybridProofMetadata,
) -> Result<(), NativeProofError> {
    encoder.byte(hybrid.failure_policy as u8);
    encoder.byte(hybrid.fusion_method as u8);
    encoder.byte(hybrid.duplicate_policy as u8);
    encoder.count(hybrid.branches.len())?;
    for branch in &hybrid.branches {
        encoder.extend(&branch.proof_digest);
        encoder.u32(branch.weight_millionths);
        encoder.u32(branch.candidate_limit);
    }
    Ok(())
}

fn decode_hybrid(
    decoder: &mut Decoder<'_>,
    maximum_branches: usize,
) -> Result<HybridProofMetadata, NativeProofError> {
    let failure_policy = match decoder.byte()? {
        1 => HybridFailurePolicy::FailClosed,
        2 => HybridFailurePolicy::AllowPartial,
        _ => return Err(NativeProofError::Invalid("invalid hybrid failure policy")),
    };
    let fusion_method = match decoder.byte()? {
        1 => HybridFusionMethod::WeightedReciprocalRank,
        2 => HybridFusionMethod::WeightedScore,
        _ => return Err(NativeProofError::Invalid("invalid hybrid fusion method")),
    };
    let duplicate_policy = match decoder.byte()? {
        1 => HybridDuplicatePolicy::MergeByObjectId,
        2 => HybridDuplicatePolicy::PreserveBranches,
        _ => return Err(NativeProofError::Invalid("invalid hybrid duplicate policy")),
    };
    let count = decoder.count(maximum_branches, "hybrid branches")?;
    let mut branches = Vec::new();
    branches
        .try_reserve_exact(count)
        .map_err(|_| NativeProofError::LengthOverflow)?;
    for _ in 0..count {
        branches.push(HybridBranchBinding {
            proof_digest: decoder.array()?,
            weight_millionths: decoder.u32()?,
            candidate_limit: decoder.u32()?,
        });
    }
    Ok(HybridProofMetadata {
        branches,
        failure_policy,
        fusion_method,
        duplicate_policy,
    })
}

fn decode_kind(value: u8) -> Result<NativeProofKind, NativeProofError> {
    match value {
        1 => Ok(NativeProofKind::Point),
        2 => Ok(NativeProofKind::Sql),
        3 => Ok(NativeProofKind::Lexical),
        4 => Ok(NativeProofKind::ExactVector),
        5 => Ok(NativeProofKind::Ann),
        6 => Ok(NativeProofKind::Hybrid),
        7 => Ok(NativeProofKind::Catalog),
        _ => Err(NativeProofError::Invalid("invalid native proof kind")),
    }
}

fn decode_completion(value: u8) -> Result<CompletionStatus, NativeProofError> {
    match value {
        1 => Ok(CompletionStatus::Complete),
        2 => Ok(CompletionStatus::Truncated),
        _ => Err(NativeProofError::Invalid("invalid completion status")),
    }
}

pub(crate) fn encode_anchor(encoder: &mut Encoder, anchor: NativeProofAnchor) {
    encoder.extend(&anchor.directory_lineage);
    encoder.u64(anchor.history_epoch);
    encoder.u64(anchor.visible_csn);
    encoder.u64(anchor.catalog_version);
    encoder.extend(&anchor.root_digest);
    encoder.u64(anchor.checkpoint_sequence);
    encoder.extend(&anchor.checkpoint_digest);
}

pub(crate) fn decode_anchor(
    decoder: &mut Decoder<'_>,
) -> Result<NativeProofAnchor, NativeProofError> {
    let anchor = NativeProofAnchor {
        directory_lineage: decoder.array()?,
        history_epoch: decoder.u64()?,
        visible_csn: decoder.u64()?,
        catalog_version: decoder.u64()?,
        root_digest: decoder.array()?,
        checkpoint_sequence: decoder.u64()?,
        checkpoint_digest: decoder.array()?,
    };
    validate_anchor(anchor)?;
    Ok(anchor)
}

pub(crate) fn seal_envelope(
    magic: [u8; 8],
    kind: u8,
    secondary: u8,
    payload: &[u8],
    maximum_bytes: u64,
    resource: &'static str,
    domain: &[u8],
) -> Result<Vec<u8>, NativeProofError> {
    let file_length = HEADER_BYTES
        .checked_add(payload.len())
        .ok_or(NativeProofError::LengthOverflow)?;
    check_encoded_limit(file_length, maximum_bytes, resource)?;
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(file_length)
        .map_err(|_| NativeProofError::LengthOverflow)?;
    encoded.resize(HEADER_BYTES, 0);
    encoded[..8].copy_from_slice(&magic);
    encoded[8..10].copy_from_slice(&NATIVE_PROOF_VERSION.to_le_bytes());
    encoded[12] = kind;
    encoded[13] = secondary;
    encoded[16..24].copy_from_slice(
        &u64::try_from(payload.len())
            .map_err(|_| NativeProofError::LengthOverflow)?
            .to_le_bytes(),
    );
    encoded.extend_from_slice(payload);
    let checksum = envelope_checksum(&encoded, payload);
    encoded[24..28].copy_from_slice(&checksum.to_le_bytes());
    let digest = envelope_digest(&encoded, payload, domain);
    encoded[PROOF_DIGEST_OFFSET..HEADER_BYTES].copy_from_slice(&digest);
    Ok(encoded)
}

pub(crate) fn verify_envelope(
    encoded: &[u8],
    payload: &[u8],
    domain: &[u8],
) -> Result<(), NativeProofError> {
    let expected_checksum = read_u32(&encoded[24..28]);
    if envelope_checksum(encoded, payload) != expected_checksum {
        return Err(NativeProofError::ChecksumMismatch);
    }
    let expected_digest: [u8; 32] = copy_array(&encoded[PROOF_DIGEST_OFFSET..HEADER_BYTES]);
    if envelope_digest(encoded, payload, domain) != expected_digest {
        return Err(NativeProofError::DigestMismatch("envelope"));
    }
    Ok(())
}

fn envelope_checksum(encoded: &[u8], payload: &[u8]) -> u32 {
    let mut header = [0_u8; PROOF_DIGEST_OFFSET];
    header.copy_from_slice(&encoded[..PROOF_DIGEST_OFFSET]);
    header[24..28].fill(0);
    crc32c_parts(&[&header, payload])
}

fn envelope_digest(encoded: &[u8], payload: &[u8], domain: &[u8]) -> [u8; 32] {
    blake3_parts(&[domain, &encoded[..PROOF_DIGEST_OFFSET], payload])
}

fn check_version(version: u16) -> Result<(), NativeProofError> {
    if version == NATIVE_PROOF_VERSION {
        Ok(())
    } else {
        Err(NativeProofError::UnsupportedVersion {
            found: version,
            supported: NATIVE_PROOF_VERSION,
        })
    }
}

pub(crate) fn check_encoded_limit(
    actual: usize,
    maximum: u64,
    resource: &'static str,
) -> Result<(), NativeProofError> {
    let actual = u64::try_from(actual).map_err(|_| NativeProofError::LengthOverflow)?;
    if actual > maximum {
        Err(limit(resource, actual, maximum))
    } else {
        Ok(())
    }
}

fn read_length(bytes: &[u8]) -> Result<usize, NativeProofError> {
    usize::try_from(read_u64(bytes)).map_err(|_| NativeProofError::LengthOverflow)
}

pub(crate) fn read_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(copy_array(bytes))
}

pub(crate) fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(copy_array(bytes))
}

pub(crate) fn read_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(copy_array(bytes))
}

pub(crate) fn copy_array<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut result = [0; N];
    result.copy_from_slice(bytes);
    result
}

#[derive(Default)]
pub(crate) struct Encoder {
    pub(crate) bytes: Vec<u8>,
}

impl Encoder {
    pub(crate) fn byte(&mut self, value: u8) {
        self.bytes.push(value);
    }

    pub(crate) fn u16(&mut self, value: u16) {
        self.extend(&value.to_le_bytes());
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.extend(&value.to_le_bytes());
    }

    pub(crate) fn u64(&mut self, value: u64) {
        self.extend(&value.to_le_bytes());
    }

    pub(crate) fn u128(&mut self, value: u128) {
        self.extend(&value.to_le_bytes());
    }

    pub(crate) fn count(&mut self, value: usize) -> Result<(), NativeProofError> {
        self.u32(u32::try_from(value).map_err(|_| NativeProofError::LengthOverflow)?);
        Ok(())
    }

    pub(crate) fn extend(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    fn canonical_section(&mut self, value: &CanonicalBytes) -> Result<(), NativeProofError> {
        self.u64(u64::try_from(value.bytes.len()).map_err(|_| NativeProofError::LengthOverflow)?);
        self.extend(&value.digest);
        self.extend(&value.bytes);
        Ok(())
    }
}

pub(crate) struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    pub(crate) const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    pub(crate) fn byte(&mut self) -> Result<u8, NativeProofError> {
        let value = self.take(1)?[0];
        Ok(value)
    }

    pub(crate) const fn has_remaining(&self) -> bool {
        self.offset < self.bytes.len()
    }

    pub(crate) fn u16(&mut self) -> Result<u16, NativeProofError> {
        Ok(read_u16(self.take(2)?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, NativeProofError> {
        Ok(read_u32(self.take(4)?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, NativeProofError> {
        Ok(read_u64(self.take(8)?))
    }

    pub(crate) fn u128(&mut self) -> Result<u128, NativeProofError> {
        Ok(u128::from_le_bytes(copy_array(self.take(16)?)))
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], NativeProofError> {
        Ok(copy_array(self.take(N)?))
    }

    pub(crate) fn count(
        &mut self,
        maximum: usize,
        resource: &'static str,
    ) -> Result<usize, NativeProofError> {
        let count = usize::try_from(self.u32()?).map_err(|_| NativeProofError::LengthOverflow)?;
        if count > maximum {
            return Err(limit(resource, count, maximum));
        }
        Ok(count)
    }

    pub(crate) fn take(&mut self, length: usize) -> Result<&'a [u8], NativeProofError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(NativeProofError::LengthOverflow)?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(NativeProofError::Invalid("truncated proof payload"))?;
        self.offset = end;
        Ok(value)
    }

    fn canonical_section(
        &mut self,
        limits: &ProofCodecLimits,
        decoded_bytes: &mut u64,
    ) -> Result<CanonicalBytes, NativeProofError> {
        let raw_length = self.u64()?;
        if raw_length > limits.max_section_bytes {
            return Err(limit(
                "canonical section bytes",
                raw_length,
                limits.max_section_bytes,
            ));
        }
        *decoded_bytes = decoded_bytes
            .checked_add(raw_length)
            .ok_or(NativeProofError::LengthOverflow)?;
        if *decoded_bytes > limits.max_decoded_bytes {
            return Err(limit(
                "proof decoded bytes",
                *decoded_bytes,
                limits.max_decoded_bytes,
            ));
        }
        let digest = self.array()?;
        let length = usize::try_from(raw_length).map_err(|_| NativeProofError::LengthOverflow)?;
        let bytes = self.owned(length)?;
        if canonical_bytes_digest(&bytes) != digest {
            return Err(NativeProofError::DigestMismatch("canonical section"));
        }
        Ok(CanonicalBytes { bytes, digest })
    }

    pub(crate) fn finish(self) -> Result<(), NativeProofError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(NativeProofError::Invalid("trailing proof payload bytes"))
        }
    }

    pub(crate) fn owned(&mut self, length: usize) -> Result<Vec<u8>, NativeProofError> {
        let source = self.take(length)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| NativeProofError::LengthOverflow)?;
        bytes.extend_from_slice(source);
        Ok(bytes)
    }
}

const _: [(); ANCHOR_BYTES] = [(); 24 + 8 + 8 + 8 + 32 + 8 + 32];
