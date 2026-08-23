// SPDX-License-Identifier: Apache-2.0

//! Model-attestation envelopes and their pure offline verifier.
//!
//! An attestation records how a model output entered a search pipeline.
//! Two classes exist and are never interchangeable:
//!
//! - **`AttestedLocal`** — a locally executed model: the weights digest, the
//!   canonical input digest, and the output digest are all BLAKE3 and the
//!   claim is replayable (rerun the same weights over the same input and
//!   the output digest must reproduce).
//! - **`DeclaredProvider`** — an external provider's response: the provider
//!   and model identifiers plus request and response digests are recorded
//!   as a declaration. The envelope proves what was sent and received, not
//!   that the provider computed it deterministically.
//!
//! The envelope is a bounded canonical encoding with a fail-closed decoder
//! (strict lengths, no trailing bytes) and a pure verifier with zero
//! dependencies beyond the digest primitive already in the proof subsystem.

use super::model::NativeProofError;

/// Envelope magic.
const ATTESTATION_MAGIC: &[u8; 8] = b"HYATTS01";
/// Maximum UTF-8 bytes for provider, model, and target names.
pub const MAX_ATTESTATION_NAME_BYTES: usize = 256;
/// Exact encoded envelope bound.
pub const MAX_ATTESTATION_BYTES: usize = 4 * 1024;

/// How a model output entered the pipeline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelAttestation {
    /// Locally executed model with a replayable digest chain.
    AttestedLocal {
        /// Model target name, for example `bge-small-en-v1.5`.
        target: String,
        /// BLAKE3 digest of the exact model weights.
        weights_digest: [u8; 32],
        /// BLAKE3 digest of the canonical model input.
        input_digest: [u8; 32],
        /// BLAKE3 digest of the canonical model output.
        output_digest: [u8; 32],
    },
    /// External provider response recorded as a declaration.
    DeclaredProvider {
        /// Provider identifier, for example `openai`.
        provider: String,
        /// Provider-scoped model identifier.
        model: String,
        /// BLAKE3 digest of the canonical request.
        request_digest: [u8; 32],
        /// BLAKE3 digest of the canonical response.
        response_digest: [u8; 32],
    },
}

impl ModelAttestation {
    /// Returns the stable class tag recorded in proofs.
    #[must_use]
    pub const fn class(&self) -> AttestationClass {
        match self {
            Self::AttestedLocal { .. } => AttestationClass::AttestedLocal,
            Self::DeclaredProvider { .. } => AttestationClass::DeclaredProvider,
        }
    }

    /// Returns the output digest the attested stage produced: the local
    /// output digest, or the declared response digest.
    #[must_use]
    pub const fn output_digest(&self) -> &[u8; 32] {
        match self {
            Self::AttestedLocal { output_digest, .. } => output_digest,
            Self::DeclaredProvider {
                response_digest, ..
            } => response_digest,
        }
    }

    /// Encodes the canonical bounded envelope.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty or oversized name.
    pub fn encode(&self) -> Result<Vec<u8>, NativeProofError> {
        let mut encoded = Vec::with_capacity(256);
        encoded.extend_from_slice(ATTESTATION_MAGIC);
        match self {
            Self::AttestedLocal {
                target,
                weights_digest,
                input_digest,
                output_digest,
            } => {
                encoded.push(1);
                put_name(&mut encoded, target)?;
                encoded.extend_from_slice(weights_digest);
                encoded.extend_from_slice(input_digest);
                encoded.extend_from_slice(output_digest);
            }
            Self::DeclaredProvider {
                provider,
                model,
                request_digest,
                response_digest,
            } => {
                encoded.push(2);
                put_name(&mut encoded, provider)?;
                put_name(&mut encoded, model)?;
                encoded.extend_from_slice(request_digest);
                encoded.extend_from_slice(response_digest);
            }
        }
        if encoded.len() > MAX_ATTESTATION_BYTES {
            return Err(NativeProofError::Invalid("attestation envelope too large"));
        }
        Ok(encoded)
    }

    /// Decodes and structurally verifies one canonical envelope: exact
    /// magic, known class, bounded names, exact digest lengths, and no
    /// trailing bytes. Re-encoding reproduces the input exactly.
    ///
    /// # Errors
    ///
    /// Returns an error for any malformed, oversized, or noncanonical
    /// envelope.
    pub fn decode(encoded: &[u8]) -> Result<Self, NativeProofError> {
        if encoded.len() > MAX_ATTESTATION_BYTES {
            return Err(NativeProofError::Invalid("attestation envelope too large"));
        }
        let mut offset = 0_usize;
        let magic = take(encoded, &mut offset, 8)?;
        if magic != ATTESTATION_MAGIC {
            return Err(NativeProofError::Invalid("attestation magic differs"));
        }
        let class = take(encoded, &mut offset, 1)?[0];
        let attestation = match class {
            1 => Self::AttestedLocal {
                target: take_name(encoded, &mut offset)?,
                weights_digest: take_digest(encoded, &mut offset)?,
                input_digest: take_digest(encoded, &mut offset)?,
                output_digest: take_digest(encoded, &mut offset)?,
            },
            2 => Self::DeclaredProvider {
                provider: take_name(encoded, &mut offset)?,
                model: take_name(encoded, &mut offset)?,
                request_digest: take_digest(encoded, &mut offset)?,
                response_digest: take_digest(encoded, &mut offset)?,
            },
            _ => return Err(NativeProofError::Invalid("unknown attestation class")),
        };
        if offset != encoded.len() {
            return Err(NativeProofError::Invalid("trailing attestation bytes"));
        }
        Ok(attestation)
    }
}

/// Stable attestation class recorded in proofs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttestationClass {
    /// Replayable local execution.
    AttestedLocal = 1,
    /// Declared external provider response.
    DeclaredProvider = 2,
}

/// Verifies one envelope offline: canonical structure, and when the caller
/// holds the exact bytes an attested stage consumed or produced, digest
/// equality against the envelope's claims.
///
/// # Errors
///
/// Returns a structural error for a malformed envelope and a digest error
/// when supplied bytes do not reproduce the recorded digests.
pub fn verify_attestation(
    encoded: &[u8],
    input_bytes: Option<&[u8]>,
    output_bytes: Option<&[u8]>,
) -> Result<ModelAttestation, NativeProofError> {
    let attestation = ModelAttestation::decode(encoded)?;
    if attestation.encode()? != encoded {
        return Err(NativeProofError::Invalid(
            "noncanonical attestation encoding",
        ));
    }
    if let Some(bytes) = input_bytes {
        let expected = match &attestation {
            ModelAttestation::AttestedLocal { input_digest, .. } => input_digest,
            ModelAttestation::DeclaredProvider { request_digest, .. } => request_digest,
        };
        if blake3::hash(bytes).as_bytes() != expected {
            return Err(NativeProofError::Invalid("attested input digest differs"));
        }
    }
    if let Some(bytes) = output_bytes
        && blake3::hash(bytes).as_bytes() != attestation.output_digest()
    {
        return Err(NativeProofError::Invalid("attested output digest differs"));
    }
    Ok(attestation)
}

fn put_name(encoded: &mut Vec<u8>, name: &str) -> Result<(), NativeProofError> {
    if name.is_empty() || name.len() > MAX_ATTESTATION_NAME_BYTES {
        return Err(NativeProofError::Invalid("attestation name is unbounded"));
    }
    let length = u16::try_from(name.len())
        .map_err(|_| NativeProofError::Invalid("attestation name is unbounded"))?;
    encoded.extend_from_slice(&length.to_le_bytes());
    encoded.extend_from_slice(name.as_bytes());
    Ok(())
}

fn take<'bytes>(
    encoded: &'bytes [u8],
    offset: &mut usize,
    length: usize,
) -> Result<&'bytes [u8], NativeProofError> {
    let end = offset
        .checked_add(length)
        .ok_or(NativeProofError::Invalid("attestation length overflow"))?;
    if end > encoded.len() {
        return Err(NativeProofError::Invalid("attestation is truncated"));
    }
    let taken = &encoded[*offset..end];
    *offset = end;
    Ok(taken)
}

fn take_name(encoded: &[u8], offset: &mut usize) -> Result<String, NativeProofError> {
    let length = u16::from_le_bytes(
        take(encoded, offset, 2)?
            .try_into()
            .map_err(|_| NativeProofError::Invalid("attestation is truncated"))?,
    ) as usize;
    if length == 0 || length > MAX_ATTESTATION_NAME_BYTES {
        return Err(NativeProofError::Invalid("attestation name is unbounded"));
    }
    let bytes = take(encoded, offset, length)?;
    String::from_utf8(bytes.to_vec())
        .map_err(|_| NativeProofError::Invalid("attestation name is not UTF-8"))
}

fn take_digest(encoded: &[u8], offset: &mut usize) -> Result<[u8; 32], NativeProofError> {
    take(encoded, offset, 32)?
        .try_into()
        .map_err(|_| NativeProofError::Invalid("attestation is truncated"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> ModelAttestation {
        ModelAttestation::AttestedLocal {
            target: "bge-small-en-v1.5".to_owned(),
            weights_digest: *blake3::hash(b"weights").as_bytes(),
            input_digest: *blake3::hash(b"input").as_bytes(),
            output_digest: *blake3::hash(b"output").as_bytes(),
        }
    }

    fn declared() -> ModelAttestation {
        ModelAttestation::DeclaredProvider {
            provider: "openai".to_owned(),
            model: "text-embedding-3-small".to_owned(),
            request_digest: *blake3::hash(b"request").as_bytes(),
            response_digest: *blake3::hash(b"response").as_bytes(),
        }
    }

    #[test]
    fn envelopes_round_trip_canonically() -> Result<(), NativeProofError> {
        for attestation in [local(), declared()] {
            let encoded = attestation.encode()?;
            assert_eq!(ModelAttestation::decode(&encoded)?, attestation);
            assert_eq!(
                verify_attestation(&encoded, None, None)?.class(),
                attestation.class()
            );
        }
        Ok(())
    }

    #[test]
    fn verifier_checks_supplied_bytes_against_digests() -> Result<(), NativeProofError> {
        let encoded = local().encode()?;
        assert!(verify_attestation(&encoded, Some(b"input"), Some(b"output")).is_ok());
        assert!(verify_attestation(&encoded, Some(b"tampered"), None).is_err());
        assert!(verify_attestation(&encoded, None, Some(b"tampered")).is_err());
        let encoded = declared().encode()?;
        assert!(verify_attestation(&encoded, Some(b"request"), Some(b"response")).is_ok());
        Ok(())
    }

    #[test]
    fn declared_envelope_matches_the_cross_language_golden() -> Result<(), NativeProofError> {
        // The SDK provider layers replicate this envelope byte-exactly; the
        // same hex is asserted in the Python and TypeScript suites.
        let encoded = declared().encode()?;
        let mut expected = Vec::new();
        expected.extend_from_slice(b"HYATTS01\x02");
        expected.extend_from_slice(&6_u16.to_le_bytes());
        expected.extend_from_slice(b"openai");
        expected.extend_from_slice(&22_u16.to_le_bytes());
        expected.extend_from_slice(b"text-embedding-3-small");
        expected.extend_from_slice(blake3::hash(b"request").as_bytes());
        expected.extend_from_slice(blake3::hash(b"response").as_bytes());
        assert_eq!(encoded, expected);
        Ok(())
    }

    #[test]
    fn malformed_envelopes_fail_closed() -> Result<(), NativeProofError> {
        let mut encoded = local().encode()?;
        encoded.push(0);
        assert!(ModelAttestation::decode(&encoded).is_err());
        let mut truncated = local().encode()?;
        truncated.pop();
        assert!(ModelAttestation::decode(&truncated).is_err());
        let mut wrong_magic = local().encode()?;
        wrong_magic[0] ^= 1;
        assert!(ModelAttestation::decode(&wrong_magic).is_err());
        let mut wrong_class = local().encode()?;
        wrong_class[8] = 9;
        assert!(ModelAttestation::decode(&wrong_class).is_err());
        assert!(
            ModelAttestation::AttestedLocal {
                target: String::new(),
                weights_digest: [0; 32],
                input_digest: [0; 32],
                output_digest: [0; 32],
            }
            .encode()
            .is_err()
        );
        Ok(())
    }
}
