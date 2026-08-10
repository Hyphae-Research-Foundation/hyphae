// SPDX-License-Identifier: GPL-3.0-only

use super::{
    codec::decode_native_proof,
    model::{
        ExternalTrustedAnchor, NativeProofError, NativeProofVerificationReport,
        NativeVerificationLimits, NativeVerificationScope, NativeWitnessEntry,
    },
    witness::decode_native_witness,
};

/// Verifies a complete proof and witness without consulting the originating data directory.
///
/// The verifier first establishes artifact integrity. For a recognized v2 operation contract it
/// then extracts the complete witness, opens it as native authority, reexecutes the operation, and
/// compares the canonical ordered result, execution evidence, object bindings, and metadata.
///
/// # Errors
///
/// Rejects malformed, noncanonical, corrupt, mismatched, or over-limit artifacts and an anchor
/// which does not match the caller's independently trusted digest.
pub fn verify_native_proof_offline(
    proof_bytes: &[u8],
    witness_bytes: &[u8],
    trusted_anchor: ExternalTrustedAnchor,
    limits: &NativeVerificationLimits,
) -> Result<NativeProofVerificationReport, NativeProofError> {
    let proof = decode_native_proof(proof_bytes, &limits.proof)?;
    let witness = decode_native_witness(witness_bytes, &limits.witness)?;
    let proof_anchor_digest = proof.content.anchor.digest();
    if proof_anchor_digest != trusted_anchor.digest() {
        return Err(NativeProofError::TrustedAnchorMismatch);
    }
    if proof.content.anchor != witness.anchor {
        return Err(NativeProofError::WitnessAnchorMismatch);
    }
    let witness_length =
        u64::try_from(witness_bytes.len()).map_err(|_| NativeProofError::LengthOverflow)?;
    if proof.content.witness.digest != witness.witness_digest
        || proof.content.witness.file_bytes != witness_length
    {
        return Err(NativeProofError::WitnessReferenceMismatch);
    }
    let mut file_count = 0_usize;
    let mut directory_count = 0_usize;
    let mut total_file_bytes = 0_u64;
    for entry in &witness.entries {
        match entry {
            NativeWitnessEntry::Directory { .. } => {
                directory_count = directory_count
                    .checked_add(1)
                    .ok_or(NativeProofError::LengthOverflow)?;
            }
            NativeWitnessEntry::File { bytes, .. } => {
                file_count = file_count
                    .checked_add(1)
                    .ok_or(NativeProofError::LengthOverflow)?;
                total_file_bytes = total_file_bytes
                    .checked_add(
                        u64::try_from(bytes.len()).map_err(|_| NativeProofError::LengthOverflow)?,
                    )
                    .ok_or(NativeProofError::LengthOverflow)?;
            }
        }
    }
    let semantic_reexecution_performed =
        super::operation::reexecute_native_operation_proof(&proof, &witness, limits)?;
    Ok(NativeProofVerificationReport {
        scope: if semantic_reexecution_performed {
            NativeVerificationScope::SemanticReexecution
        } else {
            NativeVerificationScope::ArtifactIntegrity
        },
        kind: proof.content.kind,
        anchor_digest: proof_anchor_digest,
        proof_digest: proof.proof_digest,
        witness_digest: witness.witness_digest,
        request_digest: proof.content.request.digest,
        result_digest: proof.content.result.digest,
        evidence_digest: proof.content.evidence.digest,
        file_count,
        directory_count,
        total_file_bytes,
        semantic_reexecution_performed,
    })
}
