// SPDX-License-Identifier: Apache-2.0

use hyphae_core::{
    Q15Vector, VectorMetric, VectorSpaceDefinition, VectorSpaceName, VectorValueError,
};
use hyphae_query::FieldPath;
use hyphae_retrieval::{LexicalField, LexicalIndexDefinition};
use thiserror::Error;

use crate::log::MAX_OPERATION_BYTES;

const PUT: u8 = 1;
const DELETE: u8 = 2;
const DEFINE_VECTOR_SPACE: u8 = 3;
const UPSERT_VECTOR: u8 = 4;
const DELETE_VECTOR: u8 = 5;
const DEFINE_LEXICAL_INDEX: u8 = 6;
const PUT_HEADER_LENGTH: usize = 13;
const DELETE_HEADER_LENGTH: usize = 5;

/// Maximum encoded key length accepted by the embedded KV layer.
pub const MAX_KEY_BYTES: usize = 1024 * 1024;

/// A deterministic mutation persisted inside one transaction operation frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Mutation {
    /// Replaces the value stored under a nonempty binary key.
    Put {
        /// Binary key.
        key: Vec<u8>,
        /// Binary value.
        value: Vec<u8>,
    },
    /// Removes a binary key. Deleting a missing key is idempotent.
    Delete {
        /// Binary key.
        key: Vec<u8>,
    },
    /// Defines one immutable named vector space.
    DefineVectorSpace {
        /// Canonical vector-space definition.
        definition: VectorSpaceDefinition,
    },
    /// Replaces one vector at `(space, key)`.
    UpsertVector {
        /// Canonical vector-space identifier.
        space: VectorSpaceName,
        /// Binary object key.
        key: Vec<u8>,
        /// Canonical Q15 vector.
        vector: Q15Vector,
    },
    /// Removes one vector. Deleting a missing vector is idempotent.
    DeleteVector {
        /// Canonical vector-space identifier.
        space: VectorSpaceName,
        /// Binary object key.
        key: Vec<u8>,
    },
    /// Defines one immutable named lexical index.
    DefineLexicalIndex {
        /// Canonical lexical definition.
        definition: LexicalIndexDefinition,
    },
}

impl Mutation {
    /// Creates a put mutation.
    pub fn put(key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> Self {
        Self::Put {
            key: key.into(),
            value: value.into(),
        }
    }

    /// Creates a delete mutation.
    pub fn delete(key: impl Into<Vec<u8>>) -> Self {
        Self::Delete { key: key.into() }
    }

    /// Creates a vector-space definition mutation.
    pub fn define_vector_space(definition: VectorSpaceDefinition) -> Self {
        Self::DefineVectorSpace { definition }
    }

    /// Creates a durable vector upsert mutation.
    pub fn upsert_vector(
        space: VectorSpaceName,
        key: impl Into<Vec<u8>>,
        vector: Q15Vector,
    ) -> Self {
        Self::UpsertVector {
            space,
            key: key.into(),
            vector,
        }
    }

    /// Creates a durable vector delete mutation.
    pub fn delete_vector(space: VectorSpaceName, key: impl Into<Vec<u8>>) -> Self {
        Self::DeleteVector {
            space,
            key: key.into(),
        }
    }

    /// Creates a lexical-index definition mutation.
    pub fn define_lexical_index(definition: LexicalIndexDefinition) -> Self {
        Self::DefineLexicalIndex { definition }
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, MutationError> {
        match self {
            Self::Put { key, value } => encode_put(key, value),
            Self::Delete { key } => encode_delete(key),
            Self::DefineVectorSpace { definition } => encode_vector_space(definition),
            Self::UpsertVector { space, key, vector } => encode_vector(space, key, vector),
            Self::DeleteVector { space, key } => encode_vector_delete(space, key),
            Self::DefineLexicalIndex { definition } => encode_lexical_index(definition),
        }
    }

    pub(crate) fn decode(encoded: &[u8]) -> Result<Self, MutationError> {
        let Some(kind) = encoded.first().copied() else {
            return Err(MutationError::Malformed);
        };
        match kind {
            PUT => decode_put(encoded),
            DELETE => decode_delete(encoded),
            DEFINE_VECTOR_SPACE => decode_vector_space(encoded),
            UPSERT_VECTOR => decode_vector(encoded),
            DELETE_VECTOR => decode_vector_delete(encoded),
            DEFINE_LEXICAL_INDEX => decode_lexical_index(encoded),
            kind => Err(MutationError::UnknownKind { kind }),
        }
    }
}

/// Failure while validating or decoding a persisted mutation.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum MutationError {
    /// Empty keys are forbidden.
    #[error("mutation key must not be empty")]
    EmptyKey,

    /// A key exceeds the stable storage limit.
    #[error("mutation key is {length} bytes; maximum is {maximum}")]
    KeyTooLarge {
        /// Actual key length.
        length: usize,
        /// Maximum key length.
        maximum: usize,
    },

    /// The full encoded operation exceeds one log frame.
    #[error("encoded mutation is {length} bytes; maximum is {maximum}")]
    OperationTooLarge {
        /// Actual encoded length.
        length: usize,
        /// Maximum operation length.
        maximum: usize,
    },

    /// Persisted bytes do not form one canonical mutation.
    #[error("malformed persisted mutation")]
    Malformed,

    /// The mutation kind is not defined by this disk format.
    #[error("unknown persisted mutation kind {kind}")]
    UnknownKind {
        /// Raw kind byte.
        kind: u8,
    },

    /// A canonical shared vector value is invalid.
    #[error(transparent)]
    Vector(#[from] VectorValueError),

    /// A canonical lexical definition is invalid.
    #[error("invalid lexical definition")]
    Lexical,
}

fn encode_lexical_index(definition: &LexicalIndexDefinition) -> Result<Vec<u8>, MutationError> {
    let mut encoded = Vec::new();
    encoded.push(DEFINE_LEXICAL_INDEX);
    encode_space(&definition.name, &mut encoded)?;
    let field_count = u8::try_from(definition.fields.len()).map_err(|_| MutationError::Lexical)?;
    encoded.push(field_count);
    for field in &definition.fields {
        let segment_count =
            u8::try_from(field.path.segments().len()).map_err(|_| MutationError::Lexical)?;
        encoded.push(segment_count);
        for segment in field.path.segments() {
            let bytes = segment.as_bytes();
            let length = u16::try_from(bytes.len()).map_err(|_| MutationError::Lexical)?;
            encoded.extend_from_slice(&length.to_le_bytes());
            encoded.extend_from_slice(bytes);
        }
        encoded.extend_from_slice(&field.weight_micros.to_le_bytes());
    }
    validate_operation_length(encoded.len())?;
    Ok(encoded)
}

fn decode_lexical_index(encoded: &[u8]) -> Result<Mutation, MutationError> {
    let mut cursor = 1;
    let name = decode_space(encoded, &mut cursor)?;
    let field_count = usize::from(*encoded.get(cursor).ok_or(MutationError::Malformed)?);
    cursor = cursor.checked_add(1).ok_or(MutationError::Malformed)?;
    let mut fields = Vec::with_capacity(field_count);
    for _ in 0..field_count {
        let segment_count = usize::from(*encoded.get(cursor).ok_or(MutationError::Malformed)?);
        cursor = cursor.checked_add(1).ok_or(MutationError::Malformed)?;
        let mut segments = Vec::with_capacity(segment_count);
        for _ in 0..segment_count {
            let end = cursor.checked_add(2).ok_or(MutationError::Malformed)?;
            let length = usize::from(u16::from_le_bytes(copy_array(
                encoded.get(cursor..end).ok_or(MutationError::Malformed)?,
            )));
            cursor = end;
            let end = cursor.checked_add(length).ok_or(MutationError::Malformed)?;
            let segment =
                std::str::from_utf8(encoded.get(cursor..end).ok_or(MutationError::Malformed)?)
                    .map_err(|_| MutationError::Malformed)?
                    .to_owned();
            cursor = end;
            segments.push(segment);
        }
        let end = cursor.checked_add(4).ok_or(MutationError::Malformed)?;
        let weight_micros = u32::from_le_bytes(copy_array(
            encoded.get(cursor..end).ok_or(MutationError::Malformed)?,
        ));
        cursor = end;
        fields.push(LexicalField {
            path: FieldPath::new(segments),
            weight_micros,
        });
    }
    if cursor != encoded.len() {
        return Err(MutationError::Malformed);
    }
    let definition =
        LexicalIndexDefinition::new(name, fields).map_err(|_| MutationError::Lexical)?;
    Ok(Mutation::DefineLexicalIndex { definition })
}

fn encode_space(space: &VectorSpaceName, encoded: &mut Vec<u8>) -> Result<(), MutationError> {
    let length = u8::try_from(space.as_str().len()).map_err(|_| MutationError::Malformed)?;
    encoded.push(length);
    encoded.extend_from_slice(space.as_str().as_bytes());
    Ok(())
}

fn decode_space(encoded: &[u8], cursor: &mut usize) -> Result<VectorSpaceName, MutationError> {
    let length = usize::from(*encoded.get(*cursor).ok_or(MutationError::Malformed)?);
    *cursor = cursor.checked_add(1).ok_or(MutationError::Malformed)?;
    let end = cursor.checked_add(length).ok_or(MutationError::Malformed)?;
    let raw = encoded.get(*cursor..end).ok_or(MutationError::Malformed)?;
    *cursor = end;
    let value = std::str::from_utf8(raw).map_err(|_| MutationError::Malformed)?;
    Ok(VectorSpaceName::new(value.to_owned())?)
}

fn encode_vector_space(definition: &VectorSpaceDefinition) -> Result<Vec<u8>, MutationError> {
    let mut encoded = Vec::with_capacity(1 + 1 + definition.name.as_str().len() + 4);
    encoded.push(DEFINE_VECTOR_SPACE);
    encode_space(&definition.name, &mut encoded)?;
    encoded.extend_from_slice(&definition.dimension.to_le_bytes());
    encoded.push(definition.metric as u8);
    encoded.push(1);
    validate_operation_length(encoded.len())?;
    Ok(encoded)
}

fn decode_vector_space(encoded: &[u8]) -> Result<Mutation, MutationError> {
    let mut cursor = 1;
    let name = decode_space(encoded, &mut cursor)?;
    let dimension_end = cursor.checked_add(2).ok_or(MutationError::Malformed)?;
    let dimension = u16::from_le_bytes(copy_array(
        encoded
            .get(cursor..dimension_end)
            .ok_or(MutationError::Malformed)?,
    ));
    cursor = dimension_end;
    if encoded.get(cursor) != Some(&(VectorMetric::Cosine as u8))
        || encoded.get(cursor + 1) != Some(&1)
        || cursor + 2 != encoded.len()
    {
        return Err(MutationError::Malformed);
    }
    let definition = VectorSpaceDefinition::cosine(name, dimension)?;
    Ok(Mutation::DefineVectorSpace { definition })
}

fn encode_vector(
    space: &VectorSpaceName,
    key: &[u8],
    vector: &Q15Vector,
) -> Result<Vec<u8>, MutationError> {
    validate_key(key)?;
    let key_length = u32::try_from(key.len()).map_err(|_| MutationError::KeyTooLarge {
        length: key.len(),
        maximum: MAX_KEY_BYTES,
    })?;
    let mut encoded = Vec::with_capacity(
        1 + 1 + space.as_str().len() + 4 + key.len() + 2 + 2 * vector.as_slice().len(),
    );
    encoded.push(UPSERT_VECTOR);
    encode_space(space, &mut encoded)?;
    encoded.extend_from_slice(&key_length.to_le_bytes());
    encoded.extend_from_slice(key);
    encoded.extend_from_slice(&vector.dimension().to_le_bytes());
    for value in vector.as_slice() {
        encoded.extend_from_slice(&value.to_le_bytes());
    }
    validate_operation_length(encoded.len())?;
    Ok(encoded)
}

fn decode_vector(encoded: &[u8]) -> Result<Mutation, MutationError> {
    let mut cursor = 1;
    let space = decode_space(encoded, &mut cursor)?;
    let key_length_end = cursor.checked_add(4).ok_or(MutationError::Malformed)?;
    let key_length = usize::try_from(u32::from_le_bytes(copy_array(
        encoded
            .get(cursor..key_length_end)
            .ok_or(MutationError::Malformed)?,
    )))
    .map_err(|_| MutationError::Malformed)?;
    cursor = key_length_end;
    let key_end = cursor
        .checked_add(key_length)
        .ok_or(MutationError::Malformed)?;
    let key = encoded
        .get(cursor..key_end)
        .ok_or(MutationError::Malformed)?
        .to_vec();
    validate_key(&key)?;
    cursor = key_end;
    let dimension_end = cursor.checked_add(2).ok_or(MutationError::Malformed)?;
    let dimension = usize::from(u16::from_le_bytes(copy_array(
        encoded
            .get(cursor..dimension_end)
            .ok_or(MutationError::Malformed)?,
    )));
    cursor = dimension_end;
    let vector_bytes = dimension.checked_mul(2).ok_or(MutationError::Malformed)?;
    let vector_end = cursor
        .checked_add(vector_bytes)
        .ok_or(MutationError::Malformed)?;
    if vector_end != encoded.len() {
        return Err(MutationError::Malformed);
    }
    let mut values = Vec::with_capacity(dimension);
    for chunk in encoded[cursor..vector_end].chunks_exact(2) {
        values.push(i16::from_le_bytes(copy_array(chunk)));
    }
    Ok(Mutation::UpsertVector {
        space,
        key,
        vector: Q15Vector::new(values)?,
    })
}

fn encode_vector_delete(space: &VectorSpaceName, key: &[u8]) -> Result<Vec<u8>, MutationError> {
    validate_key(key)?;
    let key_length = u32::try_from(key.len()).map_err(|_| MutationError::KeyTooLarge {
        length: key.len(),
        maximum: MAX_KEY_BYTES,
    })?;
    let mut encoded = Vec::with_capacity(1 + 1 + space.as_str().len() + 4 + key.len());
    encoded.push(DELETE_VECTOR);
    encode_space(space, &mut encoded)?;
    encoded.extend_from_slice(&key_length.to_le_bytes());
    encoded.extend_from_slice(key);
    validate_operation_length(encoded.len())?;
    Ok(encoded)
}

fn decode_vector_delete(encoded: &[u8]) -> Result<Mutation, MutationError> {
    let mut cursor = 1;
    let space = decode_space(encoded, &mut cursor)?;
    let key_length_end = cursor.checked_add(4).ok_or(MutationError::Malformed)?;
    let key_length = usize::try_from(u32::from_le_bytes(copy_array(
        encoded
            .get(cursor..key_length_end)
            .ok_or(MutationError::Malformed)?,
    )))
    .map_err(|_| MutationError::Malformed)?;
    cursor = key_length_end;
    let key_end = cursor
        .checked_add(key_length)
        .ok_or(MutationError::Malformed)?;
    if key_end != encoded.len() {
        return Err(MutationError::Malformed);
    }
    let key = encoded[cursor..key_end].to_vec();
    validate_key(&key)?;
    Ok(Mutation::DeleteVector { space, key })
}

fn encode_put(key: &[u8], value: &[u8]) -> Result<Vec<u8>, MutationError> {
    validate_key(key)?;
    let key_length = u32::try_from(key.len()).map_err(|_| MutationError::KeyTooLarge {
        length: key.len(),
        maximum: MAX_KEY_BYTES,
    })?;
    let value_length =
        u64::try_from(value.len()).map_err(|_| MutationError::OperationTooLarge {
            length: usize::MAX,
            maximum: MAX_OPERATION_BYTES,
        })?;
    let encoded_length = PUT_HEADER_LENGTH
        .checked_add(key.len())
        .and_then(|length| length.checked_add(value.len()))
        .ok_or(MutationError::OperationTooLarge {
            length: usize::MAX,
            maximum: MAX_OPERATION_BYTES,
        })?;
    validate_operation_length(encoded_length)?;

    let mut encoded = Vec::with_capacity(encoded_length);
    encoded.push(PUT);
    encoded.extend_from_slice(&key_length.to_le_bytes());
    encoded.extend_from_slice(&value_length.to_le_bytes());
    encoded.extend_from_slice(key);
    encoded.extend_from_slice(value);
    Ok(encoded)
}

fn encode_delete(key: &[u8]) -> Result<Vec<u8>, MutationError> {
    validate_key(key)?;
    let key_length = u32::try_from(key.len()).map_err(|_| MutationError::KeyTooLarge {
        length: key.len(),
        maximum: MAX_KEY_BYTES,
    })?;
    let encoded_length =
        DELETE_HEADER_LENGTH
            .checked_add(key.len())
            .ok_or(MutationError::OperationTooLarge {
                length: usize::MAX,
                maximum: MAX_OPERATION_BYTES,
            })?;
    validate_operation_length(encoded_length)?;

    let mut encoded = Vec::with_capacity(encoded_length);
    encoded.push(DELETE);
    encoded.extend_from_slice(&key_length.to_le_bytes());
    encoded.extend_from_slice(key);
    Ok(encoded)
}

fn decode_put(encoded: &[u8]) -> Result<Mutation, MutationError> {
    if encoded.len() < PUT_HEADER_LENGTH {
        return Err(MutationError::Malformed);
    }
    let key_length = usize::try_from(u32::from_le_bytes(copy_array(&encoded[1..5])))
        .map_err(|_| MutationError::Malformed)?;
    let value_length = usize::try_from(u64::from_le_bytes(copy_array(&encoded[5..13])))
        .map_err(|_| MutationError::Malformed)?;
    let key_end = PUT_HEADER_LENGTH
        .checked_add(key_length)
        .ok_or(MutationError::Malformed)?;
    let value_end = key_end
        .checked_add(value_length)
        .ok_or(MutationError::Malformed)?;
    if value_end != encoded.len() {
        return Err(MutationError::Malformed);
    }
    let key = encoded[PUT_HEADER_LENGTH..key_end].to_vec();
    validate_key(&key)?;
    validate_operation_length(encoded.len())?;
    Ok(Mutation::Put {
        key,
        value: encoded[key_end..value_end].to_vec(),
    })
}

fn decode_delete(encoded: &[u8]) -> Result<Mutation, MutationError> {
    if encoded.len() < DELETE_HEADER_LENGTH {
        return Err(MutationError::Malformed);
    }
    let key_length = usize::try_from(u32::from_le_bytes(copy_array(&encoded[1..5])))
        .map_err(|_| MutationError::Malformed)?;
    let key_end = DELETE_HEADER_LENGTH
        .checked_add(key_length)
        .ok_or(MutationError::Malformed)?;
    if key_end != encoded.len() {
        return Err(MutationError::Malformed);
    }
    let key = encoded[DELETE_HEADER_LENGTH..key_end].to_vec();
    validate_key(&key)?;
    validate_operation_length(encoded.len())?;
    Ok(Mutation::Delete { key })
}

pub(crate) fn validate_key(key: &[u8]) -> Result<(), MutationError> {
    if key.is_empty() {
        return Err(MutationError::EmptyKey);
    }
    if key.len() > MAX_KEY_BYTES {
        return Err(MutationError::KeyTooLarge {
            length: key.len(),
            maximum: MAX_KEY_BYTES,
        });
    }
    Ok(())
}

fn validate_operation_length(length: usize) -> Result<(), MutationError> {
    if length > MAX_OPERATION_BYTES {
        return Err(MutationError::OperationTooLarge {
            length,
            maximum: MAX_OPERATION_BYTES,
        });
    }
    Ok(())
}

fn copy_array<const N: usize>(source: &[u8]) -> [u8; N] {
    let mut output = [0_u8; N];
    output.copy_from_slice(source);
    output
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use hyphae_core::{Q15Vector, VectorSpaceDefinition, VectorSpaceName};
    use hyphae_query::FieldPath;
    use hyphae_retrieval::{LexicalField, LexicalIndexDefinition};

    use super::{MAX_KEY_BYTES, Mutation, MutationError};

    #[test]
    fn mutation_codec_round_trips_binary_values() -> Result<(), Box<dyn Error>> {
        let mutations = [
            Mutation::put([0, 1, 2], [255, 0, 3]),
            Mutation::delete([7, 8, 9]),
        ];
        for mutation in mutations {
            assert_eq!(Mutation::decode(&mutation.encode()?)?, mutation);
        }
        Ok(())
    }

    #[test]
    fn vector_mutations_round_trip_canonically() -> Result<(), Box<dyn Error>> {
        let space = VectorSpaceName::new("semantic.v1")?;
        let mutations = [
            Mutation::define_vector_space(VectorSpaceDefinition::cosine(space.clone(), 2)?),
            Mutation::upsert_vector(space.clone(), b"object", Q15Vector::new(vec![32_767, -12])?),
            Mutation::delete_vector(space, b"object"),
        ];
        for mutation in mutations {
            assert_eq!(Mutation::decode(&mutation.encode()?)?, mutation);
        }
        Ok(())
    }

    #[test]
    fn lexical_definition_mutation_round_trips_canonically() -> Result<(), Box<dyn Error>> {
        let definition = LexicalIndexDefinition::new(
            VectorSpaceName::new("documents")?,
            vec![
                LexicalField {
                    path: FieldPath::new(["body", "text"]),
                    weight_micros: 1_000_000,
                },
                LexicalField {
                    path: FieldPath::field("title"),
                    weight_micros: 2_000_000,
                },
            ],
        )?;
        let mutation = Mutation::define_lexical_index(definition);
        assert_eq!(Mutation::decode(&mutation.encode()?)?, mutation);
        Ok(())
    }

    #[test]
    fn vector_mutation_decoder_rejects_invalid_q15_and_trailing_bytes() -> Result<(), Box<dyn Error>>
    {
        let space = VectorSpaceName::new("semantic")?;
        let vector = Mutation::upsert_vector(space, b"object", Q15Vector::new(vec![1, 2])?);
        let mut encoded = vector.encode()?;
        encoded.push(0);
        assert_eq!(Mutation::decode(&encoded), Err(MutationError::Malformed));
        Ok(())
    }

    #[test]
    fn mutation_codec_rejects_noncanonical_lengths() -> Result<(), Box<dyn Error>> {
        let mut encoded = Mutation::put(b"key", b"value").encode()?;
        encoded.push(0);
        assert_eq!(Mutation::decode(&encoded), Err(MutationError::Malformed));
        Ok(())
    }

    #[test]
    fn keys_are_bounded_before_encoding() {
        let result = Mutation::delete(vec![0; MAX_KEY_BYTES + 1]).encode();
        assert!(matches!(result, Err(MutationError::KeyTooLarge { .. })));
    }
}
