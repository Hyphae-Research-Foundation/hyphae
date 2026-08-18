// SPDX-License-Identifier: Apache-2.0

//! Bounded native proof and complete directory-witness artifacts.

mod codec;
mod crypto;
mod model;
mod operation;
mod verify;
mod witness;

pub use codec::{decode_native_proof, encode_native_proof};
pub use model::{
    AdmittedProofLimits, AnnFilterStrategy, AnnProofMetadata, ApproximationLabel, CanonicalBytes,
    CompletionStatus, ExternalTrustedAnchor, HybridBranchBinding, HybridDuplicatePolicy,
    HybridFailurePolicy, HybridFusionMethod, HybridProofMetadata, MAX_NATIVE_PROOF_BYTES,
    MAX_NATIVE_WITNESS_BYTES, NATIVE_PROOF_VERSION, NativeDirectoryWitness,
    NativeOperationProofArtifact, NativeProof, NativeProofAnchor, NativeProofContent,
    NativeProofError, NativeProofGenerationLimits, NativeProofKind, NativeProofVerificationReport,
    NativeVerificationLimits, NativeVerificationScope, NativeWitnessArtifact, NativeWitnessEntry,
    NativeWitnessReference, ProofCodecLimits, ProofObjectBinding, VectorMetric, WitnessCodecLimits,
};
pub(crate) use operation::dispatch_proven_operation;
pub use operation::{generate_native_operation_proof, reexecute_native_operation_proof};
pub use verify::verify_native_proof_offline;
pub(crate) use verify::verify_native_proof_offline_with_checkpoint;
pub use witness::{
    bundle_native_witness, decode_native_witness, encode_native_witness, read_native_witness,
    write_native_witness,
};
