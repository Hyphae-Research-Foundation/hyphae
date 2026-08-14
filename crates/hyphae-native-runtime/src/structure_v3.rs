// SPDX-License-Identifier: AGPL-3.0-only

//! Canonical physical codecs for the incarnation-fenced `HYSTRBT3` layout.
//!
//! The module remains private until migration and complete command-surface
//! integration are accepted. Keeping the codec executable now freezes the
//! bytes that those later paths must share.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ops::{Bound, ControlFlow},
};

use hyphae_native_blobs::BlobStore;
use hyphae_native_btree::{BTREE_MAX_KEY_SIZE, BTree};
use hyphae_native_pages::{BufferPool, PageStore};
use hyphae_native_records::BlobReference;
use hyphae_native_types::{Csn, TransactionId};
use thiserror::Error;

use crate::{
    EngineKind, HashFieldEntry, HashPatternMatchBudget, HashPatternScanPage,
    HashPatternScanRequest, HashPatternScanStop, Mutation, NativeRuntimeError, Opcode,
    STRUCTURE_EXPIRY_LIVE, STRUCTURE_EXPIRY_TOMBSTONE, STRUCTURE_HASH_EXPIRY_LIVE,
    STRUCTURE_HASH_FIELD_EXPIRY_LIVE, STRUCTURE_HASH_FIELD_PREFIX, STRUCTURE_LIST_CHUNK_PREFIX,
    STRUCTURE_LIST_EXPIRY_LIVE, STRUCTURE_SET_EXPIRY_LIVE, STRUCTURE_SET_MEMBER_PREFIX,
    STRUCTURE_SORTED_SET_EXPIRY_LIVE, STRUCTURE_SORTED_SET_MEMBER_PREFIX,
    STRUCTURE_SORTED_SET_ORDER_PREFIX, STRUCTURE_STREAM_ENTRY_PREFIX, STRUCTURE_STREAM_EXPIRY_LIVE,
    SetAlgebraError, SetAlgebraExecution, SetAlgebraOperation, SetAlgebraRequest, SetAlgebraResult,
    SortedSetDirection, SortedSetEntry, SortedSetScore, StructureEntry, Ttl, byte_prefix_successor,
    decode_list_chunk_storage, decode_set_member_value, decode_sorted_set_score,
    decode_stream_wal_entry, decode_structure_value, encode_list_chunk_storage,
    encode_sorted_set_score, encode_stream_wal_entry, hash_pattern_lower_bound,
    is_structure_tombstone, set_member_live_value, structure_expiry_key, structure_hash_meta_key,
    structure_key, structure_list_meta_key, structure_set_meta_key, structure_sorted_set_meta_key,
    structure_storage_value, structure_stream_meta_key, structure_tombstone_value,
    structure_value_expiry,
};

pub(crate) const STRUCTURE_FORMAT_VALUE_V3: &[u8; 8] = b"HYSTRBT3";
pub(crate) const STRUCTURE_INCARNATION_BYTES: usize = 20;
pub(crate) const STRUCTURE_RETIREMENT_PREFIX: u8 = 15;
pub(crate) const MAX_STRUCTURE_RETIREMENT_STEP_ENTRIES: usize = 1_024;

const COLLECTION_METADATA_MAGIC: &[u8; 8] = b"HYSMV301";
const RETIREMENT_MAGIC: &[u8; 8] = b"HYSRT301";
const COLLECTION_METADATA_HEADER_BYTES: usize = 36;
const RETIREMENT_HEADER_BYTES: usize = 92;
const METADATA_LIVE: u8 = 1;
const METADATA_TOMBSTONE: u8 = 2;
const RETIREMENT_ACTIVE: u8 = 1;
const RESERVED_BYTES: [u8; 6] = [0; 6];
const COLLECTION_COMMON_PAYLOAD_BYTES: usize = 24;
const COLLECTION_LIST_PAYLOAD_BYTES: usize = 48;
const COLLECTION_STREAM_PAYLOAD_BYTES: usize = 32;
const PAYLOAD_HAS_EXPIRY: u8 = 1;
const COLLECTION_HASH_PAYLOAD_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum StructureV3CodecError {
    #[error("HYSTRBT3 identity exceeds the native B+tree key limit")]
    IdentityTooLarge,
    #[error("HYSTRBT3 encoding is malformed or noncanonical")]
    Malformed,
    #[error("HYSTRBT3 retirement step violates its bounded progress contract")]
    InvalidRetirementStep,
    #[error("HYSTRBT3 lifecycle mutation ordinal exceeds 32 bits")]
    MutationOrdinalOverflow,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct StructureIncarnation {
    transaction_id: TransactionId,
    mutation_ordinal: u32,
}

impl StructureIncarnation {
    pub(crate) const fn new(transaction_id: TransactionId, mutation_ordinal: u32) -> Self {
        Self {
            transaction_id,
            mutation_ordinal,
        }
    }

    pub(crate) const fn transaction_id(self) -> TransactionId {
        self.transaction_id
    }

    pub(crate) const fn mutation_ordinal(self) -> u32 {
        self.mutation_ordinal
    }

    pub(crate) fn from_mutation_index(
        transaction_id: TransactionId,
        mutation_index: usize,
    ) -> Result<Self, StructureV3CodecError> {
        let mutation_ordinal = u32::try_from(mutation_index)
            .map_err(|_| StructureV3CodecError::MutationOrdinalOverflow)?;
        Ok(Self::new(transaction_id, mutation_ordinal))
    }

    fn encode(self) -> [u8; STRUCTURE_INCARNATION_BYTES] {
        let mut encoded = [0_u8; STRUCTURE_INCARNATION_BYTES];
        encoded[..16].copy_from_slice(&self.transaction_id.get().to_be_bytes());
        encoded[16..].copy_from_slice(&self.mutation_ordinal.to_be_bytes());
        encoded
    }

    fn decode(encoded: &[u8]) -> Result<Self, StructureV3CodecError> {
        if encoded.len() != STRUCTURE_INCARNATION_BYTES {
            return Err(StructureV3CodecError::Malformed);
        }
        let transaction_id = TransactionId::new(u128::from_be_bytes(
            encoded[..16]
                .try_into()
                .map_err(|_| StructureV3CodecError::Malformed)?,
        ))
        .map_err(|_| StructureV3CodecError::Malformed)?;
        let mutation_ordinal = u32::from_be_bytes(
            encoded[16..]
                .try_into()
                .map_err(|_| StructureV3CodecError::Malformed)?,
        );
        Ok(Self::new(transaction_id, mutation_ordinal))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum StructureCollectionFamily {
    Hash = 1,
    Set = 2,
    List = 3,
    SortedSet = 4,
    Stream = 5,
}

impl StructureCollectionFamily {
    fn decode(encoded: u8) -> Result<Self, StructureV3CodecError> {
        match encoded {
            1 => Ok(Self::Hash),
            2 => Ok(Self::Set),
            3 => Ok(Self::List),
            4 => Ok(Self::SortedSet),
            5 => Ok(Self::Stream),
            _ => Err(StructureV3CodecError::Malformed),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CollectionMetadataV3 {
    Live {
        family: StructureCollectionFamily,
        incarnation: StructureIncarnation,
        family_payload: Vec<u8>,
    },
    Tombstone {
        family: StructureCollectionFamily,
        retired_incarnation: StructureIncarnation,
    },
}

fn encode_collection_metadata(metadata: &CollectionMetadataV3) -> Vec<u8> {
    let (family, state, incarnation, payload) = match metadata {
        CollectionMetadataV3::Live {
            family,
            incarnation,
            family_payload,
        } => (
            *family,
            METADATA_LIVE,
            *incarnation,
            family_payload.as_slice(),
        ),
        CollectionMetadataV3::Tombstone {
            family,
            retired_incarnation,
        } => (*family, METADATA_TOMBSTONE, *retired_incarnation, &[][..]),
    };
    let mut encoded = Vec::with_capacity(COLLECTION_METADATA_HEADER_BYTES + payload.len());
    encoded.extend_from_slice(COLLECTION_METADATA_MAGIC);
    encoded.push(family as u8);
    encoded.push(state);
    encoded.extend_from_slice(&RESERVED_BYTES);
    encoded.extend_from_slice(&incarnation.encode());
    encoded.extend_from_slice(payload);
    encoded
}

fn decode_collection_metadata(
    encoded: &[u8],
) -> Result<CollectionMetadataV3, StructureV3CodecError> {
    if encoded.len() < COLLECTION_METADATA_HEADER_BYTES
        || encoded.get(..8) != Some(COLLECTION_METADATA_MAGIC.as_slice())
        || encoded.get(10..16) != Some(RESERVED_BYTES.as_slice())
    {
        return Err(StructureV3CodecError::Malformed);
    }
    let family = StructureCollectionFamily::decode(encoded[8])?;
    let incarnation = StructureIncarnation::decode(&encoded[16..36])?;
    let payload = &encoded[COLLECTION_METADATA_HEADER_BYTES..];
    match encoded[9] {
        METADATA_LIVE => Ok(CollectionMetadataV3::Live {
            family,
            incarnation,
            family_payload: payload.to_vec(),
        }),
        METADATA_TOMBSTONE if payload.is_empty() => Ok(CollectionMetadataV3::Tombstone {
            family,
            retired_incarnation: incarnation,
        }),
        _ => Err(StructureV3CodecError::Malformed),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CollectionStateV3 {
    Hash {
        field_count: u64,
        field_expiry_count: u64,
        expires_at_micros: Option<i64>,
    },
    Set {
        member_count: u64,
        expires_at_micros: Option<i64>,
    },
    List {
        element_count: u64,
        logical_value_bytes: u64,
        head_chunk: i64,
        tail_chunk: i64,
        expires_at_micros: Option<i64>,
    },
    SortedSet {
        member_count: u64,
        expires_at_micros: Option<i64>,
    },
    Stream {
        entry_count: u64,
        last_id: u64,
        expires_at_micros: Option<i64>,
    },
}

impl CollectionStateV3 {
    pub(crate) const fn family(self) -> StructureCollectionFamily {
        match self {
            Self::Hash { .. } => StructureCollectionFamily::Hash,
            Self::Set { .. } => StructureCollectionFamily::Set,
            Self::List { .. } => StructureCollectionFamily::List,
            Self::SortedSet { .. } => StructureCollectionFamily::SortedSet,
            Self::Stream { .. } => StructureCollectionFamily::Stream,
        }
    }

    pub(crate) const fn logical_items(self) -> u64 {
        match self {
            Self::Hash { field_count, .. } => field_count,
            Self::Set { member_count, .. } | Self::SortedSet { member_count, .. } => member_count,
            Self::List { element_count, .. } => element_count,
            Self::Stream { entry_count, .. } => entry_count,
        }
    }

    pub(crate) const fn expires_at_micros(self) -> Option<i64> {
        match self {
            Self::Hash {
                expires_at_micros, ..
            }
            | Self::Set {
                expires_at_micros, ..
            }
            | Self::List {
                expires_at_micros, ..
            }
            | Self::SortedSet {
                expires_at_micros, ..
            }
            | Self::Stream {
                expires_at_micros, ..
            } => expires_at_micros,
        }
    }

    const fn with_expiry(self, expires_at_micros: i64) -> Self {
        match self {
            Self::Hash {
                field_count,
                field_expiry_count,
                ..
            } => Self::Hash {
                field_count,
                field_expiry_count,
                expires_at_micros: Some(expires_at_micros),
            },
            Self::Set { member_count, .. } => Self::Set {
                member_count,
                expires_at_micros: Some(expires_at_micros),
            },
            Self::List {
                element_count,
                logical_value_bytes,
                head_chunk,
                tail_chunk,
                ..
            } => Self::List {
                element_count,
                logical_value_bytes,
                head_chunk,
                tail_chunk,
                expires_at_micros: Some(expires_at_micros),
            },
            Self::SortedSet { member_count, .. } => Self::SortedSet {
                member_count,
                expires_at_micros: Some(expires_at_micros),
            },
            Self::Stream {
                entry_count,
                last_id,
                ..
            } => Self::Stream {
                entry_count,
                last_id,
                expires_at_micros: Some(expires_at_micros),
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TypedCollectionMetadataV3 {
    Live {
        incarnation: StructureIncarnation,
        state: CollectionStateV3,
    },
    Tombstone {
        family: StructureCollectionFamily,
        retired_incarnation: StructureIncarnation,
    },
}

pub(crate) fn encode_typed_collection_metadata(
    metadata: &TypedCollectionMetadataV3,
) -> Result<Vec<u8>, StructureV3CodecError> {
    let generic = match metadata {
        TypedCollectionMetadataV3::Live { incarnation, state } => CollectionMetadataV3::Live {
            family: state.family(),
            incarnation: *incarnation,
            family_payload: encode_collection_state(*state)?,
        },
        TypedCollectionMetadataV3::Tombstone {
            family,
            retired_incarnation,
        } => CollectionMetadataV3::Tombstone {
            family: *family,
            retired_incarnation: *retired_incarnation,
        },
    };
    Ok(encode_collection_metadata(&generic))
}

pub(crate) fn decode_typed_collection_metadata(
    encoded: &[u8],
) -> Result<TypedCollectionMetadataV3, StructureV3CodecError> {
    match decode_collection_metadata(encoded)? {
        CollectionMetadataV3::Live {
            family,
            incarnation,
            family_payload,
        } => Ok(TypedCollectionMetadataV3::Live {
            incarnation,
            state: decode_collection_state(family, &family_payload)?,
        }),
        CollectionMetadataV3::Tombstone {
            family,
            retired_incarnation,
        } => Ok(TypedCollectionMetadataV3::Tombstone {
            family,
            retired_incarnation,
        }),
    }
}

fn encode_collection_state(state: CollectionStateV3) -> Result<Vec<u8>, StructureV3CodecError> {
    validate_collection_state(state)?;
    let mut encoded = Vec::with_capacity(match state {
        CollectionStateV3::Hash { .. } => COLLECTION_HASH_PAYLOAD_BYTES,
        CollectionStateV3::List { .. } => COLLECTION_LIST_PAYLOAD_BYTES,
        CollectionStateV3::Stream { .. } => COLLECTION_STREAM_PAYLOAD_BYTES,
        _ => COLLECTION_COMMON_PAYLOAD_BYTES,
    });
    encode_common_collection_state(
        state.logical_items(),
        state.expires_at_micros(),
        &mut encoded,
    );
    match state {
        CollectionStateV3::Hash {
            field_expiry_count, ..
        } => {
            encoded.extend_from_slice(&field_expiry_count.to_be_bytes());
        }
        CollectionStateV3::List {
            logical_value_bytes,
            head_chunk,
            tail_chunk,
            ..
        } => {
            encoded.extend_from_slice(&logical_value_bytes.to_be_bytes());
            encoded.extend_from_slice(&head_chunk.to_be_bytes());
            encoded.extend_from_slice(&tail_chunk.to_be_bytes());
        }
        CollectionStateV3::Stream { last_id, .. } => {
            encoded.extend_from_slice(&last_id.to_be_bytes());
        }
        _ => {}
    }
    Ok(encoded)
}

fn decode_collection_state(
    family: StructureCollectionFamily,
    encoded: &[u8],
) -> Result<CollectionStateV3, StructureV3CodecError> {
    let expected_length = match family {
        StructureCollectionFamily::Hash => COLLECTION_HASH_PAYLOAD_BYTES,
        StructureCollectionFamily::List => COLLECTION_LIST_PAYLOAD_BYTES,
        StructureCollectionFamily::Stream => COLLECTION_STREAM_PAYLOAD_BYTES,
        _ => COLLECTION_COMMON_PAYLOAD_BYTES,
    };
    if encoded.len() != expected_length {
        return Err(StructureV3CodecError::Malformed);
    }
    let (logical_items, expires_at_micros) = decode_common_collection_state(encoded)?;
    let state = match family {
        StructureCollectionFamily::Hash => CollectionStateV3::Hash {
            field_count: logical_items,
            field_expiry_count: decode_u64(&encoded[24..32])?,
            expires_at_micros,
        },
        StructureCollectionFamily::Set => CollectionStateV3::Set {
            member_count: logical_items,
            expires_at_micros,
        },
        StructureCollectionFamily::List => CollectionStateV3::List {
            element_count: logical_items,
            logical_value_bytes: decode_u64(&encoded[24..32])?,
            head_chunk: decode_i64(&encoded[32..40])?,
            tail_chunk: decode_i64(&encoded[40..48])?,
            expires_at_micros,
        },
        StructureCollectionFamily::SortedSet => CollectionStateV3::SortedSet {
            member_count: logical_items,
            expires_at_micros,
        },
        StructureCollectionFamily::Stream => CollectionStateV3::Stream {
            entry_count: logical_items,
            last_id: decode_u64(&encoded[24..32])?,
            expires_at_micros,
        },
    };
    validate_collection_state(state)?;
    Ok(state)
}

fn encode_common_collection_state(
    logical_items: u64,
    expires_at_micros: Option<i64>,
    encoded: &mut Vec<u8>,
) {
    encoded.extend_from_slice(&logical_items.to_be_bytes());
    encoded.push(u8::from(expires_at_micros.is_some()) * PAYLOAD_HAS_EXPIRY);
    encoded.extend_from_slice(&[0; 7]);
    encoded.extend_from_slice(&expires_at_micros.unwrap_or(0).to_be_bytes());
}

fn decode_common_collection_state(
    encoded: &[u8],
) -> Result<(u64, Option<i64>), StructureV3CodecError> {
    if encoded.len() < COLLECTION_COMMON_PAYLOAD_BYTES
        || encoded[9..16].iter().any(|byte| *byte != 0)
    {
        return Err(StructureV3CodecError::Malformed);
    }
    let logical_items = decode_u64(&encoded[..8])?;
    let raw_expiry = decode_i64(&encoded[16..24])?;
    let expires_at_micros = match encoded[8] {
        0 if raw_expiry == 0 => None,
        PAYLOAD_HAS_EXPIRY => Some(raw_expiry),
        _ => return Err(StructureV3CodecError::Malformed),
    };
    Ok((logical_items, expires_at_micros))
}

fn validate_collection_state(state: CollectionStateV3) -> Result<(), StructureV3CodecError> {
    match state {
        CollectionStateV3::Hash {
            field_count,
            field_expiry_count,
            ..
        } if field_expiry_count > field_count => {
            return Err(StructureV3CodecError::Malformed);
        }
        CollectionStateV3::List {
            element_count,
            logical_value_bytes,
            head_chunk,
            tail_chunk,
            ..
        } => {
            if element_count == 0 {
                if logical_value_bytes != 0 || head_chunk != 0 || tail_chunk != 0 {
                    return Err(StructureV3CodecError::Malformed);
                }
            } else {
                if head_chunk > tail_chunk {
                    return Err(StructureV3CodecError::Malformed);
                }
                let chunk_count = tail_chunk
                    .checked_sub(head_chunk)
                    .and_then(|distance| distance.checked_add(1))
                    .and_then(|count| u64::try_from(count).ok())
                    .ok_or(StructureV3CodecError::Malformed)?;
                if chunk_count > element_count {
                    return Err(StructureV3CodecError::Malformed);
                }
            }
        }
        CollectionStateV3::Stream {
            entry_count,
            last_id,
            ..
        } if (entry_count == 0) != (last_id == 0) || entry_count > last_id => {
            return Err(StructureV3CodecError::Malformed);
        }
        _ => {}
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DecodedCollectionChild<'encoded> {
    pub(crate) prefix: u8,
    pub(crate) collection_key: &'encoded [u8],
    pub(crate) incarnation: StructureIncarnation,
    pub(crate) child_identity: &'encoded [u8],
}

pub(crate) fn encode_collection_child_key(
    prefix: u8,
    collection_key: &[u8],
    incarnation: StructureIncarnation,
    child_identity: &[u8],
) -> Result<Vec<u8>, StructureV3CodecError> {
    validate_child_prefix(prefix)?;
    let key_length =
        u32::try_from(collection_key.len()).map_err(|_| StructureV3CodecError::IdentityTooLarge)?;
    let encoded_length = 1_usize
        .checked_add(4)
        .and_then(|length| length.checked_add(collection_key.len()))
        .and_then(|length| length.checked_add(STRUCTURE_INCARNATION_BYTES))
        .and_then(|length| length.checked_add(child_identity.len()))
        .ok_or(StructureV3CodecError::IdentityTooLarge)?;
    if encoded_length > BTREE_MAX_KEY_SIZE {
        return Err(StructureV3CodecError::IdentityTooLarge);
    }
    let mut encoded = Vec::with_capacity(encoded_length);
    encoded.push(prefix);
    encoded.extend_from_slice(&key_length.to_be_bytes());
    encoded.extend_from_slice(collection_key);
    encoded.extend_from_slice(&incarnation.encode());
    encoded.extend_from_slice(child_identity);
    Ok(encoded)
}

fn validate_collection_child_identity_v3(
    collection_key: &[u8],
    child_identity: &[u8],
) -> Result<(), NativeRuntimeError> {
    let encoded_length = 1_usize
        .checked_add(4)
        .and_then(|length| length.checked_add(collection_key.len()))
        .and_then(|length| length.checked_add(STRUCTURE_INCARNATION_BYTES))
        .and_then(|length| length.checked_add(child_identity.len()))
        .ok_or(NativeRuntimeError::StructureIdentityTooLarge)?;
    if encoded_length > BTREE_MAX_KEY_SIZE {
        return Err(NativeRuntimeError::StructureIdentityTooLarge);
    }
    Ok(())
}

pub(crate) fn decode_collection_child_key(
    encoded: &[u8],
) -> Result<DecodedCollectionChild<'_>, StructureV3CodecError> {
    let prefix = *encoded.first().ok_or(StructureV3CodecError::Malformed)?;
    validate_child_prefix(prefix)?;
    let key_length = decode_u32(encoded.get(1..5).ok_or(StructureV3CodecError::Malformed)?)?;
    let key_length = usize::try_from(key_length).map_err(|_| StructureV3CodecError::Malformed)?;
    let incarnation_start = 5_usize
        .checked_add(key_length)
        .ok_or(StructureV3CodecError::Malformed)?;
    let child_start = incarnation_start
        .checked_add(STRUCTURE_INCARNATION_BYTES)
        .ok_or(StructureV3CodecError::Malformed)?;
    if child_start > encoded.len() {
        return Err(StructureV3CodecError::Malformed);
    }
    Ok(DecodedCollectionChild {
        prefix,
        collection_key: &encoded[5..incarnation_start],
        incarnation: StructureIncarnation::decode(&encoded[incarnation_start..child_start])?,
        child_identity: &encoded[child_start..],
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DecodedCollectionExpiry<'encoded> {
    pub(crate) expires_at_micros: i64,
    pub(crate) collection_key: &'encoded [u8],
    pub(crate) incarnation: StructureIncarnation,
    pub(crate) child_identity: &'encoded [u8],
}

pub(crate) fn encode_collection_expiry_key(
    prefix: u8,
    expires_at_micros: i64,
    collection_key: &[u8],
    incarnation: StructureIncarnation,
    child_identity: &[u8],
) -> Result<Vec<u8>, StructureV3CodecError> {
    if !matches!(
        prefix,
        crate::STRUCTURE_EXPIRY_PREFIX | crate::STRUCTURE_HASH_FIELD_EXPIRY_PREFIX
    ) {
        return Err(StructureV3CodecError::Malformed);
    }
    let key_length =
        u32::try_from(collection_key.len()).map_err(|_| StructureV3CodecError::IdentityTooLarge)?;
    let encoded_length = 1_usize
        .checked_add(8)
        .and_then(|length| length.checked_add(4))
        .and_then(|length| length.checked_add(collection_key.len()))
        .and_then(|length| length.checked_add(STRUCTURE_INCARNATION_BYTES))
        .and_then(|length| length.checked_add(child_identity.len()))
        .ok_or(StructureV3CodecError::IdentityTooLarge)?;
    if encoded_length > BTREE_MAX_KEY_SIZE {
        return Err(StructureV3CodecError::IdentityTooLarge);
    }
    let sortable = expires_at_micros.cast_unsigned() ^ (1_u64 << 63);
    let mut encoded = Vec::with_capacity(encoded_length);
    encoded.push(prefix);
    encoded.extend_from_slice(&sortable.to_be_bytes());
    encoded.extend_from_slice(&key_length.to_be_bytes());
    encoded.extend_from_slice(collection_key);
    encoded.extend_from_slice(&incarnation.encode());
    encoded.extend_from_slice(child_identity);
    Ok(encoded)
}

pub(crate) fn decode_collection_expiry_key(
    encoded: &[u8],
) -> Result<DecodedCollectionExpiry<'_>, StructureV3CodecError> {
    if !matches!(
        encoded.first(),
        Some(&crate::STRUCTURE_EXPIRY_PREFIX | &crate::STRUCTURE_HASH_FIELD_EXPIRY_PREFIX)
    ) {
        return Err(StructureV3CodecError::Malformed);
    }
    let sortable = u64::from_be_bytes(
        encoded
            .get(1..9)
            .ok_or(StructureV3CodecError::Malformed)?
            .try_into()
            .map_err(|_| StructureV3CodecError::Malformed)?,
    );
    let key_length = decode_u32(encoded.get(9..13).ok_or(StructureV3CodecError::Malformed)?)?;
    let key_length = usize::try_from(key_length).map_err(|_| StructureV3CodecError::Malformed)?;
    let incarnation_start = 13_usize
        .checked_add(key_length)
        .ok_or(StructureV3CodecError::Malformed)?;
    let child_start = incarnation_start
        .checked_add(STRUCTURE_INCARNATION_BYTES)
        .ok_or(StructureV3CodecError::Malformed)?;
    if child_start > encoded.len() {
        return Err(StructureV3CodecError::Malformed);
    }
    Ok(DecodedCollectionExpiry {
        expires_at_micros: (sortable ^ (1_u64 << 63)).cast_signed(),
        collection_key: &encoded[13..incarnation_start],
        incarnation: StructureIncarnation::decode(&encoded[incarnation_start..child_start])?,
        child_identity: &encoded[child_start..],
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RetirementRecordV3 {
    pub(crate) family: StructureCollectionFamily,
    pub(crate) declared_logical_items: u64,
    pub(crate) remaining_logical_items: u64,
    pub(crate) remaining_primary_entries: u64,
    pub(crate) remaining_secondary_entries: u64,
    pub(crate) remaining_expiry_entries: u64,
    pub(crate) remaining_logical_bytes: u64,
    pub(crate) list_head_chunk: i64,
    pub(crate) list_tail_chunk: i64,
    pub(crate) stream_last_id: u64,
    pub(crate) exclusive_cursor: Option<Vec<u8>>,
}

impl RetirementRecordV3 {
    pub(crate) fn new(
        family: StructureCollectionFamily,
        declared_logical_items: u64,
        primary_entries: u64,
        secondary_entries: u64,
        expiry_entries: u64,
        logical_bytes: u64,
    ) -> Result<Self, StructureV3CodecError> {
        let record = Self {
            family,
            declared_logical_items,
            remaining_logical_items: declared_logical_items,
            remaining_primary_entries: primary_entries,
            remaining_secondary_entries: secondary_entries,
            remaining_expiry_entries: expiry_entries,
            remaining_logical_bytes: logical_bytes,
            list_head_chunk: 0,
            list_tail_chunk: 0,
            stream_last_id: 0,
            exclusive_cursor: None,
        };
        validate_retirement_record_shape(&record)?;
        Ok(record)
    }

    pub(crate) fn new_list(
        declared_logical_items: u64,
        logical_bytes: u64,
        head_chunk: i64,
        tail_chunk: i64,
    ) -> Result<Self, StructureV3CodecError> {
        let primary_entries = if declared_logical_items == 0 {
            0
        } else {
            list_chunk_range_count(head_chunk, tail_chunk)?
        };
        let record = Self {
            family: StructureCollectionFamily::List,
            declared_logical_items,
            remaining_logical_items: declared_logical_items,
            remaining_primary_entries: primary_entries,
            remaining_secondary_entries: 0,
            remaining_expiry_entries: 0,
            remaining_logical_bytes: logical_bytes,
            list_head_chunk: head_chunk,
            list_tail_chunk: tail_chunk,
            stream_last_id: 0,
            exclusive_cursor: None,
        };
        validate_retirement_record_shape(&record)?;
        Ok(record)
    }

    pub(crate) fn new_stream(
        entry_count: u64,
        last_id: u64,
    ) -> Result<Self, StructureV3CodecError> {
        let record = Self {
            family: StructureCollectionFamily::Stream,
            declared_logical_items: entry_count,
            remaining_logical_items: entry_count,
            remaining_primary_entries: entry_count,
            remaining_secondary_entries: 0,
            remaining_expiry_entries: 0,
            remaining_logical_bytes: 0,
            list_head_chunk: 0,
            list_tail_chunk: 0,
            stream_last_id: last_id,
            exclusive_cursor: None,
        };
        validate_retirement_record_shape(&record)?;
        Ok(record)
    }
}

pub(crate) fn encode_retirement_key(
    collection_key: &[u8],
    incarnation: StructureIncarnation,
) -> Result<Vec<u8>, StructureV3CodecError> {
    let key_length =
        u32::try_from(collection_key.len()).map_err(|_| StructureV3CodecError::IdentityTooLarge)?;
    let encoded_length = 5_usize
        .checked_add(collection_key.len())
        .and_then(|length| length.checked_add(STRUCTURE_INCARNATION_BYTES))
        .ok_or(StructureV3CodecError::IdentityTooLarge)?;
    if encoded_length > BTREE_MAX_KEY_SIZE {
        return Err(StructureV3CodecError::IdentityTooLarge);
    }
    let mut encoded = Vec::with_capacity(encoded_length);
    encoded.push(STRUCTURE_RETIREMENT_PREFIX);
    encoded.extend_from_slice(&key_length.to_be_bytes());
    encoded.extend_from_slice(collection_key);
    encoded.extend_from_slice(&incarnation.encode());
    Ok(encoded)
}

pub(crate) fn decode_retirement_key(
    encoded: &[u8],
) -> Result<(&[u8], StructureIncarnation), StructureV3CodecError> {
    if encoded.first() != Some(&STRUCTURE_RETIREMENT_PREFIX) {
        return Err(StructureV3CodecError::Malformed);
    }
    let key_length = decode_u32(encoded.get(1..5).ok_or(StructureV3CodecError::Malformed)?)?;
    let key_length = usize::try_from(key_length).map_err(|_| StructureV3CodecError::Malformed)?;
    let incarnation_start = 5_usize
        .checked_add(key_length)
        .ok_or(StructureV3CodecError::Malformed)?;
    let incarnation_end = incarnation_start
        .checked_add(STRUCTURE_INCARNATION_BYTES)
        .ok_or(StructureV3CodecError::Malformed)?;
    if incarnation_end != encoded.len() {
        return Err(StructureV3CodecError::Malformed);
    }
    Ok((
        &encoded[5..incarnation_start],
        StructureIncarnation::decode(&encoded[incarnation_start..incarnation_end])?,
    ))
}

pub(crate) fn encode_retirement_record(
    record: &RetirementRecordV3,
) -> Result<Vec<u8>, StructureV3CodecError> {
    validate_retirement_record_shape(record)?;
    let cursor = record.exclusive_cursor.as_deref().unwrap_or_default();
    if record.exclusive_cursor.is_some() && (cursor.is_empty() || cursor.len() > BTREE_MAX_KEY_SIZE)
    {
        return Err(StructureV3CodecError::Malformed);
    }
    let cursor_length =
        u32::try_from(cursor.len()).map_err(|_| StructureV3CodecError::Malformed)?;
    let mut encoded = Vec::with_capacity(RETIREMENT_HEADER_BYTES + cursor.len());
    encoded.extend_from_slice(RETIREMENT_MAGIC);
    encoded.push(record.family as u8);
    encoded.push(RETIREMENT_ACTIVE);
    encoded.extend_from_slice(&RESERVED_BYTES);
    for counter in [
        record.declared_logical_items,
        record.remaining_logical_items,
        record.remaining_primary_entries,
        record.remaining_secondary_entries,
        record.remaining_expiry_entries,
        record.remaining_logical_bytes,
    ] {
        encoded.extend_from_slice(&counter.to_be_bytes());
    }
    encoded.extend_from_slice(&record.list_head_chunk.to_be_bytes());
    encoded.extend_from_slice(&record.list_tail_chunk.to_be_bytes());
    encoded.extend_from_slice(&record.stream_last_id.to_be_bytes());
    encoded.extend_from_slice(&cursor_length.to_be_bytes());
    encoded.extend_from_slice(cursor);
    Ok(encoded)
}

pub(crate) fn decode_retirement_record(
    encoded: &[u8],
) -> Result<RetirementRecordV3, StructureV3CodecError> {
    if encoded.len() < RETIREMENT_HEADER_BYTES
        || encoded.get(..8) != Some(RETIREMENT_MAGIC.as_slice())
        || encoded[9] != RETIREMENT_ACTIVE
        || encoded.get(10..16) != Some(RESERVED_BYTES.as_slice())
    {
        return Err(StructureV3CodecError::Malformed);
    }
    let cursor_length = decode_u32(&encoded[88..92])?;
    let cursor_length =
        usize::try_from(cursor_length).map_err(|_| StructureV3CodecError::Malformed)?;
    if encoded.len() != RETIREMENT_HEADER_BYTES.saturating_add(cursor_length) {
        return Err(StructureV3CodecError::Malformed);
    }
    let cursor = &encoded[RETIREMENT_HEADER_BYTES..];
    if cursor.len() > BTREE_MAX_KEY_SIZE {
        return Err(StructureV3CodecError::Malformed);
    }
    let record = RetirementRecordV3 {
        family: StructureCollectionFamily::decode(encoded[8])?,
        declared_logical_items: decode_u64(&encoded[16..24])?,
        remaining_logical_items: decode_u64(&encoded[24..32])?,
        remaining_primary_entries: decode_u64(&encoded[32..40])?,
        remaining_secondary_entries: decode_u64(&encoded[40..48])?,
        remaining_expiry_entries: decode_u64(&encoded[48..56])?,
        remaining_logical_bytes: decode_u64(&encoded[56..64])?,
        list_head_chunk: decode_i64(&encoded[64..72])?,
        list_tail_chunk: decode_i64(&encoded[72..80])?,
        stream_last_id: decode_u64(&encoded[80..88])?,
        exclusive_cursor: (!cursor.is_empty()).then(|| cursor.to_vec()),
    };
    validate_retirement_record_shape(&record)?;
    Ok(record)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RetirementCandidateV3<'key> {
    pub(crate) physical_key: &'key [u8],
    pub(crate) live: bool,
    pub(crate) logical_items: u64,
    pub(crate) logical_bytes: u64,
    pub(crate) associated_secondary_entries: u64,
    pub(crate) associated_expiry_entries: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RetirementStepV3 {
    pub(crate) record: RetirementRecordV3,
    pub(crate) processed_entries: usize,
    pub(crate) more_remaining: bool,
}

pub(crate) fn advance_retirement_record(
    retirement_key: &[u8],
    record: &RetirementRecordV3,
    candidates: &[RetirementCandidateV3<'_>],
    entry_budget: usize,
    scan_exhausted: bool,
) -> Result<RetirementStepV3, StructureV3CodecError> {
    validate_retirement_record_shape(record)?;
    validate_retirement_state(retirement_key, record)?;
    if entry_budget == 0
        || entry_budget > MAX_STRUCTURE_RETIREMENT_STEP_ENTRIES
        || candidates.len() > entry_budget
        || (candidates.is_empty() && !scan_exhausted)
    {
        return Err(StructureV3CodecError::InvalidRetirementStep);
    }
    let (collection_key, incarnation) = decode_retirement_key(retirement_key)?;
    let mut updated = record.clone();
    let mut previous = record.exclusive_cursor.as_deref();
    for candidate in candidates {
        if previous.is_some_and(|cursor| candidate.physical_key <= cursor) {
            return Err(StructureV3CodecError::InvalidRetirementStep);
        }
        validate_list_retirement_candidate(&updated, candidate)?;
        validate_stream_retirement_candidate(&updated, candidate)?;
        let counters =
            retirement_candidate_counters(candidate, collection_key, incarnation, record.family)?;
        updated.remaining_logical_items = updated
            .remaining_logical_items
            .checked_sub(counters.logical_items)
            .ok_or(StructureV3CodecError::InvalidRetirementStep)?;
        updated.remaining_primary_entries = updated
            .remaining_primary_entries
            .checked_sub(counters.primary_entries)
            .ok_or(StructureV3CodecError::InvalidRetirementStep)?;
        updated.remaining_secondary_entries = updated
            .remaining_secondary_entries
            .checked_sub(counters.secondary_entries)
            .ok_or(StructureV3CodecError::InvalidRetirementStep)?;
        updated.remaining_expiry_entries = updated
            .remaining_expiry_entries
            .checked_sub(counters.expiry_entries)
            .ok_or(StructureV3CodecError::InvalidRetirementStep)?;
        updated.remaining_logical_bytes = updated
            .remaining_logical_bytes
            .checked_sub(counters.logical_bytes)
            .ok_or(StructureV3CodecError::InvalidRetirementStep)?;
        previous = Some(candidate.physical_key);
    }
    if let Some(last) = candidates.last() {
        updated.exclusive_cursor = Some(last.physical_key.to_vec());
    }
    validate_retirement_record_shape(&updated)
        .map_err(|_| StructureV3CodecError::InvalidRetirementStep)?;
    if scan_exhausted && retirement_has_remaining_work(&updated) {
        return Err(StructureV3CodecError::InvalidRetirementStep);
    }
    Ok(RetirementStepV3 {
        record: updated,
        processed_entries: candidates.len(),
        more_remaining: !scan_exhausted,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RetirementCandidateCounters {
    logical_items: u64,
    primary_entries: u64,
    secondary_entries: u64,
    expiry_entries: u64,
    logical_bytes: u64,
}

fn retirement_candidate_counters(
    candidate: &RetirementCandidateV3<'_>,
    collection_key: &[u8],
    incarnation: StructureIncarnation,
    family: StructureCollectionFamily,
) -> Result<RetirementCandidateCounters, StructureV3CodecError> {
    let decoded = decode_collection_child_key(candidate.physical_key)
        .map_err(|_| StructureV3CodecError::InvalidRetirementStep)?;
    if decoded.collection_key != collection_key
        || decoded.incarnation != incarnation
        || !child_prefix_belongs_to_family(decoded.prefix, family)
    {
        return Err(StructureV3CodecError::InvalidRetirementStep);
    }
    if !candidate.live {
        if candidate.logical_items != 0
            || candidate.logical_bytes != 0
            || candidate.associated_secondary_entries != 0
            || candidate.associated_expiry_entries != 0
        {
            return Err(StructureV3CodecError::InvalidRetirementStep);
        }
        return Ok(RetirementCandidateCounters {
            logical_items: 0,
            primary_entries: 0,
            secondary_entries: 0,
            expiry_entries: 0,
            logical_bytes: 0,
        });
    }
    let (primary_entries, secondary_entries) = match (family, decoded.prefix) {
        (StructureCollectionFamily::List, STRUCTURE_LIST_CHUNK_PREFIX) => {
            if candidate.logical_items == 0
                || candidate.associated_secondary_entries != 0
                || candidate.associated_expiry_entries != 0
            {
                return Err(StructureV3CodecError::InvalidRetirementStep);
            }
            (1, 0)
        }
        (StructureCollectionFamily::Hash, STRUCTURE_HASH_FIELD_PREFIX) => {
            if candidate.logical_items != 1
                || candidate.associated_secondary_entries != 0
                || candidate.associated_expiry_entries > 1
            {
                return Err(StructureV3CodecError::InvalidRetirementStep);
            }
            (1, 0)
        }
        (StructureCollectionFamily::SortedSet, STRUCTURE_SORTED_SET_MEMBER_PREFIX) => {
            if candidate.logical_items != 1
                || candidate.logical_bytes != 0
                || candidate.associated_secondary_entries != 1
                || candidate.associated_expiry_entries != 0
            {
                return Err(StructureV3CodecError::InvalidRetirementStep);
            }
            (1, 1)
        }
        _ => {
            if candidate.logical_items != 1
                || candidate.logical_bytes != 0
                || candidate.associated_secondary_entries != 0
                || candidate.associated_expiry_entries != 0
            {
                return Err(StructureV3CodecError::InvalidRetirementStep);
            }
            (1, 0)
        }
    };
    Ok(RetirementCandidateCounters {
        logical_items: candidate.logical_items,
        primary_entries,
        secondary_entries,
        expiry_entries: candidate.associated_expiry_entries,
        logical_bytes: candidate.logical_bytes,
    })
}

const fn retirement_has_remaining_work(record: &RetirementRecordV3) -> bool {
    record.remaining_logical_items != 0
        || record.remaining_primary_entries != 0
        || record.remaining_secondary_entries != 0
        || record.remaining_expiry_entries != 0
        || record.remaining_logical_bytes != 0
}

fn validate_list_retirement_candidate(
    record: &RetirementRecordV3,
    candidate: &RetirementCandidateV3<'_>,
) -> Result<(), StructureV3CodecError> {
    if record.family != StructureCollectionFamily::List || !candidate.live {
        return Ok(());
    }
    let decoded = decode_collection_child_key(candidate.physical_key)
        .map_err(|_| StructureV3CodecError::InvalidRetirementStep)?;
    let chunk_id = decode_list_chunk_identity_v3(decoded.child_identity)
        .map_err(|_| StructureV3CodecError::InvalidRetirementStep)?;
    let total_chunks = list_chunk_range_count(record.list_head_chunk, record.list_tail_chunk)
        .map_err(|_| StructureV3CodecError::InvalidRetirementStep)?;
    let completed_chunks = total_chunks
        .checked_sub(record.remaining_primary_entries)
        .ok_or(StructureV3CodecError::InvalidRetirementStep)?;
    let completed_chunks = i64::try_from(completed_chunks)
        .map_err(|_| StructureV3CodecError::InvalidRetirementStep)?;
    let expected = record
        .list_head_chunk
        .checked_add(completed_chunks)
        .ok_or(StructureV3CodecError::InvalidRetirementStep)?;
    if chunk_id != expected {
        return Err(StructureV3CodecError::InvalidRetirementStep);
    }
    Ok(())
}

fn validate_stream_retirement_candidate(
    record: &RetirementRecordV3,
    candidate: &RetirementCandidateV3<'_>,
) -> Result<(), StructureV3CodecError> {
    if record.family != StructureCollectionFamily::Stream || !candidate.live {
        return Ok(());
    }
    let decoded = decode_collection_child_key(candidate.physical_key)
        .map_err(|_| StructureV3CodecError::InvalidRetirementStep)?;
    let id = decode_u64(decoded.child_identity)
        .map_err(|_| StructureV3CodecError::InvalidRetirementStep)?;
    if id == 0
        || id > record.stream_last_id
        || (record.remaining_primary_entries == 1 && id != record.stream_last_id)
    {
        return Err(StructureV3CodecError::InvalidRetirementStep);
    }
    Ok(())
}

pub(crate) fn validate_retirement_state(
    retirement_key: &[u8],
    record: &RetirementRecordV3,
) -> Result<(), StructureV3CodecError> {
    validate_retirement_record_shape(record)?;
    let (collection_key, incarnation) = decode_retirement_key(retirement_key)?;
    let Some(cursor) = record.exclusive_cursor.as_deref() else {
        return Ok(());
    };
    let decoded = decode_collection_child_key(cursor)?;
    if decoded.collection_key != collection_key
        || decoded.incarnation != incarnation
        || decoded.prefix != retirement_primary_prefix(record.family)
    {
        return Err(StructureV3CodecError::Malformed);
    }
    Ok(())
}

const fn retirement_primary_prefix(family: StructureCollectionFamily) -> u8 {
    match family {
        StructureCollectionFamily::Hash => STRUCTURE_HASH_FIELD_PREFIX,
        StructureCollectionFamily::Set => STRUCTURE_SET_MEMBER_PREFIX,
        StructureCollectionFamily::List => STRUCTURE_LIST_CHUNK_PREFIX,
        StructureCollectionFamily::SortedSet => STRUCTURE_SORTED_SET_MEMBER_PREFIX,
        StructureCollectionFamily::Stream => STRUCTURE_STREAM_ENTRY_PREFIX,
    }
}

fn validate_retirement_record_shape(
    record: &RetirementRecordV3,
) -> Result<(), StructureV3CodecError> {
    if record.remaining_logical_items > record.declared_logical_items {
        return Err(StructureV3CodecError::Malformed);
    }
    let max_expiry = match record.family {
        StructureCollectionFamily::Hash => record.declared_logical_items,
        _ => 0,
    };
    if record.remaining_expiry_entries > max_expiry {
        return Err(StructureV3CodecError::Malformed);
    }
    match record.family {
        StructureCollectionFamily::Hash | StructureCollectionFamily::Set => {
            if record.remaining_primary_entries != record.remaining_logical_items
                || record.remaining_secondary_entries != 0
                || record.remaining_logical_bytes != 0
            {
                return Err(StructureV3CodecError::Malformed);
            }
        }
        StructureCollectionFamily::Stream => {
            if record.remaining_primary_entries != record.remaining_logical_items
                || record.remaining_secondary_entries != 0
                || record.remaining_logical_bytes != 0
                || (record.declared_logical_items == 0) != (record.stream_last_id == 0)
                || record.declared_logical_items > record.stream_last_id
            {
                return Err(StructureV3CodecError::Malformed);
            }
        }
        StructureCollectionFamily::List => {
            let total_chunks = if record.declared_logical_items == 0 {
                if record.list_head_chunk != 0 || record.list_tail_chunk != 0 {
                    return Err(StructureV3CodecError::Malformed);
                }
                0
            } else {
                list_chunk_range_count(record.list_head_chunk, record.list_tail_chunk)?
            };
            if total_chunks > record.declared_logical_items
                || record.remaining_primary_entries > total_chunks
                || record.remaining_primary_entries > record.remaining_logical_items
                || record.remaining_secondary_entries != 0
                || (record.remaining_logical_items == 0
                    && (record.remaining_primary_entries != 0
                        || record.remaining_logical_bytes != 0))
            {
                return Err(StructureV3CodecError::Malformed);
            }
        }
        StructureCollectionFamily::SortedSet => {
            if record.remaining_primary_entries != record.remaining_logical_items
                || record.remaining_secondary_entries != record.remaining_primary_entries
                || record.remaining_logical_bytes != 0
            {
                return Err(StructureV3CodecError::Malformed);
            }
        }
    }
    if record.family != StructureCollectionFamily::List
        && (record.list_head_chunk != 0 || record.list_tail_chunk != 0)
    {
        return Err(StructureV3CodecError::Malformed);
    }
    if record.family != StructureCollectionFamily::Stream && record.stream_last_id != 0 {
        return Err(StructureV3CodecError::Malformed);
    }
    Ok(())
}

fn list_chunk_range_count(head_chunk: i64, tail_chunk: i64) -> Result<u64, StructureV3CodecError> {
    tail_chunk
        .checked_sub(head_chunk)
        .and_then(|distance| distance.checked_add(1))
        .and_then(|count| u64::try_from(count).ok())
        .ok_or(StructureV3CodecError::Malformed)
}

fn encode_list_chunk_identity_v3(chunk_id: i64) -> [u8; 8] {
    (chunk_id.cast_unsigned() ^ (1_u64 << 63)).to_be_bytes()
}

fn decode_list_chunk_identity_v3(encoded: &[u8]) -> Result<i64, StructureV3CodecError> {
    let sortable = u64::from_be_bytes(
        encoded
            .try_into()
            .map_err(|_| StructureV3CodecError::Malformed)?,
    );
    Ok((sortable ^ (1_u64 << 63)).cast_signed())
}

fn validate_child_prefix(prefix: u8) -> Result<(), StructureV3CodecError> {
    if matches!(
        prefix,
        STRUCTURE_HASH_FIELD_PREFIX
            | STRUCTURE_SET_MEMBER_PREFIX
            | STRUCTURE_LIST_CHUNK_PREFIX
            | STRUCTURE_SORTED_SET_MEMBER_PREFIX
            | STRUCTURE_SORTED_SET_ORDER_PREFIX
            | STRUCTURE_STREAM_ENTRY_PREFIX
    ) {
        Ok(())
    } else {
        Err(StructureV3CodecError::Malformed)
    }
}

const fn child_prefix_belongs_to_family(prefix: u8, family: StructureCollectionFamily) -> bool {
    matches!(
        (family, prefix),
        (StructureCollectionFamily::Hash, STRUCTURE_HASH_FIELD_PREFIX)
            | (StructureCollectionFamily::Set, STRUCTURE_SET_MEMBER_PREFIX)
            | (StructureCollectionFamily::List, STRUCTURE_LIST_CHUNK_PREFIX)
            | (
                StructureCollectionFamily::SortedSet,
                STRUCTURE_SORTED_SET_MEMBER_PREFIX | STRUCTURE_SORTED_SET_ORDER_PREFIX
            )
            | (
                StructureCollectionFamily::Stream,
                STRUCTURE_STREAM_ENTRY_PREFIX
            )
    )
}

fn decode_u32(encoded: &[u8]) -> Result<u32, StructureV3CodecError> {
    Ok(u32::from_be_bytes(
        encoded
            .try_into()
            .map_err(|_| StructureV3CodecError::Malformed)?,
    ))
}

fn decode_u64(encoded: &[u8]) -> Result<u64, StructureV3CodecError> {
    Ok(u64::from_be_bytes(
        encoded
            .try_into()
            .map_err(|_| StructureV3CodecError::Malformed)?,
    ))
}

fn decode_i64(encoded: &[u8]) -> Result<i64, StructureV3CodecError> {
    Ok(i64::from_be_bytes(
        encoded
            .try_into()
            .map_err(|_| StructureV3CodecError::Malformed)?,
    ))
}

fn map_codec_error(error: StructureV3CodecError) -> NativeRuntimeError {
    match error {
        StructureV3CodecError::IdentityTooLarge => NativeRuntimeError::StructureIdentityTooLarge,
        StructureV3CodecError::MutationOrdinalOverflow => {
            NativeRuntimeError::InvalidPreparedMutation
        }
        StructureV3CodecError::Malformed | StructureV3CodecError::InvalidRetirementStep => {
            NativeRuntimeError::InvalidStructureTree
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StructureV3ValidationSummary {
    physical_entries: usize,
    live_collections: usize,
    active_retirements: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct StructureV3CompactionSummary {
    pub(super) scanned_entries: usize,
    pub(super) retained_entries: usize,
    pub(super) dropped_tombstones: usize,
    pub(super) active_retirements: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LiveCollectionV3 {
    family: StructureCollectionFamily,
    incarnation: StructureIncarnation,
    state: CollectionStateV3,
}

#[derive(Clone, Debug)]
struct ActiveRetirementV3 {
    collection_key: Vec<u8>,
    incarnation: StructureIncarnation,
    record: RetirementRecordV3,
}

#[derive(Default)]
struct StructureV3Inventory {
    live_collections: BTreeMap<Vec<u8>, LiveCollectionV3>,
    active_retirements: Vec<ActiveRetirementV3>,
    live_scalars: BTreeMap<Vec<u8>, Option<i64>>,
    child_entries: Vec<(Vec<u8>, Vec<u8>)>,
    expiry_entries: Vec<(Vec<u8>, Vec<u8>)>,
}

fn validate_structure_v3_tree(
    pages: &PageStore,
    blobs: &BlobStore,
    tree: BTree,
) -> Result<StructureV3ValidationSummary, NativeRuntimeError> {
    let entries = tree.scan(pages)?;
    match entries.first() {
        Some((key, value))
            if key.as_slice() == crate::STRUCTURE_FORMAT_KEY
                && value.as_slice() == STRUCTURE_FORMAT_VALUE_V3 => {}
        _ => return Err(NativeRuntimeError::InvalidStructureTree),
    }
    let physical = entries.into_iter().collect::<BTreeMap<_, _>>();
    let inventory = inventory_structure_v3_entries(&physical, blobs)?;
    if inventory
        .live_collections
        .keys()
        .any(|key| inventory.live_scalars.contains_key(key))
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    validate_live_collections_v3(pages, blobs, tree, &inventory.live_collections)?;
    validate_child_ownership_v3(
        &physical,
        &inventory.child_entries,
        &inventory.live_collections,
        &inventory.active_retirements,
    )?;
    validate_expiry_entries_v3(
        &physical,
        &inventory.expiry_entries,
        &inventory.live_scalars,
        &inventory.live_collections,
        &inventory.active_retirements,
    )?;
    for retirement in &inventory.active_retirements {
        validate_active_retirement_v3(retirement, &physical, &inventory.child_entries)?;
    }
    Ok(StructureV3ValidationSummary {
        physical_entries: physical.len(),
        live_collections: inventory.live_collections.len(),
        active_retirements: inventory.active_retirements.len(),
    })
}

pub(super) fn load_structure_state_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    tree: BTree,
) -> Result<crate::model::StructureState, NativeRuntimeError> {
    #[cfg(test)]
    crate::reject_full_structure_state_load_for_test()?;
    validate_structure_v3_tree(pages, blobs, tree)?;
    let physical = tree.scan(pages)?.into_iter().collect::<BTreeMap<_, _>>();
    let inventory = inventory_structure_v3_entries(&physical, blobs)?;
    let mut state = crate::model::StructureState::default();
    for (physical_key, encoded) in &physical {
        if physical_key.first() != Some(&crate::STRUCTURE_ENTRY_PREFIX) {
            continue;
        }
        if let Some(entry) = decode_structure_value(encoded, blobs)? {
            state.entries.insert(physical_key[1..].to_vec(), entry);
        }
    }
    for (key, collection) in inventory.live_collections {
        materialize_collection_v3(pages, blobs, tree, key, collection, &mut state)?;
    }
    Ok(state)
}

pub(super) fn apply_structure_mutations_v3(
    pages: &mut PageStore,
    mut tree: BTree,
    creating_csn: Csn,
    transaction_id: TransactionId,
    mutations: &[Mutation],
    blob_references: &BTreeMap<[u8; 32], BlobReference>,
) -> Result<BTree, NativeRuntimeError> {
    for (mutation_index, mutation) in mutations.iter().enumerate() {
        if mutation.engine != EngineKind::Structure {
            continue;
        }
        tree = match mutation.opcode {
            Opcode::SetValue | Opcode::ExpireValue | Opcode::DeleteValue => {
                apply_scalar_mutation_v3(pages, tree, creating_csn, mutation, blob_references)?
            }
            Opcode::CreateHash
            | Opcode::SetHashField
            | Opcode::DeleteHashField
            | Opcode::ExpireHash
            | Opcode::ExpireHashField
            | Opcode::DeleteHash => apply_hash_mutation_v3(
                pages,
                tree,
                creating_csn,
                transaction_id,
                mutation_index,
                mutation,
                blob_references,
            )?,
            Opcode::CreateSet
            | Opcode::AddSetMember
            | Opcode::DeleteSetMember
            | Opcode::ExpireSet
            | Opcode::DeleteSet => apply_set_mutation_v3(
                pages,
                tree,
                creating_csn,
                transaction_id,
                mutation_index,
                mutation,
            )?,
            Opcode::CreateList
            | Opcode::PushListHead
            | Opcode::PushListTail
            | Opcode::PopListHead
            | Opcode::PopListTail
            | Opcode::ExpireList
            | Opcode::DeleteList => apply_list_mutation_v3(
                pages,
                tree,
                creating_csn,
                transaction_id,
                mutation_index,
                mutation,
                blob_references,
            )?,
            Opcode::CreateStream
            | Opcode::AppendStreamEntry
            | Opcode::ExpireStream
            | Opcode::DeleteStream => apply_stream_mutation_v3(
                pages,
                tree,
                creating_csn,
                transaction_id,
                mutation_index,
                mutation,
            )?,
            Opcode::CreateSortedSet
            | Opcode::UpsertSortedSetMember
            | Opcode::DeleteSortedSetMember
            | Opcode::ExpireSortedSet
            | Opcode::DeleteSortedSet => apply_sorted_set_mutation_v3(
                pages,
                tree,
                creating_csn,
                transaction_id,
                mutation_index,
                mutation,
            )?,
            _ => return Err(NativeRuntimeError::StructureV3MutationUnsupported),
        };
    }
    Ok(tree)
}

fn apply_scalar_mutation_v3(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    mutation: &Mutation,
    blob_references: &BTreeMap<[u8; 32], BlobReference>,
) -> Result<BTree, NativeRuntimeError> {
    ensure_v3_scalar_key_has_no_live_collection(pages, tree, &mutation.key)?;
    let entry_key = structure_key(&mutation.key);
    let previous = tree.get(pages, &entry_key)?;
    let previous_live = previous
        .as_deref()
        .filter(|encoded| !is_structure_tombstone(encoded));
    let previous_expiry = previous_live
        .map(structure_value_expiry)
        .transpose()?
        .flatten();
    let (value, new_expiry) = match mutation.opcode {
        Opcode::SetValue => (
            structure_storage_value(&mutation.value, mutation.expires_at_micros, blob_references)?,
            mutation.expires_at_micros,
        ),
        Opcode::ExpireValue => {
            let expiry = mutation
                .expires_at_micros
                .ok_or(NativeRuntimeError::InvalidPreparedMutation)?;
            let previous = previous_live.ok_or(NativeRuntimeError::InvalidStructureTree)?;
            let expected =
                structure_storage_value(&mutation.value, previous_expiry, blob_references)?;
            if previous != expected {
                return Err(NativeRuntimeError::InvalidStructureTree);
            }
            (
                structure_storage_value(&mutation.value, Some(expiry), blob_references)?,
                Some(expiry),
            )
        }
        Opcode::DeleteValue
            if mutation.value.is_empty() && mutation.expires_at_micros.is_none() =>
        {
            previous_live.ok_or(NativeRuntimeError::InvalidStructureTree)?;
            (structure_tombstone_value(), None)
        }
        _ => return Err(NativeRuntimeError::InvalidPreparedMutation),
    };
    let mut entries = vec![(entry_key, value)];
    if let Some(expiry) = previous_expiry {
        let expiry_key = structure_expiry_key(expiry, &mutation.key)?;
        if tree.get(pages, &expiry_key)?.as_deref() != Some(&[STRUCTURE_EXPIRY_LIVE]) {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        if previous_expiry != new_expiry {
            entries.push((expiry_key, vec![STRUCTURE_EXPIRY_TOMBSTONE]));
        }
    }
    if previous_expiry != new_expiry
        && let Some(expiry) = new_expiry
    {
        let expiry_key = structure_expiry_key(expiry, &mutation.key)?;
        if tree
            .get(pages, &expiry_key)?
            .is_some_and(|marker| marker != [STRUCTURE_EXPIRY_TOMBSTONE])
        {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        entries.push((expiry_key, vec![STRUCTURE_EXPIRY_LIVE]));
    }
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree)
}

fn ensure_v3_scalar_key_has_no_live_collection(
    pages: &PageStore,
    tree: BTree,
    key: &[u8],
) -> Result<(), NativeRuntimeError> {
    if tree
        .get(pages, &structure_hash_meta_key(key))?
        .as_deref()
        .map(decode_live_hash_metadata_v3)
        .transpose()?
        .flatten()
        .is_some()
        || tree
            .get(pages, &structure_set_meta_key(key))?
            .as_deref()
            .map(decode_live_set_metadata_v3)
            .transpose()?
            .flatten()
            .is_some()
        || tree
            .get(pages, &structure_list_meta_key(key)?)?
            .as_deref()
            .map(decode_live_list_metadata_v3)
            .transpose()?
            .flatten()
            .is_some()
        || tree
            .get(pages, &structure_stream_meta_key(key)?)?
            .as_deref()
            .map(decode_live_stream_metadata_v3)
            .transpose()?
            .flatten()
            .is_some()
        || tree
            .get(pages, &structure_sorted_set_meta_key(key)?)?
            .as_deref()
            .map(decode_live_sorted_set_metadata_v3)
            .transpose()?
            .flatten()
            .is_some()
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(())
}

fn mutation_incarnation_v3(
    transaction_id: TransactionId,
    mutation_index: usize,
) -> Result<StructureIncarnation, NativeRuntimeError> {
    StructureIncarnation::from_mutation_index(transaction_id, mutation_index)
        .map_err(map_codec_error)
}

fn apply_hash_mutation_v3(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    transaction_id: TransactionId,
    mutation_index: usize,
    mutation: &Mutation,
    blob_references: &BTreeMap<[u8; 32], BlobReference>,
) -> Result<BTree, NativeRuntimeError> {
    match mutation.opcode {
        Opcode::CreateHash => {
            ensure_v3_collection_key_available(pages, tree, &mutation.key)?;
            create_hash_v3_in_tree(
                pages,
                tree,
                creating_csn,
                &mutation.key,
                mutation_incarnation_v3(transaction_id, mutation_index)?,
            )
        }
        Opcode::SetHashField => {
            let (key, field) = crate::decode_hash_field_identity(&mutation.key)?;
            Ok(put_hash_field_v3_in_tree(
                pages,
                tree,
                creating_csn,
                HashFieldWriteV3 {
                    key,
                    field,
                    value: &mutation.value,
                    expires_at_micros: None,
                },
                blob_references,
            )?
            .0)
        }
        Opcode::DeleteHashField => {
            let (key, field) = crate::decode_hash_field_identity(&mutation.key)?;
            delete_hash_field_v3_in_tree(pages, tree, creating_csn, key, field)
        }
        Opcode::ExpireHash => expire_collection_v3_in_tree(
            pages,
            tree,
            creating_csn,
            &mutation.key,
            StructureCollectionFamily::Hash,
            mutation
                .expires_at_micros
                .ok_or(NativeRuntimeError::InvalidPreparedMutation)?,
        ),
        Opcode::ExpireHashField => {
            let (key, field) = crate::decode_hash_field_identity(&mutation.key)?;
            expire_hash_field_v3_in_tree(
                pages,
                tree,
                creating_csn,
                key,
                field,
                mutation
                    .expires_at_micros
                    .ok_or(NativeRuntimeError::InvalidPreparedMutation)?,
            )
        }
        Opcode::DeleteHash => {
            Ok(delete_hash_v3_in_tree(pages, tree, creating_csn, &mutation.key)?.0)
        }
        _ => Err(NativeRuntimeError::StructureV3MutationUnsupported),
    }
}

fn apply_set_mutation_v3(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    transaction_id: TransactionId,
    mutation_index: usize,
    mutation: &Mutation,
) -> Result<BTree, NativeRuntimeError> {
    match mutation.opcode {
        Opcode::CreateSet => {
            ensure_v3_collection_key_available(pages, tree, &mutation.key)?;
            create_set_v3_in_tree(
                pages,
                tree,
                creating_csn,
                &mutation.key,
                mutation_incarnation_v3(transaction_id, mutation_index)?,
            )
        }
        Opcode::AddSetMember => {
            let (key, member) = crate::decode_set_member_identity(&mutation.key)?;
            Ok(add_set_member_v3_in_tree(pages, tree, creating_csn, key, member)?.0)
        }
        Opcode::DeleteSetMember => {
            let (key, member) = crate::decode_set_member_identity(&mutation.key)?;
            delete_set_member_v3_in_tree(pages, tree, creating_csn, key, member)
        }
        Opcode::ExpireSet => expire_collection_v3_in_tree(
            pages,
            tree,
            creating_csn,
            &mutation.key,
            StructureCollectionFamily::Set,
            mutation
                .expires_at_micros
                .ok_or(NativeRuntimeError::InvalidPreparedMutation)?,
        ),
        Opcode::DeleteSet => Ok(delete_set_v3_in_tree(pages, tree, creating_csn, &mutation.key)?.0),
        _ => Err(NativeRuntimeError::StructureV3MutationUnsupported),
    }
}

fn apply_list_mutation_v3(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    transaction_id: TransactionId,
    mutation_index: usize,
    mutation: &Mutation,
    blob_references: &BTreeMap<[u8; 32], BlobReference>,
) -> Result<BTree, NativeRuntimeError> {
    match mutation.opcode {
        Opcode::CreateList => {
            ensure_v3_collection_key_available(pages, tree, &mutation.key)?;
            create_list_v3_in_tree(
                pages,
                tree,
                creating_csn,
                &mutation.key,
                mutation_incarnation_v3(transaction_id, mutation_index)?,
            )
        }
        Opcode::PushListHead | Opcode::PushListTail => push_list_value_v3_in_tree(
            pages,
            tree,
            creating_csn,
            &mutation.key,
            &mutation.value,
            mutation.opcode == Opcode::PushListHead,
            blob_references,
        ),
        Opcode::PopListHead | Opcode::PopListTail => pop_list_value_v3_in_tree(
            pages,
            tree,
            creating_csn,
            &mutation.key,
            &mutation.value,
            mutation.opcode == Opcode::PopListHead,
            blob_references,
        ),
        Opcode::ExpireList => expire_collection_v3_in_tree(
            pages,
            tree,
            creating_csn,
            &mutation.key,
            StructureCollectionFamily::List,
            mutation
                .expires_at_micros
                .ok_or(NativeRuntimeError::InvalidPreparedMutation)?,
        ),
        Opcode::DeleteList => {
            Ok(delete_list_v3_in_tree(pages, tree, creating_csn, &mutation.key)?.0)
        }
        _ => Err(NativeRuntimeError::StructureV3MutationUnsupported),
    }
}

fn apply_stream_mutation_v3(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    transaction_id: TransactionId,
    mutation_index: usize,
    mutation: &Mutation,
) -> Result<BTree, NativeRuntimeError> {
    match mutation.opcode {
        Opcode::CreateStream => {
            ensure_v3_collection_key_available(pages, tree, &mutation.key)?;
            create_stream_v3_in_tree(
                pages,
                tree,
                creating_csn,
                &mutation.key,
                mutation_incarnation_v3(transaction_id, mutation_index)?,
            )
        }
        Opcode::AppendStreamEntry => {
            let (id, fields) = decode_stream_wal_entry(&mutation.value)?;
            append_stream_entry_v3_in_tree(pages, tree, creating_csn, &mutation.key, id, &fields)
        }
        Opcode::ExpireStream => expire_collection_v3_in_tree(
            pages,
            tree,
            creating_csn,
            &mutation.key,
            StructureCollectionFamily::Stream,
            mutation
                .expires_at_micros
                .ok_or(NativeRuntimeError::InvalidPreparedMutation)?,
        ),
        Opcode::DeleteStream => {
            Ok(delete_stream_v3_in_tree(pages, tree, creating_csn, &mutation.key)?.0)
        }
        _ => Err(NativeRuntimeError::StructureV3MutationUnsupported),
    }
}

fn apply_sorted_set_mutation_v3(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    transaction_id: TransactionId,
    mutation_index: usize,
    mutation: &Mutation,
) -> Result<BTree, NativeRuntimeError> {
    match mutation.opcode {
        Opcode::CreateSortedSet => {
            ensure_v3_collection_key_available(pages, tree, &mutation.key)?;
            create_sorted_set_v3_in_tree(
                pages,
                tree,
                creating_csn,
                &mutation.key,
                mutation_incarnation_v3(transaction_id, mutation_index)?,
            )
        }
        Opcode::UpsertSortedSetMember => {
            let (key, member) = crate::decode_sorted_set_member_identity(&mutation.key)?;
            let score = crate::decode_sorted_set_wal_score(&mutation.value)?;
            Ok(
                upsert_sorted_set_member_v3_in_tree(pages, tree, creating_csn, key, member, score)?
                    .0,
            )
        }
        Opcode::DeleteSortedSetMember => {
            let (key, member) = crate::decode_sorted_set_member_identity(&mutation.key)?;
            delete_sorted_set_member_v3_in_tree(pages, tree, creating_csn, key, member)
        }
        Opcode::ExpireSortedSet => expire_collection_v3_in_tree(
            pages,
            tree,
            creating_csn,
            &mutation.key,
            StructureCollectionFamily::SortedSet,
            mutation
                .expires_at_micros
                .ok_or(NativeRuntimeError::InvalidPreparedMutation)?,
        ),
        Opcode::DeleteSortedSet => {
            Ok(delete_sorted_set_v3_in_tree(pages, tree, creating_csn, &mutation.key)?.0)
        }
        _ => Err(NativeRuntimeError::StructureV3MutationUnsupported),
    }
}

fn ensure_v3_collection_key_available(
    pages: &PageStore,
    tree: BTree,
    key: &[u8],
) -> Result<(), NativeRuntimeError> {
    if let Some(encoded) = tree.get(pages, &structure_key(key))?
        && !is_structure_tombstone(&encoded)
    {
        structure_value_expiry(&encoded)?;
        return Err(NativeRuntimeError::StructureKeyExists);
    }
    if tree
        .get(pages, &structure_hash_meta_key(key))?
        .as_deref()
        .map(decode_live_hash_metadata_v3)
        .transpose()?
        .flatten()
        .is_some()
        || tree
            .get(pages, &structure_set_meta_key(key))?
            .as_deref()
            .map(decode_live_set_metadata_v3)
            .transpose()?
            .flatten()
            .is_some()
        || tree
            .get(pages, &structure_list_meta_key(key)?)?
            .as_deref()
            .map(decode_live_list_metadata_v3)
            .transpose()?
            .flatten()
            .is_some()
        || tree
            .get(pages, &structure_stream_meta_key(key)?)?
            .as_deref()
            .map(decode_live_stream_metadata_v3)
            .transpose()?
            .flatten()
            .is_some()
        || tree
            .get(pages, &structure_sorted_set_meta_key(key)?)?
            .as_deref()
            .map(decode_live_sorted_set_metadata_v3)
            .transpose()?
            .flatten()
            .is_some()
    {
        return Err(NativeRuntimeError::StructureKeyExists);
    }
    Ok(())
}

fn collection_metadata_key_v3(
    family: StructureCollectionFamily,
    key: &[u8],
) -> Result<Vec<u8>, NativeRuntimeError> {
    match family {
        StructureCollectionFamily::Hash => Ok(structure_hash_meta_key(key)),
        StructureCollectionFamily::Set => Ok(structure_set_meta_key(key)),
        StructureCollectionFamily::List => structure_list_meta_key(key),
        StructureCollectionFamily::SortedSet => structure_sorted_set_meta_key(key),
        StructureCollectionFamily::Stream => structure_stream_meta_key(key),
    }
}

fn expire_collection_v3_in_tree(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    key: &[u8],
    family: StructureCollectionFamily,
    expires_at_micros: i64,
) -> Result<BTree, NativeRuntimeError> {
    let metadata_key = collection_metadata_key_v3(family, key)?;
    let encoded = tree
        .get(pages, &metadata_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let TypedCollectionMetadataV3::Live { incarnation, state } =
        decode_typed_collection_metadata(&encoded).map_err(map_codec_error)?
    else {
        return Err(NativeRuntimeError::InvalidStructureTree);
    };
    if state.family() != family {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let new_expiry_key = encode_collection_expiry_key(
        crate::STRUCTURE_EXPIRY_PREFIX,
        expires_at_micros,
        key,
        incarnation,
        &[],
    )
    .map_err(map_codec_error)?;
    let mut entries = Vec::with_capacity(3);
    if let Some(previous_expiry) = state.expires_at_micros()
        && previous_expiry != expires_at_micros
    {
        let previous_expiry_key = encode_collection_expiry_key(
            crate::STRUCTURE_EXPIRY_PREFIX,
            previous_expiry,
            key,
            incarnation,
            &[],
        )
        .map_err(map_codec_error)?;
        if tree.get(pages, &previous_expiry_key)?.as_deref()
            != Some(&[collection_expiry_marker_v3(family)])
        {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        entries.push((previous_expiry_key, vec![STRUCTURE_EXPIRY_TOMBSTONE]));
    }
    entries.push((
        metadata_key,
        encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
            incarnation,
            state: state.with_expiry(expires_at_micros),
        })
        .map_err(map_codec_error)?,
    ));
    entries.push((new_expiry_key, vec![collection_expiry_marker_v3(family)]));
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree)
}

fn expire_hash_field_v3_in_tree(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    key: &[u8],
    field: &[u8],
    expires_at_micros: i64,
) -> Result<BTree, NativeRuntimeError> {
    let metadata_key = structure_hash_meta_key(key);
    let metadata = tree
        .get(pages, &metadata_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let state =
        decode_live_hash_metadata_v3(&metadata)?.ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let field_key =
        encode_collection_child_key(STRUCTURE_HASH_FIELD_PREFIX, key, state.incarnation, field)
            .map_err(map_codec_error)?;
    let encoded = tree
        .get(pages, &field_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    if is_structure_tombstone(&encoded) {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let previous_expiry = structure_value_expiry(&encoded)?;
    let new_expiry_key = encode_collection_expiry_key(
        crate::STRUCTURE_HASH_FIELD_EXPIRY_PREFIX,
        expires_at_micros,
        key,
        state.incarnation,
        field,
    )
    .map_err(map_codec_error)?;
    let field_expiry_count = if previous_expiry.is_none() {
        state
            .field_expiry_count
            .checked_add(1)
            .ok_or(NativeRuntimeError::InvalidStructureTree)?
    } else {
        state.field_expiry_count
    };
    let mut entries = Vec::with_capacity(4);
    if let Some(previous_expiry) = previous_expiry
        && previous_expiry != expires_at_micros
    {
        let previous_expiry_key = encode_collection_expiry_key(
            crate::STRUCTURE_HASH_FIELD_EXPIRY_PREFIX,
            previous_expiry,
            key,
            state.incarnation,
            field,
        )
        .map_err(map_codec_error)?;
        if tree.get(pages, &previous_expiry_key)?.as_deref()
            != Some(&[STRUCTURE_HASH_FIELD_EXPIRY_LIVE])
        {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        entries.push((previous_expiry_key, vec![STRUCTURE_EXPIRY_TOMBSTONE]));
    }
    entries.push((
        metadata_key,
        encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
            incarnation: state.incarnation,
            state: CollectionStateV3::Hash {
                field_count: state.field_count,
                field_expiry_count,
                expires_at_micros: state.expires_at_micros,
            },
        })
        .map_err(map_codec_error)?,
    ));
    entries.push((
        field_key,
        crate::replace_structure_value_expiry(&encoded, Some(expires_at_micros))?,
    ));
    entries.push((new_expiry_key, vec![STRUCTURE_HASH_FIELD_EXPIRY_LIVE]));
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree)
}

pub(super) fn next_active_retirement_key_v3(
    pages: &PageStore,
    pool: &BufferPool,
    tree: BTree,
) -> Result<Option<Vec<u8>>, NativeRuntimeError> {
    let mut candidate = None;
    let _ = tree.visit_prefix_cached(
        pages,
        pool,
        &[STRUCTURE_RETIREMENT_PREFIX],
        None,
        |key, value| {
            if is_structure_tombstone(value) {
                ControlFlow::Continue(())
            } else {
                candidate = Some((key.to_vec(), value.to_vec()));
                ControlFlow::Break(())
            }
        },
    )?;
    let Some((key, encoded_record)) = candidate else {
        return Ok(None);
    };
    let record = decode_retirement_record(&encoded_record).map_err(map_codec_error)?;
    validate_retirement_state(&key, &record).map_err(map_codec_error)?;
    Ok(Some(key))
}

pub(super) fn validate_due_collection_expiry_v3(
    pages: &PageStore,
    pool: &BufferPool,
    tree: BTree,
    physical_key: &[u8],
    marker: &[u8],
) -> Result<(i64, Vec<u8>, StructureCollectionFamily), NativeRuntimeError> {
    let family = expiry_marker_family_v3(marker)?;
    let decoded = decode_collection_expiry_key(physical_key).map_err(map_codec_error)?;
    if !decoded.child_identity.is_empty() {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let metadata_key = collection_metadata_key_v3(family, decoded.collection_key)?;
    let metadata = tree
        .get_cached_pinned(pages, pool, &metadata_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let TypedCollectionMetadataV3::Live { incarnation, state } =
        decode_typed_collection_metadata(metadata.bytes()).map_err(map_codec_error)?
    else {
        return Err(NativeRuntimeError::InvalidStructureTree);
    };
    if state.family() != family
        || incarnation != decoded.incarnation
        || state.expires_at_micros() != Some(decoded.expires_at_micros)
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok((
        decoded.expires_at_micros,
        decoded.collection_key.to_vec(),
        family,
    ))
}

pub(super) fn validate_due_hash_field_expiry_v3(
    pages: &PageStore,
    pool: &BufferPool,
    tree: BTree,
    physical_key: &[u8],
    marker: &[u8],
    logical_time_micros: i64,
) -> Result<Option<(i64, Vec<u8>)>, NativeRuntimeError> {
    if marker != [STRUCTURE_HASH_FIELD_EXPIRY_LIVE] {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let decoded = decode_collection_expiry_key(physical_key).map_err(map_codec_error)?;
    let metadata = tree
        .get_cached_pinned(
            pages,
            pool,
            &structure_hash_meta_key(decoded.collection_key),
        )?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let state = decode_live_hash_metadata_v3(metadata.bytes())?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    if state.incarnation != decoded.incarnation {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    if state
        .expires_at_micros
        .is_some_and(|expiry| expiry <= logical_time_micros)
    {
        return Ok(None);
    }
    let field_key = encode_collection_child_key(
        STRUCTURE_HASH_FIELD_PREFIX,
        decoded.collection_key,
        decoded.incarnation,
        decoded.child_identity,
    )
    .map_err(map_codec_error)?;
    let field = tree
        .get_cached_pinned(pages, pool, &field_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    if is_structure_tombstone(field.bytes())
        || structure_value_expiry(field.bytes())? != Some(decoded.expires_at_micros)
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(Some((
        decoded.expires_at_micros,
        crate::hash_field_identity(decoded.collection_key, decoded.child_identity)?,
    )))
}

pub(super) fn expire_due_structure_mutations_v3(
    pages: &mut PageStore,
    mut tree: BTree,
    creating_csn: Csn,
    logical_time_micros: i64,
    mutations: &[Mutation],
) -> Result<BTree, NativeRuntimeError> {
    for mutation in mutations {
        if mutation.engine != EngineKind::Structure
            || mutation.target.is_some()
            || !mutation.value.is_empty()
            || mutation.expires_at_micros.is_some()
        {
            return Err(NativeRuntimeError::InvalidPreparedMutation);
        }
        tree = match mutation.opcode {
            Opcode::DeleteValue => expire_due_scalar_v3(
                pages,
                tree,
                creating_csn,
                logical_time_micros,
                &mutation.key,
            )?,
            Opcode::DeleteHash => expire_due_collection_v3(
                pages,
                tree,
                creating_csn,
                logical_time_micros,
                &mutation.key,
                StructureCollectionFamily::Hash,
            )?,
            Opcode::DeleteSet => expire_due_collection_v3(
                pages,
                tree,
                creating_csn,
                logical_time_micros,
                &mutation.key,
                StructureCollectionFamily::Set,
            )?,
            Opcode::DeleteList => expire_due_collection_v3(
                pages,
                tree,
                creating_csn,
                logical_time_micros,
                &mutation.key,
                StructureCollectionFamily::List,
            )?,
            Opcode::DeleteStream => expire_due_collection_v3(
                pages,
                tree,
                creating_csn,
                logical_time_micros,
                &mutation.key,
                StructureCollectionFamily::Stream,
            )?,
            Opcode::DeleteSortedSet => expire_due_collection_v3(
                pages,
                tree,
                creating_csn,
                logical_time_micros,
                &mutation.key,
                StructureCollectionFamily::SortedSet,
            )?,
            Opcode::DeleteHashField => expire_due_hash_field_v3(
                pages,
                tree,
                creating_csn,
                logical_time_micros,
                &mutation.key,
            )?,
            _ => return Err(NativeRuntimeError::InvalidPreparedMutation),
        };
    }
    Ok(tree)
}

fn expire_due_scalar_v3(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    logical_time_micros: i64,
    key: &[u8],
) -> Result<BTree, NativeRuntimeError> {
    let scalar_key = structure_key(key);
    let scalar = tree
        .get(pages, &scalar_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let expiry = structure_value_expiry(&scalar)?
        .filter(|expiry| *expiry <= logical_time_micros)
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let expiry_key = crate::structure_expiry_key(expiry, key)?;
    if tree.get(pages, &expiry_key)?.as_deref() != Some(&[crate::STRUCTURE_EXPIRY_LIVE]) {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let mut entries = vec![
        (scalar_key, structure_tombstone_value()),
        (expiry_key, vec![STRUCTURE_EXPIRY_TOMBSTONE]),
    ];
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    Ok(tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree)
}

fn expire_due_collection_v3(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    logical_time_micros: i64,
    key: &[u8],
    family: StructureCollectionFamily,
) -> Result<BTree, NativeRuntimeError> {
    let metadata = tree
        .get(pages, &collection_metadata_key_v3(family, key)?)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let TypedCollectionMetadataV3::Live { state, .. } =
        decode_typed_collection_metadata(&metadata).map_err(map_codec_error)?
    else {
        return Err(NativeRuntimeError::InvalidStructureTree);
    };
    if state.family() != family
        || state
            .expires_at_micros()
            .is_none_or(|expiry| expiry > logical_time_micros)
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    match family {
        StructureCollectionFamily::Hash => {
            Ok(delete_hash_v3_in_tree(pages, tree, creating_csn, key)?.0)
        }
        StructureCollectionFamily::Set => {
            Ok(delete_set_v3_in_tree(pages, tree, creating_csn, key)?.0)
        }
        StructureCollectionFamily::List => {
            Ok(delete_list_v3_in_tree(pages, tree, creating_csn, key)?.0)
        }
        StructureCollectionFamily::SortedSet => {
            Ok(delete_sorted_set_v3_in_tree(pages, tree, creating_csn, key)?.0)
        }
        StructureCollectionFamily::Stream => {
            Ok(delete_stream_v3_in_tree(pages, tree, creating_csn, key)?.0)
        }
    }
}

fn expire_due_hash_field_v3(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    logical_time_micros: i64,
    identity: &[u8],
) -> Result<BTree, NativeRuntimeError> {
    let (key, field) = crate::decode_hash_field_identity(identity)?;
    let metadata_key = structure_hash_meta_key(key);
    let metadata = tree
        .get(pages, &metadata_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let state =
        decode_live_hash_metadata_v3(&metadata)?.ok_or(NativeRuntimeError::InvalidStructureTree)?;
    if state
        .expires_at_micros
        .is_some_and(|expiry| expiry <= logical_time_micros)
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let field_key =
        encode_collection_child_key(STRUCTURE_HASH_FIELD_PREFIX, key, state.incarnation, field)
            .map_err(map_codec_error)?;
    let field_value = tree
        .get(pages, &field_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let expiry = structure_value_expiry(&field_value)?
        .filter(|expiry| *expiry <= logical_time_micros)
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let expiry_key = encode_collection_expiry_key(
        crate::STRUCTURE_HASH_FIELD_EXPIRY_PREFIX,
        expiry,
        key,
        state.incarnation,
        field,
    )
    .map_err(map_codec_error)?;
    if tree.get(pages, &expiry_key)?.as_deref() != Some(&[STRUCTURE_HASH_FIELD_EXPIRY_LIVE]) {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let field_count = state
        .field_count
        .checked_sub(1)
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let field_expiry_count = state
        .field_expiry_count
        .checked_sub(1)
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let metadata = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
        incarnation: state.incarnation,
        state: CollectionStateV3::Hash {
            field_count,
            field_expiry_count,
            expires_at_micros: state.expires_at_micros,
        },
    })
    .map_err(map_codec_error)?;
    let mut entries = vec![
        (metadata_key, metadata),
        (field_key, structure_tombstone_value()),
        (expiry_key, vec![STRUCTURE_EXPIRY_TOMBSTONE]),
    ];
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    Ok(tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree)
}

pub(super) fn cleanup_structure_retirement_v3_step(
    pages: &mut PageStore,
    pool: &BufferPool,
    tree: BTree,
    creating_csn: Csn,
    retirement_key: &[u8],
    entry_budget: usize,
) -> Result<BTree, NativeRuntimeError> {
    let encoded_record = tree
        .get(pages, retirement_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    if is_structure_tombstone(&encoded_record) {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    let record = decode_retirement_record(&encoded_record).map_err(map_codec_error)?;
    let (tree, _, _) = match record.family {
        StructureCollectionFamily::Hash => cleanup_hash_retirement_v3_step(
            pages,
            pool,
            tree,
            creating_csn,
            retirement_key,
            entry_budget,
        )?,
        StructureCollectionFamily::Set => cleanup_set_retirement_v3_step(
            pages,
            pool,
            tree,
            creating_csn,
            retirement_key,
            entry_budget,
        )?,
        StructureCollectionFamily::List => cleanup_list_retirement_v3_step(
            pages,
            pool,
            tree,
            creating_csn,
            retirement_key,
            entry_budget,
        )?,
        StructureCollectionFamily::SortedSet => cleanup_sorted_set_retirement_v3_step(
            pages,
            pool,
            tree,
            creating_csn,
            retirement_key,
            entry_budget,
        )?,
        StructureCollectionFamily::Stream => cleanup_stream_retirement_v3_step(
            pages,
            pool,
            tree,
            creating_csn,
            retirement_key,
            entry_budget,
        )?,
    };
    Ok(tree)
}

fn materialize_collection_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    tree: BTree,
    key: Vec<u8>,
    collection: LiveCollectionV3,
    state: &mut crate::model::StructureState,
) -> Result<(), NativeRuntimeError> {
    match collection.family {
        StructureCollectionFamily::Hash => {
            let fields = read_hash_fields_v3(pages, blobs, tree, &key)?
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
            let mut values = BTreeMap::new();
            for field in fields {
                if let Some(expiry) = field.expires_at_micros {
                    state
                        .hash_field_expiries
                        .insert((key.clone(), field.field.clone()), expiry);
                }
                values.insert(field.field, field.value);
            }
            state.hashes.insert(key.clone(), values);
        }
        StructureCollectionFamily::Set => {
            let members = read_set_members_v3(pages, tree, &key)?
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
            state.sets.insert(key.clone(), BTreeSet::from_iter(members));
        }
        StructureCollectionFamily::List => {
            let values = read_list_values_v3(pages, blobs, tree, &key)?
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
            state.lists.insert(key.clone(), VecDeque::from(values));
        }
        StructureCollectionFamily::SortedSet => {
            let members = read_sorted_set_members_v3(pages, tree, &key)?
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
            state.sorted_sets.insert(
                key.clone(),
                members
                    .into_iter()
                    .map(|member| (member.member, member.score))
                    .collect(),
            );
        }
        StructureCollectionFamily::Stream => {
            let entries = read_stream_entries_v3(pages, tree, &key)?
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
            state
                .streams
                .insert(key.clone(), entries.into_iter().collect());
        }
    }
    if let Some(expiry) = collection.state.expires_at_micros() {
        match collection.family {
            StructureCollectionFamily::Hash => {
                state.hash_expiries.insert(key, expiry);
            }
            StructureCollectionFamily::Set => {
                state.set_expiries.insert(key, expiry);
            }
            StructureCollectionFamily::List => {
                state.list_expiries.insert(key, expiry);
            }
            StructureCollectionFamily::SortedSet => {
                state.sorted_set_expiries.insert(key, expiry);
            }
            StructureCollectionFamily::Stream => {
                state.stream_expiries.insert(key, expiry);
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MigrationCollectionV3 {
    family: StructureCollectionFamily,
    incarnation: StructureIncarnation,
    state: CollectionStateV3,
}

type MigratedPhysicalEntryV3 = Option<(Vec<u8>, Vec<u8>)>;
type PhysicalEntriesV3 = Vec<(Vec<u8>, Vec<u8>)>;

pub(super) fn migrate_v2_structure_tree_to_v3(
    pages: &mut PageStore,
    blobs: &BlobStore,
    source: BTree,
    creating_csn: Csn,
    transaction_id: TransactionId,
) -> Result<BTree, NativeRuntimeError> {
    let source_root = source
        .root()
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let source_state = crate::load_structure_state_root(pages, blobs, source_root)?;
    let source_entries = source.scan(pages)?;
    if source_entries
        .first()
        .map(|entry| (entry.0.as_slice(), entry.1.as_slice()))
        != Some((
            crate::STRUCTURE_FORMAT_KEY,
            crate::STRUCTURE_FORMAT_VALUE_V2,
        ))
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let collections = migration_collections_v3(&source_entries, &source_state, transaction_id)?;
    let mut migrated_entries = Vec::with_capacity(source_entries.len());
    migrated_entries.push((
        crate::STRUCTURE_FORMAT_KEY.to_vec(),
        STRUCTURE_FORMAT_VALUE_V3.to_vec(),
    ));
    for (physical_key, encoded) in source_entries.into_iter().skip(1) {
        if let Some(entry) = migrate_v2_entry_to_v3(&physical_key, &encoded, &collections)? {
            migrated_entries.push(entry);
        }
    }
    migrated_entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    if migrated_entries
        .windows(2)
        .any(|pair| pair[0].0 == pair[1].0)
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let migrated = BTree::empty()
        .upsert_sorted_batch(pages, creating_csn, migrated_entries)?
        .tree;
    let migrated_state = load_structure_state_v3(pages, blobs, migrated)?;
    if migrated_state != source_state {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(migrated)
}

fn migration_collections_v3(
    entries: &[(Vec<u8>, Vec<u8>)],
    state: &crate::model::StructureState,
    transaction_id: TransactionId,
) -> Result<BTreeMap<Vec<u8>, MigrationCollectionV3>, NativeRuntimeError> {
    let mut seeds = Vec::<(Vec<u8>, StructureCollectionFamily, CollectionStateV3)>::new();
    for (physical_key, encoded) in entries {
        let Some(prefix) = physical_key.first().copied() else {
            return Err(NativeRuntimeError::InvalidStructureTree);
        };
        let Some(family) = metadata_family_v3(prefix) else {
            continue;
        };
        if is_structure_tombstone(encoded) {
            continue;
        }
        let key = physical_key[1..].to_vec();
        let collection_state = migration_collection_state_v3(family, &key, encoded, state)?;
        seeds.push((key, family, collection_state));
    }
    let mut collections = BTreeMap::new();
    for (ordinal, (key, family, state)) in seeds.into_iter().enumerate() {
        let incarnation = StructureIncarnation::from_mutation_index(transaction_id, ordinal)
            .map_err(map_codec_error)?;
        if collections
            .insert(
                key,
                MigrationCollectionV3 {
                    family,
                    incarnation,
                    state,
                },
            )
            .is_some()
        {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
    }
    Ok(collections)
}

fn migration_collection_state_v3(
    family: StructureCollectionFamily,
    key: &[u8],
    encoded: &[u8],
    state: &crate::model::StructureState,
) -> Result<CollectionStateV3, NativeRuntimeError> {
    match family {
        StructureCollectionFamily::Hash => {
            let metadata = crate::decode_live_hash_metadata(encoded)?
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
            let field_expiry_count = state
                .hash_field_expiries
                .keys()
                .filter(|(hash, _)| hash.as_slice() == key)
                .count();
            Ok(CollectionStateV3::Hash {
                field_count: metadata.field_count,
                field_expiry_count: u64::try_from(field_expiry_count)
                    .map_err(|_| NativeRuntimeError::InvalidStructureTree)?,
                expires_at_micros: metadata.expires_at_micros,
            })
        }
        StructureCollectionFamily::Set => {
            let metadata = crate::decode_live_set_metadata(encoded)?
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
            Ok(CollectionStateV3::Set {
                member_count: metadata.member_count,
                expires_at_micros: metadata.expires_at_micros,
            })
        }
        StructureCollectionFamily::List => {
            let metadata = crate::decode_live_list_metadata(encoded)?
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
            let logical_value_bytes = state
                .lists
                .get(key)
                .ok_or(NativeRuntimeError::InvalidStructureTree)?
                .iter()
                .try_fold(0_u64, |total, value| {
                    total
                        .checked_add(
                            u64::try_from(value.len())
                                .map_err(|_| NativeRuntimeError::InvalidStructureTree)?,
                        )
                        .ok_or(NativeRuntimeError::InvalidStructureTree)
                })?;
            Ok(CollectionStateV3::List {
                element_count: metadata.length,
                logical_value_bytes,
                head_chunk: metadata.head_chunk,
                tail_chunk: metadata.tail_chunk,
                expires_at_micros: metadata.expires_at_micros,
            })
        }
        StructureCollectionFamily::SortedSet => {
            let (member_count, expires_at_micros) =
                crate::decode_sorted_set_metadata_state(encoded)?;
            Ok(CollectionStateV3::SortedSet {
                member_count,
                expires_at_micros,
            })
        }
        StructureCollectionFamily::Stream => {
            let (last_id, expires_at_micros) = crate::decode_stream_metadata(encoded)?;
            let entry_count = state
                .streams
                .get(key)
                .ok_or(NativeRuntimeError::InvalidStructureTree)?
                .len();
            Ok(CollectionStateV3::Stream {
                entry_count: u64::try_from(entry_count)
                    .map_err(|_| NativeRuntimeError::InvalidStructureTree)?,
                last_id,
                expires_at_micros,
            })
        }
    }
}

fn migrate_v2_entry_to_v3(
    physical_key: &[u8],
    encoded: &[u8],
    collections: &BTreeMap<Vec<u8>, MigrationCollectionV3>,
) -> Result<MigratedPhysicalEntryV3, NativeRuntimeError> {
    let prefix = physical_key
        .first()
        .copied()
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    if let Some(family) = metadata_family_v3(prefix) {
        return migrate_v2_metadata_to_v3(physical_key, encoded, family, collections);
    }
    match prefix {
        crate::STRUCTURE_ENTRY_PREFIX => {
            Ok((!is_structure_tombstone(encoded))
                .then(|| (physical_key.to_vec(), encoded.to_vec())))
        }
        STRUCTURE_HASH_FIELD_PREFIX => migrate_v2_member_to_v3(
            physical_key,
            encoded,
            collections,
            StructureCollectionFamily::Hash,
            crate::decode_hash_field_identity,
        ),
        STRUCTURE_SET_MEMBER_PREFIX => migrate_v2_member_to_v3(
            physical_key,
            encoded,
            collections,
            StructureCollectionFamily::Set,
            crate::decode_set_member_identity,
        ),
        STRUCTURE_LIST_CHUNK_PREFIX => {
            migrate_v2_list_chunk_to_v3(physical_key, encoded, collections)
        }
        STRUCTURE_SORTED_SET_MEMBER_PREFIX => migrate_v2_member_to_v3(
            physical_key,
            encoded,
            collections,
            StructureCollectionFamily::SortedSet,
            crate::decode_sorted_set_member_identity,
        ),
        STRUCTURE_SORTED_SET_ORDER_PREFIX => {
            migrate_v2_sorted_order_to_v3(physical_key, encoded, collections)
        }
        STRUCTURE_STREAM_ENTRY_PREFIX => {
            migrate_v2_stream_entry_to_v3(physical_key, encoded, collections)
        }
        crate::STRUCTURE_EXPIRY_PREFIX => {
            migrate_v2_whole_expiry_to_v3(physical_key, encoded, collections)
        }
        crate::STRUCTURE_HASH_FIELD_EXPIRY_PREFIX => {
            migrate_v2_hash_field_expiry_to_v3(physical_key, encoded, collections)
        }
        _ => Err(NativeRuntimeError::InvalidStructureTree),
    }
}

fn migrate_v2_metadata_to_v3(
    physical_key: &[u8],
    encoded: &[u8],
    family: StructureCollectionFamily,
    collections: &BTreeMap<Vec<u8>, MigrationCollectionV3>,
) -> Result<MigratedPhysicalEntryV3, NativeRuntimeError> {
    if is_structure_tombstone(encoded) {
        return Ok(None);
    }
    let collection = migration_collection_v3(collections, &physical_key[1..], family)?;
    let metadata = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
        incarnation: collection.incarnation,
        state: collection.state,
    })
    .map_err(map_codec_error)?;
    Ok(Some((physical_key.to_vec(), metadata)))
}

fn migrate_v2_member_to_v3<'identity>(
    physical_key: &'identity [u8],
    encoded: &[u8],
    collections: &BTreeMap<Vec<u8>, MigrationCollectionV3>,
    family: StructureCollectionFamily,
    decode_identity: impl Fn(
        &'identity [u8],
    ) -> Result<(&'identity [u8], &'identity [u8]), NativeRuntimeError>,
) -> Result<MigratedPhysicalEntryV3, NativeRuntimeError> {
    if is_structure_tombstone(encoded) {
        return Ok(None);
    }
    let (key, child_identity) = decode_identity(&physical_key[1..])?;
    let collection = migration_collection_v3(collections, key, family)?;
    let migrated_key =
        encode_collection_child_key(physical_key[0], key, collection.incarnation, child_identity)
            .map_err(map_codec_error)?;
    Ok(Some((migrated_key, encoded.to_vec())))
}

fn migrate_v2_list_chunk_to_v3(
    physical_key: &[u8],
    encoded: &[u8],
    collections: &BTreeMap<Vec<u8>, MigrationCollectionV3>,
) -> Result<MigratedPhysicalEntryV3, NativeRuntimeError> {
    if is_structure_tombstone(encoded) {
        return Ok(None);
    }
    let (key, chunk_id) = crate::decode_list_chunk_identity(&physical_key[1..])?;
    let collection = migration_collection_v3(collections, key, StructureCollectionFamily::List)?;
    let migrated_key = encode_collection_child_key(
        STRUCTURE_LIST_CHUNK_PREFIX,
        key,
        collection.incarnation,
        &encode_list_chunk_identity_v3(chunk_id),
    )
    .map_err(map_codec_error)?;
    Ok(Some((migrated_key, encoded.to_vec())))
}

fn migrate_v2_sorted_order_to_v3(
    physical_key: &[u8],
    encoded: &[u8],
    collections: &BTreeMap<Vec<u8>, MigrationCollectionV3>,
) -> Result<MigratedPhysicalEntryV3, NativeRuntimeError> {
    if is_structure_tombstone(encoded) {
        return Ok(None);
    }
    let (key, score, member) = crate::decode_sorted_set_order_identity(&physical_key[1..])?;
    let collection =
        migration_collection_v3(collections, key, StructureCollectionFamily::SortedSet)?;
    let migrated_key = v3_sorted_set_order_key(key, collection.incarnation, score, member)?;
    Ok(Some((migrated_key, encoded.to_vec())))
}

fn migrate_v2_stream_entry_to_v3(
    physical_key: &[u8],
    encoded: &[u8],
    collections: &BTreeMap<Vec<u8>, MigrationCollectionV3>,
) -> Result<MigratedPhysicalEntryV3, NativeRuntimeError> {
    if is_structure_tombstone(encoded) {
        return Ok(None);
    }
    let (key, collection) = collections
        .iter()
        .find(|(key, collection)| {
            collection.family == StructureCollectionFamily::Stream
                && physical_key[1..].len() == key.len().saturating_add(8)
                && physical_key[1..].starts_with(key)
        })
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let id = physical_key
        .get(1 + key.len()..)
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let migrated_key = encode_collection_child_key(
        STRUCTURE_STREAM_ENTRY_PREFIX,
        key,
        collection.incarnation,
        id,
    )
    .map_err(map_codec_error)?;
    Ok(Some((migrated_key, encoded.to_vec())))
}

fn migrate_v2_whole_expiry_to_v3(
    physical_key: &[u8],
    marker: &[u8],
    collections: &BTreeMap<Vec<u8>, MigrationCollectionV3>,
) -> Result<MigratedPhysicalEntryV3, NativeRuntimeError> {
    if marker == [STRUCTURE_EXPIRY_TOMBSTONE] {
        return Ok(None);
    }
    if marker == [crate::STRUCTURE_EXPIRY_LIVE] {
        crate::decode_structure_expiry_identity(&physical_key[1..])?;
        return Ok(Some((physical_key.to_vec(), marker.to_vec())));
    }
    let family = expiry_marker_family_v3(marker)?;
    let (expiry, key) = crate::decode_structure_expiry_identity(&physical_key[1..])?;
    let collection = migration_collection_v3(collections, key, family)?;
    let migrated_key = encode_collection_expiry_key(
        crate::STRUCTURE_EXPIRY_PREFIX,
        expiry,
        key,
        collection.incarnation,
        &[],
    )
    .map_err(map_codec_error)?;
    Ok(Some((migrated_key, marker.to_vec())))
}

fn migrate_v2_hash_field_expiry_to_v3(
    physical_key: &[u8],
    marker: &[u8],
    collections: &BTreeMap<Vec<u8>, MigrationCollectionV3>,
) -> Result<MigratedPhysicalEntryV3, NativeRuntimeError> {
    if marker == [STRUCTURE_EXPIRY_TOMBSTONE] {
        return Ok(None);
    }
    if marker != [STRUCTURE_HASH_FIELD_EXPIRY_LIVE] {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let (expiry, key, field) =
        crate::decode_structure_hash_field_expiry_identity(&physical_key[1..])?;
    let collection = migration_collection_v3(collections, key, StructureCollectionFamily::Hash)?;
    let migrated_key = encode_collection_expiry_key(
        crate::STRUCTURE_HASH_FIELD_EXPIRY_PREFIX,
        expiry,
        key,
        collection.incarnation,
        field,
    )
    .map_err(map_codec_error)?;
    Ok(Some((migrated_key, marker.to_vec())))
}

fn migration_collection_v3<'collection>(
    collections: &'collection BTreeMap<Vec<u8>, MigrationCollectionV3>,
    key: &[u8],
    family: StructureCollectionFamily,
) -> Result<&'collection MigrationCollectionV3, NativeRuntimeError> {
    collections
        .get(key)
        .filter(|collection| collection.family == family)
        .ok_or(NativeRuntimeError::InvalidStructureTree)
}

fn expiry_marker_family_v3(marker: &[u8]) -> Result<StructureCollectionFamily, NativeRuntimeError> {
    match marker {
        [STRUCTURE_HASH_EXPIRY_LIVE] => Ok(StructureCollectionFamily::Hash),
        [STRUCTURE_SET_EXPIRY_LIVE] => Ok(StructureCollectionFamily::Set),
        [STRUCTURE_LIST_EXPIRY_LIVE] => Ok(StructureCollectionFamily::List),
        [STRUCTURE_SORTED_SET_EXPIRY_LIVE] => Ok(StructureCollectionFamily::SortedSet),
        [STRUCTURE_STREAM_EXPIRY_LIVE] => Ok(StructureCollectionFamily::Stream),
        _ => Err(NativeRuntimeError::InvalidStructureTree),
    }
}

fn inventory_structure_v3_entries(
    physical: &BTreeMap<Vec<u8>, Vec<u8>>,
    blobs: &BlobStore,
) -> Result<StructureV3Inventory, NativeRuntimeError> {
    let mut inventory = StructureV3Inventory::default();
    for (key, value) in physical {
        if key.as_slice() == crate::STRUCTURE_FORMAT_KEY {
            continue;
        }
        let prefix = key
            .first()
            .copied()
            .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        if let Some(family) = metadata_family_v3(prefix) {
            let collection_key = key[1..].to_vec();
            match decode_typed_collection_metadata(value).map_err(map_codec_error)? {
                TypedCollectionMetadataV3::Live { incarnation, state }
                    if state.family() == family =>
                {
                    if inventory
                        .live_collections
                        .insert(
                            collection_key,
                            LiveCollectionV3 {
                                family,
                                incarnation,
                                state,
                            },
                        )
                        .is_some()
                    {
                        return Err(NativeRuntimeError::InvalidStructureTree);
                    }
                }
                TypedCollectionMetadataV3::Tombstone {
                    family: encoded_family,
                    ..
                } if encoded_family == family => {}
                _ => return Err(NativeRuntimeError::InvalidStructureTree),
            }
            continue;
        }
        match prefix {
            crate::STRUCTURE_ENTRY_PREFIX => {
                if let Some(entry) = decode_structure_value(value, blobs)?
                    && inventory
                        .live_scalars
                        .insert(key[1..].to_vec(), entry.expires_at_micros)
                        .is_some()
                {
                    return Err(NativeRuntimeError::InvalidStructureTree);
                }
            }
            STRUCTURE_HASH_FIELD_PREFIX
            | STRUCTURE_SET_MEMBER_PREFIX
            | STRUCTURE_LIST_CHUNK_PREFIX
            | STRUCTURE_SORTED_SET_MEMBER_PREFIX
            | STRUCTURE_SORTED_SET_ORDER_PREFIX
            | STRUCTURE_STREAM_ENTRY_PREFIX => {
                decode_collection_child_key(key).map_err(map_codec_error)?;
                inventory.child_entries.push((key.clone(), value.clone()));
            }
            crate::STRUCTURE_EXPIRY_PREFIX | crate::STRUCTURE_HASH_FIELD_EXPIRY_PREFIX => {
                inventory.expiry_entries.push((key.clone(), value.clone()));
            }
            STRUCTURE_RETIREMENT_PREFIX => {
                let (collection_key, incarnation) =
                    decode_retirement_key(key).map_err(map_codec_error)?;
                if !is_structure_tombstone(value) {
                    let record = decode_retirement_record(value).map_err(map_codec_error)?;
                    validate_retirement_state(key, &record).map_err(map_codec_error)?;
                    inventory.active_retirements.push(ActiveRetirementV3 {
                        collection_key: collection_key.to_vec(),
                        incarnation,
                        record,
                    });
                }
            }
            _ => return Err(NativeRuntimeError::InvalidStructureTree),
        }
    }
    Ok(inventory)
}

pub(super) fn plan_structure_v3_compaction(
    pages: &PageStore,
    blobs: &BlobStore,
    tree: BTree,
) -> Result<StructureV3CompactionSummary, NativeRuntimeError> {
    structure_v3_compaction_entries(pages, blobs, tree).map(|(_, summary)| summary)
}

pub(super) fn compact_structure_v3_tree(
    pages: &mut PageStore,
    blobs: &BlobStore,
    tree: BTree,
    creating_csn: Csn,
) -> Result<(BTree, StructureV3CompactionSummary), NativeRuntimeError> {
    let (retained, summary) = structure_v3_compaction_entries(pages, blobs, tree)?;
    let compacted = BTree::empty()
        .upsert_sorted_batch(pages, creating_csn, retained)?
        .tree;
    let compacted_validation = validate_structure_v3_tree(pages, blobs, compacted)?;
    if compacted_validation.active_retirements != summary.active_retirements {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok((compacted, summary))
}

fn structure_v3_compaction_entries(
    pages: &PageStore,
    blobs: &BlobStore,
    tree: BTree,
) -> Result<(PhysicalEntriesV3, StructureV3CompactionSummary), NativeRuntimeError> {
    let validation = validate_structure_v3_tree(pages, blobs, tree)?;
    let entries = tree.scan(pages)?;
    let scanned_entries = entries.len();
    let mut retained = Vec::with_capacity(scanned_entries);
    let mut dropped_tombstones = 0_usize;
    for (key, value) in entries {
        if compactable_structure_v3_tombstone(&key, &value)? {
            dropped_tombstones = dropped_tombstones
                .checked_add(1)
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        } else {
            retained.push((key, value));
        }
    }
    let retained_entries = retained.len();
    Ok((
        retained,
        StructureV3CompactionSummary {
            scanned_entries,
            retained_entries,
            dropped_tombstones,
            active_retirements: validation.active_retirements,
        },
    ))
}

fn metadata_family_v3(prefix: u8) -> Option<StructureCollectionFamily> {
    match prefix {
        crate::STRUCTURE_HASH_META_PREFIX => Some(StructureCollectionFamily::Hash),
        crate::STRUCTURE_SET_META_PREFIX => Some(StructureCollectionFamily::Set),
        crate::STRUCTURE_LIST_META_PREFIX => Some(StructureCollectionFamily::List),
        crate::STRUCTURE_SORTED_SET_META_PREFIX => Some(StructureCollectionFamily::SortedSet),
        crate::STRUCTURE_STREAM_META_PREFIX => Some(StructureCollectionFamily::Stream),
        _ => None,
    }
}

fn compactable_structure_v3_tombstone(
    key: &[u8],
    value: &[u8],
) -> Result<bool, NativeRuntimeError> {
    if key == crate::STRUCTURE_FORMAT_KEY {
        return Ok(false);
    }
    let prefix = key
        .first()
        .copied()
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    if metadata_family_v3(prefix).is_some() {
        return Ok(matches!(
            decode_typed_collection_metadata(value).map_err(map_codec_error)?,
            TypedCollectionMetadataV3::Tombstone { .. }
        ));
    }
    match prefix {
        crate::STRUCTURE_ENTRY_PREFIX
        | STRUCTURE_HASH_FIELD_PREFIX
        | STRUCTURE_SET_MEMBER_PREFIX
        | STRUCTURE_LIST_CHUNK_PREFIX
        | STRUCTURE_SORTED_SET_MEMBER_PREFIX
        | STRUCTURE_SORTED_SET_ORDER_PREFIX
        | STRUCTURE_STREAM_ENTRY_PREFIX
        | STRUCTURE_RETIREMENT_PREFIX => Ok(is_structure_tombstone(value)),
        crate::STRUCTURE_EXPIRY_PREFIX | crate::STRUCTURE_HASH_FIELD_EXPIRY_PREFIX => {
            Ok(value == [STRUCTURE_EXPIRY_TOMBSTONE])
        }
        _ => Err(NativeRuntimeError::InvalidStructureTree),
    }
}

fn validate_live_collections_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    tree: BTree,
    collections: &BTreeMap<Vec<u8>, LiveCollectionV3>,
) -> Result<(), NativeRuntimeError> {
    for (key, collection) in collections {
        let present = match collection.family {
            StructureCollectionFamily::Hash => {
                read_hash_fields_v3(pages, blobs, tree, key)?.is_some()
            }
            StructureCollectionFamily::Set => read_set_members_v3(pages, tree, key)?.is_some(),
            StructureCollectionFamily::List => {
                read_list_values_v3(pages, blobs, tree, key)?.is_some()
            }
            StructureCollectionFamily::SortedSet => {
                read_sorted_set_members_v3(pages, tree, key)?.is_some()
            }
            StructureCollectionFamily::Stream => {
                read_stream_entries_v3(pages, tree, key)?.is_some()
            }
        };
        if !present {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
    }
    Ok(())
}

fn validate_child_ownership_v3(
    physical: &BTreeMap<Vec<u8>, Vec<u8>>,
    children: &[(Vec<u8>, Vec<u8>)],
    collections: &BTreeMap<Vec<u8>, LiveCollectionV3>,
    retirements: &[ActiveRetirementV3],
) -> Result<(), NativeRuntimeError> {
    for (physical_key, value) in children {
        let decoded = decode_collection_child_key(physical_key).map_err(map_codec_error)?;
        let family =
            child_family_v3(decoded.prefix).ok_or(NativeRuntimeError::InvalidStructureTree)?;
        let live = validate_child_payload_v3(decoded.prefix, value)?;
        if !live {
            continue;
        }
        let current_owner = collections
            .get(decoded.collection_key)
            .is_some_and(|collection| {
                collection.family == family && collection.incarnation == decoded.incarnation
            });
        let retirement_owner = retirements.iter().any(|retirement| {
            retirement.record.family == family
                && retirement.collection_key == decoded.collection_key
                && retirement.incarnation == decoded.incarnation
        });
        if !current_owner && !retirement_owner {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        if decoded.prefix == STRUCTURE_SORTED_SET_ORDER_PREFIX {
            validate_sorted_set_order_pair_v3(physical, decoded)?;
        }
    }
    Ok(())
}

fn child_family_v3(prefix: u8) -> Option<StructureCollectionFamily> {
    match prefix {
        STRUCTURE_HASH_FIELD_PREFIX => Some(StructureCollectionFamily::Hash),
        STRUCTURE_SET_MEMBER_PREFIX => Some(StructureCollectionFamily::Set),
        STRUCTURE_LIST_CHUNK_PREFIX => Some(StructureCollectionFamily::List),
        STRUCTURE_SORTED_SET_MEMBER_PREFIX | STRUCTURE_SORTED_SET_ORDER_PREFIX => {
            Some(StructureCollectionFamily::SortedSet)
        }
        STRUCTURE_STREAM_ENTRY_PREFIX => Some(StructureCollectionFamily::Stream),
        _ => None,
    }
}

fn validate_child_payload_v3(prefix: u8, encoded: &[u8]) -> Result<bool, NativeRuntimeError> {
    match prefix {
        STRUCTURE_HASH_FIELD_PREFIX => {
            if is_structure_tombstone(encoded) {
                Ok(false)
            } else {
                structure_value_expiry(encoded).map(|_| true)
            }
        }
        STRUCTURE_SET_MEMBER_PREFIX | STRUCTURE_SORTED_SET_ORDER_PREFIX => {
            decode_set_member_value(encoded)
        }
        STRUCTURE_LIST_CHUNK_PREFIX => list_chunk_summary_v3(encoded).map(|value| value.is_some()),
        STRUCTURE_SORTED_SET_MEMBER_PREFIX => {
            decode_sorted_set_score(encoded).map(|value| value.is_some())
        }
        STRUCTURE_STREAM_ENTRY_PREFIX => {
            if is_structure_tombstone(encoded) {
                Ok(false)
            } else {
                decode_stream_wal_entry(encoded)
                    .map(|_| true)
                    .map_err(|_| NativeRuntimeError::InvalidStructureTree)
            }
        }
        _ => Err(NativeRuntimeError::InvalidStructureTree),
    }
}

fn validate_sorted_set_order_pair_v3(
    physical: &BTreeMap<Vec<u8>, Vec<u8>>,
    order: DecodedCollectionChild<'_>,
) -> Result<(), NativeRuntimeError> {
    let (score, member) = decode_sorted_set_order_identity_v3(order.child_identity)?;
    let member_key = encode_collection_child_key(
        STRUCTURE_SORTED_SET_MEMBER_PREFIX,
        order.collection_key,
        order.incarnation,
        member,
    )
    .map_err(map_codec_error)?;
    let stored_score = physical
        .get(&member_key)
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    if decode_sorted_set_score(stored_score)? != Some(score) {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(())
}

fn decode_sorted_set_order_identity_v3(
    encoded: &[u8],
) -> Result<(SortedSetScore, &[u8]), NativeRuntimeError> {
    let sortable = u64::from_be_bytes(
        encoded
            .get(..8)
            .ok_or(NativeRuntimeError::InvalidStructureTree)?
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidStructureTree)?,
    );
    let score = SortedSetScore::from_sortable_bits(sortable)
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    Ok((score, &encoded[8..]))
}

fn validate_expiry_entries_v3(
    physical: &BTreeMap<Vec<u8>, Vec<u8>>,
    expiry_entries: &[(Vec<u8>, Vec<u8>)],
    live_scalars: &BTreeMap<Vec<u8>, Option<i64>>,
    collections: &BTreeMap<Vec<u8>, LiveCollectionV3>,
    retirements: &[ActiveRetirementV3],
) -> Result<(), NativeRuntimeError> {
    for (key, marker) in expiry_entries {
        if marker.as_slice() == [STRUCTURE_EXPIRY_TOMBSTONE] {
            continue;
        }
        match key.first().copied() {
            Some(crate::STRUCTURE_EXPIRY_PREFIX) => {
                validate_whole_expiry_entry_v3(key, marker, live_scalars, collections)?;
            }
            Some(crate::STRUCTURE_HASH_FIELD_EXPIRY_PREFIX) => {
                if marker.as_slice() != [STRUCTURE_HASH_FIELD_EXPIRY_LIVE] {
                    return Err(NativeRuntimeError::InvalidStructureTree);
                }
                let expiry = decode_collection_expiry_key(key).map_err(map_codec_error)?;
                let current_owner = collections.get(expiry.collection_key).is_some_and(|owner| {
                    owner.family == StructureCollectionFamily::Hash
                        && owner.incarnation == expiry.incarnation
                });
                let retirement_owner = retirements.iter().any(|owner| {
                    owner.record.family == StructureCollectionFamily::Hash
                        && owner.collection_key == expiry.collection_key
                        && owner.incarnation == expiry.incarnation
                });
                if !current_owner && !retirement_owner {
                    return Err(NativeRuntimeError::InvalidStructureTree);
                }
                let field_key = encode_collection_child_key(
                    STRUCTURE_HASH_FIELD_PREFIX,
                    expiry.collection_key,
                    expiry.incarnation,
                    expiry.child_identity,
                )
                .map_err(map_codec_error)?;
                let field = physical
                    .get(&field_key)
                    .ok_or(NativeRuntimeError::InvalidStructureTree)?;
                if is_structure_tombstone(field)
                    || structure_value_expiry(field)? != Some(expiry.expires_at_micros)
                {
                    return Err(NativeRuntimeError::InvalidStructureTree);
                }
            }
            _ => return Err(NativeRuntimeError::InvalidStructureTree),
        }
    }
    validate_required_expiry_entries_v3(physical, live_scalars, collections)?;
    Ok(())
}

fn validate_required_expiry_entries_v3(
    physical: &BTreeMap<Vec<u8>, Vec<u8>>,
    live_scalars: &BTreeMap<Vec<u8>, Option<i64>>,
    collections: &BTreeMap<Vec<u8>, LiveCollectionV3>,
) -> Result<(), NativeRuntimeError> {
    for (key, expiry) in live_scalars {
        let Some(expiry) = expiry else {
            continue;
        };
        let expiry_key = crate::structure_expiry_key(*expiry, key)?;
        if physical.get(&expiry_key).map(Vec::as_slice) != Some(&[crate::STRUCTURE_EXPIRY_LIVE]) {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
    }
    for (key, collection) in collections {
        let Some(expiry) = collection.state.expires_at_micros() else {
            continue;
        };
        let expiry_key = encode_collection_expiry_key(
            crate::STRUCTURE_EXPIRY_PREFIX,
            expiry,
            key,
            collection.incarnation,
            &[],
        )
        .map_err(map_codec_error)?;
        if physical.get(&expiry_key).map(Vec::as_slice)
            != Some(&[collection_expiry_marker_v3(collection.family)])
        {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
    }
    for (physical_key, encoded) in physical {
        if physical_key.first() != Some(&STRUCTURE_HASH_FIELD_PREFIX)
            || is_structure_tombstone(encoded)
        {
            continue;
        }
        let Some(expiry) = structure_value_expiry(encoded)? else {
            continue;
        };
        let decoded = decode_collection_child_key(physical_key).map_err(map_codec_error)?;
        let expiry_key = encode_collection_expiry_key(
            crate::STRUCTURE_HASH_FIELD_EXPIRY_PREFIX,
            expiry,
            decoded.collection_key,
            decoded.incarnation,
            decoded.child_identity,
        )
        .map_err(map_codec_error)?;
        if physical.get(&expiry_key).map(Vec::as_slice) != Some(&[STRUCTURE_HASH_FIELD_EXPIRY_LIVE])
        {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
    }
    Ok(())
}

const fn collection_expiry_marker_v3(family: StructureCollectionFamily) -> u8 {
    match family {
        StructureCollectionFamily::Hash => STRUCTURE_HASH_EXPIRY_LIVE,
        StructureCollectionFamily::Set => STRUCTURE_SET_EXPIRY_LIVE,
        StructureCollectionFamily::List => STRUCTURE_LIST_EXPIRY_LIVE,
        StructureCollectionFamily::SortedSet => STRUCTURE_SORTED_SET_EXPIRY_LIVE,
        StructureCollectionFamily::Stream => STRUCTURE_STREAM_EXPIRY_LIVE,
    }
}

fn validate_whole_expiry_entry_v3(
    key: &[u8],
    marker: &[u8],
    live_scalars: &BTreeMap<Vec<u8>, Option<i64>>,
    collections: &BTreeMap<Vec<u8>, LiveCollectionV3>,
) -> Result<(), NativeRuntimeError> {
    if marker == [crate::STRUCTURE_EXPIRY_LIVE] {
        let (expiry, scalar_key) = crate::decode_structure_expiry_identity(&key[1..])?;
        if live_scalars.get(scalar_key) != Some(&Some(expiry)) {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        return Ok(());
    }
    let family = match marker {
        [STRUCTURE_HASH_EXPIRY_LIVE] => StructureCollectionFamily::Hash,
        [STRUCTURE_SET_EXPIRY_LIVE] => StructureCollectionFamily::Set,
        [STRUCTURE_LIST_EXPIRY_LIVE] => StructureCollectionFamily::List,
        [STRUCTURE_STREAM_EXPIRY_LIVE] => StructureCollectionFamily::Stream,
        [STRUCTURE_SORTED_SET_EXPIRY_LIVE] => StructureCollectionFamily::SortedSet,
        _ => return Err(NativeRuntimeError::InvalidStructureTree),
    };
    let expiry = decode_collection_expiry_key(key).map_err(map_codec_error)?;
    if !expiry.child_identity.is_empty()
        || collections
            .get(expiry.collection_key)
            .is_none_or(|collection| {
                collection.family != family
                    || collection.incarnation != expiry.incarnation
                    || collection.state.expires_at_micros() != Some(expiry.expires_at_micros)
            })
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(())
}

fn validate_active_retirement_v3(
    retirement: &ActiveRetirementV3,
    physical: &BTreeMap<Vec<u8>, Vec<u8>>,
    children: &[(Vec<u8>, Vec<u8>)],
) -> Result<(), NativeRuntimeError> {
    let cursor = retirement.record.exclusive_cursor.as_deref();
    let mut candidates = Vec::new();
    for (physical_key, encoded) in children {
        let decoded = decode_collection_child_key(physical_key).map_err(map_codec_error)?;
        if decoded.collection_key != retirement.collection_key
            || decoded.incarnation != retirement.incarnation
            || child_family_v3(decoded.prefix) != Some(retirement.record.family)
            || (retirement.record.family == StructureCollectionFamily::SortedSet
                && decoded.prefix != STRUCTURE_SORTED_SET_MEMBER_PREFIX)
        {
            continue;
        }
        let live = validate_child_payload_v3(decoded.prefix, encoded)?;
        if cursor.is_some_and(|cursor| physical_key.as_slice() <= cursor) {
            if live {
                return Err(NativeRuntimeError::InvalidStructureTree);
            }
            continue;
        }
        let (logical_items, logical_bytes, associated_secondary_entries, associated_expiry_entries) =
            retirement_candidate_summary_v3(
                retirement,
                physical,
                physical_key,
                encoded,
                decoded,
                live,
            )?;
        candidates.push(RetirementCandidateV3 {
            physical_key,
            live,
            logical_items,
            logical_bytes,
            associated_secondary_entries,
            associated_expiry_entries,
        });
    }
    validate_complete_retirement_remainder_v3(retirement, &candidates)
}

fn retirement_candidate_summary_v3(
    retirement: &ActiveRetirementV3,
    physical: &BTreeMap<Vec<u8>, Vec<u8>>,
    physical_key: &[u8],
    encoded: &[u8],
    decoded: DecodedCollectionChild<'_>,
    live: bool,
) -> Result<(u64, u64, u64, u64), NativeRuntimeError> {
    if !live {
        return Ok((0, 0, 0, 0));
    }
    match retirement.record.family {
        StructureCollectionFamily::Hash => {
            let expiry = structure_value_expiry(encoded)?;
            if let Some(expiry) = expiry {
                let expiry_key = encode_collection_expiry_key(
                    crate::STRUCTURE_HASH_FIELD_EXPIRY_PREFIX,
                    expiry,
                    &retirement.collection_key,
                    retirement.incarnation,
                    decoded.child_identity,
                )
                .map_err(map_codec_error)?;
                if physical.get(&expiry_key).map(Vec::as_slice)
                    != Some(&[STRUCTURE_HASH_FIELD_EXPIRY_LIVE])
                {
                    return Err(NativeRuntimeError::InvalidStructureTree);
                }
            }
            Ok((1, 0, 0, u64::from(expiry.is_some())))
        }
        StructureCollectionFamily::Set => Ok((1, 0, 0, 0)),
        StructureCollectionFamily::List => {
            let (items, bytes) =
                list_chunk_summary_v3(encoded)?.ok_or(NativeRuntimeError::InvalidStructureTree)?;
            Ok((items, bytes, 0, 0))
        }
        StructureCollectionFamily::SortedSet => {
            let score = decode_sorted_set_score(encoded)?
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
            let order_key = v3_sorted_set_order_key(
                &retirement.collection_key,
                retirement.incarnation,
                score,
                decoded.child_identity,
            )?;
            if physical.get(&order_key).map(Vec::as_slice) != Some(&set_member_live_value()) {
                return Err(NativeRuntimeError::InvalidStructureTree);
            }
            Ok((1, 0, 1, 0))
        }
        StructureCollectionFamily::Stream => {
            let physical_id = decode_u64(decoded.child_identity).map_err(map_codec_error)?;
            let (payload_id, _) = decode_stream_wal_entry(encoded)
                .map_err(|_| NativeRuntimeError::InvalidStructureTree)?;
            if physical_id != payload_id || physical_key != decoded_physical_key(decoded)? {
                return Err(NativeRuntimeError::InvalidStructureTree);
            }
            Ok((1, 0, 0, 0))
        }
    }
}

fn decoded_physical_key(
    decoded: DecodedCollectionChild<'_>,
) -> Result<Vec<u8>, NativeRuntimeError> {
    encode_collection_child_key(
        decoded.prefix,
        decoded.collection_key,
        decoded.incarnation,
        decoded.child_identity,
    )
    .map_err(map_codec_error)
}

fn validate_complete_retirement_remainder_v3(
    retirement: &ActiveRetirementV3,
    candidates: &[RetirementCandidateV3<'_>],
) -> Result<(), NativeRuntimeError> {
    let mut remaining = retirement.record.clone();
    let mut previous = remaining.exclusive_cursor.as_deref();
    for candidate in candidates {
        if previous.is_some_and(|cursor| candidate.physical_key <= cursor) {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        validate_list_retirement_candidate(&remaining, candidate).map_err(map_codec_error)?;
        validate_stream_retirement_candidate(&remaining, candidate).map_err(map_codec_error)?;
        let counters = retirement_candidate_counters(
            candidate,
            &retirement.collection_key,
            retirement.incarnation,
            retirement.record.family,
        )
        .map_err(map_codec_error)?;
        remaining.remaining_logical_items = remaining
            .remaining_logical_items
            .checked_sub(counters.logical_items)
            .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        remaining.remaining_primary_entries = remaining
            .remaining_primary_entries
            .checked_sub(counters.primary_entries)
            .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        remaining.remaining_secondary_entries = remaining
            .remaining_secondary_entries
            .checked_sub(counters.secondary_entries)
            .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        remaining.remaining_expiry_entries = remaining
            .remaining_expiry_entries
            .checked_sub(counters.expiry_entries)
            .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        remaining.remaining_logical_bytes = remaining
            .remaining_logical_bytes
            .checked_sub(counters.logical_bytes)
            .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        previous = Some(candidate.physical_key);
    }
    if retirement_has_remaining_work(&remaining) {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(())
}

fn v3_hash_field_prefix(
    key: &[u8],
    incarnation: StructureIncarnation,
) -> Result<Vec<u8>, NativeRuntimeError> {
    encode_collection_child_key(STRUCTURE_HASH_FIELD_PREFIX, key, incarnation, &[])
        .map_err(map_codec_error)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HashMetadataStateV3 {
    incarnation: StructureIncarnation,
    field_count: u64,
    field_expiry_count: u64,
    expires_at_micros: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HashFieldWriteV3<'value> {
    key: &'value [u8],
    field: &'value [u8],
    value: &'value [u8],
    expires_at_micros: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HashFieldReadV3 {
    field: Vec<u8>,
    value: Vec<u8>,
    expires_at_micros: Option<i64>,
}

fn decode_live_hash_metadata_v3(
    encoded: &[u8],
) -> Result<Option<HashMetadataStateV3>, NativeRuntimeError> {
    match decode_typed_collection_metadata(encoded).map_err(map_codec_error)? {
        TypedCollectionMetadataV3::Live {
            incarnation,
            state:
                CollectionStateV3::Hash {
                    field_count,
                    field_expiry_count,
                    expires_at_micros,
                },
        } => Ok(Some(HashMetadataStateV3 {
            incarnation,
            field_count,
            field_expiry_count,
            expires_at_micros,
        })),
        TypedCollectionMetadataV3::Tombstone {
            family: StructureCollectionFamily::Hash,
            ..
        } => Ok(None),
        _ => Err(NativeRuntimeError::InvalidStructureTree),
    }
}

fn create_hash_v3_in_tree(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    key: &[u8],
    incarnation: StructureIncarnation,
) -> Result<BTree, NativeRuntimeError> {
    let metadata_key = structure_hash_meta_key(key);
    if let Some(encoded) = tree.get(pages, &metadata_key)?
        && decode_live_hash_metadata_v3(&encoded)?.is_some()
    {
        return Err(NativeRuntimeError::StructureKeyExists);
    }
    let metadata = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
        incarnation,
        state: CollectionStateV3::Hash {
            field_count: 0,
            field_expiry_count: 0,
            expires_at_micros: None,
        },
    })
    .map_err(map_codec_error)?;
    Ok(tree
        .upsert(pages, creating_csn, metadata_key, metadata)?
        .tree)
}

fn put_hash_field_v3_in_tree(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    write: HashFieldWriteV3<'_>,
    blob_references: &BTreeMap<[u8; 32], BlobReference>,
) -> Result<(BTree, bool), NativeRuntimeError> {
    let metadata_key = structure_hash_meta_key(write.key);
    let metadata = tree
        .get(pages, &metadata_key)?
        .ok_or(NativeRuntimeError::UnknownStructureHash)?;
    let state =
        decode_live_hash_metadata_v3(&metadata)?.ok_or(NativeRuntimeError::UnknownStructureHash)?;
    let field_key = encode_collection_child_key(
        STRUCTURE_HASH_FIELD_PREFIX,
        write.key,
        state.incarnation,
        write.field,
    )
    .map_err(map_codec_error)?;
    let previous = tree.get(pages, &field_key)?;
    let previous_live = previous
        .as_deref()
        .filter(|encoded| !is_structure_tombstone(encoded));
    let previous_expiry = previous_live
        .map(structure_value_expiry)
        .transpose()?
        .flatten();
    let inserted = previous_live.is_none();
    let field_count = if inserted {
        state
            .field_count
            .checked_add(1)
            .ok_or(NativeRuntimeError::InvalidStructureTree)?
    } else {
        state.field_count
    };
    let field_expiry_count = match (previous_expiry.is_some(), write.expires_at_micros.is_some()) {
        (false, true) => state
            .field_expiry_count
            .checked_add(1)
            .ok_or(NativeRuntimeError::InvalidStructureTree)?,
        (true, false) => state
            .field_expiry_count
            .checked_sub(1)
            .ok_or(NativeRuntimeError::InvalidStructureTree)?,
        _ => state.field_expiry_count,
    };
    let metadata = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
        incarnation: state.incarnation,
        state: CollectionStateV3::Hash {
            field_count,
            field_expiry_count,
            expires_at_micros: state.expires_at_micros,
        },
    })
    .map_err(map_codec_error)?;
    let mut entries = vec![
        (metadata_key, metadata),
        (
            field_key,
            structure_storage_value(write.value, write.expires_at_micros, blob_references)?,
        ),
    ];
    if let Some(previous_expiry) = previous_expiry {
        let previous_expiry_key = encode_collection_expiry_key(
            crate::STRUCTURE_HASH_FIELD_EXPIRY_PREFIX,
            previous_expiry,
            write.key,
            state.incarnation,
            write.field,
        )
        .map_err(map_codec_error)?;
        if tree.get(pages, &previous_expiry_key)?.as_deref()
            != Some(&[STRUCTURE_HASH_FIELD_EXPIRY_LIVE])
        {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        if Some(previous_expiry) != write.expires_at_micros {
            entries.push((previous_expiry_key, vec![STRUCTURE_EXPIRY_TOMBSTONE]));
        }
    }
    if let Some(expiry) = write.expires_at_micros
        && previous_expiry != Some(expiry)
    {
        let expiry_key = encode_collection_expiry_key(
            crate::STRUCTURE_HASH_FIELD_EXPIRY_PREFIX,
            expiry,
            write.key,
            state.incarnation,
            write.field,
        )
        .map_err(map_codec_error)?;
        entries.push((expiry_key, vec![STRUCTURE_HASH_FIELD_EXPIRY_LIVE]));
    }
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok((
        tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree,
        inserted,
    ))
}

fn delete_hash_field_v3_in_tree(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    key: &[u8],
    field: &[u8],
) -> Result<BTree, NativeRuntimeError> {
    let metadata_key = structure_hash_meta_key(key);
    let metadata = tree
        .get(pages, &metadata_key)?
        .ok_or(NativeRuntimeError::UnknownStructureHash)?;
    let state =
        decode_live_hash_metadata_v3(&metadata)?.ok_or(NativeRuntimeError::UnknownStructureHash)?;
    let field_key =
        encode_collection_child_key(STRUCTURE_HASH_FIELD_PREFIX, key, state.incarnation, field)
            .map_err(map_codec_error)?;
    let previous = tree
        .get(pages, &field_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    if is_structure_tombstone(&previous) {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let previous_expiry = structure_value_expiry(&previous)?;
    let field_count = state
        .field_count
        .checked_sub(1)
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let field_expiry_count = state
        .field_expiry_count
        .checked_sub(u64::from(previous_expiry.is_some()))
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let metadata = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
        incarnation: state.incarnation,
        state: CollectionStateV3::Hash {
            field_count,
            field_expiry_count,
            expires_at_micros: state.expires_at_micros,
        },
    })
    .map_err(map_codec_error)?;
    let mut entries = vec![
        (metadata_key, metadata),
        (field_key, structure_tombstone_value()),
    ];
    if let Some(expiry) = previous_expiry {
        let expiry_key = encode_collection_expiry_key(
            crate::STRUCTURE_HASH_FIELD_EXPIRY_PREFIX,
            expiry,
            key,
            state.incarnation,
            field,
        )
        .map_err(map_codec_error)?;
        if tree.get(pages, &expiry_key)?.as_deref() != Some(&[STRUCTURE_HASH_FIELD_EXPIRY_LIVE]) {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        entries.push((expiry_key, vec![STRUCTURE_EXPIRY_TOMBSTONE]));
    }
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    Ok(tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree)
}

fn read_hash_fields_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    tree: BTree,
    key: &[u8],
) -> Result<Option<Vec<HashFieldReadV3>>, NativeRuntimeError> {
    let metadata = tree
        .get(pages, &structure_hash_meta_key(key))?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let Some(state) = decode_live_hash_metadata_v3(&metadata)? else {
        return Ok(None);
    };
    let prefix = v3_hash_field_prefix(key, state.incarnation)?;
    let mut fields = Vec::new();
    let mut observed_expiries = 0_u64;
    for (physical_key, encoded) in tree.scan_prefix(pages, &prefix)? {
        let decoded = decode_collection_child_key(&physical_key).map_err(map_codec_error)?;
        if decoded.collection_key != key || decoded.incarnation != state.incarnation {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        if let Some(entry) = decode_structure_value(&encoded, blobs)? {
            observed_expiries = observed_expiries
                .checked_add(u64::from(entry.expires_at_micros.is_some()))
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
            fields.push(HashFieldReadV3 {
                field: decoded.child_identity.to_vec(),
                value: entry.value,
                expires_at_micros: entry.expires_at_micros,
            });
        }
    }
    if u64::try_from(fields.len()).map_err(|_| NativeRuntimeError::InvalidStructureTree)?
        != state.field_count
        || observed_expiries != state.field_expiry_count
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(Some(fields))
}

fn delete_hash_v3_in_tree(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    key: &[u8],
) -> Result<(BTree, Vec<u8>, usize), NativeRuntimeError> {
    let metadata_key = structure_hash_meta_key(key);
    let metadata = tree
        .get(pages, &metadata_key)?
        .ok_or(NativeRuntimeError::UnknownStructureHash)?;
    let state =
        decode_live_hash_metadata_v3(&metadata)?.ok_or(NativeRuntimeError::UnknownStructureHash)?;
    let retirement_key = encode_retirement_key(key, state.incarnation).map_err(map_codec_error)?;
    if tree.get(pages, &retirement_key)?.is_some() {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let retirement = RetirementRecordV3::new(
        StructureCollectionFamily::Hash,
        state.field_count,
        state.field_count,
        0,
        state.field_expiry_count,
        0,
    )
    .and_then(|record| encode_retirement_record(&record))
    .map_err(map_codec_error)?;
    let tombstone = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Tombstone {
        family: StructureCollectionFamily::Hash,
        retired_incarnation: state.incarnation,
    })
    .map_err(map_codec_error)?;
    let mut entries = vec![
        (metadata_key, tombstone),
        (retirement_key.clone(), retirement),
    ];
    if let Some(expiry) = state.expires_at_micros {
        let expiry_key = encode_collection_expiry_key(
            crate::STRUCTURE_EXPIRY_PREFIX,
            expiry,
            key,
            state.incarnation,
            &[],
        )
        .map_err(map_codec_error)?;
        if tree.get(pages, &expiry_key)?.as_deref() != Some(&[STRUCTURE_HASH_EXPIRY_LIVE]) {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        entries.push((expiry_key, vec![STRUCTURE_EXPIRY_TOMBSTONE]));
    }
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let physical_mutations = entries.len();
    Ok((
        tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree,
        retirement_key,
        physical_mutations,
    ))
}

fn cleanup_hash_retirement_v3_step(
    pages: &mut PageStore,
    pool: &BufferPool,
    tree: BTree,
    creating_csn: Csn,
    retirement_key: &[u8],
    entry_budget: usize,
) -> Result<(BTree, RetirementStepV3, usize), NativeRuntimeError> {
    if entry_budget == 0 || entry_budget > MAX_STRUCTURE_RETIREMENT_STEP_ENTRIES {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    let encoded_record = tree
        .get(pages, retirement_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    if is_structure_tombstone(&encoded_record) {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    let record = decode_retirement_record(&encoded_record).map_err(map_codec_error)?;
    if record.family != StructureCollectionFamily::Hash {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let (key, incarnation) = decode_retirement_key(retirement_key).map_err(map_codec_error)?;
    let prefix = v3_hash_field_prefix(key, incarnation)?;
    let mut reached = Vec::<(Vec<u8>, Vec<u8>)>::new();
    let outcome = tree.visit_prefix_cached(
        pages,
        pool,
        &prefix,
        record.exclusive_cursor.as_deref(),
        |physical_key, value| {
            reached.push((physical_key.to_vec(), value.to_vec()));
            if reached.len() == entry_budget {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        },
    )?;
    let scan_exhausted = matches!(outcome, ControlFlow::Continue(()));
    let candidates = reached
        .iter()
        .map(|(physical_key, value)| {
            let live = !is_structure_tombstone(value);
            let expiry = live
                .then(|| structure_value_expiry(value))
                .transpose()?
                .flatten();
            Ok(RetirementCandidateV3 {
                physical_key,
                live,
                logical_items: u64::from(live),
                logical_bytes: 0,
                associated_secondary_entries: 0,
                associated_expiry_entries: u64::from(expiry.is_some()),
            })
        })
        .collect::<Result<Vec<_>, NativeRuntimeError>>()?;
    let step = advance_retirement_record(
        retirement_key,
        &record,
        &candidates,
        entry_budget,
        scan_exhausted,
    )
    .map_err(map_codec_error)?;
    let mut entries = Vec::new();
    for (physical_key, value) in &reached {
        if is_structure_tombstone(value) {
            continue;
        }
        entries.push((physical_key.clone(), structure_tombstone_value()));
        if let Some(expiry) = structure_value_expiry(value)? {
            let decoded = decode_collection_child_key(physical_key).map_err(map_codec_error)?;
            let expiry_key = encode_collection_expiry_key(
                crate::STRUCTURE_HASH_FIELD_EXPIRY_PREFIX,
                expiry,
                key,
                incarnation,
                decoded.child_identity,
            )
            .map_err(map_codec_error)?;
            if tree.get(pages, &expiry_key)?.as_deref() != Some(&[STRUCTURE_HASH_FIELD_EXPIRY_LIVE])
            {
                return Err(NativeRuntimeError::InvalidStructureTree);
            }
            entries.push((expiry_key, vec![STRUCTURE_EXPIRY_TOMBSTONE]));
        }
    }
    let retirement_value = if step.more_remaining {
        encode_retirement_record(&step.record).map_err(map_codec_error)?
    } else {
        structure_tombstone_value()
    };
    entries.push((retirement_key.to_vec(), retirement_value));
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let physical_mutations = entries.len();
    Ok((
        tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree,
        step,
        physical_mutations,
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ListMetadataStateV3 {
    incarnation: StructureIncarnation,
    element_count: u64,
    logical_value_bytes: u64,
    head_chunk: i64,
    tail_chunk: i64,
    expires_at_micros: Option<i64>,
}

fn decode_live_list_metadata_v3(
    encoded: &[u8],
) -> Result<Option<ListMetadataStateV3>, NativeRuntimeError> {
    match decode_typed_collection_metadata(encoded).map_err(map_codec_error)? {
        TypedCollectionMetadataV3::Live {
            incarnation,
            state:
                CollectionStateV3::List {
                    element_count,
                    logical_value_bytes,
                    head_chunk,
                    tail_chunk,
                    expires_at_micros,
                },
        } => Ok(Some(ListMetadataStateV3 {
            incarnation,
            element_count,
            logical_value_bytes,
            head_chunk,
            tail_chunk,
            expires_at_micros,
        })),
        TypedCollectionMetadataV3::Tombstone {
            family: StructureCollectionFamily::List,
            ..
        } => Ok(None),
        _ => Err(NativeRuntimeError::InvalidStructureTree),
    }
}

fn v3_list_chunk_prefix(
    key: &[u8],
    incarnation: StructureIncarnation,
) -> Result<Vec<u8>, NativeRuntimeError> {
    encode_collection_child_key(STRUCTURE_LIST_CHUNK_PREFIX, key, incarnation, &[])
        .map_err(map_codec_error)
}

fn create_list_v3_in_tree(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    key: &[u8],
    incarnation: StructureIncarnation,
) -> Result<BTree, NativeRuntimeError> {
    let metadata_key = structure_list_meta_key(key)?;
    if let Some(encoded) = tree.get(pages, &metadata_key)?
        && decode_live_list_metadata_v3(&encoded)?.is_some()
    {
        return Err(NativeRuntimeError::StructureKeyExists);
    }
    let metadata = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
        incarnation,
        state: CollectionStateV3::List {
            element_count: 0,
            logical_value_bytes: 0,
            head_chunk: 0,
            tail_chunk: 0,
            expires_at_micros: None,
        },
    })
    .map_err(map_codec_error)?;
    Ok(tree
        .upsert(pages, creating_csn, metadata_key, metadata)?
        .tree)
}

fn append_list_chunk_v3_in_tree<Value>(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    key: &[u8],
    values: &[Value],
    blob_references: &BTreeMap<[u8; 32], BlobReference>,
) -> Result<BTree, NativeRuntimeError>
where
    Value: AsRef<[u8]>,
{
    if values.is_empty() {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    let metadata_key = structure_list_meta_key(key)?;
    let metadata = tree
        .get(pages, &metadata_key)?
        .ok_or(NativeRuntimeError::UnknownStructureList)?;
    let state =
        decode_live_list_metadata_v3(&metadata)?.ok_or(NativeRuntimeError::UnknownStructureList)?;
    let chunk_id = if state.element_count == 0 {
        0
    } else {
        state
            .tail_chunk
            .checked_add(1)
            .ok_or(NativeRuntimeError::InvalidStructureTree)?
    };
    let stored_values = values
        .iter()
        .map(|value| structure_storage_value(value.as_ref(), None, blob_references))
        .collect::<Result<Vec<_>, NativeRuntimeError>>()?;
    let chunk_value = encode_list_chunk_storage(&stored_values)?;
    let logical_items =
        u64::try_from(values.len()).map_err(|_| NativeRuntimeError::InvalidPreparedMutation)?;
    let logical_bytes = values.iter().try_fold(0_u64, |total, value| {
        total
            .checked_add(
                u64::try_from(value.as_ref().len())
                    .map_err(|_| NativeRuntimeError::InvalidPreparedMutation)?,
            )
            .ok_or(NativeRuntimeError::InvalidStructureTree)
    })?;
    let element_count = state
        .element_count
        .checked_add(logical_items)
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let logical_value_bytes = state
        .logical_value_bytes
        .checked_add(logical_bytes)
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let head_chunk = if state.element_count == 0 {
        chunk_id
    } else {
        state.head_chunk
    };
    let metadata = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
        incarnation: state.incarnation,
        state: CollectionStateV3::List {
            element_count,
            logical_value_bytes,
            head_chunk,
            tail_chunk: chunk_id,
            expires_at_micros: state.expires_at_micros,
        },
    })
    .map_err(map_codec_error)?;
    let chunk_key = encode_collection_child_key(
        STRUCTURE_LIST_CHUNK_PREFIX,
        key,
        state.incarnation,
        &encode_list_chunk_identity_v3(chunk_id),
    )
    .map_err(map_codec_error)?;
    if tree
        .get(pages, &chunk_key)?
        .is_some_and(|encoded| !is_structure_tombstone(&encoded))
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let mut entries = vec![(metadata_key, metadata), (chunk_key, chunk_value)];
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    Ok(tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree)
}

fn push_list_value_v3_in_tree(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    key: &[u8],
    value: &[u8],
    at_head: bool,
    blob_references: &BTreeMap<[u8; 32], BlobReference>,
) -> Result<BTree, NativeRuntimeError> {
    if !at_head {
        return append_list_chunk_v3_in_tree(
            pages,
            tree,
            creating_csn,
            key,
            std::slice::from_ref(&value),
            blob_references,
        );
    }
    let metadata_key = structure_list_meta_key(key)?;
    let metadata = tree
        .get(pages, &metadata_key)?
        .ok_or(NativeRuntimeError::UnknownStructureList)?;
    let state =
        decode_live_list_metadata_v3(&metadata)?.ok_or(NativeRuntimeError::UnknownStructureList)?;
    let chunk_id = if state.element_count == 0 {
        0
    } else {
        state
            .head_chunk
            .checked_sub(1)
            .ok_or(NativeRuntimeError::StructureListIndexExhausted)?
    };
    let chunk_key = encode_collection_child_key(
        STRUCTURE_LIST_CHUNK_PREFIX,
        key,
        state.incarnation,
        &encode_list_chunk_identity_v3(chunk_id),
    )
    .map_err(map_codec_error)?;
    if tree
        .get(pages, &chunk_key)?
        .is_some_and(|encoded| !is_structure_tombstone(&encoded))
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let stored = structure_storage_value(value, None, blob_references)?;
    let element_count = state
        .element_count
        .checked_add(1)
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let logical_value_bytes = state
        .logical_value_bytes
        .checked_add(
            u64::try_from(value.len()).map_err(|_| NativeRuntimeError::InvalidPreparedMutation)?,
        )
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let tail_chunk = if state.element_count == 0 {
        chunk_id
    } else {
        state.tail_chunk
    };
    let metadata = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
        incarnation: state.incarnation,
        state: CollectionStateV3::List {
            element_count,
            logical_value_bytes,
            head_chunk: chunk_id,
            tail_chunk,
            expires_at_micros: state.expires_at_micros,
        },
    })
    .map_err(map_codec_error)?;
    let mut entries = vec![
        (metadata_key, metadata),
        (chunk_key, encode_list_chunk_storage(&[stored])?),
    ];
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    Ok(tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree)
}

fn pop_list_value_v3_in_tree(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    key: &[u8],
    expected_value: &[u8],
    at_head: bool,
    blob_references: &BTreeMap<[u8; 32], BlobReference>,
) -> Result<BTree, NativeRuntimeError> {
    let metadata_key = structure_list_meta_key(key)?;
    let metadata = tree
        .get(pages, &metadata_key)?
        .ok_or(NativeRuntimeError::UnknownStructureList)?;
    let state =
        decode_live_list_metadata_v3(&metadata)?.ok_or(NativeRuntimeError::UnknownStructureList)?;
    if state.element_count == 0 {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let boundary = if at_head {
        state.head_chunk
    } else {
        state.tail_chunk
    };
    let chunk_key = encode_collection_child_key(
        STRUCTURE_LIST_CHUNK_PREFIX,
        key,
        state.incarnation,
        &encode_list_chunk_identity_v3(boundary),
    )
    .map_err(map_codec_error)?;
    let encoded_chunk = tree
        .get(pages, &chunk_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let mut elements = decode_list_chunk_storage(&encoded_chunk)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let removed = if at_head {
        if elements.is_empty() {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        elements.remove(0)
    } else {
        elements
            .pop()
            .ok_or(NativeRuntimeError::InvalidStructureTree)?
    };
    if removed != structure_storage_value(expected_value, None, blob_references)? {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let element_count = state
        .element_count
        .checked_sub(1)
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let logical_value_bytes = state
        .logical_value_bytes
        .checked_sub(
            u64::try_from(expected_value.len())
                .map_err(|_| NativeRuntimeError::InvalidPreparedMutation)?,
        )
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let (head_chunk, tail_chunk, chunk_value) = if elements.is_empty() {
        if element_count == 0 {
            (0, 0, structure_tombstone_value())
        } else if at_head {
            (
                state
                    .head_chunk
                    .checked_add(1)
                    .ok_or(NativeRuntimeError::StructureListIndexExhausted)?,
                state.tail_chunk,
                structure_tombstone_value(),
            )
        } else {
            (
                state.head_chunk,
                state
                    .tail_chunk
                    .checked_sub(1)
                    .ok_or(NativeRuntimeError::StructureListIndexExhausted)?,
                structure_tombstone_value(),
            )
        }
    } else {
        (
            state.head_chunk,
            state.tail_chunk,
            encode_list_chunk_storage(&elements)?,
        )
    };
    let metadata = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
        incarnation: state.incarnation,
        state: CollectionStateV3::List {
            element_count,
            logical_value_bytes,
            head_chunk,
            tail_chunk,
            expires_at_micros: state.expires_at_micros,
        },
    })
    .map_err(map_codec_error)?;
    let mut entries = vec![(metadata_key, metadata), (chunk_key, chunk_value)];
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    Ok(tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree)
}

fn list_chunk_summary_v3(encoded: &[u8]) -> Result<Option<(u64, u64)>, NativeRuntimeError> {
    let Some(elements) = decode_list_chunk_storage(encoded)? else {
        return Ok(None);
    };
    let logical_items =
        u64::try_from(elements.len()).map_err(|_| NativeRuntimeError::InvalidStructureTree)?;
    let logical_bytes = elements.iter().try_fold(0_u64, |total, element| {
        if structure_value_expiry(element)?.is_some() {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        let payload = element
            .get(crate::STRUCTURE_VALUE_HEADER_SIZE..)
            .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        let value_bytes = match element[9] {
            crate::STRUCTURE_VALUE_INLINE => u64::try_from(payload.len())
                .map_err(|_| NativeRuntimeError::InvalidStructureTree)?,
            crate::STRUCTURE_VALUE_BLOB => BlobReference::decode(payload)?.logical_length,
            _ => return Err(NativeRuntimeError::InvalidStructureTree),
        };
        total
            .checked_add(value_bytes)
            .ok_or(NativeRuntimeError::InvalidStructureTree)
    })?;
    Ok(Some((logical_items, logical_bytes)))
}

fn read_list_values_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    tree: BTree,
    key: &[u8],
) -> Result<Option<Vec<Vec<u8>>>, NativeRuntimeError> {
    let metadata = tree
        .get(pages, &structure_list_meta_key(key)?)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let Some(state) = decode_live_list_metadata_v3(&metadata)? else {
        return Ok(None);
    };
    let prefix = v3_list_chunk_prefix(key, state.incarnation)?;
    let mut expected_chunk = state.head_chunk;
    let mut observed_tail = None;
    let mut logical_bytes = 0_u64;
    let mut values = Vec::new();
    for (physical_key, encoded) in tree.scan_prefix(pages, &prefix)? {
        let decoded = decode_collection_child_key(&physical_key).map_err(map_codec_error)?;
        if decoded.collection_key != key || decoded.incarnation != state.incarnation {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        let Some(elements) = decode_list_chunk_storage(&encoded)? else {
            continue;
        };
        let chunk_id =
            decode_list_chunk_identity_v3(decoded.child_identity).map_err(map_codec_error)?;
        if chunk_id != expected_chunk {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        for element in elements {
            let entry = decode_structure_value(&element, blobs)?
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
            if entry.expires_at_micros.is_some() {
                return Err(NativeRuntimeError::InvalidStructureTree);
            }
            logical_bytes = logical_bytes
                .checked_add(
                    u64::try_from(entry.value.len())
                        .map_err(|_| NativeRuntimeError::InvalidStructureTree)?,
                )
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
            values.push(entry.value);
        }
        observed_tail = Some(chunk_id);
        if chunk_id != state.tail_chunk {
            expected_chunk = expected_chunk
                .checked_add(1)
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        }
    }
    if u64::try_from(values.len()).map_err(|_| NativeRuntimeError::InvalidStructureTree)?
        != state.element_count
        || logical_bytes != state.logical_value_bytes
        || (state.element_count != 0 && observed_tail != Some(state.tail_chunk))
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(Some(values))
}

fn delete_list_v3_in_tree(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    key: &[u8],
) -> Result<(BTree, Vec<u8>, usize), NativeRuntimeError> {
    let metadata_key = structure_list_meta_key(key)?;
    let metadata = tree
        .get(pages, &metadata_key)?
        .ok_or(NativeRuntimeError::UnknownStructureList)?;
    let state =
        decode_live_list_metadata_v3(&metadata)?.ok_or(NativeRuntimeError::UnknownStructureList)?;
    let retirement_key = encode_retirement_key(key, state.incarnation).map_err(map_codec_error)?;
    if tree.get(pages, &retirement_key)?.is_some() {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let retirement = RetirementRecordV3::new_list(
        state.element_count,
        state.logical_value_bytes,
        state.head_chunk,
        state.tail_chunk,
    )
    .and_then(|record| encode_retirement_record(&record))
    .map_err(map_codec_error)?;
    let tombstone = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Tombstone {
        family: StructureCollectionFamily::List,
        retired_incarnation: state.incarnation,
    })
    .map_err(map_codec_error)?;
    let mut entries = vec![
        (metadata_key, tombstone),
        (retirement_key.clone(), retirement),
    ];
    if let Some(expiry) = state.expires_at_micros {
        let expiry_key = encode_collection_expiry_key(
            crate::STRUCTURE_EXPIRY_PREFIX,
            expiry,
            key,
            state.incarnation,
            &[],
        )
        .map_err(map_codec_error)?;
        if tree.get(pages, &expiry_key)?.as_deref() != Some(&[STRUCTURE_LIST_EXPIRY_LIVE]) {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        entries.push((expiry_key, vec![STRUCTURE_EXPIRY_TOMBSTONE]));
    }
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let physical_mutations = entries.len();
    Ok((
        tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree,
        retirement_key,
        physical_mutations,
    ))
}

fn cleanup_list_retirement_v3_step(
    pages: &mut PageStore,
    pool: &BufferPool,
    tree: BTree,
    creating_csn: Csn,
    retirement_key: &[u8],
    entry_budget: usize,
) -> Result<(BTree, RetirementStepV3, usize), NativeRuntimeError> {
    if entry_budget == 0 || entry_budget > MAX_STRUCTURE_RETIREMENT_STEP_ENTRIES {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    let encoded_record = tree
        .get(pages, retirement_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    if is_structure_tombstone(&encoded_record) {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    let record = decode_retirement_record(&encoded_record).map_err(map_codec_error)?;
    if record.family != StructureCollectionFamily::List {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let (key, incarnation) = decode_retirement_key(retirement_key).map_err(map_codec_error)?;
    let prefix = v3_list_chunk_prefix(key, incarnation)?;
    let mut reached = Vec::<(Vec<u8>, Vec<u8>)>::new();
    let outcome = tree.visit_prefix_cached(
        pages,
        pool,
        &prefix,
        record.exclusive_cursor.as_deref(),
        |physical_key, value| {
            reached.push((physical_key.to_vec(), value.to_vec()));
            if reached.len() == entry_budget {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        },
    )?;
    let scan_exhausted = matches!(outcome, ControlFlow::Continue(()));
    let summaries = reached
        .iter()
        .map(|(_, value)| list_chunk_summary_v3(value))
        .collect::<Result<Vec<_>, NativeRuntimeError>>()?;
    let candidates = reached
        .iter()
        .zip(&summaries)
        .map(|((physical_key, _), summary)| RetirementCandidateV3 {
            physical_key,
            live: summary.is_some(),
            logical_items: summary.map_or(0, |summary| summary.0),
            logical_bytes: summary.map_or(0, |summary| summary.1),
            associated_secondary_entries: 0,
            associated_expiry_entries: 0,
        })
        .collect::<Vec<_>>();
    let step = advance_retirement_record(
        retirement_key,
        &record,
        &candidates,
        entry_budget,
        scan_exhausted,
    )
    .map_err(map_codec_error)?;
    let mut entries = reached
        .iter()
        .zip(&summaries)
        .filter(|(_, summary)| summary.is_some())
        .map(|((physical_key, _), _)| (physical_key.clone(), structure_tombstone_value()))
        .collect::<Vec<_>>();
    let retirement_value = if step.more_remaining {
        encode_retirement_record(&step.record).map_err(map_codec_error)?
    } else {
        structure_tombstone_value()
    };
    entries.push((retirement_key.to_vec(), retirement_value));
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let physical_mutations = entries.len();
    Ok((
        tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree,
        step,
        physical_mutations,
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StreamMetadataStateV3 {
    incarnation: StructureIncarnation,
    entry_count: u64,
    last_id: u64,
    expires_at_micros: Option<i64>,
}

fn decode_live_stream_metadata_v3(
    encoded: &[u8],
) -> Result<Option<StreamMetadataStateV3>, NativeRuntimeError> {
    match decode_typed_collection_metadata(encoded).map_err(map_codec_error)? {
        TypedCollectionMetadataV3::Live {
            incarnation,
            state:
                CollectionStateV3::Stream {
                    entry_count,
                    last_id,
                    expires_at_micros,
                },
        } => Ok(Some(StreamMetadataStateV3 {
            incarnation,
            entry_count,
            last_id,
            expires_at_micros,
        })),
        TypedCollectionMetadataV3::Tombstone {
            family: StructureCollectionFamily::Stream,
            ..
        } => Ok(None),
        _ => Err(NativeRuntimeError::InvalidStructureTree),
    }
}

fn v3_stream_entry_prefix(
    key: &[u8],
    incarnation: StructureIncarnation,
) -> Result<Vec<u8>, NativeRuntimeError> {
    encode_collection_child_key(STRUCTURE_STREAM_ENTRY_PREFIX, key, incarnation, &[])
        .map_err(map_codec_error)
}

fn create_stream_v3_in_tree(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    key: &[u8],
    incarnation: StructureIncarnation,
) -> Result<BTree, NativeRuntimeError> {
    let metadata_key = structure_stream_meta_key(key)?;
    if let Some(encoded) = tree.get(pages, &metadata_key)?
        && decode_live_stream_metadata_v3(&encoded)?.is_some()
    {
        return Err(NativeRuntimeError::StructureKeyExists);
    }
    let metadata = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
        incarnation,
        state: CollectionStateV3::Stream {
            entry_count: 0,
            last_id: 0,
            expires_at_micros: None,
        },
    })
    .map_err(map_codec_error)?;
    Ok(tree
        .upsert(pages, creating_csn, metadata_key, metadata)?
        .tree)
}

fn append_stream_entry_v3_in_tree(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    key: &[u8],
    id: u64,
    fields: &[(Vec<u8>, Vec<u8>)],
) -> Result<BTree, NativeRuntimeError> {
    let metadata_key = structure_stream_meta_key(key)?;
    let metadata = tree
        .get(pages, &metadata_key)?
        .ok_or(NativeRuntimeError::UnknownStructureStream)?;
    let state = decode_live_stream_metadata_v3(&metadata)?
        .ok_or(NativeRuntimeError::UnknownStructureStream)?;
    if id == 0 || id <= state.last_id {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    let entry_key = encode_collection_child_key(
        STRUCTURE_STREAM_ENTRY_PREFIX,
        key,
        state.incarnation,
        &id.to_be_bytes(),
    )
    .map_err(map_codec_error)?;
    if tree.get(pages, &entry_key)?.is_some() {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let entry_count = state
        .entry_count
        .checked_add(1)
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let metadata = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
        incarnation: state.incarnation,
        state: CollectionStateV3::Stream {
            entry_count,
            last_id: id,
            expires_at_micros: state.expires_at_micros,
        },
    })
    .map_err(map_codec_error)?;
    let mut entries = vec![
        (metadata_key, metadata),
        (entry_key, encode_stream_wal_entry(id, fields)?),
    ];
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    Ok(tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree)
}

fn read_stream_entries_v3(
    pages: &PageStore,
    tree: BTree,
    key: &[u8],
) -> Result<Option<Vec<(u64, crate::model::StreamFields)>>, NativeRuntimeError> {
    let metadata = tree
        .get(pages, &structure_stream_meta_key(key)?)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let Some(state) = decode_live_stream_metadata_v3(&metadata)? else {
        return Ok(None);
    };
    let prefix = v3_stream_entry_prefix(key, state.incarnation)?;
    let mut entries = Vec::new();
    for (physical_key, encoded) in tree.scan_prefix(pages, &prefix)? {
        let decoded = decode_collection_child_key(&physical_key).map_err(map_codec_error)?;
        if decoded.collection_key != key || decoded.incarnation != state.incarnation {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        if is_structure_tombstone(&encoded) {
            continue;
        }
        let physical_id = decode_u64(decoded.child_identity).map_err(map_codec_error)?;
        let (payload_id, fields) = decode_stream_wal_entry(&encoded)
            .map_err(|_| NativeRuntimeError::InvalidStructureTree)?;
        if physical_id == 0 || physical_id != payload_id || physical_id > state.last_id {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        entries.push((physical_id, fields));
    }
    if u64::try_from(entries.len()).map_err(|_| NativeRuntimeError::InvalidStructureTree)?
        != state.entry_count
        || entries.last().map_or(0, |entry| entry.0) != state.last_id
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(Some(entries))
}

fn delete_stream_v3_in_tree(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    key: &[u8],
) -> Result<(BTree, Vec<u8>, usize), NativeRuntimeError> {
    let metadata_key = structure_stream_meta_key(key)?;
    let metadata = tree
        .get(pages, &metadata_key)?
        .ok_or(NativeRuntimeError::UnknownStructureStream)?;
    let state = decode_live_stream_metadata_v3(&metadata)?
        .ok_or(NativeRuntimeError::UnknownStructureStream)?;
    let retirement_key = encode_retirement_key(key, state.incarnation).map_err(map_codec_error)?;
    if tree.get(pages, &retirement_key)?.is_some() {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let retirement = RetirementRecordV3::new_stream(state.entry_count, state.last_id)
        .and_then(|record| encode_retirement_record(&record))
        .map_err(map_codec_error)?;
    let tombstone = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Tombstone {
        family: StructureCollectionFamily::Stream,
        retired_incarnation: state.incarnation,
    })
    .map_err(map_codec_error)?;
    let mut entries = vec![
        (metadata_key, tombstone),
        (retirement_key.clone(), retirement),
    ];
    if let Some(expiry) = state.expires_at_micros {
        let expiry_key = encode_collection_expiry_key(
            crate::STRUCTURE_EXPIRY_PREFIX,
            expiry,
            key,
            state.incarnation,
            &[],
        )
        .map_err(map_codec_error)?;
        if tree.get(pages, &expiry_key)?.as_deref() != Some(&[STRUCTURE_STREAM_EXPIRY_LIVE]) {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        entries.push((expiry_key, vec![STRUCTURE_EXPIRY_TOMBSTONE]));
    }
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let physical_mutations = entries.len();
    Ok((
        tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree,
        retirement_key,
        physical_mutations,
    ))
}

fn cleanup_stream_retirement_v3_step(
    pages: &mut PageStore,
    pool: &BufferPool,
    tree: BTree,
    creating_csn: Csn,
    retirement_key: &[u8],
    entry_budget: usize,
) -> Result<(BTree, RetirementStepV3, usize), NativeRuntimeError> {
    if entry_budget == 0 || entry_budget > MAX_STRUCTURE_RETIREMENT_STEP_ENTRIES {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    let encoded_record = tree
        .get(pages, retirement_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    if is_structure_tombstone(&encoded_record) {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    let record = decode_retirement_record(&encoded_record).map_err(map_codec_error)?;
    if record.family != StructureCollectionFamily::Stream {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let (key, incarnation) = decode_retirement_key(retirement_key).map_err(map_codec_error)?;
    let prefix = v3_stream_entry_prefix(key, incarnation)?;
    let mut reached = Vec::<(Vec<u8>, Vec<u8>)>::new();
    let outcome = tree.visit_prefix_cached(
        pages,
        pool,
        &prefix,
        record.exclusive_cursor.as_deref(),
        |physical_key, value| {
            reached.push((physical_key.to_vec(), value.to_vec()));
            if reached.len() == entry_budget {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        },
    )?;
    let scan_exhausted = matches!(outcome, ControlFlow::Continue(()));
    let candidates = reached
        .iter()
        .map(|(physical_key, encoded)| {
            let live = !is_structure_tombstone(encoded);
            if live {
                let decoded = decode_collection_child_key(physical_key).map_err(map_codec_error)?;
                let physical_id = decode_u64(decoded.child_identity).map_err(map_codec_error)?;
                let (payload_id, _) = decode_stream_wal_entry(encoded)
                    .map_err(|_| NativeRuntimeError::InvalidStructureTree)?;
                if physical_id != payload_id {
                    return Err(NativeRuntimeError::InvalidStructureTree);
                }
            }
            Ok(RetirementCandidateV3 {
                physical_key,
                live,
                logical_items: u64::from(live),
                logical_bytes: 0,
                associated_secondary_entries: 0,
                associated_expiry_entries: 0,
            })
        })
        .collect::<Result<Vec<_>, NativeRuntimeError>>()?;
    let step = advance_retirement_record(
        retirement_key,
        &record,
        &candidates,
        entry_budget,
        scan_exhausted,
    )
    .map_err(map_codec_error)?;
    let mut entries = reached
        .iter()
        .filter(|(_, encoded)| !is_structure_tombstone(encoded))
        .map(|(physical_key, _)| (physical_key.clone(), structure_tombstone_value()))
        .collect::<Vec<_>>();
    let retirement_value = if step.more_remaining {
        encode_retirement_record(&step.record).map_err(map_codec_error)?
    } else {
        structure_tombstone_value()
    };
    entries.push((retirement_key.to_vec(), retirement_value));
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let physical_mutations = entries.len();
    Ok((
        tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree,
        step,
        physical_mutations,
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SortedSetMetadataStateV3 {
    incarnation: StructureIncarnation,
    member_count: u64,
    expires_at_micros: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SortedSetMemberV3 {
    member: Vec<u8>,
    score: SortedSetScore,
}

fn decode_live_sorted_set_metadata_v3(
    encoded: &[u8],
) -> Result<Option<SortedSetMetadataStateV3>, NativeRuntimeError> {
    match decode_typed_collection_metadata(encoded).map_err(map_codec_error)? {
        TypedCollectionMetadataV3::Live {
            incarnation,
            state:
                CollectionStateV3::SortedSet {
                    member_count,
                    expires_at_micros,
                },
        } => Ok(Some(SortedSetMetadataStateV3 {
            incarnation,
            member_count,
            expires_at_micros,
        })),
        TypedCollectionMetadataV3::Tombstone {
            family: StructureCollectionFamily::SortedSet,
            ..
        } => Ok(None),
        _ => Err(NativeRuntimeError::InvalidStructureTree),
    }
}

fn v3_sorted_set_member_prefix(
    key: &[u8],
    incarnation: StructureIncarnation,
) -> Result<Vec<u8>, NativeRuntimeError> {
    encode_collection_child_key(STRUCTURE_SORTED_SET_MEMBER_PREFIX, key, incarnation, &[])
        .map_err(map_codec_error)
}

fn v3_sorted_set_order_key(
    key: &[u8],
    incarnation: StructureIncarnation,
    score: SortedSetScore,
    member: &[u8],
) -> Result<Vec<u8>, NativeRuntimeError> {
    let mut identity = Vec::with_capacity(8_usize.saturating_add(member.len()));
    identity.extend_from_slice(&score.sortable_bits().to_be_bytes());
    identity.extend_from_slice(member);
    encode_collection_child_key(
        STRUCTURE_SORTED_SET_ORDER_PREFIX,
        key,
        incarnation,
        &identity,
    )
    .map_err(map_codec_error)
}

fn create_sorted_set_v3_in_tree(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    key: &[u8],
    incarnation: StructureIncarnation,
) -> Result<BTree, NativeRuntimeError> {
    let metadata_key = structure_sorted_set_meta_key(key)?;
    if let Some(encoded) = tree.get(pages, &metadata_key)?
        && decode_live_sorted_set_metadata_v3(&encoded)?.is_some()
    {
        return Err(NativeRuntimeError::StructureKeyExists);
    }
    let metadata = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
        incarnation,
        state: CollectionStateV3::SortedSet {
            member_count: 0,
            expires_at_micros: None,
        },
    })
    .map_err(map_codec_error)?;
    Ok(tree
        .upsert(pages, creating_csn, metadata_key, metadata)?
        .tree)
}

fn upsert_sorted_set_member_v3_in_tree(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    key: &[u8],
    member: &[u8],
    score: SortedSetScore,
) -> Result<(BTree, bool), NativeRuntimeError> {
    let metadata_key = structure_sorted_set_meta_key(key)?;
    let metadata = tree
        .get(pages, &metadata_key)?
        .ok_or(NativeRuntimeError::UnknownStructureSortedSet)?;
    let state = decode_live_sorted_set_metadata_v3(&metadata)?
        .ok_or(NativeRuntimeError::UnknownStructureSortedSet)?;
    let member_key = encode_collection_child_key(
        STRUCTURE_SORTED_SET_MEMBER_PREFIX,
        key,
        state.incarnation,
        member,
    )
    .map_err(map_codec_error)?;
    let previous = tree
        .get(pages, &member_key)?
        .map(|encoded| decode_sorted_set_score(&encoded))
        .transpose()?
        .flatten();
    if previous == Some(score) {
        return Ok((tree, false));
    }
    let inserted = previous.is_none();
    let member_count = if inserted {
        state
            .member_count
            .checked_add(1)
            .ok_or(NativeRuntimeError::InvalidStructureTree)?
    } else {
        state.member_count
    };
    let metadata = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
        incarnation: state.incarnation,
        state: CollectionStateV3::SortedSet {
            member_count,
            expires_at_micros: state.expires_at_micros,
        },
    })
    .map_err(map_codec_error)?;
    let order_key = v3_sorted_set_order_key(key, state.incarnation, score, member)?;
    if let Some(encoded) = tree.get(pages, &order_key)?
        && decode_set_member_value(&encoded)?
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let mut entries = vec![
        (metadata_key, metadata),
        (member_key, encode_sorted_set_score(score)),
        (order_key, set_member_live_value()),
    ];
    if let Some(previous) = previous {
        let previous_order_key = v3_sorted_set_order_key(key, state.incarnation, previous, member)?;
        if tree.get(pages, &previous_order_key)?.as_deref() != Some(&set_member_live_value()) {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        entries.push((previous_order_key, structure_tombstone_value()));
    }
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok((
        tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree,
        inserted,
    ))
}

fn delete_sorted_set_member_v3_in_tree(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    key: &[u8],
    member: &[u8],
) -> Result<BTree, NativeRuntimeError> {
    let metadata_key = structure_sorted_set_meta_key(key)?;
    let metadata = tree
        .get(pages, &metadata_key)?
        .ok_or(NativeRuntimeError::UnknownStructureSortedSet)?;
    let state = decode_live_sorted_set_metadata_v3(&metadata)?
        .ok_or(NativeRuntimeError::UnknownStructureSortedSet)?;
    let member_key = encode_collection_child_key(
        STRUCTURE_SORTED_SET_MEMBER_PREFIX,
        key,
        state.incarnation,
        member,
    )
    .map_err(map_codec_error)?;
    let encoded_score = tree
        .get(pages, &member_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let score =
        decode_sorted_set_score(&encoded_score)?.ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let order_key = v3_sorted_set_order_key(key, state.incarnation, score, member)?;
    if tree.get(pages, &order_key)?.as_deref() != Some(&set_member_live_value()) {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let member_count = state
        .member_count
        .checked_sub(1)
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let metadata = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
        incarnation: state.incarnation,
        state: CollectionStateV3::SortedSet {
            member_count,
            expires_at_micros: state.expires_at_micros,
        },
    })
    .map_err(map_codec_error)?;
    let mut entries = vec![
        (metadata_key, metadata),
        (member_key, structure_tombstone_value()),
        (order_key, structure_tombstone_value()),
    ];
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    Ok(tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree)
}

fn read_sorted_set_members_v3(
    pages: &PageStore,
    tree: BTree,
    key: &[u8],
) -> Result<Option<Vec<SortedSetMemberV3>>, NativeRuntimeError> {
    let metadata = tree
        .get(pages, &structure_sorted_set_meta_key(key)?)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let Some(state) = decode_live_sorted_set_metadata_v3(&metadata)? else {
        return Ok(None);
    };
    let prefix = v3_sorted_set_member_prefix(key, state.incarnation)?;
    let mut members = Vec::new();
    for (physical_key, encoded) in tree.scan_prefix(pages, &prefix)? {
        let decoded = decode_collection_child_key(&physical_key).map_err(map_codec_error)?;
        if decoded.collection_key != key || decoded.incarnation != state.incarnation {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        let Some(score) = decode_sorted_set_score(&encoded)? else {
            continue;
        };
        let order_key =
            v3_sorted_set_order_key(key, state.incarnation, score, decoded.child_identity)?;
        if tree.get(pages, &order_key)?.as_deref() != Some(&set_member_live_value()) {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        members.push(SortedSetMemberV3 {
            member: decoded.child_identity.to_vec(),
            score,
        });
    }
    if u64::try_from(members.len()).map_err(|_| NativeRuntimeError::InvalidStructureTree)?
        != state.member_count
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(Some(members))
}

fn delete_sorted_set_v3_in_tree(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    key: &[u8],
) -> Result<(BTree, Vec<u8>, usize), NativeRuntimeError> {
    let metadata_key = structure_sorted_set_meta_key(key)?;
    let metadata = tree
        .get(pages, &metadata_key)?
        .ok_or(NativeRuntimeError::UnknownStructureSortedSet)?;
    let state = decode_live_sorted_set_metadata_v3(&metadata)?
        .ok_or(NativeRuntimeError::UnknownStructureSortedSet)?;
    let retirement_key = encode_retirement_key(key, state.incarnation).map_err(map_codec_error)?;
    if tree.get(pages, &retirement_key)?.is_some() {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let retirement = RetirementRecordV3::new(
        StructureCollectionFamily::SortedSet,
        state.member_count,
        state.member_count,
        state.member_count,
        0,
        0,
    )
    .and_then(|record| encode_retirement_record(&record))
    .map_err(map_codec_error)?;
    let tombstone = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Tombstone {
        family: StructureCollectionFamily::SortedSet,
        retired_incarnation: state.incarnation,
    })
    .map_err(map_codec_error)?;
    let mut entries = vec![
        (metadata_key, tombstone),
        (retirement_key.clone(), retirement),
    ];
    if let Some(expiry) = state.expires_at_micros {
        let expiry_key = encode_collection_expiry_key(
            crate::STRUCTURE_EXPIRY_PREFIX,
            expiry,
            key,
            state.incarnation,
            &[],
        )
        .map_err(map_codec_error)?;
        if tree.get(pages, &expiry_key)?.as_deref() != Some(&[STRUCTURE_SORTED_SET_EXPIRY_LIVE]) {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        entries.push((expiry_key, vec![STRUCTURE_EXPIRY_TOMBSTONE]));
    }
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let physical_mutations = entries.len();
    Ok((
        tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree,
        retirement_key,
        physical_mutations,
    ))
}

fn cleanup_sorted_set_retirement_v3_step(
    pages: &mut PageStore,
    pool: &BufferPool,
    tree: BTree,
    creating_csn: Csn,
    retirement_key: &[u8],
    entry_budget: usize,
) -> Result<(BTree, RetirementStepV3, usize), NativeRuntimeError> {
    if !(2..=MAX_STRUCTURE_RETIREMENT_STEP_ENTRIES).contains(&entry_budget) {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    let encoded_record = tree
        .get(pages, retirement_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    if is_structure_tombstone(&encoded_record) {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    let record = decode_retirement_record(&encoded_record).map_err(map_codec_error)?;
    if record.family != StructureCollectionFamily::SortedSet {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let (key, incarnation) = decode_retirement_key(retirement_key).map_err(map_codec_error)?;
    let prefix = v3_sorted_set_member_prefix(key, incarnation)?;
    let candidate_budget = entry_budget / 2;
    let mut reached = Vec::<(Vec<u8>, Vec<u8>, Vec<u8>)>::new();
    let outcome = tree.visit_prefix_cached(
        pages,
        pool,
        &prefix,
        record.exclusive_cursor.as_deref(),
        |physical_key, value| {
            reached.push((physical_key.to_vec(), value.to_vec(), Vec::new()));
            if reached.len() == candidate_budget {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        },
    )?;
    let scan_exhausted = matches!(outcome, ControlFlow::Continue(()));
    for (physical_key, encoded, order_key) in &mut reached {
        let Some(score) = decode_sorted_set_score(encoded)? else {
            continue;
        };
        let decoded = decode_collection_child_key(physical_key).map_err(map_codec_error)?;
        *order_key = v3_sorted_set_order_key(key, incarnation, score, decoded.child_identity)?;
        if tree.get(pages, order_key)?.as_deref() != Some(&set_member_live_value()) {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
    }
    let candidates = reached
        .iter()
        .map(|(physical_key, encoded, _)| {
            let live = !is_structure_tombstone(encoded);
            RetirementCandidateV3 {
                physical_key,
                live,
                logical_items: u64::from(live),
                logical_bytes: 0,
                associated_secondary_entries: u64::from(live),
                associated_expiry_entries: 0,
            }
        })
        .collect::<Vec<_>>();
    let step = advance_retirement_record(
        retirement_key,
        &record,
        &candidates,
        candidate_budget,
        scan_exhausted,
    )
    .map_err(map_codec_error)?;
    let mut entries = Vec::new();
    for (physical_key, encoded, order_key) in &reached {
        if is_structure_tombstone(encoded) {
            continue;
        }
        entries.push((physical_key.clone(), structure_tombstone_value()));
        entries.push((order_key.clone(), structure_tombstone_value()));
    }
    let retirement_value = if step.more_remaining {
        encode_retirement_record(&step.record).map_err(map_codec_error)?
    } else {
        structure_tombstone_value()
    };
    entries.push((retirement_key.to_vec(), retirement_value));
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let physical_mutations = entries.len();
    Ok((
        tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree,
        step,
        physical_mutations,
    ))
}

fn v3_set_member_prefix(
    key: &[u8],
    incarnation: StructureIncarnation,
) -> Result<Vec<u8>, NativeRuntimeError> {
    encode_collection_child_key(STRUCTURE_SET_MEMBER_PREFIX, key, incarnation, &[])
        .map_err(map_codec_error)
}

fn decode_live_set_metadata_v3(
    encoded: &[u8],
) -> Result<Option<(StructureIncarnation, u64, Option<i64>)>, NativeRuntimeError> {
    match decode_typed_collection_metadata(encoded).map_err(map_codec_error)? {
        TypedCollectionMetadataV3::Live {
            incarnation,
            state:
                CollectionStateV3::Set {
                    member_count,
                    expires_at_micros,
                },
        } => Ok(Some((incarnation, member_count, expires_at_micros))),
        TypedCollectionMetadataV3::Tombstone {
            family: StructureCollectionFamily::Set,
            ..
        } => Ok(None),
        _ => Err(NativeRuntimeError::InvalidStructureTree),
    }
}

fn create_set_v3_in_tree(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    key: &[u8],
    incarnation: StructureIncarnation,
) -> Result<BTree, NativeRuntimeError> {
    let metadata_key = structure_set_meta_key(key);
    if let Some(encoded) = tree.get(pages, &metadata_key)?
        && decode_live_set_metadata_v3(&encoded)?.is_some()
    {
        return Err(NativeRuntimeError::StructureKeyExists);
    }
    let metadata = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
        incarnation,
        state: CollectionStateV3::Set {
            member_count: 0,
            expires_at_micros: None,
        },
    })
    .map_err(map_codec_error)?;
    Ok(tree
        .upsert(pages, creating_csn, metadata_key, metadata)?
        .tree)
}

fn add_set_member_v3_in_tree(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    key: &[u8],
    member: &[u8],
) -> Result<(BTree, bool), NativeRuntimeError> {
    let metadata_key = structure_set_meta_key(key);
    let metadata = tree
        .get(pages, &metadata_key)?
        .ok_or(NativeRuntimeError::UnknownStructureSet)?;
    let (incarnation, member_count, expires_at_micros) =
        decode_live_set_metadata_v3(&metadata)?.ok_or(NativeRuntimeError::UnknownStructureSet)?;
    let member_key =
        encode_collection_child_key(STRUCTURE_SET_MEMBER_PREFIX, key, incarnation, member)
            .map_err(map_codec_error)?;
    if let Some(encoded) = tree.get(pages, &member_key)?
        && decode_set_member_value(&encoded)?
    {
        return Ok((tree, false));
    }
    let member_count = member_count
        .checked_add(1)
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let metadata = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
        incarnation,
        state: CollectionStateV3::Set {
            member_count,
            expires_at_micros,
        },
    })
    .map_err(map_codec_error)?;
    let entries = vec![
        (metadata_key, metadata),
        (member_key, set_member_live_value()),
    ];
    Ok((
        tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree,
        true,
    ))
}

fn delete_set_member_v3_in_tree(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    key: &[u8],
    member: &[u8],
) -> Result<BTree, NativeRuntimeError> {
    let metadata_key = structure_set_meta_key(key);
    let metadata = tree
        .get(pages, &metadata_key)?
        .ok_or(NativeRuntimeError::UnknownStructureSet)?;
    let (incarnation, member_count, expires_at_micros) =
        decode_live_set_metadata_v3(&metadata)?.ok_or(NativeRuntimeError::UnknownStructureSet)?;
    let member_key =
        encode_collection_child_key(STRUCTURE_SET_MEMBER_PREFIX, key, incarnation, member)
            .map_err(map_codec_error)?;
    let encoded = tree
        .get(pages, &member_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    if !decode_set_member_value(&encoded)? {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let member_count = member_count
        .checked_sub(1)
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let metadata = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
        incarnation,
        state: CollectionStateV3::Set {
            member_count,
            expires_at_micros,
        },
    })
    .map_err(map_codec_error)?;
    let entries = vec![
        (metadata_key, metadata),
        (member_key, structure_tombstone_value()),
    ];
    Ok(tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree)
}

fn read_set_members_v3(
    pages: &PageStore,
    tree: BTree,
    key: &[u8],
) -> Result<Option<Vec<Vec<u8>>>, NativeRuntimeError> {
    let metadata = tree
        .get(pages, &structure_set_meta_key(key))?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let Some((incarnation, member_count, _)) = decode_live_set_metadata_v3(&metadata)? else {
        return Ok(None);
    };
    let prefix = v3_set_member_prefix(key, incarnation)?;
    let mut members = Vec::new();
    for (physical_key, value) in tree.scan_prefix(pages, &prefix)? {
        let decoded = decode_collection_child_key(&physical_key).map_err(map_codec_error)?;
        if decoded.collection_key != key || decoded.incarnation != incarnation {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        if decode_set_member_value(&value)? {
            members.push(decoded.child_identity.to_vec());
        }
    }
    if u64::try_from(members.len()).map_err(|_| NativeRuntimeError::InvalidStructureTree)?
        != member_count
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(Some(members))
}

fn delete_set_v3_in_tree(
    pages: &mut PageStore,
    tree: BTree,
    creating_csn: Csn,
    key: &[u8],
) -> Result<(BTree, Vec<u8>, usize), NativeRuntimeError> {
    let metadata_key = structure_set_meta_key(key);
    let metadata = tree
        .get(pages, &metadata_key)?
        .ok_or(NativeRuntimeError::UnknownStructureSet)?;
    let (incarnation, member_count, expires_at_micros) =
        decode_live_set_metadata_v3(&metadata)?.ok_or(NativeRuntimeError::UnknownStructureSet)?;
    let retirement_key = encode_retirement_key(key, incarnation).map_err(map_codec_error)?;
    if tree.get(pages, &retirement_key)?.is_some() {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let retirement = RetirementRecordV3::new(
        StructureCollectionFamily::Set,
        member_count,
        member_count,
        0,
        0,
        0,
    )
    .and_then(|record| encode_retirement_record(&record))
    .map_err(map_codec_error)?;
    let tombstone = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Tombstone {
        family: StructureCollectionFamily::Set,
        retired_incarnation: incarnation,
    })
    .map_err(map_codec_error)?;
    let mut entries = vec![
        (metadata_key, tombstone),
        (retirement_key.clone(), retirement),
    ];
    if let Some(expiry) = expires_at_micros {
        let expiry_key = encode_collection_expiry_key(
            crate::STRUCTURE_EXPIRY_PREFIX,
            expiry,
            key,
            incarnation,
            &[],
        )
        .map_err(map_codec_error)?;
        if tree.get(pages, &expiry_key)?.as_deref() != Some(&[STRUCTURE_SET_EXPIRY_LIVE]) {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        entries.push((expiry_key, vec![STRUCTURE_EXPIRY_TOMBSTONE]));
    }
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let physical_mutations = entries.len();
    Ok((
        tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree,
        retirement_key,
        physical_mutations,
    ))
}

fn cleanup_set_retirement_v3_step(
    pages: &mut PageStore,
    pool: &BufferPool,
    tree: BTree,
    creating_csn: Csn,
    retirement_key: &[u8],
    entry_budget: usize,
) -> Result<(BTree, RetirementStepV3, usize), NativeRuntimeError> {
    if entry_budget == 0 || entry_budget > MAX_STRUCTURE_RETIREMENT_STEP_ENTRIES {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    let encoded_record = tree
        .get(pages, retirement_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    if is_structure_tombstone(&encoded_record) {
        return Err(NativeRuntimeError::InvalidPreparedMutation);
    }
    let record = decode_retirement_record(&encoded_record).map_err(map_codec_error)?;
    if record.family != StructureCollectionFamily::Set {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let (key, incarnation) = decode_retirement_key(retirement_key).map_err(map_codec_error)?;
    let prefix = v3_set_member_prefix(key, incarnation)?;
    let mut reached = Vec::<(Vec<u8>, Vec<u8>)>::new();
    let outcome = tree.visit_prefix_cached(
        pages,
        pool,
        &prefix,
        record.exclusive_cursor.as_deref(),
        |physical_key, value| {
            reached.push((physical_key.to_vec(), value.to_vec()));
            if reached.len() == entry_budget {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        },
    )?;
    let scan_exhausted = matches!(outcome, ControlFlow::Continue(()));
    let candidates = reached
        .iter()
        .map(|(physical_key, value)| {
            let live = decode_set_member_value(value)?;
            Ok(RetirementCandidateV3 {
                physical_key,
                live,
                logical_items: u64::from(live),
                logical_bytes: 0,
                associated_secondary_entries: 0,
                associated_expiry_entries: 0,
            })
        })
        .collect::<Result<Vec<_>, NativeRuntimeError>>()?;
    let step = advance_retirement_record(
        retirement_key,
        &record,
        &candidates,
        entry_budget,
        scan_exhausted,
    )
    .map_err(map_codec_error)?;
    let mut entries = Vec::new();
    for (physical_key, value) in &reached {
        if decode_set_member_value(value)? {
            entries.push((physical_key.clone(), structure_tombstone_value()));
        }
    }
    let retirement_value = if step.more_remaining {
        encode_retirement_record(&step.record).map_err(map_codec_error)?
    } else {
        structure_tombstone_value()
    };
    entries.push((retirement_key.to_vec(), retirement_value));
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let physical_mutations = entries.len();
    Ok((
        tree.upsert_sorted_batch(pages, creating_csn, entries)?.tree,
        step,
        physical_mutations,
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LiveStructureKindV3 {
    Scalar,
    Collection,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DeltaScalarStateV3 {
    pub(super) scalar: Option<StructureEntry>,
    pub(super) collection: Option<CollectionStateV3>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DeltaScalarEnvelopeStateV3 {
    pub(super) scalar: Option<DeltaHashFieldValueV3>,
    pub(super) collection: Option<CollectionStateV3>,
}

pub(super) fn delta_scalar_envelope_latest_at_v3(
    pages: &PageStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
) -> Result<DeltaScalarEnvelopeStateV3, NativeRuntimeError> {
    let scalar = tree
        .get_cached_pinned(pages, pool, &structure_key(key))?
        .map(|encoded| decode_delta_hash_field_value_v3(encoded.bytes()))
        .transpose()?
        .flatten();
    let mut collection = None;
    for family in [
        StructureCollectionFamily::Hash,
        StructureCollectionFamily::Set,
        StructureCollectionFamily::List,
        StructureCollectionFamily::SortedSet,
        StructureCollectionFamily::Stream,
    ] {
        if let Some((_, state)) = live_collection_state_v3(pages, pool, tree, key, family)?
            && collection.replace(state).is_some()
        {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
    }
    if scalar.is_some() && collection.is_some() {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(DeltaScalarEnvelopeStateV3 { scalar, collection })
}

pub(super) fn delta_scalar_state_latest_at_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
) -> Result<DeltaScalarStateV3, NativeRuntimeError> {
    let envelope = delta_scalar_envelope_latest_at_v3(pages, pool, tree, key)?;
    let scalar = envelope
        .scalar
        .map(|scalar| decode_structure_value(&scalar.encoded, blobs))
        .transpose()?
        .flatten();
    Ok(DeltaScalarStateV3 {
        scalar,
        collection: envelope.collection,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DeltaHashFieldValueV3 {
    pub(super) encoded: Vec<u8>,
    pub(super) expires_at_micros: Option<i64>,
    pub(super) logical_value_bytes: u64,
    pub(super) blob_reference: Option<BlobReference>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DeltaHashFieldStateV3 {
    pub(super) incarnation: StructureIncarnation,
    pub(super) field_count: u64,
    pub(super) field_expiry_count: u64,
    pub(super) expires_at_micros: Option<i64>,
    pub(super) field: Option<DeltaHashFieldValueV3>,
}

fn exact_delta_hash_metadata_latest_at_v3(
    pages: &PageStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
) -> Result<Option<HashMetadataStateV3>, NativeRuntimeError> {
    let state = tree
        .get_cached_pinned(pages, pool, &structure_hash_meta_key(key))?
        .map(|encoded| decode_live_hash_metadata_v3(encoded.bytes()))
        .transpose()?
        .flatten();
    let other_kind_count = exact_non_hash_structure_kind_count_v3(pages, pool, tree, key)?;
    let Some(state) = state else {
        return match other_kind_count {
            0 => Ok(None),
            1 => Err(NativeRuntimeError::StructureKindMismatch),
            _ => Err(NativeRuntimeError::InvalidStructureTree),
        };
    };
    if other_kind_count != 0 {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    validate_collection_expiry_backlink_v3(
        pages,
        pool,
        tree,
        key,
        state.incarnation,
        state.expires_at_micros,
        STRUCTURE_HASH_EXPIRY_LIVE,
    )?;
    Ok(Some(state))
}

pub(super) fn delta_hash_ttl_latest_at_v3(
    pages: &PageStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    logical_time_micros: i64,
) -> Result<Ttl, NativeRuntimeError> {
    let Some(state) = exact_delta_hash_metadata_latest_at_v3(pages, pool, tree, key)? else {
        return Ok(Ttl::Missing);
    };
    Ok(match state.expires_at_micros {
        None => Ttl::Persistent,
        Some(expiry) if expiry > logical_time_micros => {
            Ttl::RemainingMicros(expiry.saturating_sub(logical_time_micros))
        }
        Some(_) => Ttl::Missing,
    })
}

/// Reads one hash metadata record and one incarnation-fenced field envelope.
///
/// Blob payloads remain unresolved so the caller can admit their declared
/// logical bytes before reading them. A due field is returned with its raw TTL
/// instead of being filtered; later delta semantics decide whether it is
/// visible, added, or eligible for TTL cleanup.
pub(super) fn delta_hash_field_state_latest_at_v3(
    pages: &PageStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    field_identity: &[u8],
) -> Result<Option<DeltaHashFieldStateV3>, NativeRuntimeError> {
    let Some(state) = exact_delta_hash_metadata_latest_at_v3(pages, pool, tree, key)? else {
        return Ok(None);
    };
    let field_key = encode_collection_child_key(
        STRUCTURE_HASH_FIELD_PREFIX,
        key,
        state.incarnation,
        field_identity,
    )
    .map_err(map_codec_error)?;
    let field = tree
        .get_cached_pinned(pages, pool, &field_key)?
        .map(|encoded| decode_delta_hash_field_value_v3(encoded.bytes()))
        .transpose()?
        .flatten();
    if field.is_some() && state.field_count == 0 {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    if state.field_count > 0
        && state.field_expiry_count == state.field_count
        && field
            .as_ref()
            .is_some_and(|field| field.expires_at_micros.is_none())
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    if let Some(field_expiry) = field.as_ref().and_then(|field| field.expires_at_micros) {
        if state.field_expiry_count == 0 {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        let expiry_key = encode_collection_expiry_key(
            crate::STRUCTURE_HASH_FIELD_EXPIRY_PREFIX,
            field_expiry,
            key,
            state.incarnation,
            field_identity,
        )
        .map_err(map_codec_error)?;
        if tree
            .get_cached_pinned(pages, pool, &expiry_key)?
            .is_none_or(|marker| marker.bytes() != [STRUCTURE_HASH_FIELD_EXPIRY_LIVE])
        {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
    }
    Ok(Some(DeltaHashFieldStateV3 {
        incarnation: state.incarnation,
        field_count: state.field_count,
        field_expiry_count: state.field_expiry_count,
        expires_at_micros: state.expires_at_micros,
        field,
    }))
}

fn exact_non_hash_structure_kind_count_v3(
    pages: &PageStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
) -> Result<usize, NativeRuntimeError> {
    let scalar = tree
        .get_cached_pinned(pages, pool, &structure_key(key))?
        .map(|encoded| decode_delta_hash_field_value_v3(encoded.bytes()))
        .transpose()?
        .flatten()
        .is_some();
    let mut count = usize::from(scalar);
    for family in [
        StructureCollectionFamily::Set,
        StructureCollectionFamily::List,
        StructureCollectionFamily::SortedSet,
        StructureCollectionFamily::Stream,
    ] {
        if live_collection_state_v3(pages, pool, tree, key, family)?.is_some() {
            count = count
                .checked_add(1)
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        }
    }
    Ok(count)
}

fn validate_collection_expiry_backlink_v3(
    pages: &PageStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    incarnation: StructureIncarnation,
    expires_at_micros: Option<i64>,
    marker: u8,
) -> Result<(), NativeRuntimeError> {
    let Some(expiry) = expires_at_micros else {
        return Ok(());
    };
    let expiry_key = encode_collection_expiry_key(
        crate::STRUCTURE_EXPIRY_PREFIX,
        expiry,
        key,
        incarnation,
        &[],
    )
    .map_err(map_codec_error)?;
    if tree
        .get_cached_pinned(pages, pool, &expiry_key)?
        .is_none_or(|encoded| encoded.bytes() != [marker])
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(())
}

fn decode_delta_hash_field_value_v3(
    encoded: &[u8],
) -> Result<Option<DeltaHashFieldValueV3>, NativeRuntimeError> {
    if is_structure_tombstone(encoded) {
        return Ok(None);
    }
    let expires_at_micros = structure_value_expiry(encoded)?;
    let payload = &encoded[crate::STRUCTURE_VALUE_HEADER_SIZE..];
    let (logical_value_bytes, blob_reference) = match encoded[9] {
        crate::STRUCTURE_VALUE_INLINE => (
            u64::try_from(payload.len()).map_err(|_| NativeRuntimeError::InvalidStructureTree)?,
            None,
        ),
        crate::STRUCTURE_VALUE_BLOB => {
            let reference = BlobReference::decode(payload)
                .map_err(|_| NativeRuntimeError::InvalidStructureTree)?;
            if reference.logical_length <= crate::STRUCTURE_INLINE_VALUE_LIMIT as u64
                || reference.logical_length > hyphae_native_blobs::MAX_BLOB_SIZE as u64
            {
                return Err(NativeRuntimeError::InvalidStructureTree);
            }
            (reference.logical_length, Some(reference))
        }
        _ => return Err(NativeRuntimeError::InvalidStructureTree),
    };
    Ok(Some(DeltaHashFieldValueV3 {
        encoded: encoded.to_vec(),
        expires_at_micros,
        logical_value_bytes,
        blob_reference,
    }))
}

fn live_collection_state_v3(
    pages: &PageStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    family: StructureCollectionFamily,
) -> Result<Option<(StructureIncarnation, CollectionStateV3)>, NativeRuntimeError> {
    let metadata_key = collection_metadata_key_v3(family, key)?;
    let Some(encoded) = tree.get_cached_pinned(pages, pool, &metadata_key)? else {
        return Ok(None);
    };
    match decode_typed_collection_metadata(encoded.bytes()).map_err(map_codec_error)? {
        TypedCollectionMetadataV3::Live { incarnation, state } if state.family() == family => {
            Ok(Some((incarnation, state)))
        }
        TypedCollectionMetadataV3::Tombstone {
            family: tombstone_family,
            ..
        } if tombstone_family == family => Ok(None),
        _ => Err(NativeRuntimeError::InvalidStructureTree),
    }
}

fn live_structure_kind_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    logical_time_micros: i64,
) -> Result<Option<LiveStructureKindV3>, NativeRuntimeError> {
    let mut live = None;
    if let Some(encoded) = tree.get_cached_pinned(pages, pool, &structure_key(key))?
        && decode_structure_value(encoded.bytes(), blobs)?.is_some_and(|entry| {
            entry
                .expires_at_micros
                .is_none_or(|expiry| expiry > logical_time_micros)
        })
    {
        live = Some(LiveStructureKindV3::Scalar);
    }
    for family in [
        StructureCollectionFamily::Hash,
        StructureCollectionFamily::Set,
        StructureCollectionFamily::List,
        StructureCollectionFamily::SortedSet,
        StructureCollectionFamily::Stream,
    ] {
        if let Some((_, state)) = live_collection_state_v3(pages, pool, tree, key, family)?
            && state
                .expires_at_micros()
                .is_none_or(|expiry| expiry > logical_time_micros)
        {
            if live.is_some() {
                return Err(NativeRuntimeError::InvalidStructureTree);
            }
            live = Some(LiveStructureKindV3::Collection);
        }
    }
    Ok(live)
}

const fn unknown_collection_error(family: StructureCollectionFamily) -> NativeRuntimeError {
    match family {
        StructureCollectionFamily::Hash => NativeRuntimeError::UnknownStructureHash,
        StructureCollectionFamily::Set => NativeRuntimeError::UnknownStructureSet,
        StructureCollectionFamily::List => NativeRuntimeError::UnknownStructureList,
        StructureCollectionFamily::SortedSet => NativeRuntimeError::UnknownStructureSortedSet,
        StructureCollectionFamily::Stream => NativeRuntimeError::UnknownStructureStream,
    }
}

fn visible_collection_state_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    family: StructureCollectionFamily,
    logical_time_micros: i64,
) -> Result<(StructureIncarnation, CollectionStateV3), NativeRuntimeError> {
    if let Some((incarnation, state)) = live_collection_state_v3(pages, pool, tree, key, family)? {
        if state
            .expires_at_micros()
            .is_none_or(|expiry| expiry > logical_time_micros)
        {
            return Ok((incarnation, state));
        }
        return Err(unknown_collection_error(family));
    }
    match live_structure_kind_v3(pages, blobs, pool, tree, key, logical_time_micros)? {
        Some(LiveStructureKindV3::Scalar | LiveStructureKindV3::Collection) => {
            Err(NativeRuntimeError::StructureKindMismatch)
        }
        None => Err(unknown_collection_error(family)),
    }
}

#[derive(Clone, Copy)]
struct HashFieldPointV3<'a> {
    key: &'a [u8],
    incarnation: StructureIncarnation,
    field: &'a [u8],
    logical_time_micros: i64,
}

fn hash_field_value_latest_at_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    point: HashFieldPointV3<'_>,
) -> Result<Option<Vec<u8>>, NativeRuntimeError> {
    let field_key = encode_collection_child_key(
        STRUCTURE_HASH_FIELD_PREFIX,
        point.key,
        point.incarnation,
        point.field,
    )
    .map_err(map_codec_error)?;
    tree.get_cached_pinned(pages, pool, &field_key)?
        .map(|encoded| {
            decode_structure_value(encoded.bytes(), blobs).map(|entry| {
                entry.and_then(|entry| {
                    entry
                        .expires_at_micros
                        .is_none_or(|expiry| expiry > point.logical_time_micros)
                        .then_some(entry.value)
                })
            })
        })
        .transpose()
        .map(Option::flatten)
}

pub(super) fn hash_get_latest_at_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    field: &[u8],
    logical_time_micros: i64,
) -> Result<Option<Vec<u8>>, NativeRuntimeError> {
    let (incarnation, state) = visible_collection_state_v3(
        pages,
        blobs,
        pool,
        tree,
        key,
        StructureCollectionFamily::Hash,
        logical_time_micros,
    )?;
    if !matches!(state, CollectionStateV3::Hash { .. }) {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    hash_field_value_latest_at_v3(
        pages,
        blobs,
        pool,
        tree,
        HashFieldPointV3 {
            key,
            incarnation,
            field,
            logical_time_micros,
        },
    )
}

pub(super) fn hash_get_many_latest_at_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    fields: &[Vec<u8>],
    logical_time_micros: i64,
) -> Result<Vec<Option<Vec<u8>>>, NativeRuntimeError> {
    let (incarnation, state) = visible_collection_state_v3(
        pages,
        blobs,
        pool,
        tree,
        key,
        StructureCollectionFamily::Hash,
        logical_time_micros,
    )?;
    if !matches!(state, CollectionStateV3::Hash { .. }) {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    fields
        .iter()
        .map(|field| {
            hash_field_value_latest_at_v3(
                pages,
                blobs,
                pool,
                tree,
                HashFieldPointV3 {
                    key,
                    incarnation,
                    field,
                    logical_time_micros,
                },
            )
        })
        .collect()
}

pub(super) fn hash_len_latest_at_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    logical_time_micros: i64,
) -> Result<usize, NativeRuntimeError> {
    let (incarnation, state) = visible_collection_state_v3(
        pages,
        blobs,
        pool,
        tree,
        key,
        StructureCollectionFamily::Hash,
        logical_time_micros,
    )?;
    let CollectionStateV3::Hash {
        field_count,
        field_expiry_count,
        ..
    } = state
    else {
        return Err(NativeRuntimeError::InvalidStructureTree);
    };
    if field_expiry_count == 0 {
        return usize::try_from(field_count).map_err(|_| NativeRuntimeError::InvalidStructureTree);
    }
    let prefix = v3_hash_field_prefix(key, incarnation)?;
    let mut physical_count = 0_u64;
    let mut observed_expiries = 0_u64;
    let mut visible_count = 0_usize;
    let mut failure = None;
    let outcome = tree.visit_prefix_range_cached(
        pages,
        pool,
        &prefix,
        std::ops::Bound::Unbounded,
        std::ops::Bound::Unbounded,
        |physical_key, encoded| {
            let result = (|| {
                let decoded = decode_collection_child_key(physical_key).map_err(map_codec_error)?;
                if decoded.collection_key != key || decoded.incarnation != incarnation {
                    return Err(NativeRuntimeError::InvalidStructureTree);
                }
                if is_structure_tombstone(encoded) {
                    return Ok(());
                }
                physical_count = physical_count
                    .checked_add(1)
                    .ok_or(NativeRuntimeError::InvalidStructureTree)?;
                let expiry = structure_value_expiry(encoded)?;
                observed_expiries = observed_expiries
                    .checked_add(u64::from(expiry.is_some()))
                    .ok_or(NativeRuntimeError::InvalidStructureTree)?;
                if expiry.is_none_or(|expiry| expiry > logical_time_micros) {
                    visible_count = visible_count
                        .checked_add(1)
                        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
                }
                Ok(())
            })();
            if let Err(error) = result {
                failure = Some(error);
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        },
    )?;
    if let Some(error) = failure {
        return Err(error);
    }
    if outcome == ControlFlow::Break(())
        || physical_count != field_count
        || observed_expiries != field_expiry_count
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(visible_count)
}

#[derive(Clone, Copy)]
pub(super) struct CollectionScanRequestV3<'a> {
    pub(super) key: &'a [u8],
    pub(super) start_after: Option<&'a [u8]>,
    pub(super) limit: usize,
    pub(super) logical_time_micros: i64,
}

impl<'a> CollectionScanRequestV3<'a> {
    pub(super) const fn new(
        key: &'a [u8],
        start_after: Option<&'a [u8]>,
        limit: usize,
        logical_time_micros: i64,
    ) -> Self {
        Self {
            key,
            start_after,
            limit,
            logical_time_micros,
        }
    }
}

pub(super) struct HashScanResultV3 {
    pub(super) entries: Vec<HashFieldEntry>,
    pub(super) declared_field_count: usize,
}

pub(super) struct SetScanResultV3 {
    pub(super) members: Vec<Vec<u8>>,
    pub(super) declared_member_count: usize,
}

fn current_child_identity_v3<'key>(
    physical_key: &'key [u8],
    expected_prefix: u8,
    expected_key: &[u8],
    expected_incarnation: StructureIncarnation,
) -> Result<&'key [u8], NativeRuntimeError> {
    let decoded = decode_collection_child_key(physical_key).map_err(map_codec_error)?;
    if decoded.prefix != expected_prefix
        || decoded.collection_key != expected_key
        || decoded.incarnation != expected_incarnation
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(decoded.child_identity)
}

fn hash_scan_entry_v3(
    physical_key: &[u8],
    encoded: &[u8],
    blobs: &BlobStore,
    request: CollectionScanRequestV3<'_>,
    incarnation: StructureIncarnation,
) -> Result<(bool, bool, Option<HashFieldEntry>), NativeRuntimeError> {
    let field = current_child_identity_v3(
        physical_key,
        STRUCTURE_HASH_FIELD_PREFIX,
        request.key,
        incarnation,
    )?;
    let Some(entry) = decode_structure_value(encoded, blobs)? else {
        return Ok((false, false, None));
    };
    let has_expiry = entry.expires_at_micros.is_some();
    let visible = entry
        .expires_at_micros
        .is_none_or(|expiry| expiry > request.logical_time_micros);
    Ok((
        true,
        has_expiry,
        visible.then(|| HashFieldEntry::new(field.to_vec(), entry.value)),
    ))
}

pub(super) fn hash_scan_latest_at_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    request: CollectionScanRequestV3<'_>,
) -> Result<HashScanResultV3, NativeRuntimeError> {
    hash_scan_range_latest_at_v3(
        pages,
        blobs,
        pool,
        tree,
        request,
        HashScanDirectionV3::Forward,
    )
}

pub(super) fn hash_scan_reverse_latest_at_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    request: CollectionScanRequestV3<'_>,
) -> Result<HashScanResultV3, NativeRuntimeError> {
    hash_scan_range_latest_at_v3(
        pages,
        blobs,
        pool,
        tree,
        request,
        HashScanDirectionV3::Reverse,
    )
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum HashScanDirectionV3 {
    Forward,
    Reverse,
}

struct HashScanStateV3 {
    incarnation: StructureIncarnation,
    field_count: u64,
    field_expiry_count: u64,
    declared_field_count: usize,
    prefix: Vec<u8>,
    cursor_key: Option<Vec<u8>>,
}

fn prepare_hash_scan_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    request: CollectionScanRequestV3<'_>,
) -> Result<HashScanStateV3, NativeRuntimeError> {
    validate_collection_child_identity_v3(request.key, &[])?;
    if let Some(cursor) = request.start_after {
        validate_collection_child_identity_v3(request.key, cursor)?;
    }
    let (incarnation, state) = visible_collection_state_v3(
        pages,
        blobs,
        pool,
        tree,
        request.key,
        StructureCollectionFamily::Hash,
        request.logical_time_micros,
    )?;
    let CollectionStateV3::Hash {
        field_count,
        field_expiry_count,
        ..
    } = state
    else {
        return Err(NativeRuntimeError::InvalidStructureTree);
    };
    let declared_field_count =
        usize::try_from(field_count).map_err(|_| NativeRuntimeError::InvalidStructureTree)?;
    let prefix = v3_hash_field_prefix(request.key, incarnation)?;
    let cursor_key = request
        .start_after
        .map(|field| {
            encode_collection_child_key(
                STRUCTURE_HASH_FIELD_PREFIX,
                request.key,
                incarnation,
                field,
            )
            .map_err(map_codec_error)
        })
        .transpose()?;
    Ok(HashScanStateV3 {
        incarnation,
        field_count,
        field_expiry_count,
        declared_field_count,
        prefix,
        cursor_key,
    })
}

fn hash_scan_range_latest_at_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    request: CollectionScanRequestV3<'_>,
    direction: HashScanDirectionV3,
) -> Result<HashScanResultV3, NativeRuntimeError> {
    let HashScanStateV3 {
        incarnation,
        field_count,
        field_expiry_count,
        declared_field_count,
        prefix,
        cursor_key,
    } = prepare_hash_scan_v3(pages, blobs, pool, tree, request)?;
    if request.limit == 0 {
        return Ok(HashScanResultV3 {
            entries: Vec::new(),
            declared_field_count,
        });
    }
    let verify_complete = request.start_after.is_none() && request.limit >= declared_field_count;
    let mut physical_count = 0_u64;
    let mut observed_expiries = 0_u64;
    let mut entries = Vec::with_capacity(request.limit.min(declared_field_count).min(256));
    let mut failure = None;
    let mut visit = |physical_key: &[u8], encoded: &[u8]| {
        let decoded = hash_scan_entry_v3(physical_key, encoded, blobs, request, incarnation);
        let (physical_live, has_expiry, entry) = match decoded {
            Ok(entry) => entry,
            Err(error) => {
                failure = Some(error);
                return ControlFlow::Break(());
            }
        };
        let Some(next_physical_count) = physical_count.checked_add(u64::from(physical_live)) else {
            failure = Some(NativeRuntimeError::InvalidStructureTree);
            return ControlFlow::Break(());
        };
        let Some(next_observed_expiries) = observed_expiries.checked_add(u64::from(has_expiry))
        else {
            failure = Some(NativeRuntimeError::InvalidStructureTree);
            return ControlFlow::Break(());
        };
        physical_count = next_physical_count;
        observed_expiries = next_observed_expiries;
        if physical_count > field_count || observed_expiries > field_expiry_count {
            failure = Some(NativeRuntimeError::InvalidStructureTree);
            return ControlFlow::Break(());
        }
        if let Some(entry) = entry {
            entries.push(entry);
            if entries.len() == request.limit && !verify_complete {
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    };
    let outcome = match direction {
        HashScanDirectionV3::Forward => tree.visit_prefix_range_cached(
            pages,
            pool,
            &prefix,
            cursor_key
                .as_deref()
                .map_or(Bound::Unbounded, Bound::Excluded),
            Bound::Unbounded,
            &mut visit,
        )?,
        HashScanDirectionV3::Reverse => tree.visit_prefix_range_cached_reverse(
            pages,
            pool,
            &prefix,
            Bound::Unbounded,
            cursor_key
                .as_deref()
                .map_or(Bound::Unbounded, Bound::Excluded),
            &mut visit,
        )?,
    };
    if let Some(error) = failure {
        return Err(error);
    }
    if verify_complete
        && (outcome != ControlFlow::Continue(())
            || physical_count != field_count
            || observed_expiries != field_expiry_count)
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(HashScanResultV3 {
        entries,
        declared_field_count,
    })
}

pub(super) fn hash_pattern_scan_latest_at_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    request: &HashPatternScanRequest,
    logical_time_micros: i64,
) -> Result<HashPatternScanPage, NativeRuntimeError> {
    validate_collection_child_identity_v3(key, request.compiled().leading_literal_prefix())?;
    if let Some(cursor) = request.start_after() {
        validate_collection_child_identity_v3(key, cursor)?;
    }
    let scan_request = CollectionScanRequestV3::new(
        key,
        request.start_after(),
        request.visit_limit(),
        logical_time_micros,
    );
    let state = prepare_hash_scan_v3(pages, blobs, pool, tree, scan_request)?;
    if request.compiled().is_exact_literal() {
        return hash_pattern_exact_latest_at_v3(
            pages,
            blobs,
            pool,
            tree,
            request,
            &state,
            scan_request,
        );
    }
    hash_pattern_range_latest_at_v3(pages, blobs, pool, tree, request, &state, scan_request)
}

fn empty_hash_pattern_page_v3(visited: usize) -> HashPatternScanPage {
    HashPatternScanPage::new(Vec::new(), None, HashPatternScanStop::Exhausted, visited, 0)
}

fn hash_pattern_exact_latest_at_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    request: &HashPatternScanRequest,
    state: &HashScanStateV3,
    scan_request: CollectionScanRequestV3<'_>,
) -> Result<HashPatternScanPage, NativeRuntimeError> {
    let field = request.compiled().leading_literal_prefix();
    if request.start_after().is_some_and(|cursor| field <= cursor) {
        return Ok(empty_hash_pattern_page_v3(0));
    }
    let physical_key = encode_collection_child_key(
        STRUCTURE_HASH_FIELD_PREFIX,
        scan_request.key,
        state.incarnation,
        field,
    )
    .map_err(map_codec_error)?;
    let Some(encoded) = tree.get_cached_pinned(pages, pool, &physical_key)? else {
        return Ok(empty_hash_pattern_page_v3(0));
    };
    let Some(entry) = decode_structure_value(encoded.bytes(), blobs)? else {
        return Ok(empty_hash_pattern_page_v3(1));
    };
    if state.field_count == 0
        || u64::from(entry.expires_at_micros.is_some()) > state.field_expiry_count
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    if entry
        .expires_at_micros
        .is_some_and(|expiry| expiry <= scan_request.logical_time_micros)
    {
        return Ok(empty_hash_pattern_page_v3(1));
    }
    let mut budget = HashPatternMatchBudget::new(request.match_step_limit());
    if !request.compiled().matches(field, &mut budget)? {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(HashPatternScanPage::new(
        vec![HashFieldEntry::new(field.to_vec(), entry.value)],
        None,
        HashPatternScanStop::Exhausted,
        1,
        budget.used(),
    ))
}

fn hash_pattern_range_latest_at_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    request: &HashPatternScanRequest,
    state: &HashScanStateV3,
    scan_request: CollectionScanRequestV3<'_>,
) -> Result<HashPatternScanPage, NativeRuntimeError> {
    let literal = request.compiled().leading_literal_prefix();
    let range_lower = (!literal.is_empty())
        .then(|| {
            encode_collection_child_key(
                STRUCTURE_HASH_FIELD_PREFIX,
                scan_request.key,
                state.incarnation,
                literal,
            )
            .map_err(map_codec_error)
        })
        .transpose()?;
    let range_upper = range_lower
        .as_deref()
        .map(|lower| byte_prefix_successor(lower).ok_or(NativeRuntimeError::InvalidStructureTree))
        .transpose()?;
    if range_upper.as_ref().is_some_and(|upper| {
        state
            .cursor_key
            .as_ref()
            .is_some_and(|cursor| cursor >= upper)
    }) {
        return Ok(empty_hash_pattern_page_v3(0));
    }
    let lower = hash_pattern_lower_bound(range_lower.as_deref(), state.cursor_key.as_deref());
    let upper = range_upper
        .as_deref()
        .map_or(Bound::Unbounded, Bound::Excluded);
    visit_hash_pattern_range_v3(
        StructureReadStoresV3 { pages, blobs, pool },
        tree,
        request,
        state,
        scan_request,
        HashPatternPhysicalRangeV3 { lower, upper },
    )
}

#[derive(Clone, Copy)]
struct StructureReadStoresV3<'store> {
    pages: &'store PageStore,
    blobs: &'store BlobStore,
    pool: &'store BufferPool,
}

#[derive(Clone, Copy)]
struct HashPatternPhysicalRangeV3<'key> {
    lower: Bound<&'key [u8]>,
    upper: Bound<&'key [u8]>,
}

fn visit_hash_pattern_range_v3(
    stores: StructureReadStoresV3<'_>,
    tree: BTree,
    request: &HashPatternScanRequest,
    state: &HashScanStateV3,
    scan_request: CollectionScanRequestV3<'_>,
    range: HashPatternPhysicalRangeV3<'_>,
) -> Result<HashPatternScanPage, NativeRuntimeError> {
    let mut budget = HashPatternMatchBudget::new(request.match_step_limit());
    let mut entries =
        Vec::with_capacity(request.output_limit().min(request.visit_limit()).min(256));
    let mut continuation = None;
    let mut stop = HashPatternScanStop::Exhausted;
    let mut visited = 0_usize;
    let mut physical_count = 0_u64;
    let mut observed_expiries = 0_u64;
    let mut failure = None;
    let outcome = tree.visit_prefix_range_cached(
        stores.pages,
        stores.pool,
        &state.prefix,
        range.lower,
        range.upper,
        |physical_key, encoded| {
            let result = (|| {
                let field = current_child_identity_v3(
                    physical_key,
                    STRUCTURE_HASH_FIELD_PREFIX,
                    scan_request.key,
                    state.incarnation,
                )?;
                visited = visited
                    .checked_add(1)
                    .ok_or(NativeRuntimeError::InvalidStructureTree)?;
                continuation = Some(field.to_vec());
                let Some(entry) = decode_structure_value(encoded, stores.blobs)? else {
                    return Ok(());
                };
                physical_count = physical_count
                    .checked_add(1)
                    .ok_or(NativeRuntimeError::InvalidStructureTree)?;
                observed_expiries = observed_expiries
                    .checked_add(u64::from(entry.expires_at_micros.is_some()))
                    .ok_or(NativeRuntimeError::InvalidStructureTree)?;
                if physical_count > state.field_count
                    || observed_expiries > state.field_expiry_count
                {
                    return Err(NativeRuntimeError::InvalidStructureTree);
                }
                if entry
                    .expires_at_micros
                    .is_none_or(|expiry| expiry > scan_request.logical_time_micros)
                    && request.compiled().matches(field, &mut budget)?
                {
                    entries.push(HashFieldEntry::new(field.to_vec(), entry.value));
                }
                Ok(())
            })();
            if let Err(error) = result {
                failure = Some(error);
                return ControlFlow::Break(());
            }
            if entries.len() == request.output_limit() {
                stop = HashPatternScanStop::OutputLimit;
                ControlFlow::Break(())
            } else if visited == request.visit_limit() {
                stop = HashPatternScanStop::VisitLimit;
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        },
    )?;
    if let Some(error) = failure {
        return Err(error);
    }
    if outcome == ControlFlow::Continue(()) {
        continuation = None;
    }
    let verify_complete = outcome == ControlFlow::Continue(())
        && request.start_after().is_none()
        && request.compiled().leading_literal_prefix().is_empty();
    if verify_complete
        && (physical_count != state.field_count || observed_expiries != state.field_expiry_count)
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(HashPatternScanPage::new(
        entries,
        continuation,
        stop,
        visited,
        budget.used(),
    ))
}

#[derive(Clone, Copy)]
struct SetAlgebraInputV3<'key> {
    key: &'key [u8],
    incarnation: StructureIncarnation,
    member_count: usize,
}

pub(super) struct SortedSetOrderPlanV3 {
    pub(super) member_count: usize,
    pub(super) incarnation: StructureIncarnation,
    pub(super) prefix: Vec<u8>,
    pub(super) lower: Bound<Vec<u8>>,
    pub(super) upper: Bound<Vec<u8>>,
}

pub(super) struct SortedSetSegmentBatchV3 {
    pub(super) entries: Vec<SortedSetEntry>,
}

type PhysicalBoundsV3 = (Bound<Vec<u8>>, Bound<Vec<u8>>);

pub(super) fn prepare_sorted_set_order_plan_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    bounds: Option<(Bound<SortedSetScore>, Bound<SortedSetScore>)>,
) -> Result<SortedSetOrderPlanV3, NativeRuntimeError> {
    validate_collection_child_identity_v3(key, &[0_u8; 8])?;
    let (incarnation, state) = visible_collection_state_v3(
        pages,
        blobs,
        pool,
        tree,
        key,
        StructureCollectionFamily::SortedSet,
        i64::MIN,
    )?;
    let CollectionStateV3::SortedSet { member_count, .. } = state else {
        return Err(NativeRuntimeError::InvalidStructureTree);
    };
    let prefix =
        encode_collection_child_key(STRUCTURE_SORTED_SET_ORDER_PREFIX, key, incarnation, &[])
            .map_err(map_codec_error)?;
    let (lower, upper) = match bounds {
        None => (Bound::Unbounded, Bound::Unbounded),
        Some((lower, upper)) => sorted_set_score_bounds_v3(key, incarnation, lower, upper)?,
    };
    Ok(SortedSetOrderPlanV3 {
        member_count: usize::try_from(member_count)
            .map_err(|_| NativeRuntimeError::InvalidStructureTree)?,
        incarnation,
        prefix,
        lower,
        upper,
    })
}

fn sorted_set_score_bounds_v3(
    key: &[u8],
    incarnation: StructureIncarnation,
    lower: Bound<SortedSetScore>,
    upper: Bound<SortedSetScore>,
) -> Result<PhysicalBoundsV3, NativeRuntimeError> {
    let score_prefix = |score: SortedSetScore| {
        encode_collection_child_key(
            STRUCTURE_SORTED_SET_ORDER_PREFIX,
            key,
            incarnation,
            &score.sortable_bits().to_be_bytes(),
        )
        .map_err(map_codec_error)
    };
    let lower = match lower {
        Bound::Included(score) => score_prefix(score).map(Bound::Included),
        Bound::Excluded(score) => score_prefix(score).and_then(|prefix| {
            byte_prefix_successor(&prefix)
                .map(Bound::Included)
                .ok_or(NativeRuntimeError::InvalidStructureTree)
        }),
        Bound::Unbounded => Ok(Bound::Unbounded),
    };
    let upper = match upper {
        Bound::Included(score) => score_prefix(score).and_then(|prefix| {
            byte_prefix_successor(&prefix)
                .map(Bound::Excluded)
                .ok_or(NativeRuntimeError::InvalidStructureTree)
        }),
        Bound::Excluded(score) => score_prefix(score).map(Bound::Excluded),
        Bound::Unbounded => Ok(Bound::Unbounded),
    };
    Ok((lower?, upper?))
}

fn decode_sorted_set_order_entry_v3(
    pages: &PageStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    incarnation: StructureIncarnation,
    physical_key: &[u8],
    encoded: &[u8],
) -> Result<Option<SortedSetEntry>, NativeRuntimeError> {
    let identity = current_child_identity_v3(
        physical_key,
        STRUCTURE_SORTED_SET_ORDER_PREFIX,
        key,
        incarnation,
    )?;
    let (score, member) = decode_sorted_set_order_identity_v3(identity)?;
    if !decode_set_member_value(encoded)? {
        return Ok(None);
    }
    let member_key =
        encode_collection_child_key(STRUCTURE_SORTED_SET_MEMBER_PREFIX, key, incarnation, member)
            .map_err(map_codec_error)?;
    let stored = tree
        .get_cached_pinned(pages, pool, &member_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    if decode_sorted_set_score(stored.bytes())? != Some(score) {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(Some(SortedSetEntry {
        member: member.to_vec(),
        score,
    }))
}

pub(super) fn sorted_set_rank_latest_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    member: &[u8],
    reverse: bool,
) -> Result<Option<usize>, NativeRuntimeError> {
    validate_collection_child_identity_v3(key, member)?;
    let plan = prepare_sorted_set_order_plan_v3(pages, blobs, pool, tree, key, None)?;
    let member_key = encode_collection_child_key(
        STRUCTURE_SORTED_SET_MEMBER_PREFIX,
        key,
        plan.incarnation,
        member,
    )
    .map_err(map_codec_error)?;
    let Some(encoded_score) = tree.get_cached_pinned(pages, pool, &member_key)? else {
        return Ok(None);
    };
    let Some(score) = decode_sorted_set_score(encoded_score.bytes())? else {
        return Ok(None);
    };
    let target = v3_sorted_set_order_key(key, plan.incarnation, score, member)?;
    let mut live_rank = 0_usize;
    let mut rank = None;
    let mut failure = None;
    let mut visitor = |physical_key: &[u8], encoded: &[u8]| {
        let entry = match decode_sorted_set_order_entry_v3(
            pages,
            pool,
            tree,
            key,
            plan.incarnation,
            physical_key,
            encoded,
        ) {
            Ok(entry) => entry,
            Err(error) => {
                failure = Some(error);
                return ControlFlow::Break(());
            }
        };
        if physical_key == target {
            if entry.as_ref().is_none_or(|entry| entry.member != member) {
                failure = Some(NativeRuntimeError::InvalidStructureTree);
            } else {
                rank = Some(live_rank);
            }
            return ControlFlow::Break(());
        }
        if entry.is_some() {
            live_rank = match live_rank.checked_add(1) {
                Some(rank) if rank < plan.member_count => rank,
                _ => {
                    failure = Some(NativeRuntimeError::InvalidStructureTree);
                    return ControlFlow::Break(());
                }
            };
        }
        ControlFlow::Continue(())
    };
    if reverse {
        let _ = tree.visit_prefix_range_cached_reverse(
            pages,
            pool,
            &plan.prefix,
            Bound::Included(target.as_slice()),
            Bound::Unbounded,
            &mut visitor,
        )?;
    } else {
        let _ = tree.visit_prefix_range_cached(
            pages,
            pool,
            &plan.prefix,
            Bound::Unbounded,
            Bound::Included(target.as_slice()),
            &mut visitor,
        )?;
    }
    if let Some(error) = failure {
        return Err(error);
    }
    rank.map(Some)
        .ok_or(NativeRuntimeError::InvalidStructureTree)
}

#[derive(Clone, Copy)]
pub(super) struct SortedSetRankRangeRequestV3 {
    pub(super) start: i64,
    pub(super) stop: i64,
    pub(super) direction: SortedSetDirection,
}

pub(super) fn sorted_set_rank_range_latest_v3(
    pages: &PageStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    plan: &SortedSetOrderPlanV3,
    request: SortedSetRankRangeRequestV3,
) -> Result<Vec<SortedSetEntry>, NativeRuntimeError> {
    let Some((range_start, range_stop)) =
        crate::normalize_list_range(plan.member_count, request.start, request.stop)
    else {
        return Ok(Vec::new());
    };
    let mut live_rank = 0_usize;
    let verify_complete = range_start == 0 && range_stop + 1 == plan.member_count;
    let mut entries = Vec::with_capacity(range_stop - range_start + 1);
    let mut failure = None;
    let mut visitor = |physical_key: &[u8], encoded: &[u8]| {
        let entry = match decode_sorted_set_order_entry_v3(
            pages,
            pool,
            tree,
            key,
            plan.incarnation,
            physical_key,
            encoded,
        ) {
            Ok(entry) => entry,
            Err(error) => {
                failure = Some(error);
                return ControlFlow::Break(());
            }
        };
        let Some(entry) = entry else {
            return ControlFlow::Continue(());
        };
        if live_rank >= plan.member_count {
            failure = Some(NativeRuntimeError::InvalidStructureTree);
            return ControlFlow::Break(());
        }
        if live_rank >= range_start {
            entries.push(entry);
        }
        if live_rank == range_stop && !verify_complete {
            return ControlFlow::Break(());
        }
        let Some(next_rank) = live_rank.checked_add(1) else {
            failure = Some(NativeRuntimeError::InvalidStructureTree);
            return ControlFlow::Break(());
        };
        live_rank = next_rank;
        ControlFlow::Continue(())
    };
    let outcome = match request.direction {
        SortedSetDirection::Ascending => tree.visit_prefix_range_cached(
            pages,
            pool,
            &plan.prefix,
            Bound::Unbounded,
            Bound::Unbounded,
            &mut visitor,
        )?,
        SortedSetDirection::Descending => tree.visit_prefix_range_cached_reverse(
            pages,
            pool,
            &plan.prefix,
            Bound::Unbounded,
            Bound::Unbounded,
            &mut visitor,
        )?,
    };
    if let Some(error) = failure {
        return Err(error);
    }
    if entries.len() != range_stop - range_start + 1
        || (verify_complete
            && (outcome != ControlFlow::Continue(()) || live_rank != plan.member_count))
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(entries)
}

pub(super) fn decode_sorted_set_segment_v3(
    pages: &PageStore,
    pool: &BufferPool,
    tree: BTree,
    physical_entries: Vec<(Vec<u8>, Vec<u8>)>,
    key: &[u8],
    incarnation: StructureIncarnation,
    direction: SortedSetDirection,
) -> Result<SortedSetSegmentBatchV3, NativeRuntimeError> {
    let entries = match direction {
        SortedSetDirection::Ascending => physical_entries,
        SortedSetDirection::Descending => physical_entries.into_iter().rev().collect(),
    };
    let entries = entries
        .into_iter()
        .filter_map(|(physical_key, encoded)| {
            decode_sorted_set_order_entry_v3(
                pages,
                pool,
                tree,
                key,
                incarnation,
                &physical_key,
                &encoded,
            )
            .transpose()
        })
        .collect::<Result<Vec<_>, NativeRuntimeError>>()?;
    Ok(SortedSetSegmentBatchV3 { entries })
}

#[derive(Clone, Copy)]
pub(super) struct SortedSetScoreRangeRequestV3 {
    pub(super) offset: usize,
    pub(super) limit: usize,
    pub(super) direction: SortedSetDirection,
}

pub(super) fn sorted_set_score_range_latest_v3(
    pages: &PageStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    plan: &SortedSetOrderPlanV3,
    request: SortedSetScoreRangeRequestV3,
) -> Result<Vec<SortedSetEntry>, NativeRuntimeError> {
    if request.limit == 0 {
        return Ok(Vec::new());
    }
    let mut skipped = 0_usize;
    let mut live_count = 0_usize;
    let mut entries = Vec::new();
    let mut failure = None;
    let verify_complete = matches!(plan.lower, Bound::Unbounded)
        && matches!(plan.upper, Bound::Unbounded)
        && request.offset == 0
        && request.limit >= plan.member_count;
    let mut visitor = |physical_key: &[u8], encoded: &[u8]| {
        let entry = match decode_sorted_set_order_entry_v3(
            pages,
            pool,
            tree,
            key,
            plan.incarnation,
            physical_key,
            encoded,
        ) {
            Ok(entry) => entry,
            Err(error) => {
                failure = Some(error);
                return ControlFlow::Break(());
            }
        };
        let Some(entry) = entry else {
            return ControlFlow::Continue(());
        };
        live_count = match live_count.checked_add(1) {
            Some(count) if count <= plan.member_count => count,
            _ => {
                failure = Some(NativeRuntimeError::InvalidStructureTree);
                return ControlFlow::Break(());
            }
        };
        if skipped < request.offset {
            skipped += 1;
        } else {
            entries.push(entry);
        }
        if entries.len() == request.limit && !verify_complete {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let outcome = match request.direction {
        SortedSetDirection::Ascending => tree.visit_prefix_range_cached(
            pages,
            pool,
            &plan.prefix,
            bound_as_slice_v3(&plan.lower),
            bound_as_slice_v3(&plan.upper),
            &mut visitor,
        )?,
        SortedSetDirection::Descending => tree.visit_prefix_range_cached_reverse(
            pages,
            pool,
            &plan.prefix,
            bound_as_slice_v3(&plan.lower),
            bound_as_slice_v3(&plan.upper),
            &mut visitor,
        )?,
    };
    if let Some(error) = failure {
        return Err(error);
    }
    if verify_complete && (outcome != ControlFlow::Continue(()) || live_count != plan.member_count)
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(entries)
}

pub(super) fn set_algebra_latest_at_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    request: &SetAlgebraRequest,
    logical_time_micros: i64,
) -> Result<SetAlgebraResult, NativeRuntimeError> {
    crate::validate_set_algebra_keys(request)?;
    let inputs =
        preflight_set_algebra_inputs_v3(pages, blobs, pool, tree, request, logical_time_micros)?;
    match request.operation() {
        SetAlgebraOperation::Union => set_union_latest_v3(pages, pool, tree, request, &inputs),
        SetAlgebraOperation::Intersection => {
            set_intersection_latest_v3(pages, pool, tree, request, &inputs)
        }
        SetAlgebraOperation::Difference => {
            set_difference_latest_v3(pages, pool, tree, request, &inputs)
        }
    }
}

fn preflight_set_algebra_inputs_v3<'key>(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    request: &'key SetAlgebraRequest,
    logical_time_micros: i64,
) -> Result<Vec<Option<SetAlgebraInputV3<'key>>>, NativeRuntimeError> {
    let mut inputs = Vec::with_capacity(request.keys().len());
    for key in request.keys() {
        validate_collection_child_identity_v3(key, &[])?;
    }
    for key in request.keys() {
        if let Some((incarnation, state)) =
            live_collection_state_v3(pages, pool, tree, key, StructureCollectionFamily::Set)?
        {
            let CollectionStateV3::Set {
                member_count,
                expires_at_micros,
            } = state
            else {
                return Err(NativeRuntimeError::InvalidStructureTree);
            };
            if expires_at_micros.is_none_or(|expiry| expiry > logical_time_micros) {
                inputs.push(Some(SetAlgebraInputV3 {
                    key,
                    incarnation,
                    member_count: usize::try_from(member_count)
                        .map_err(|_| NativeRuntimeError::InvalidStructureTree)?,
                }));
                continue;
            }
        }
        if live_structure_kind_v3(pages, blobs, pool, tree, key, logical_time_micros)?.is_some() {
            return Err(NativeRuntimeError::StructureKindMismatch);
        }
        inputs.push(None);
    }
    Ok(inputs)
}

fn set_union_latest_v3(
    pages: &PageStore,
    pool: &BufferPool,
    tree: BTree,
    request: &SetAlgebraRequest,
    inputs: &[Option<SetAlgebraInputV3<'_>>],
) -> Result<SetAlgebraResult, NativeRuntimeError> {
    let mut execution = SetAlgebraExecution::new(request);
    for input in inputs.iter().flatten() {
        visit_live_set_members_v3(
            pages,
            pool,
            tree,
            *input,
            &mut execution,
            |member, execution| execution.insert(member).map_err(Into::into),
        )?;
    }
    Ok(execution.finish())
}

fn set_intersection_latest_v3(
    pages: &PageStore,
    pool: &BufferPool,
    tree: BTree,
    request: &SetAlgebraRequest,
    inputs: &[Option<SetAlgebraInputV3<'_>>],
) -> Result<SetAlgebraResult, NativeRuntimeError> {
    if inputs
        .iter()
        .any(|input| input.is_none_or(|input| input.member_count == 0))
    {
        return Ok(SetAlgebraExecution::new(request).finish());
    }
    let (source_position, source) = inputs
        .iter()
        .enumerate()
        .filter_map(|(position, input)| input.map(|input| (position, input)))
        .min_by_key(|(position, input)| (input.member_count, *position))
        .ok_or(SetAlgebraError::InvalidKeyCount { requested: 0 })?;
    let mut execution = SetAlgebraExecution::new(request);
    visit_live_set_members_v3(
        pages,
        pool,
        tree,
        source,
        &mut execution,
        |member, execution| {
            for (position, input) in inputs.iter().enumerate() {
                if position != source_position
                    && !set_member_live_v3(
                        pages,
                        pool,
                        tree,
                        input.ok_or(NativeRuntimeError::InvalidStructureTree)?,
                        member,
                        execution,
                    )?
                {
                    return Ok(());
                }
            }
            execution.insert(member).map_err(Into::into)
        },
    )?;
    Ok(execution.finish())
}

fn set_difference_latest_v3(
    pages: &PageStore,
    pool: &BufferPool,
    tree: BTree,
    request: &SetAlgebraRequest,
    inputs: &[Option<SetAlgebraInputV3<'_>>],
) -> Result<SetAlgebraResult, NativeRuntimeError> {
    let Some(source) = inputs[0] else {
        return Ok(SetAlgebraExecution::new(request).finish());
    };
    let mut execution = SetAlgebraExecution::new(request);
    visit_live_set_members_v3(
        pages,
        pool,
        tree,
        source,
        &mut execution,
        |member, execution| {
            for input in inputs[1..].iter().flatten() {
                if set_member_live_v3(pages, pool, tree, *input, member, execution)? {
                    return Ok(());
                }
            }
            execution.insert(member).map_err(Into::into)
        },
    )?;
    Ok(execution.finish())
}

fn set_member_live_v3(
    pages: &PageStore,
    pool: &BufferPool,
    tree: BTree,
    input: SetAlgebraInputV3<'_>,
    member: &[u8],
    execution: &mut SetAlgebraExecution<'_>,
) -> Result<bool, NativeRuntimeError> {
    execution.consume_visit()?;
    let member_key = encode_collection_child_key(
        STRUCTURE_SET_MEMBER_PREFIX,
        input.key,
        input.incarnation,
        member,
    )
    .map_err(map_codec_error)?;
    tree.get_cached_pinned(pages, pool, &member_key)?
        .map(|encoded| decode_set_member_value(encoded.bytes()))
        .transpose()
        .map(Option::unwrap_or_default)
}

fn visit_live_set_members_v3(
    pages: &PageStore,
    pool: &BufferPool,
    tree: BTree,
    input: SetAlgebraInputV3<'_>,
    execution: &mut SetAlgebraExecution<'_>,
    mut on_live: impl FnMut(&[u8], &mut SetAlgebraExecution<'_>) -> Result<(), NativeRuntimeError>,
) -> Result<(), NativeRuntimeError> {
    let prefix = encode_collection_child_key(
        STRUCTURE_SET_MEMBER_PREFIX,
        input.key,
        input.incarnation,
        &[],
    )
    .map_err(map_codec_error)?;
    let mut live_count = 0_usize;
    let mut failure = None;
    let outcome = tree.visit_prefix_range_cached(
        pages,
        pool,
        &prefix,
        Bound::Unbounded,
        Bound::Unbounded,
        |physical_key, encoded| {
            if let Err(error) = execution.consume_visit() {
                failure = Some(error.into());
                return ControlFlow::Break(());
            }
            let member = match current_child_identity_v3(
                physical_key,
                STRUCTURE_SET_MEMBER_PREFIX,
                input.key,
                input.incarnation,
            ) {
                Ok(member) => member,
                Err(error) => {
                    failure = Some(error);
                    return ControlFlow::Break(());
                }
            };
            let live = match decode_set_member_value(encoded) {
                Ok(live) => live,
                Err(error) => {
                    failure = Some(error);
                    return ControlFlow::Break(());
                }
            };
            if live {
                live_count = match live_count.checked_add(1) {
                    Some(count) if count <= input.member_count => count,
                    _ => {
                        failure = Some(NativeRuntimeError::InvalidStructureTree);
                        return ControlFlow::Break(());
                    }
                };
                if let Err(error) = on_live(member, execution) {
                    failure = Some(error);
                    return ControlFlow::Break(());
                }
            }
            ControlFlow::Continue(())
        },
    )?;
    if let Some(error) = failure {
        return Err(error);
    }
    if outcome == ControlFlow::Break(()) || live_count != input.member_count {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(())
}

pub(super) struct ListRangePlanV3 {
    pub(super) element_count: usize,
    pub(super) logical_value_bytes: u64,
    pub(super) incarnation: StructureIncarnation,
    pub(super) head_chunk: i64,
    pub(super) tail_chunk: i64,
    pub(super) prefix: Vec<u8>,
    pub(super) lower: Bound<Vec<u8>>,
    pub(super) upper: Bound<Vec<u8>>,
    pub(super) normalized: Option<(usize, usize)>,
}

#[derive(Clone, Copy)]
pub(super) struct ListRangeRequestV3<'key> {
    pub(super) key: &'key [u8],
    pub(super) start: i64,
    pub(super) stop: i64,
    pub(super) logical_time_micros: i64,
}

pub(super) struct ListSegmentBatchV3 {
    pub(super) chunks: Vec<(i64, Vec<Vec<u8>>, u64)>,
}

pub(super) fn prepare_list_range_latest_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    request: ListRangeRequestV3<'_>,
) -> Result<ListRangePlanV3, NativeRuntimeError> {
    validate_collection_child_identity_v3(request.key, &[0_u8; 8])?;
    let (incarnation, state) = visible_collection_state_v3(
        pages,
        blobs,
        pool,
        tree,
        request.key,
        StructureCollectionFamily::List,
        request.logical_time_micros,
    )?;
    let CollectionStateV3::List {
        element_count,
        logical_value_bytes,
        head_chunk,
        tail_chunk,
        ..
    } = state
    else {
        return Err(NativeRuntimeError::InvalidStructureTree);
    };
    if (element_count == 0 && (logical_value_bytes != 0 || head_chunk != 0 || tail_chunk != 0))
        || (element_count != 0
            && (head_chunk > tail_chunk
                || list_chunk_range_count(head_chunk, tail_chunk).map_err(map_codec_error)?
                    > element_count))
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let element_count =
        usize::try_from(element_count).map_err(|_| NativeRuntimeError::InvalidStructureTree)?;
    let prefix = v3_list_chunk_prefix(request.key, incarnation)?;
    let lower = Bound::Included(
        encode_collection_child_key(
            STRUCTURE_LIST_CHUNK_PREFIX,
            request.key,
            incarnation,
            &encode_list_chunk_identity_v3(head_chunk),
        )
        .map_err(map_codec_error)?,
    );
    let upper = Bound::Included(
        encode_collection_child_key(
            STRUCTURE_LIST_CHUNK_PREFIX,
            request.key,
            incarnation,
            &encode_list_chunk_identity_v3(tail_chunk),
        )
        .map_err(map_codec_error)?,
    );
    Ok(ListRangePlanV3 {
        element_count,
        logical_value_bytes,
        incarnation,
        head_chunk,
        tail_chunk,
        prefix,
        lower,
        upper,
        normalized: crate::normalize_list_range(element_count, request.start, request.stop),
    })
}

fn read_list_chunk_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    incarnation: StructureIncarnation,
    chunk_id: i64,
) -> Result<(Vec<Vec<u8>>, u64), NativeRuntimeError> {
    let chunk_key = encode_collection_child_key(
        STRUCTURE_LIST_CHUNK_PREFIX,
        key,
        incarnation,
        &encode_list_chunk_identity_v3(chunk_id),
    )
    .map_err(map_codec_error)?;
    let encoded = tree
        .get_cached_pinned(pages, pool, &chunk_key)?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    let envelopes = decode_list_chunk_storage(encoded.bytes())?
        .ok_or(NativeRuntimeError::InvalidStructureTree)?;
    if envelopes.is_empty() {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let mut logical_bytes = 0_u64;
    let values = envelopes
        .iter()
        .map(|element| {
            let entry = decode_structure_value(element, blobs)?
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
            if entry.expires_at_micros.is_some() {
                return Err(NativeRuntimeError::InvalidStructureTree);
            }
            logical_bytes = logical_bytes
                .checked_add(
                    u64::try_from(entry.value.len())
                        .map_err(|_| NativeRuntimeError::InvalidStructureTree)?,
                )
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
            Ok(entry.value)
        })
        .collect::<Result<Vec<_>, NativeRuntimeError>>()?;
    Ok((values, logical_bytes))
}

pub(super) fn list_range_latest_at_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    plan: &ListRangePlanV3,
) -> Result<Vec<Vec<u8>>, NativeRuntimeError> {
    let Some((range_start, range_stop)) = plan.normalized else {
        return Ok(Vec::new());
    };
    let full_range = range_start == 0 && range_stop + 1 == plan.element_count;
    let from_head = range_start <= plan.element_count - 1 - range_stop;
    let mut reached_values = 0_usize;
    let mut reached_bytes = 0_u64;
    let mut values = Vec::with_capacity(range_stop - range_start + 1);
    if from_head {
        let mut position = 0_usize;
        let mut chunk_id = plan.head_chunk;
        loop {
            let (chunk_values, chunk_bytes) =
                read_list_chunk_v3(pages, blobs, pool, tree, key, plan.incarnation, chunk_id)?;
            let chunk_end = position
                .checked_add(chunk_values.len())
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
            reached_values = chunk_end;
            reached_bytes = reached_bytes
                .checked_add(chunk_bytes)
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
            if range_start < chunk_end && range_stop >= position {
                let local_start = range_start.saturating_sub(position);
                let local_stop = range_stop.min(chunk_end - 1) - position;
                values.extend_from_slice(&chunk_values[local_start..=local_stop]);
            }
            if range_stop < chunk_end || chunk_id == plan.tail_chunk {
                break;
            }
            position = chunk_end;
            chunk_id = chunk_id
                .checked_add(1)
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        }
    } else {
        let mut segments = Vec::new();
        let mut position_end = plan.element_count;
        let mut chunk_id = plan.tail_chunk;
        loop {
            let (chunk_values, chunk_bytes) =
                read_list_chunk_v3(pages, blobs, pool, tree, key, plan.incarnation, chunk_id)?;
            let chunk_start = position_end
                .checked_sub(chunk_values.len())
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
            reached_values = reached_values
                .checked_add(chunk_values.len())
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
            reached_bytes = reached_bytes
                .checked_add(chunk_bytes)
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
            if range_start < position_end && range_stop >= chunk_start {
                let local_start = range_start.saturating_sub(chunk_start);
                let local_stop = range_stop.min(position_end - 1) - chunk_start;
                segments.push(chunk_values[local_start..=local_stop].to_vec());
            }
            if range_start >= chunk_start || chunk_id == plan.head_chunk {
                break;
            }
            position_end = chunk_start;
            chunk_id = chunk_id
                .checked_sub(1)
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        }
        segments.reverse();
        values = segments.into_iter().flatten().collect();
    }
    if values.len() != range_stop - range_start + 1
        || (full_range
            && (reached_values != plan.element_count || reached_bytes != plan.logical_value_bytes))
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(values)
}

pub(super) fn decode_list_segment_v3(
    physical_entries: Vec<(Vec<u8>, Vec<u8>)>,
    key: &[u8],
    incarnation: StructureIncarnation,
    blobs: &BlobStore,
) -> Result<ListSegmentBatchV3, NativeRuntimeError> {
    let mut chunks = Vec::new();
    let mut previous = None;
    for (physical_key, encoded) in physical_entries {
        let identity = current_child_identity_v3(
            &physical_key,
            STRUCTURE_LIST_CHUNK_PREFIX,
            key,
            incarnation,
        )?;
        let chunk_id = decode_list_chunk_identity_v3(identity).map_err(map_codec_error)?;
        if previous.is_some_and(|previous| chunk_id <= previous) {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        previous = Some(chunk_id);
        let envelopes =
            decode_list_chunk_storage(&encoded)?.ok_or(NativeRuntimeError::InvalidStructureTree)?;
        if envelopes.is_empty() {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        let mut logical_bytes = 0_u64;
        let values = envelopes
            .iter()
            .map(|element| {
                let entry = decode_structure_value(element, blobs)?
                    .ok_or(NativeRuntimeError::InvalidStructureTree)?;
                if entry.expires_at_micros.is_some() {
                    return Err(NativeRuntimeError::InvalidStructureTree);
                }
                logical_bytes = logical_bytes
                    .checked_add(
                        u64::try_from(entry.value.len())
                            .map_err(|_| NativeRuntimeError::InvalidStructureTree)?,
                    )
                    .ok_or(NativeRuntimeError::InvalidStructureTree)?;
                Ok(entry.value)
            })
            .collect::<Result<Vec<_>, NativeRuntimeError>>()?;
        chunks.push((chunk_id, values, logical_bytes));
    }
    Ok(ListSegmentBatchV3 { chunks })
}

pub(super) fn finalize_list_segment_batches_v3(
    batches: Vec<ListSegmentBatchV3>,
    plan: &ListRangePlanV3,
) -> Result<Vec<Vec<u8>>, NativeRuntimeError> {
    let mut expected_chunk = plan.head_chunk;
    let mut reached_tail = false;
    let mut logical_bytes = 0_u64;
    let mut values = Vec::with_capacity(plan.element_count);
    for (chunk_id, chunk_values, chunk_bytes) in batches.into_iter().flat_map(|batch| batch.chunks)
    {
        if reached_tail || chunk_id != expected_chunk {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        logical_bytes = logical_bytes
            .checked_add(chunk_bytes)
            .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        values.extend(chunk_values);
        if chunk_id == plan.tail_chunk {
            reached_tail = true;
        } else {
            expected_chunk = expected_chunk
                .checked_add(1)
                .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        }
    }
    if !reached_tail
        || values.len() != plan.element_count
        || logical_bytes != plan.logical_value_bytes
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(values)
}

pub(super) struct StreamRangePlanV3 {
    pub(super) entry_count: u64,
    pub(super) last_id: u64,
    pub(super) incarnation: StructureIncarnation,
    pub(super) prefix: Vec<u8>,
    pub(super) lower: Bound<Vec<u8>>,
    pub(super) upper: Bound<Vec<u8>>,
    pub(super) verify_complete: bool,
    pub(super) empty: bool,
    pub(super) range_width: u64,
}

pub(super) struct StreamSegmentBatchV3 {
    pub(super) physical_live: u64,
    pub(super) first_observed: Option<u64>,
    pub(super) last_observed: Option<u64>,
    pub(super) entries: Vec<(u64, crate::model::StreamFields)>,
}

#[derive(Clone, Copy)]
pub(super) struct StreamRangeRequestV3<'key> {
    pub(super) key: &'key [u8],
    pub(super) start: u64,
    pub(super) end: u64,
    pub(super) limit: usize,
    pub(super) logical_time_micros: i64,
}

pub(super) fn prepare_stream_range_latest_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    request: StreamRangeRequestV3<'_>,
) -> Result<StreamRangePlanV3, NativeRuntimeError> {
    validate_collection_child_identity_v3(request.key, &[0_u8; 8])?;
    let (incarnation, state) = visible_collection_state_v3(
        pages,
        blobs,
        pool,
        tree,
        request.key,
        StructureCollectionFamily::Stream,
        request.logical_time_micros,
    )?;
    let CollectionStateV3::Stream {
        entry_count,
        last_id,
        ..
    } = state
    else {
        return Err(NativeRuntimeError::InvalidStructureTree);
    };
    let effective_end = request.end.min(last_id);
    let range_width = effective_end
        .checked_sub(request.start)
        .and_then(|width| width.checked_add(1))
        .unwrap_or(0);
    let empty = request.limit == 0 || range_width == 0 || entry_count == 0;
    let prefix = v3_stream_entry_prefix(request.key, incarnation)?;
    let lower = Bound::Included(
        encode_collection_child_key(
            STRUCTURE_STREAM_ENTRY_PREFIX,
            request.key,
            incarnation,
            &request.start.to_be_bytes(),
        )
        .map_err(map_codec_error)?,
    );
    let upper = Bound::Included(
        encode_collection_child_key(
            STRUCTURE_STREAM_ENTRY_PREFIX,
            request.key,
            incarnation,
            &effective_end.to_be_bytes(),
        )
        .map_err(map_codec_error)?,
    );
    let limit_covers_count = u64::try_from(request.limit).is_ok_and(|limit| limit >= entry_count);
    Ok(StreamRangePlanV3 {
        entry_count,
        last_id,
        incarnation,
        prefix,
        lower,
        upper,
        verify_complete: !empty
            && request.start <= 1
            && effective_end == last_id
            && limit_covers_count,
        empty,
        range_width,
    })
}

pub(super) fn decode_stream_segment_v3(
    physical_entries: Vec<(Vec<u8>, Vec<u8>)>,
    key: &[u8],
    incarnation: StructureIncarnation,
    last_id: u64,
) -> Result<StreamSegmentBatchV3, NativeRuntimeError> {
    let mut physical_live = 0_u64;
    let mut first_observed = None;
    let mut last_observed = None;
    let mut entries = Vec::new();
    for (physical_key, encoded) in physical_entries {
        let (id, entry) =
            decode_stream_entry_v3(&physical_key, &encoded, key, incarnation, last_id)?;
        if last_observed.is_some_and(|previous| id <= previous) {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        first_observed.get_or_insert(id);
        last_observed = Some(id);
        let Some(fields) = entry else { continue };
        physical_live = physical_live
            .checked_add(1)
            .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        entries.push((id, fields));
    }
    Ok(StreamSegmentBatchV3 {
        physical_live,
        first_observed,
        last_observed,
        entries,
    })
}

fn decode_stream_entry_v3(
    physical_key: &[u8],
    encoded: &[u8],
    key: &[u8],
    incarnation: StructureIncarnation,
    last_id: u64,
) -> Result<(u64, Option<crate::model::StreamFields>), NativeRuntimeError> {
    let identity = current_child_identity_v3(
        physical_key,
        STRUCTURE_STREAM_ENTRY_PREFIX,
        key,
        incarnation,
    )?;
    let id = u64::from_be_bytes(
        identity
            .try_into()
            .map_err(|_| NativeRuntimeError::InvalidStructureTree)?,
    );
    if id == 0 || id > last_id {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    if is_structure_tombstone(encoded) {
        return Ok((id, None));
    }
    let (payload_id, fields) =
        decode_stream_wal_entry(encoded).map_err(|_| NativeRuntimeError::InvalidStructureTree)?;
    if payload_id != id {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok((id, Some(fields)))
}

pub(super) fn stream_range_latest_at_v3(
    pages: &PageStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    plan: &StreamRangePlanV3,
    limit: usize,
) -> Result<Vec<(u64, crate::model::StreamFields)>, NativeRuntimeError> {
    if plan.empty {
        return Ok(Vec::new());
    }
    let mut physical_live = 0_u64;
    let mut last_observed = None;
    let mut entries = Vec::with_capacity(limit.min(256));
    let mut failure = None;
    let outcome = tree.visit_prefix_range_cached(
        pages,
        pool,
        &plan.prefix,
        bound_as_slice_v3(&plan.lower),
        bound_as_slice_v3(&plan.upper),
        |physical_key, encoded| {
            let decoded =
                decode_stream_entry_v3(physical_key, encoded, key, plan.incarnation, plan.last_id);
            let (id, entry) = match decoded {
                Ok(entry) => entry,
                Err(error) => {
                    failure = Some(error);
                    return ControlFlow::Break(());
                }
            };
            if last_observed.is_some_and(|previous| id <= previous) {
                failure = Some(NativeRuntimeError::InvalidStructureTree);
                return ControlFlow::Break(());
            }
            last_observed = Some(id);
            if let Some(fields) = entry {
                let Some(next) = physical_live.checked_add(1) else {
                    failure = Some(NativeRuntimeError::InvalidStructureTree);
                    return ControlFlow::Break(());
                };
                physical_live = next;
                if physical_live > plan.entry_count {
                    failure = Some(NativeRuntimeError::InvalidStructureTree);
                    return ControlFlow::Break(());
                }
                entries.push((id, fields));
                if entries.len() == limit && !plan.verify_complete {
                    return ControlFlow::Break(());
                }
            }
            ControlFlow::Continue(())
        },
    )?;
    if let Some(error) = failure {
        return Err(error);
    }
    if plan.verify_complete
        && (outcome != ControlFlow::Continue(())
            || physical_live != plan.entry_count
            || last_observed != Some(plan.last_id))
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(entries)
}

pub(super) fn finalize_stream_segment_batches_v3(
    batches: Vec<StreamSegmentBatchV3>,
    plan: &StreamRangePlanV3,
    limit: usize,
) -> Result<Vec<(u64, crate::model::StreamFields)>, NativeRuntimeError> {
    let mut physical_live = 0_u64;
    let mut last_observed = None;
    for batch in &batches {
        if let Some(first) = batch.first_observed {
            if last_observed.is_some_and(|previous| first <= previous) {
                return Err(NativeRuntimeError::InvalidStructureTree);
            }
            last_observed = batch.last_observed;
        } else if batch.last_observed.is_some() {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
        physical_live = physical_live
            .checked_add(batch.physical_live)
            .ok_or(NativeRuntimeError::InvalidStructureTree)?;
        if physical_live > plan.entry_count {
            return Err(NativeRuntimeError::InvalidStructureTree);
        }
    }
    if plan.verify_complete
        && (physical_live != plan.entry_count || last_observed != Some(plan.last_id))
    {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let mut entries = batches
        .into_iter()
        .flat_map(|batch| batch.entries)
        .collect::<Vec<_>>();
    entries.truncate(limit);
    Ok(entries)
}

fn bound_as_slice_v3(bound: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match bound {
        Bound::Included(value) => Bound::Included(value.as_slice()),
        Bound::Excluded(value) => Bound::Excluded(value.as_slice()),
        Bound::Unbounded => Bound::Unbounded,
    }
}

fn set_member_contains_v3(
    pages: &PageStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    incarnation: StructureIncarnation,
    member: &[u8],
) -> Result<bool, NativeRuntimeError> {
    let member_key =
        encode_collection_child_key(STRUCTURE_SET_MEMBER_PREFIX, key, incarnation, member)
            .map_err(map_codec_error)?;
    tree.get_cached_pinned(pages, pool, &member_key)?
        .map(|encoded| decode_set_member_value(encoded.bytes()))
        .transpose()
        .map(Option::unwrap_or_default)
}

pub(super) fn set_contains_latest_at_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    member: &[u8],
    logical_time_micros: i64,
) -> Result<bool, NativeRuntimeError> {
    let (incarnation, state) = visible_collection_state_v3(
        pages,
        blobs,
        pool,
        tree,
        key,
        StructureCollectionFamily::Set,
        logical_time_micros,
    )?;
    if !matches!(state, CollectionStateV3::Set { .. }) {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    set_member_contains_v3(pages, pool, tree, key, incarnation, member)
}

pub(super) fn set_contains_many_latest_at_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    members: &[Vec<u8>],
    logical_time_micros: i64,
) -> Result<Vec<bool>, NativeRuntimeError> {
    let (incarnation, state) = visible_collection_state_v3(
        pages,
        blobs,
        pool,
        tree,
        key,
        StructureCollectionFamily::Set,
        logical_time_micros,
    )?;
    if !matches!(state, CollectionStateV3::Set { .. }) {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    members
        .iter()
        .map(|member| set_member_contains_v3(pages, pool, tree, key, incarnation, member))
        .collect()
}

pub(super) fn set_scan_latest_at_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    request: CollectionScanRequestV3<'_>,
) -> Result<SetScanResultV3, NativeRuntimeError> {
    validate_collection_child_identity_v3(request.key, &[])?;
    if let Some(cursor) = request.start_after {
        validate_collection_child_identity_v3(request.key, cursor)?;
    }
    let (incarnation, state) = visible_collection_state_v3(
        pages,
        blobs,
        pool,
        tree,
        request.key,
        StructureCollectionFamily::Set,
        request.logical_time_micros,
    )?;
    let CollectionStateV3::Set { member_count, .. } = state else {
        return Err(NativeRuntimeError::InvalidStructureTree);
    };
    let declared_member_count =
        usize::try_from(member_count).map_err(|_| NativeRuntimeError::InvalidStructureTree)?;
    let prefix = v3_set_member_prefix(request.key, incarnation)?;
    let lower_key = request
        .start_after
        .map(|member| {
            encode_collection_child_key(
                STRUCTURE_SET_MEMBER_PREFIX,
                request.key,
                incarnation,
                member,
            )
            .map_err(map_codec_error)
        })
        .transpose()?;
    if request.limit == 0 {
        return Ok(SetScanResultV3 {
            members: Vec::new(),
            declared_member_count,
        });
    }
    let lower = lower_key
        .as_deref()
        .map_or(Bound::Unbounded, Bound::Excluded);
    let verify_complete = request.start_after.is_none() && request.limit >= declared_member_count;
    let mut physical_count = 0_u64;
    let mut members = Vec::with_capacity(request.limit.min(declared_member_count).min(256));
    let mut failure = None;
    let outcome = tree.visit_prefix_range_cached(
        pages,
        pool,
        &prefix,
        lower,
        Bound::Unbounded,
        |physical_key, encoded| {
            let member = current_child_identity_v3(
                physical_key,
                STRUCTURE_SET_MEMBER_PREFIX,
                request.key,
                incarnation,
            );
            let member = match member {
                Ok(member) => member,
                Err(error) => {
                    failure = Some(error);
                    return ControlFlow::Break(());
                }
            };
            match decode_set_member_value(encoded) {
                Ok(true) => {
                    let Some(next_physical_count) = physical_count.checked_add(1) else {
                        failure = Some(NativeRuntimeError::InvalidStructureTree);
                        return ControlFlow::Break(());
                    };
                    physical_count = next_physical_count;
                    if physical_count > member_count {
                        failure = Some(NativeRuntimeError::InvalidStructureTree);
                        return ControlFlow::Break(());
                    }
                    members.push(member.to_vec());
                    if members.len() == request.limit && !verify_complete {
                        return ControlFlow::Break(());
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    failure = Some(error);
                    return ControlFlow::Break(());
                }
            }
            ControlFlow::Continue(())
        },
    )?;
    if let Some(error) = failure {
        return Err(error);
    }
    if verify_complete && (outcome != ControlFlow::Continue(()) || physical_count != member_count) {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    Ok(SetScanResultV3 {
        members,
        declared_member_count,
    })
}

pub(super) fn collection_cardinality_latest_at_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    family: StructureCollectionFamily,
    logical_time_micros: i64,
) -> Result<usize, NativeRuntimeError> {
    let (_, state) =
        visible_collection_state_v3(pages, blobs, pool, tree, key, family, logical_time_micros)?;
    usize::try_from(state.logical_items()).map_err(|_| NativeRuntimeError::InvalidStructureTree)
}

pub(super) fn collection_ttl_latest_v3(
    pages: &PageStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    family: StructureCollectionFamily,
    logical_time_micros: i64,
) -> Result<Ttl, NativeRuntimeError> {
    let Some((_, state)) = live_collection_state_v3(pages, pool, tree, key, family)? else {
        return Ok(Ttl::Missing);
    };
    Ok(match state.expires_at_micros() {
        None => Ttl::Persistent,
        Some(expiry) if expiry > logical_time_micros => {
            Ttl::RemainingMicros(expiry.saturating_sub(logical_time_micros))
        }
        Some(_) => Ttl::Missing,
    })
}

pub(super) fn hash_field_ttl_latest_v3(
    pages: &PageStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    field: &[u8],
    logical_time_micros: i64,
) -> Result<Ttl, NativeRuntimeError> {
    let Some((incarnation, state)) =
        live_collection_state_v3(pages, pool, tree, key, StructureCollectionFamily::Hash)?
    else {
        return Ok(Ttl::Missing);
    };
    if state
        .expires_at_micros()
        .is_some_and(|expiry| expiry <= logical_time_micros)
    {
        return Ok(Ttl::Missing);
    }
    let field_key =
        encode_collection_child_key(STRUCTURE_HASH_FIELD_PREFIX, key, incarnation, field)
            .map_err(map_codec_error)?;
    let Some(encoded) = tree.get_cached_pinned(pages, pool, &field_key)? else {
        return Ok(Ttl::Missing);
    };
    if is_structure_tombstone(encoded.bytes()) {
        return Ok(Ttl::Missing);
    }
    Ok(match structure_value_expiry(encoded.bytes())? {
        None => Ttl::Persistent,
        Some(expiry) if expiry > logical_time_micros => {
            Ttl::RemainingMicros(expiry.saturating_sub(logical_time_micros))
        }
        Some(_) => Ttl::Missing,
    })
}

pub(super) fn sorted_set_score_latest_v3(
    pages: &PageStore,
    blobs: &BlobStore,
    pool: &BufferPool,
    tree: BTree,
    key: &[u8],
    member: &[u8],
) -> Result<Option<f64>, NativeRuntimeError> {
    let (incarnation, state) = visible_collection_state_v3(
        pages,
        blobs,
        pool,
        tree,
        key,
        StructureCollectionFamily::SortedSet,
        i64::MIN,
    )?;
    if !matches!(state, CollectionStateV3::SortedSet { .. }) {
        return Err(NativeRuntimeError::InvalidStructureTree);
    }
    let member_key =
        encode_collection_child_key(STRUCTURE_SORTED_SET_MEMBER_PREFIX, key, incarnation, member)
            .map_err(map_codec_error)?;
    tree.get_cached_pinned(pages, pool, &member_key)?
        .map(|encoded| decode_sorted_set_score(encoded.bytes()))
        .transpose()
        .map(|score| score.flatten().map(SortedSetScore::value))
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn create() -> Result<Self, std::io::Error> {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "hyphae-structure-v3-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path)?;
            Ok(Self(path))
        }

        fn page_file(&self) -> PathBuf {
            self.0.join("pages.hydb")
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(self.path());
        }
    }

    fn incarnation() -> Result<StructureIncarnation, Box<dyn Error>> {
        Ok(StructureIncarnation::new(
            TransactionId::new(0x0102_0304_0506_0708_1112_1314_1516_1718)?,
            0x2122_2324,
        ))
    }

    const RETIRED_SET_MEMBERS: [&[u8]; 5] = [b"a", b"b", b"c", b"d", b"e"];

    fn retired_set_values() -> Vec<Vec<u8>> {
        RETIRED_SET_MEMBERS
            .iter()
            .map(|member| member.to_vec())
            .collect()
    }

    fn seed_physical_set(
        pages: &mut PageStore,
        mut tree: BTree,
        incarnation: StructureIncarnation,
    ) -> Result<BTree, Box<dyn Error>> {
        tree = create_set_v3_in_tree(pages, tree, Csn::new(2)?, b"members", incarnation)?;
        for (offset, member) in RETIRED_SET_MEMBERS.into_iter().enumerate() {
            let (updated, added) = add_set_member_v3_in_tree(
                pages,
                tree,
                Csn::new(u64::try_from(offset)? + 3)?,
                b"members",
                member,
            )?;
            assert!(added);
            tree = updated;
        }
        Ok(tree)
    }

    fn complete_set_retirement(
        pages: &mut PageStore,
        pool: &BufferPool,
        mut tree: BTree,
        retirement_key: &[u8],
    ) -> Result<(BTree, usize), Box<dyn Error>> {
        let mut cleanup_csn = 11_u64;
        let mut steps = 0_usize;
        loop {
            let encoded = tree
                .get(pages, retirement_key)?
                .ok_or("missing retirement record")?;
            if is_structure_tombstone(&encoded) {
                return Ok((tree, steps));
            }
            let (updated, receipt, physical_mutations) = cleanup_set_retirement_v3_step(
                pages,
                pool,
                tree,
                Csn::new(cleanup_csn)?,
                retirement_key,
                2,
            )?;
            assert!(receipt.processed_entries <= 2);
            assert!(physical_mutations <= 3);
            tree = updated;
            cleanup_csn += 1;
            steps += 1;
            assert!(steps <= 4);
        }
    }

    fn assert_retired_children_and_live_recreation(
        pages: &PageStore,
        tree: BTree,
        retired: StructureIncarnation,
        live: StructureIncarnation,
    ) -> Result<(), Box<dyn Error>> {
        for member in RETIRED_SET_MEMBERS {
            let old_key = encode_collection_child_key(
                STRUCTURE_SET_MEMBER_PREFIX,
                b"members",
                retired,
                member,
            )?;
            let old_value = tree.get(pages, &old_key)?.ok_or("missing old child")?;
            assert!(is_structure_tombstone(&old_value));
        }
        let new_key =
            encode_collection_child_key(STRUCTURE_SET_MEMBER_PREFIX, b"members", live, b"new")?;
        assert!(decode_set_member_value(
            &tree.get(pages, &new_key)?.ok_or("missing new child")?
        )?);
        Ok(())
    }

    const RETIRED_HASH_FIELDS: [(&[u8], &[u8], Option<i64>); 3] = [
        (b"alpha", b"one", None),
        (b"beta", b"two", Some(500)),
        (b"gamma", b"three", Some(600)),
    ];

    fn retired_hash_values() -> Vec<HashFieldReadV3> {
        RETIRED_HASH_FIELDS
            .iter()
            .map(|(field, value, expiry)| HashFieldReadV3 {
                field: field.to_vec(),
                value: value.to_vec(),
                expires_at_micros: *expiry,
            })
            .collect()
    }

    fn seed_physical_hash(
        pages: &mut PageStore,
        mut tree: BTree,
        incarnation: StructureIncarnation,
    ) -> Result<BTree, Box<dyn Error>> {
        tree = create_hash_v3_in_tree(pages, tree, Csn::new(2)?, b"record", incarnation)?;
        let blob_references = BTreeMap::new();
        for (offset, (field, value, expiry)) in RETIRED_HASH_FIELDS.into_iter().enumerate() {
            let (updated, inserted) = put_hash_field_v3_in_tree(
                pages,
                tree,
                Csn::new(u64::try_from(offset)? + 3)?,
                HashFieldWriteV3 {
                    key: b"record",
                    field,
                    value,
                    expires_at_micros: expiry,
                },
                &blob_references,
            )?;
            assert!(inserted);
            tree = updated;
        }
        Ok(tree)
    }

    fn complete_hash_retirement(
        pages: &mut PageStore,
        pool: &BufferPool,
        mut tree: BTree,
        retirement_key: &[u8],
    ) -> Result<(BTree, usize), Box<dyn Error>> {
        let mut cleanup_csn = 9_u64;
        let mut steps = 0_usize;
        loop {
            let encoded = tree
                .get(pages, retirement_key)?
                .ok_or("missing hash retirement record")?;
            if is_structure_tombstone(&encoded) {
                return Ok((tree, steps));
            }
            let (updated, receipt, physical_mutations) = cleanup_hash_retirement_v3_step(
                pages,
                pool,
                tree,
                Csn::new(cleanup_csn)?,
                retirement_key,
                2,
            )?;
            assert!(receipt.processed_entries <= 2);
            assert!(physical_mutations <= 5);
            tree = updated;
            cleanup_csn += 1;
            steps += 1;
            assert!(steps <= 3);
        }
    }

    fn assert_retired_hash_entries(
        pages: &PageStore,
        tree: BTree,
        retired: StructureIncarnation,
    ) -> Result<(), Box<dyn Error>> {
        for (field, _, expiry) in RETIRED_HASH_FIELDS {
            let old_key = encode_collection_child_key(
                STRUCTURE_HASH_FIELD_PREFIX,
                b"record",
                retired,
                field,
            )?;
            assert!(is_structure_tombstone(
                &tree.get(pages, &old_key)?.ok_or("missing old hash field")?
            ));
            if let Some(expiry) = expiry {
                let expiry_key = encode_collection_expiry_key(
                    crate::STRUCTURE_HASH_FIELD_EXPIRY_PREFIX,
                    expiry,
                    b"record",
                    retired,
                    field,
                )?;
                assert_eq!(
                    tree.get(pages, &expiry_key)?,
                    Some(vec![STRUCTURE_EXPIRY_TOMBSTONE])
                );
            }
        }
        Ok(())
    }

    fn retired_list_chunks() -> Vec<Vec<Vec<u8>>> {
        vec![
            vec![b"a".to_vec(), b"bb".to_vec()],
            vec![b"ccc".to_vec()],
            vec![b"dddd".to_vec(), b"eeeee".to_vec()],
        ]
    }

    fn retired_list_values() -> Vec<Vec<u8>> {
        retired_list_chunks().into_iter().flatten().collect()
    }

    fn seed_physical_list(
        pages: &mut PageStore,
        mut tree: BTree,
        incarnation: StructureIncarnation,
    ) -> Result<BTree, Box<dyn Error>> {
        tree = create_list_v3_in_tree(pages, tree, Csn::new(2)?, b"queue", incarnation)?;
        for (offset, values) in retired_list_chunks().into_iter().enumerate() {
            tree = append_list_chunk_v3_in_tree(
                pages,
                tree,
                Csn::new(u64::try_from(offset)? + 3)?,
                b"queue",
                &values,
                &BTreeMap::new(),
            )?;
        }
        Ok(tree)
    }

    fn read_physical_list_range(
        pages: &PageStore,
        blobs: &BlobStore,
        pool: &BufferPool,
        tree: BTree,
    ) -> Result<Vec<Vec<u8>>, NativeRuntimeError> {
        let plan = prepare_list_range_latest_v3(
            pages,
            blobs,
            pool,
            tree,
            ListRangeRequestV3 {
                key: b"queue",
                start: 0,
                stop: -1,
                logical_time_micros: 0,
            },
        )?;
        list_range_latest_at_v3(pages, blobs, pool, tree, b"queue", &plan)
    }

    fn complete_list_retirement(
        pages: &mut PageStore,
        pool: &BufferPool,
        mut tree: BTree,
        retirement_key: &[u8],
    ) -> Result<(BTree, usize), Box<dyn Error>> {
        let mut cleanup_csn = 9_u64;
        let mut steps = 0_usize;
        loop {
            let encoded = tree
                .get(pages, retirement_key)?
                .ok_or("missing list retirement record")?;
            if is_structure_tombstone(&encoded) {
                return Ok((tree, steps));
            }
            let (updated, receipt, physical_mutations) = cleanup_list_retirement_v3_step(
                pages,
                pool,
                tree,
                Csn::new(cleanup_csn)?,
                retirement_key,
                2,
            )?;
            assert!(receipt.processed_entries <= 2);
            assert!(physical_mutations <= 3);
            tree = updated;
            cleanup_csn += 1;
            steps += 1;
            assert!(steps <= 3);
        }
    }

    fn assert_retired_list_chunks(
        pages: &PageStore,
        tree: BTree,
        retired: StructureIncarnation,
    ) -> Result<(), Box<dyn Error>> {
        for chunk_id in 0_i64..3 {
            let old_key = encode_collection_child_key(
                STRUCTURE_LIST_CHUNK_PREFIX,
                b"queue",
                retired,
                &encode_list_chunk_identity_v3(chunk_id),
            )?;
            assert!(is_structure_tombstone(
                &tree.get(pages, &old_key)?.ok_or("missing old list chunk")?
            ));
        }
        Ok(())
    }

    fn retired_stream_entries() -> Vec<(u64, crate::model::StreamFields)> {
        vec![
            (2, vec![(b"kind".to_vec(), b"start".to_vec())]),
            (9, vec![(b"kind".to_vec(), b"middle".to_vec())]),
            (42, vec![(b"kind".to_vec(), b"finish".to_vec())]),
        ]
    }

    fn seed_physical_stream(
        pages: &mut PageStore,
        mut tree: BTree,
        incarnation: StructureIncarnation,
    ) -> Result<BTree, Box<dyn Error>> {
        tree = create_stream_v3_in_tree(pages, tree, Csn::new(2)?, b"events", incarnation)?;
        for (offset, (id, fields)) in retired_stream_entries().into_iter().enumerate() {
            tree = append_stream_entry_v3_in_tree(
                pages,
                tree,
                Csn::new(u64::try_from(offset)? + 3)?,
                b"events",
                id,
                &fields,
            )?;
        }
        Ok(tree)
    }

    fn read_physical_stream_range(
        pages: &PageStore,
        blobs: &BlobStore,
        pool: &BufferPool,
        tree: BTree,
    ) -> Result<Vec<(u64, crate::model::StreamFields)>, NativeRuntimeError> {
        let plan = prepare_stream_range_latest_v3(
            pages,
            blobs,
            pool,
            tree,
            StreamRangeRequestV3 {
                key: b"events",
                start: 1,
                end: u64::MAX,
                limit: 8,
                logical_time_micros: 0,
            },
        )?;
        stream_range_latest_at_v3(pages, pool, tree, b"events", &plan, 8)
    }

    fn complete_stream_retirement(
        pages: &mut PageStore,
        pool: &BufferPool,
        mut tree: BTree,
        retirement_key: &[u8],
    ) -> Result<(BTree, usize), Box<dyn Error>> {
        let mut cleanup_csn = 9_u64;
        let mut steps = 0_usize;
        loop {
            let encoded = tree
                .get(pages, retirement_key)?
                .ok_or("missing stream retirement record")?;
            if is_structure_tombstone(&encoded) {
                return Ok((tree, steps));
            }
            let (updated, receipt, physical_mutations) = cleanup_stream_retirement_v3_step(
                pages,
                pool,
                tree,
                Csn::new(cleanup_csn)?,
                retirement_key,
                2,
            )?;
            assert!(receipt.processed_entries <= 2);
            assert!(physical_mutations <= 3);
            tree = updated;
            cleanup_csn += 1;
            steps += 1;
            assert!(steps <= 3);
        }
    }

    fn assert_retired_stream_entries(
        pages: &PageStore,
        tree: BTree,
        retired: StructureIncarnation,
    ) -> Result<(), Box<dyn Error>> {
        for (id, _) in retired_stream_entries() {
            let old_key = encode_collection_child_key(
                STRUCTURE_STREAM_ENTRY_PREFIX,
                b"events",
                retired,
                &id.to_be_bytes(),
            )?;
            assert!(is_structure_tombstone(
                &tree
                    .get(pages, &old_key)?
                    .ok_or("missing old stream entry")?
            ));
        }
        Ok(())
    }

    fn retired_sorted_set_members() -> Result<Vec<SortedSetMemberV3>, Box<dyn Error>> {
        Ok(vec![
            SortedSetMemberV3 {
                member: b"alpha".to_vec(),
                score: SortedSetScore::new(1.5).ok_or("invalid alpha score")?,
            },
            SortedSetMemberV3 {
                member: b"beta".to_vec(),
                score: SortedSetScore::new(-2.0).ok_or("invalid beta score")?,
            },
            SortedSetMemberV3 {
                member: b"gamma".to_vec(),
                score: SortedSetScore::new(1.5).ok_or("invalid gamma score")?,
            },
        ])
    }

    fn seed_physical_sorted_set(
        pages: &mut PageStore,
        mut tree: BTree,
        incarnation: StructureIncarnation,
    ) -> Result<BTree, Box<dyn Error>> {
        tree = create_sorted_set_v3_in_tree(pages, tree, Csn::new(2)?, b"rank", incarnation)?;
        for (offset, entry) in retired_sorted_set_members()?.into_iter().enumerate() {
            let (updated, inserted) = upsert_sorted_set_member_v3_in_tree(
                pages,
                tree,
                Csn::new(u64::try_from(offset)? + 3)?,
                b"rank",
                &entry.member,
                entry.score,
            )?;
            assert!(inserted);
            tree = updated;
        }
        Ok(tree)
    }

    fn read_physical_sorted_set_range(
        pages: &PageStore,
        blobs: &BlobStore,
        pool: &BufferPool,
        tree: BTree,
    ) -> Result<Vec<SortedSetEntry>, NativeRuntimeError> {
        let plan = prepare_sorted_set_order_plan_v3(pages, blobs, pool, tree, b"rank", None)?;
        sorted_set_rank_range_latest_v3(
            pages,
            pool,
            tree,
            b"rank",
            &plan,
            SortedSetRankRangeRequestV3 {
                start: 0,
                stop: -1,
                direction: SortedSetDirection::Ascending,
            },
        )
    }

    fn complete_sorted_set_retirement(
        pages: &mut PageStore,
        pool: &BufferPool,
        mut tree: BTree,
        retirement_key: &[u8],
    ) -> Result<(BTree, usize), Box<dyn Error>> {
        let mut cleanup_csn = 9_u64;
        let mut steps = 0_usize;
        loop {
            let encoded = tree
                .get(pages, retirement_key)?
                .ok_or("missing sorted-set retirement record")?;
            if is_structure_tombstone(&encoded) {
                return Ok((tree, steps));
            }
            let (updated, receipt, physical_mutations) = cleanup_sorted_set_retirement_v3_step(
                pages,
                pool,
                tree,
                Csn::new(cleanup_csn)?,
                retirement_key,
                4,
            )?;
            assert!(receipt.processed_entries <= 2);
            assert!(physical_mutations <= 5);
            tree = updated;
            cleanup_csn += 1;
            steps += 1;
            assert!(steps <= 3);
        }
    }

    fn assert_retired_sorted_set_entries(
        pages: &PageStore,
        tree: BTree,
        retired: StructureIncarnation,
    ) -> Result<(), Box<dyn Error>> {
        for entry in retired_sorted_set_members()? {
            let member_key = encode_collection_child_key(
                STRUCTURE_SORTED_SET_MEMBER_PREFIX,
                b"rank",
                retired,
                &entry.member,
            )?;
            let order_key = v3_sorted_set_order_key(b"rank", retired, entry.score, &entry.member)?;
            for key in [member_key, order_key] {
                assert!(is_structure_tombstone(
                    &tree
                        .get(pages, &key)?
                        .ok_or("missing old sorted-set entry")?
                ));
            }
        }
        Ok(())
    }

    #[test]
    fn incarnation_child_and_expiry_keys_have_golden_bytes() -> Result<(), Box<dyn Error>> {
        let child = encode_collection_child_key(
            STRUCTURE_SET_MEMBER_PREFIX,
            b"set",
            incarnation()?,
            b"member",
        )?;
        let mut expected = vec![STRUCTURE_SET_MEMBER_PREFIX, 0, 0, 0, 3];
        expected.extend_from_slice(b"set");
        expected.extend_from_slice(&[
            1, 2, 3, 4, 5, 6, 7, 8, 17, 18, 19, 20, 21, 22, 23, 24, 33, 34, 35, 36,
        ]);
        expected.extend_from_slice(b"member");
        assert_eq!(child, expected);
        let decoded = decode_collection_child_key(&child)?;
        assert_eq!(decoded.prefix, STRUCTURE_SET_MEMBER_PREFIX);
        assert_eq!(decoded.collection_key, b"set");
        assert_eq!(decoded.incarnation, incarnation()?);
        assert_eq!(decoded.child_identity, b"member");

        let expiry = encode_collection_expiry_key(
            crate::STRUCTURE_HASH_FIELD_EXPIRY_PREFIX,
            -1,
            b"map",
            incarnation()?,
            b"field",
        )?;
        let decoded = decode_collection_expiry_key(&expiry)?;
        assert_eq!(decoded.expires_at_micros, -1);
        assert_eq!(decoded.collection_key, b"map");
        assert_eq!(decoded.incarnation, incarnation()?);
        assert_eq!(decoded.child_identity, b"field");
        Ok(())
    }

    #[test]
    fn mutation_index_is_the_zero_based_incarnation_ordinal() -> Result<(), Box<dyn Error>> {
        let transaction_id = incarnation()?.transaction_id();
        assert_eq!(
            StructureIncarnation::from_mutation_index(transaction_id, 0)?.mutation_ordinal(),
            0
        );
        assert_ne!(
            StructureIncarnation::from_mutation_index(transaction_id, 3)?,
            StructureIncarnation::from_mutation_index(transaction_id, 4)?
        );
        if usize::BITS > u32::BITS {
            assert_eq!(
                StructureIncarnation::from_mutation_index(transaction_id, u32::MAX as usize + 1),
                Err(StructureV3CodecError::MutationOrdinalOverflow)
            );
        }
        Ok(())
    }

    #[test]
    fn metadata_and_retirement_round_trip_canonical_state() -> Result<(), Box<dyn Error>> {
        let live = CollectionMetadataV3::Live {
            family: StructureCollectionFamily::List,
            incarnation: incarnation()?,
            family_payload: b"family-metadata".to_vec(),
        };
        assert_eq!(
            decode_collection_metadata(&encode_collection_metadata(&live)),
            Ok(live)
        );
        let tombstone = CollectionMetadataV3::Tombstone {
            family: StructureCollectionFamily::List,
            retired_incarnation: incarnation()?,
        };
        assert_eq!(
            decode_collection_metadata(&encode_collection_metadata(&tombstone)),
            Ok(tombstone)
        );

        let key = encode_retirement_key(b"queue", incarnation()?)?;
        assert_eq!(
            decode_retirement_key(&key),
            Ok((&b"queue"[..], incarnation()?))
        );
        let record = RetirementRecordV3 {
            family: StructureCollectionFamily::List,
            declared_logical_items: 65,
            remaining_logical_items: 65,
            remaining_primary_entries: 2,
            remaining_secondary_entries: 0,
            remaining_expiry_entries: 0,
            remaining_logical_bytes: 4096,
            list_head_chunk: -1,
            list_tail_chunk: 0,
            stream_last_id: 0,
            exclusive_cursor: Some(b"last-physical-child".to_vec()),
        };
        let encoded = encode_retirement_record(&record)?;
        assert_eq!(decode_retirement_record(&encoded), Ok(record));
        Ok(())
    }

    #[test]
    fn typed_metadata_covers_every_collection_family_and_rejects_cross_family_payloads()
    -> Result<(), Box<dyn Error>> {
        let states = [
            CollectionStateV3::Hash {
                field_count: 3,
                field_expiry_count: 2,
                expires_at_micros: Some(-7),
            },
            CollectionStateV3::Set {
                member_count: 4,
                expires_at_micros: None,
            },
            CollectionStateV3::List {
                element_count: 65,
                logical_value_bytes: 4096,
                head_chunk: -1,
                tail_chunk: 1,
                expires_at_micros: Some(i64::MAX),
            },
            CollectionStateV3::SortedSet {
                member_count: 5,
                expires_at_micros: None,
            },
            CollectionStateV3::Stream {
                entry_count: 3,
                last_id: 9,
                expires_at_micros: Some(10),
            },
        ];
        for state in states {
            let metadata = TypedCollectionMetadataV3::Live {
                incarnation: incarnation()?,
                state,
            };
            let encoded = encode_typed_collection_metadata(&metadata)?;
            assert_eq!(decode_typed_collection_metadata(&encoded), Ok(metadata));
        }

        let list = TypedCollectionMetadataV3::Live {
            incarnation: incarnation()?,
            state: CollectionStateV3::List {
                element_count: 1,
                logical_value_bytes: 8,
                head_chunk: 0,
                tail_chunk: 0,
                expires_at_micros: None,
            },
        };
        let mut wrong_family = encode_typed_collection_metadata(&list)?;
        wrong_family[8] = StructureCollectionFamily::Hash as u8;
        assert_eq!(
            decode_typed_collection_metadata(&wrong_family),
            Err(StructureV3CodecError::Malformed)
        );

        let invalid_list = TypedCollectionMetadataV3::Live {
            incarnation: incarnation()?,
            state: CollectionStateV3::List {
                element_count: 1,
                logical_value_bytes: 8,
                head_chunk: 2,
                tail_chunk: 1,
                expires_at_micros: None,
            },
        };
        assert_eq!(
            encode_typed_collection_metadata(&invalid_list),
            Err(StructureV3CodecError::Malformed)
        );
        let invalid_stream = TypedCollectionMetadataV3::Live {
            incarnation: incarnation()?,
            state: CollectionStateV3::Stream {
                entry_count: 2,
                last_id: 1,
                expires_at_micros: None,
            },
        };
        assert_eq!(
            encode_typed_collection_metadata(&invalid_stream),
            Err(StructureV3CodecError::Malformed)
        );
        Ok(())
    }

    #[test]
    fn codecs_reject_zero_transaction_truncation_reserved_bytes_and_trailing_identity()
    -> Result<(), Box<dyn Error>> {
        let mut zero_transaction = encode_collection_child_key(
            STRUCTURE_HASH_FIELD_PREFIX,
            b"map",
            incarnation()?,
            b"field",
        )?;
        zero_transaction[8..24].fill(0);
        assert_eq!(
            decode_collection_child_key(&zero_transaction),
            Err(StructureV3CodecError::Malformed)
        );

        let mut metadata = encode_collection_metadata(&CollectionMetadataV3::Tombstone {
            family: StructureCollectionFamily::Hash,
            retired_incarnation: incarnation()?,
        });
        metadata[10] = 1;
        assert_eq!(
            decode_collection_metadata(&metadata),
            Err(StructureV3CodecError::Malformed)
        );

        let mut retirement_key = encode_retirement_key(b"map", incarnation()?)?;
        retirement_key.push(0);
        assert_eq!(
            decode_retirement_key(&retirement_key),
            Err(StructureV3CodecError::Malformed)
        );

        let record = RetirementRecordV3 {
            family: StructureCollectionFamily::Hash,
            declared_logical_items: 1,
            remaining_logical_items: 1,
            remaining_primary_entries: 1,
            remaining_secondary_entries: 0,
            remaining_expiry_entries: 0,
            remaining_logical_bytes: 0,
            list_head_chunk: 0,
            list_tail_chunk: 0,
            stream_last_id: 0,
            exclusive_cursor: None,
        };
        let encoded = encode_retirement_record(&record)?;
        let mut impossible_remaining = encoded.clone();
        impossible_remaining[24..32].copy_from_slice(&2_u64.to_be_bytes());
        assert_eq!(
            decode_retirement_record(&impossible_remaining),
            Err(StructureV3CodecError::Malformed)
        );
        let mut encoded = encoded;
        encoded.truncate(encoded.len() - 1);
        assert_eq!(
            decode_retirement_record(&encoded),
            Err(StructureV3CodecError::Malformed)
        );
        Ok(())
    }

    #[test]
    fn retirement_cursor_is_fenced_to_family_key_and_incarnation() -> Result<(), Box<dyn Error>> {
        let retirement_key = encode_retirement_key(b"members", incarnation()?)?;
        let cursor = encode_collection_child_key(
            STRUCTURE_SET_MEMBER_PREFIX,
            b"members",
            incarnation()?,
            b"last",
        )?;
        let mut record = RetirementRecordV3 {
            family: StructureCollectionFamily::Set,
            declared_logical_items: 2,
            remaining_logical_items: 1,
            remaining_primary_entries: 1,
            remaining_secondary_entries: 0,
            remaining_expiry_entries: 0,
            remaining_logical_bytes: 0,
            list_head_chunk: 0,
            list_tail_chunk: 0,
            stream_last_id: 0,
            exclusive_cursor: Some(cursor),
        };
        assert_eq!(validate_retirement_state(&retirement_key, &record), Ok(()));

        record.exclusive_cursor = Some(encode_collection_child_key(
            STRUCTURE_HASH_FIELD_PREFIX,
            b"members",
            incarnation()?,
            b"last",
        )?);
        assert_eq!(
            validate_retirement_state(&retirement_key, &record),
            Err(StructureV3CodecError::Malformed)
        );

        record.exclusive_cursor = Some(encode_collection_child_key(
            STRUCTURE_SET_MEMBER_PREFIX,
            b"other",
            incarnation()?,
            b"last",
        )?);
        assert_eq!(
            validate_retirement_state(&retirement_key, &record),
            Err(StructureV3CodecError::Malformed)
        );
        Ok(())
    }

    #[test]
    fn codecs_enforce_the_complete_physical_key_limit() -> Result<(), Box<dyn Error>> {
        let oversized = vec![0_u8; BTREE_MAX_KEY_SIZE];
        assert_eq!(
            encode_collection_child_key(
                STRUCTURE_STREAM_ENTRY_PREFIX,
                &oversized,
                incarnation()?,
                &[]
            ),
            Err(StructureV3CodecError::IdentityTooLarge)
        );
        assert_eq!(
            encode_retirement_key(&oversized, incarnation()?),
            Err(StructureV3CodecError::IdentityTooLarge)
        );
        Ok(())
    }

    #[test]
    fn retirement_steps_are_bounded_monotonic_and_terminal_only_at_exact_zero()
    -> Result<(), Box<dyn Error>> {
        let key = encode_retirement_key(b"members", incarnation()?)?;
        let first = encode_collection_child_key(
            STRUCTURE_SET_MEMBER_PREFIX,
            b"members",
            incarnation()?,
            b"a",
        )?;
        let second = encode_collection_child_key(
            STRUCTURE_SET_MEMBER_PREFIX,
            b"members",
            incarnation()?,
            b"b",
        )?;
        let initial = RetirementRecordV3::new(StructureCollectionFamily::Set, 2, 2, 0, 0, 0)?;
        let first_candidates = [
            RetirementCandidateV3 {
                physical_key: &first,
                live: true,
                logical_items: 1,
                logical_bytes: 0,
                associated_secondary_entries: 0,
                associated_expiry_entries: 0,
            },
            RetirementCandidateV3 {
                physical_key: &second,
                live: true,
                logical_items: 1,
                logical_bytes: 0,
                associated_secondary_entries: 0,
                associated_expiry_entries: 0,
            },
        ];
        let first_step = advance_retirement_record(&key, &initial, &first_candidates, 2, true)?;
        assert_eq!(first_step.processed_entries, 2);
        assert!(!first_step.more_remaining);
        assert_eq!(first_step.record.remaining_logical_items, 0);
        assert_eq!(first_step.record.remaining_primary_entries, 0);
        assert_eq!(first_step.record.remaining_expiry_entries, 0);
        assert_eq!(
            first_step.record.exclusive_cursor.as_deref(),
            Some(second.as_slice())
        );

        Ok(())
    }

    #[test]
    fn hash_retirement_consumes_at_most_one_direct_field_expiry() -> Result<(), Box<dyn Error>> {
        let retirement_key = encode_retirement_key(b"record", incarnation()?)?;
        let field_key = encode_collection_child_key(
            STRUCTURE_HASH_FIELD_PREFIX,
            b"record",
            incarnation()?,
            b"field",
        )?;
        let record = RetirementRecordV3::new(StructureCollectionFamily::Hash, 1, 1, 0, 1, 0)?;
        let candidate = RetirementCandidateV3 {
            physical_key: &field_key,
            live: true,
            logical_items: 1,
            logical_bytes: 0,
            associated_secondary_entries: 0,
            associated_expiry_entries: 1,
        };
        let step = advance_retirement_record(&retirement_key, &record, &[candidate], 1, true)?;
        assert!(!step.more_remaining);
        assert_eq!(step.record.remaining_logical_items, 0);
        assert_eq!(step.record.remaining_expiry_entries, 0);

        let invalid = RetirementCandidateV3 {
            associated_expiry_entries: 2,
            ..candidate
        };
        assert_eq!(
            advance_retirement_record(&retirement_key, &record, &[invalid], 1, true),
            Err(StructureV3CodecError::InvalidRetirementStep)
        );
        Ok(())
    }

    #[test]
    fn list_retirement_requires_contiguous_live_chunk_identities() -> Result<(), Box<dyn Error>> {
        let retirement_key = encode_retirement_key(b"queue", incarnation()?)?;
        let first_key = encode_collection_child_key(
            STRUCTURE_LIST_CHUNK_PREFIX,
            b"queue",
            incarnation()?,
            &encode_list_chunk_identity_v3(-1),
        )?;
        let second_key = encode_collection_child_key(
            STRUCTURE_LIST_CHUNK_PREFIX,
            b"queue",
            incarnation()?,
            &encode_list_chunk_identity_v3(0),
        )?;
        let record = RetirementRecordV3::new_list(3, 9, -1, 0)?;
        let first = RetirementCandidateV3 {
            physical_key: &first_key,
            live: true,
            logical_items: 2,
            logical_bytes: 5,
            associated_secondary_entries: 0,
            associated_expiry_entries: 0,
        };
        let second = RetirementCandidateV3 {
            physical_key: &second_key,
            live: true,
            logical_items: 1,
            logical_bytes: 4,
            associated_secondary_entries: 0,
            associated_expiry_entries: 0,
        };
        let step = advance_retirement_record(&retirement_key, &record, &[first, second], 2, true)?;
        assert!(!step.more_remaining);
        assert_eq!(step.record.remaining_logical_items, 0);
        assert_eq!(step.record.remaining_primary_entries, 0);
        assert_eq!(step.record.remaining_logical_bytes, 0);
        assert_eq!(
            advance_retirement_record(&retirement_key, &record, &[second], 1, false),
            Err(StructureV3CodecError::InvalidRetirementStep)
        );
        Ok(())
    }

    #[test]
    fn stream_retirement_requires_the_declared_terminal_id() -> Result<(), Box<dyn Error>> {
        let retirement_key = encode_retirement_key(b"events", incarnation()?)?;
        let first_key = encode_collection_child_key(
            STRUCTURE_STREAM_ENTRY_PREFIX,
            b"events",
            incarnation()?,
            &2_u64.to_be_bytes(),
        )?;
        let last_key = encode_collection_child_key(
            STRUCTURE_STREAM_ENTRY_PREFIX,
            b"events",
            incarnation()?,
            &9_u64.to_be_bytes(),
        )?;
        let wrong_last_key = encode_collection_child_key(
            STRUCTURE_STREAM_ENTRY_PREFIX,
            b"events",
            incarnation()?,
            &8_u64.to_be_bytes(),
        )?;
        let record = RetirementRecordV3::new_stream(2, 9)?;
        let first = RetirementCandidateV3 {
            physical_key: &first_key,
            live: true,
            logical_items: 1,
            logical_bytes: 0,
            associated_secondary_entries: 0,
            associated_expiry_entries: 0,
        };
        let last = RetirementCandidateV3 {
            physical_key: &last_key,
            ..first
        };
        assert!(
            !advance_retirement_record(&retirement_key, &record, &[first, last], 2, true,)?
                .more_remaining
        );
        let wrong_last = RetirementCandidateV3 {
            physical_key: &wrong_last_key,
            ..first
        };
        assert_eq!(
            advance_retirement_record(&retirement_key, &record, &[first, wrong_last], 2, true,),
            Err(StructureV3CodecError::InvalidRetirementStep)
        );
        Ok(())
    }

    #[test]
    fn sorted_set_retirement_consumes_one_validated_order_entry_per_member()
    -> Result<(), Box<dyn Error>> {
        let retirement_key = encode_retirement_key(b"rank", incarnation()?)?;
        let member_key = encode_collection_child_key(
            STRUCTURE_SORTED_SET_MEMBER_PREFIX,
            b"rank",
            incarnation()?,
            b"member",
        )?;
        let record = RetirementRecordV3::new(StructureCollectionFamily::SortedSet, 1, 1, 1, 0, 0)?;
        let candidate = RetirementCandidateV3 {
            physical_key: &member_key,
            live: true,
            logical_items: 1,
            logical_bytes: 0,
            associated_secondary_entries: 1,
            associated_expiry_entries: 0,
        };
        let step = advance_retirement_record(&retirement_key, &record, &[candidate], 1, true)?;
        assert!(!step.more_remaining);
        assert_eq!(step.record.remaining_primary_entries, 0);
        assert_eq!(step.record.remaining_secondary_entries, 0);

        let missing_order = RetirementCandidateV3 {
            associated_secondary_entries: 0,
            ..candidate
        };
        assert_eq!(
            advance_retirement_record(&retirement_key, &record, &[missing_order], 1, true),
            Err(StructureV3CodecError::InvalidRetirementStep)
        );
        Ok(())
    }

    #[test]
    fn retirement_steps_reject_underflow_empty_progress_and_nonmonotonic_keys()
    -> Result<(), Box<dyn Error>> {
        let key = encode_retirement_key(b"members", incarnation()?)?;
        let member = encode_collection_child_key(
            STRUCTURE_SET_MEMBER_PREFIX,
            b"members",
            incarnation()?,
            b"a",
        )?;
        let record = RetirementRecordV3::new(StructureCollectionFamily::Set, 1, 1, 0, 0, 0)?;
        assert_eq!(
            advance_retirement_record(&key, &record, &[], 1, true),
            Err(StructureV3CodecError::InvalidRetirementStep)
        );
        let candidate = RetirementCandidateV3 {
            physical_key: &member,
            live: true,
            logical_items: 1,
            logical_bytes: 0,
            associated_secondary_entries: 0,
            associated_expiry_entries: 0,
        };
        let exhausted = RetirementRecordV3::new(StructureCollectionFamily::Set, 0, 0, 0, 0, 0)?;
        assert_eq!(
            advance_retirement_record(&key, &exhausted, &[candidate], 1, false),
            Err(StructureV3CodecError::InvalidRetirementStep)
        );

        let mut after = record.clone();
        after.exclusive_cursor = Some(member.clone());
        assert_eq!(
            advance_retirement_record(&key, &after, &[candidate], 1, false),
            Err(StructureV3CodecError::InvalidRetirementStep)
        );
        assert_eq!(
            advance_retirement_record(&key, &after, &[candidate], 0, false),
            Err(StructureV3CodecError::InvalidRetirementStep)
        );
        Ok(())
    }

    #[test]
    fn retirement_visits_tombstones_without_consuming_live_counters() -> Result<(), Box<dyn Error>>
    {
        let key = encode_retirement_key(b"members", incarnation()?)?;
        let tombstone_key = encode_collection_child_key(
            STRUCTURE_SET_MEMBER_PREFIX,
            b"members",
            incarnation()?,
            b"already-removed",
        )?;
        let record = RetirementRecordV3 {
            family: StructureCollectionFamily::Set,
            declared_logical_items: 0,
            remaining_logical_items: 0,
            remaining_primary_entries: 0,
            remaining_secondary_entries: 0,
            remaining_expiry_entries: 0,
            remaining_logical_bytes: 0,
            list_head_chunk: 0,
            list_tail_chunk: 0,
            stream_last_id: 0,
            exclusive_cursor: None,
        };
        let step = advance_retirement_record(
            &key,
            &record,
            &[RetirementCandidateV3 {
                physical_key: &tombstone_key,
                live: false,
                logical_items: 0,
                logical_bytes: 0,
                associated_secondary_entries: 0,
                associated_expiry_entries: 0,
            }],
            1,
            true,
        )?;
        assert_eq!(step.processed_entries, 1);
        assert!(!step.more_remaining);
        assert_eq!(step.record.remaining_primary_entries, 0);
        assert_eq!(
            step.record.exclusive_cursor.as_deref(),
            Some(tombstone_key.as_slice())
        );
        Ok(())
    }

    #[test]
    fn physical_set_delete_recreate_and_incremental_cleanup_are_incarnation_fenced()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create()?;
        let mut pages = PageStore::create(temporary.page_file())?;
        let pool = BufferPool::new(32, 4)?;
        let first_incarnation =
            StructureIncarnation::from_mutation_index(TransactionId::new(101)?, 0)?;
        let second_incarnation =
            StructureIncarnation::from_mutation_index(TransactionId::new(102)?, 0)?;
        let mut tree = BTree::empty()
            .upsert(
                &mut pages,
                Csn::new(1)?,
                crate::STRUCTURE_FORMAT_KEY.to_vec(),
                STRUCTURE_FORMAT_VALUE_V3.to_vec(),
            )?
            .tree;
        tree = seed_physical_set(&mut pages, tree, first_incarnation)?;
        let historical = tree;
        assert_eq!(
            read_set_members_v3(&pages, historical, b"members")?,
            Some(retired_set_values())
        );

        let (deleted, retirement_key, foreground_mutations) =
            delete_set_v3_in_tree(&mut pages, tree, Csn::new(8)?, b"members")?;
        assert_eq!(foreground_mutations, 2);
        assert_eq!(read_set_members_v3(&pages, deleted, b"members")?, None);

        tree = create_set_v3_in_tree(
            &mut pages,
            deleted,
            Csn::new(9)?,
            b"members",
            second_incarnation,
        )?;
        let (updated, added) =
            add_set_member_v3_in_tree(&mut pages, tree, Csn::new(10)?, b"members", b"new")?;
        assert!(added);
        tree = updated;
        assert_eq!(
            read_set_members_v3(&pages, tree, b"members")?,
            Some(vec![b"new".to_vec()])
        );
        assert_eq!(
            read_set_members_v3(&pages, historical, b"members")?,
            Some(retired_set_values())
        );

        let (tree, steps) = complete_set_retirement(&mut pages, &pool, tree, &retirement_key)?;
        assert_eq!(steps, 3);
        assert_eq!(
            read_set_members_v3(&pages, tree, b"members")?,
            Some(vec![b"new".to_vec()])
        );
        assert_retired_children_and_live_recreation(
            &pages,
            tree,
            first_incarnation,
            second_incarnation,
        )?;
        Ok(())
    }

    #[test]
    fn physical_hash_delete_recreate_and_field_expiry_cleanup_are_incarnation_fenced()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create()?;
        let blobs = BlobStore::create(temporary.path())?;
        let mut pages = PageStore::create(temporary.page_file())?;
        let pool = BufferPool::new(32, 4)?;
        let first_incarnation =
            StructureIncarnation::from_mutation_index(TransactionId::new(201)?, 0)?;
        let second_incarnation =
            StructureIncarnation::from_mutation_index(TransactionId::new(202)?, 0)?;
        let mut tree = BTree::empty()
            .upsert(
                &mut pages,
                Csn::new(1)?,
                crate::STRUCTURE_FORMAT_KEY.to_vec(),
                STRUCTURE_FORMAT_VALUE_V3.to_vec(),
            )?
            .tree;
        tree = seed_physical_hash(&mut pages, tree, first_incarnation)?;
        let historical = tree;
        assert_eq!(
            read_hash_fields_v3(&pages, &blobs, historical, b"record")?,
            Some(retired_hash_values())
        );

        let (deleted, retirement_key, foreground_mutations) =
            delete_hash_v3_in_tree(&mut pages, tree, Csn::new(6)?, b"record")?;
        assert_eq!(foreground_mutations, 2);
        assert_eq!(
            read_hash_fields_v3(&pages, &blobs, deleted, b"record")?,
            None
        );

        tree = create_hash_v3_in_tree(
            &mut pages,
            deleted,
            Csn::new(7)?,
            b"record",
            second_incarnation,
        )?;
        let (updated, inserted) = put_hash_field_v3_in_tree(
            &mut pages,
            tree,
            Csn::new(8)?,
            HashFieldWriteV3 {
                key: b"record",
                field: b"new",
                value: b"value",
                expires_at_micros: None,
            },
            &BTreeMap::new(),
        )?;
        assert!(inserted);
        tree = updated;
        assert_eq!(
            read_hash_fields_v3(&pages, &blobs, tree, b"record")?,
            Some(vec![HashFieldReadV3 {
                field: b"new".to_vec(),
                value: b"value".to_vec(),
                expires_at_micros: None,
            }])
        );
        assert_eq!(
            read_hash_fields_v3(&pages, &blobs, historical, b"record")?,
            Some(retired_hash_values())
        );

        let (tree, steps) = complete_hash_retirement(&mut pages, &pool, tree, &retirement_key)?;
        assert_eq!(steps, 2);
        assert_eq!(
            read_hash_fields_v3(&pages, &blobs, tree, b"record")?,
            Some(vec![HashFieldReadV3 {
                field: b"new".to_vec(),
                value: b"value".to_vec(),
                expires_at_micros: None,
            }])
        );
        assert_retired_hash_entries(&pages, tree, first_incarnation)?;
        Ok(())
    }

    #[test]
    fn physical_list_delete_recreate_and_chunk_cleanup_are_incarnation_fenced()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create()?;
        let blobs = BlobStore::create(temporary.path())?;
        let mut pages = PageStore::create(temporary.page_file())?;
        let pool = BufferPool::new(32, 4)?;
        let first_incarnation =
            StructureIncarnation::from_mutation_index(TransactionId::new(301)?, 0)?;
        let second_incarnation =
            StructureIncarnation::from_mutation_index(TransactionId::new(302)?, 0)?;
        let mut tree = BTree::empty()
            .upsert(
                &mut pages,
                Csn::new(1)?,
                crate::STRUCTURE_FORMAT_KEY.to_vec(),
                STRUCTURE_FORMAT_VALUE_V3.to_vec(),
            )?
            .tree;
        tree = seed_physical_list(&mut pages, tree, first_incarnation)?;
        let historical = tree;
        assert_eq!(
            read_list_values_v3(&pages, &blobs, historical, b"queue")?,
            Some(retired_list_values())
        );

        let (deleted, retirement_key, foreground_mutations) =
            delete_list_v3_in_tree(&mut pages, tree, Csn::new(6)?, b"queue")?;
        assert_eq!(foreground_mutations, 2);
        assert_eq!(
            read_list_values_v3(&pages, &blobs, deleted, b"queue")?,
            None
        );

        tree = create_list_v3_in_tree(
            &mut pages,
            deleted,
            Csn::new(7)?,
            b"queue",
            second_incarnation,
        )?;
        tree = append_list_chunk_v3_in_tree(
            &mut pages,
            tree,
            Csn::new(8)?,
            b"queue",
            &[b"new".to_vec()],
            &BTreeMap::new(),
        )?;
        assert_eq!(
            read_list_values_v3(&pages, &blobs, tree, b"queue")?,
            Some(vec![b"new".to_vec()])
        );
        assert_eq!(
            read_list_values_v3(&pages, &blobs, historical, b"queue")?,
            Some(retired_list_values())
        );

        let (tree, steps) = complete_list_retirement(&mut pages, &pool, tree, &retirement_key)?;
        assert_eq!(steps, 2);
        assert_eq!(
            read_list_values_v3(&pages, &blobs, tree, b"queue")?,
            Some(vec![b"new".to_vec()])
        );
        assert_retired_list_chunks(&pages, tree, first_incarnation)?;
        Ok(())
    }

    #[test]
    fn physical_list_range_rejects_reached_corruption_and_metadata_drift()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create()?;
        let blobs = BlobStore::create(temporary.path())?;
        let mut pages = PageStore::create(temporary.page_file())?;
        let pool = BufferPool::new(32, 4)?;
        let incarnation = StructureIncarnation::from_mutation_index(TransactionId::new(303)?, 0)?;
        let tree = BTree::empty()
            .upsert(
                &mut pages,
                Csn::new(1)?,
                crate::STRUCTURE_FORMAT_KEY.to_vec(),
                STRUCTURE_FORMAT_VALUE_V3.to_vec(),
            )?
            .tree;
        let tree = seed_physical_list(&mut pages, tree, incarnation)?;
        assert_eq!(
            read_physical_list_range(&pages, &blobs, &pool, tree)?,
            retired_list_values()
        );

        let metadata_key = structure_list_meta_key(b"queue")?;
        for (element_count, logical_value_bytes, head_chunk, tail_chunk) in [
            (4, 15, 0, 2),
            (6, 15, 0, 2),
            (5, 14, 0, 2),
            (5, 15, 1, 2),
            (5, 15, 0, 1),
        ] {
            let metadata = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
                incarnation,
                state: CollectionStateV3::List {
                    element_count,
                    logical_value_bytes,
                    head_chunk,
                    tail_chunk,
                    expires_at_micros: None,
                },
            })?;
            let drifted = tree
                .upsert(&mut pages, Csn::new(10)?, metadata_key.clone(), metadata)?
                .tree;
            assert!(matches!(
                read_physical_list_range(&pages, &blobs, &pool, drifted),
                Err(NativeRuntimeError::InvalidStructureTree)
            ));
        }

        let middle_chunk = encode_collection_child_key(
            STRUCTURE_LIST_CHUNK_PREFIX,
            b"queue",
            incarnation,
            &encode_list_chunk_identity_v3(1),
        )?;
        for invalid in [structure_tombstone_value(), vec![0xff]] {
            let corrupted = tree
                .upsert(&mut pages, Csn::new(11)?, middle_chunk.clone(), invalid)?
                .tree;
            assert!(matches!(
                read_physical_list_range(&pages, &blobs, &pool, corrupted),
                Err(NativeRuntimeError::InvalidStructureTree)
            ));
        }
        Ok(())
    }

    #[test]
    fn physical_stream_delete_recreate_and_entry_cleanup_are_incarnation_fenced()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create()?;
        let mut pages = PageStore::create(temporary.page_file())?;
        let pool = BufferPool::new(32, 4)?;
        let first_incarnation =
            StructureIncarnation::from_mutation_index(TransactionId::new(401)?, 0)?;
        let second_incarnation =
            StructureIncarnation::from_mutation_index(TransactionId::new(402)?, 0)?;
        let mut tree = BTree::empty()
            .upsert(
                &mut pages,
                Csn::new(1)?,
                crate::STRUCTURE_FORMAT_KEY.to_vec(),
                STRUCTURE_FORMAT_VALUE_V3.to_vec(),
            )?
            .tree;
        tree = seed_physical_stream(&mut pages, tree, first_incarnation)?;
        let historical = tree;
        assert_eq!(
            read_stream_entries_v3(&pages, historical, b"events")?,
            Some(retired_stream_entries())
        );

        let (deleted, retirement_key, foreground_mutations) =
            delete_stream_v3_in_tree(&mut pages, tree, Csn::new(6)?, b"events")?;
        assert_eq!(foreground_mutations, 2);
        assert_eq!(read_stream_entries_v3(&pages, deleted, b"events")?, None);

        tree = create_stream_v3_in_tree(
            &mut pages,
            deleted,
            Csn::new(7)?,
            b"events",
            second_incarnation,
        )?;
        let new_fields = vec![(b"kind".to_vec(), b"new".to_vec())];
        tree = append_stream_entry_v3_in_tree(
            &mut pages,
            tree,
            Csn::new(8)?,
            b"events",
            1,
            &new_fields,
        )?;
        assert_eq!(
            read_stream_entries_v3(&pages, tree, b"events")?,
            Some(vec![(1, new_fields)])
        );
        assert_eq!(
            read_stream_entries_v3(&pages, historical, b"events")?,
            Some(retired_stream_entries())
        );

        let (tree, steps) = complete_stream_retirement(&mut pages, &pool, tree, &retirement_key)?;
        assert_eq!(steps, 2);
        assert_retired_stream_entries(&pages, tree, first_incarnation)?;
        Ok(())
    }

    #[test]
    fn physical_stream_range_rejects_reached_corruption_and_metadata_drift()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create()?;
        let blobs = BlobStore::create(temporary.path())?;
        let mut pages = PageStore::create(temporary.page_file())?;
        let pool = BufferPool::new(32, 4)?;
        let incarnation = StructureIncarnation::from_mutation_index(TransactionId::new(403)?, 0)?;
        let tree = BTree::empty()
            .upsert(
                &mut pages,
                Csn::new(1)?,
                crate::STRUCTURE_FORMAT_KEY.to_vec(),
                STRUCTURE_FORMAT_VALUE_V3.to_vec(),
            )?
            .tree;
        let tree = seed_physical_stream(&mut pages, tree, incarnation)?;
        assert_eq!(
            read_physical_stream_range(&pages, &blobs, &pool, tree)?,
            retired_stream_entries()
        );

        let metadata_key = structure_stream_meta_key(b"events")?;
        for (entry_count, last_id) in [(2, 42), (3, 41), (4, 42), (3, 43)] {
            let metadata = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
                incarnation,
                state: CollectionStateV3::Stream {
                    entry_count,
                    last_id,
                    expires_at_micros: None,
                },
            })?;
            let drifted = tree
                .upsert(&mut pages, Csn::new(10)?, metadata_key.clone(), metadata)?
                .tree;
            let result = read_physical_stream_range(&pages, &blobs, &pool, drifted);
            assert!(
                matches!(&result, Err(NativeRuntimeError::InvalidStructureTree)),
                "entry_count={entry_count} last_id={last_id}: {result:?}"
            );
        }

        let id_nine_key = encode_collection_child_key(
            STRUCTURE_STREAM_ENTRY_PREFIX,
            b"events",
            incarnation,
            &9_u64.to_be_bytes(),
        )?;
        let payload_mismatch = tree
            .upsert(
                &mut pages,
                Csn::new(11)?,
                id_nine_key,
                encode_stream_wal_entry(10, &[(b"kind".to_vec(), b"wrong".to_vec())])?,
            )?
            .tree;
        assert!(matches!(
            read_physical_stream_range(&pages, &blobs, &pool, payload_mismatch),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));

        let malformed_key = encode_collection_child_key(
            STRUCTURE_STREAM_ENTRY_PREFIX,
            b"events",
            incarnation,
            &[0, 0, 0, 0, 0, 0, 0, 9, 0],
        )?;
        let malformed = tree
            .upsert(
                &mut pages,
                Csn::new(12)?,
                malformed_key,
                encode_stream_wal_entry(9, &[(b"kind".to_vec(), b"wrong".to_vec())])?,
            )?
            .tree;
        assert!(matches!(
            read_physical_stream_range(&pages, &blobs, &pool, malformed),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));

        let last_key = encode_collection_child_key(
            STRUCTURE_STREAM_ENTRY_PREFIX,
            b"events",
            incarnation,
            &42_u64.to_be_bytes(),
        )?;
        let missing_last = tree
            .upsert(
                &mut pages,
                Csn::new(13)?,
                last_key,
                structure_tombstone_value(),
            )?
            .tree;
        assert!(matches!(
            read_physical_stream_range(&pages, &blobs, &pool, missing_last),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        Ok(())
    }

    #[test]
    fn physical_sorted_set_delete_recreate_and_dual_index_cleanup_are_incarnation_fenced()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create()?;
        let mut pages = PageStore::create(temporary.page_file())?;
        let pool = BufferPool::new(32, 4)?;
        let first_incarnation =
            StructureIncarnation::from_mutation_index(TransactionId::new(501)?, 0)?;
        let second_incarnation =
            StructureIncarnation::from_mutation_index(TransactionId::new(502)?, 0)?;
        let mut tree = BTree::empty()
            .upsert(
                &mut pages,
                Csn::new(1)?,
                crate::STRUCTURE_FORMAT_KEY.to_vec(),
                STRUCTURE_FORMAT_VALUE_V3.to_vec(),
            )?
            .tree;
        tree = seed_physical_sorted_set(&mut pages, tree, first_incarnation)?;
        let historical = tree;
        assert_eq!(
            read_sorted_set_members_v3(&pages, historical, b"rank")?,
            Some(retired_sorted_set_members()?)
        );

        let (deleted, retirement_key, foreground_mutations) =
            delete_sorted_set_v3_in_tree(&mut pages, tree, Csn::new(6)?, b"rank")?;
        assert_eq!(foreground_mutations, 2);
        assert_eq!(read_sorted_set_members_v3(&pages, deleted, b"rank")?, None);

        tree = create_sorted_set_v3_in_tree(
            &mut pages,
            deleted,
            Csn::new(7)?,
            b"rank",
            second_incarnation,
        )?;
        let new_score = SortedSetScore::new(7.0).ok_or("invalid new score")?;
        let (updated, inserted) = upsert_sorted_set_member_v3_in_tree(
            &mut pages,
            tree,
            Csn::new(8)?,
            b"rank",
            b"new",
            new_score,
        )?;
        assert!(inserted);
        tree = updated;
        assert_eq!(
            read_sorted_set_members_v3(&pages, tree, b"rank")?,
            Some(vec![SortedSetMemberV3 {
                member: b"new".to_vec(),
                score: new_score,
            }])
        );
        assert_eq!(
            read_sorted_set_members_v3(&pages, historical, b"rank")?,
            Some(retired_sorted_set_members()?)
        );

        let (tree, steps) =
            complete_sorted_set_retirement(&mut pages, &pool, tree, &retirement_key)?;
        assert_eq!(steps, 2);
        assert_eq!(
            read_sorted_set_members_v3(&pages, tree, b"rank")?,
            Some(vec![SortedSetMemberV3 {
                member: b"new".to_vec(),
                score: new_score,
            }])
        );
        assert_retired_sorted_set_entries(&pages, tree, first_incarnation)?;
        Ok(())
    }

    #[test]
    fn physical_sorted_set_ranges_reject_dual_index_and_count_drift() -> Result<(), Box<dyn Error>>
    {
        let temporary = TestDirectory::create()?;
        let blobs = BlobStore::create(temporary.path())?;
        let mut pages = PageStore::create(temporary.page_file())?;
        let pool = BufferPool::new(32, 4)?;
        let incarnation = StructureIncarnation::from_mutation_index(TransactionId::new(503)?, 0)?;
        let tree = BTree::empty()
            .upsert(
                &mut pages,
                Csn::new(1)?,
                crate::STRUCTURE_FORMAT_KEY.to_vec(),
                STRUCTURE_FORMAT_VALUE_V3.to_vec(),
            )?
            .tree;
        let tree = seed_physical_sorted_set(&mut pages, tree, incarnation)?;
        let entries = read_physical_sorted_set_range(&pages, &blobs, &pool, tree)?;
        assert_eq!(
            entries
                .iter()
                .map(SortedSetEntry::member)
                .collect::<Vec<_>>(),
            vec![b"beta".as_slice(), b"alpha".as_slice(), b"gamma".as_slice()]
        );

        let metadata_key = structure_sorted_set_meta_key(b"rank")?;
        for member_count in [2, 4] {
            let metadata = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
                incarnation,
                state: CollectionStateV3::SortedSet {
                    member_count,
                    expires_at_micros: None,
                },
            })?;
            let drifted = tree
                .upsert(&mut pages, Csn::new(10)?, metadata_key.clone(), metadata)?
                .tree;
            assert!(matches!(
                read_physical_sorted_set_range(&pages, &blobs, &pool, drifted),
                Err(NativeRuntimeError::InvalidStructureTree)
            ));
        }

        let alpha_member = encode_collection_child_key(
            STRUCTURE_SORTED_SET_MEMBER_PREFIX,
            b"rank",
            incarnation,
            b"alpha",
        )?;
        let wrong_score = SortedSetScore::new(9.0).ok_or("invalid score")?;
        let mismatched = tree
            .upsert(
                &mut pages,
                Csn::new(11)?,
                alpha_member,
                encode_sorted_set_score(wrong_score),
            )?
            .tree;
        assert!(matches!(
            read_physical_sorted_set_range(&pages, &blobs, &pool, mismatched),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));

        let alpha_score = SortedSetScore::new(1.5).ok_or("invalid score")?;
        let alpha_order = v3_sorted_set_order_key(b"rank", incarnation, alpha_score, b"alpha")?;
        let missing_order = tree
            .upsert(
                &mut pages,
                Csn::new(12)?,
                alpha_order,
                structure_tombstone_value(),
            )?
            .tree;
        assert!(matches!(
            sorted_set_rank_latest_v3(
                &pages,
                &blobs,
                &pool,
                missing_order,
                b"rank",
                b"alpha",
                false,
            ),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        Ok(())
    }

    #[test]
    fn full_tree_validation_and_compaction_preserve_partial_retirement()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create()?;
        let blobs = BlobStore::create(temporary.path())?;
        let mut pages = PageStore::create(temporary.page_file())?;
        let pool = BufferPool::new(32, 4)?;
        let retired = StructureIncarnation::from_mutation_index(TransactionId::new(601)?, 0)?;
        let mut tree = BTree::empty()
            .upsert(
                &mut pages,
                Csn::new(1)?,
                crate::STRUCTURE_FORMAT_KEY.to_vec(),
                STRUCTURE_FORMAT_VALUE_V3.to_vec(),
            )?
            .tree;
        tree = seed_physical_set(&mut pages, tree, retired)?;
        let historical = tree;
        let (deleted, retirement_key, _) =
            delete_set_v3_in_tree(&mut pages, tree, Csn::new(8)?, b"members")?;
        let (partial, step, _) = cleanup_set_retirement_v3_step(
            &mut pages,
            &pool,
            deleted,
            Csn::new(9)?,
            &retirement_key,
            2,
        )?;
        assert!(step.more_remaining);
        let validation = validate_structure_v3_tree(&pages, &blobs, partial)?;
        assert_eq!(validation.active_retirements, 1);

        let (compacted, compaction) =
            compact_structure_v3_tree(&mut pages, &blobs, partial, Csn::new(10)?)?;
        assert_eq!(compaction.active_retirements, 1);
        assert!(compaction.dropped_tombstones >= 2);
        validate_structure_v3_tree(&pages, &blobs, compacted)?;
        let (compacted, _) =
            complete_set_retirement(&mut pages, &pool, compacted, &retirement_key)?;
        assert_eq!(
            read_set_members_v3(&pages, historical, b"members")?,
            Some(retired_set_values())
        );
        validate_structure_v3_tree(&pages, &blobs, compacted)?;

        let orphan_incarnation =
            StructureIncarnation::from_mutation_index(TransactionId::new(999)?, 0)?;
        let orphan_key = encode_collection_child_key(
            STRUCTURE_SET_MEMBER_PREFIX,
            b"orphan",
            orphan_incarnation,
            b"member",
        )?;
        let forged = compacted
            .upsert(
                &mut pages,
                Csn::new(20)?,
                orphan_key,
                set_member_live_value(),
            )?
            .tree;
        assert!(matches!(
            validate_structure_v3_tree(&pages, &blobs, forged),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        Ok(())
    }

    #[test]
    fn full_tree_validation_rejects_retirement_counter_and_cursor_corruption()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create()?;
        let blobs = BlobStore::create(temporary.path())?;
        let mut pages = PageStore::create(temporary.page_file())?;
        let pool = BufferPool::new(32, 4)?;
        let retired = StructureIncarnation::from_mutation_index(TransactionId::new(602)?, 0)?;
        let tree = BTree::empty()
            .upsert(
                &mut pages,
                Csn::new(1)?,
                crate::STRUCTURE_FORMAT_KEY.to_vec(),
                STRUCTURE_FORMAT_VALUE_V3.to_vec(),
            )?
            .tree;
        let tree = seed_physical_set(&mut pages, tree, retired)?;
        let (deleted, retirement_key, _) =
            delete_set_v3_in_tree(&mut pages, tree, Csn::new(8)?, b"members")?;
        let encoded = deleted
            .get(&pages, &retirement_key)?
            .ok_or("missing retirement record")?;
        let mut record = decode_retirement_record(&encoded)?;
        record.remaining_logical_items -= 1;
        record.remaining_primary_entries -= 1;
        let wrong_counters = deleted
            .upsert(
                &mut pages,
                Csn::new(9)?,
                retirement_key.clone(),
                encode_retirement_record(&record)?,
            )?
            .tree;
        assert!(matches!(
            validate_structure_v3_tree(&pages, &blobs, wrong_counters),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));

        let (partial, step, _) = cleanup_set_retirement_v3_step(
            &mut pages,
            &pool,
            deleted,
            Csn::new(10)?,
            &retirement_key,
            2,
        )?;
        assert!(step.more_remaining);
        let processed_key = encode_collection_child_key(
            STRUCTURE_SET_MEMBER_PREFIX,
            b"members",
            retired,
            RETIRED_SET_MEMBERS[0],
        )?;
        let resurrected_before_cursor = partial
            .upsert(
                &mut pages,
                Csn::new(11)?,
                processed_key,
                set_member_live_value(),
            )?
            .tree;
        assert!(matches!(
            validate_structure_v3_tree(&pages, &blobs, resurrected_before_cursor),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        Ok(())
    }

    #[test]
    fn full_tree_validation_rejects_unpaired_sorted_order_entries() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create()?;
        let blobs = BlobStore::create(temporary.path())?;
        let mut pages = PageStore::create(temporary.page_file())?;
        let incarnation = StructureIncarnation::from_mutation_index(TransactionId::new(603)?, 0)?;
        let tree = BTree::empty()
            .upsert(
                &mut pages,
                Csn::new(1)?,
                crate::STRUCTURE_FORMAT_KEY.to_vec(),
                STRUCTURE_FORMAT_VALUE_V3.to_vec(),
            )?
            .tree;
        let tree = seed_physical_sorted_set(&mut pages, tree, incarnation)?;
        let wrong_score = SortedSetScore::new(99.0).ok_or("invalid score")?;
        let unpaired_order = v3_sorted_set_order_key(b"rank", incarnation, wrong_score, b"alpha")?;
        let forged = tree
            .upsert(
                &mut pages,
                Csn::new(9)?,
                unpaired_order,
                set_member_live_value(),
            )?
            .tree;
        assert!(matches!(
            validate_structure_v3_tree(&pages, &blobs, forged),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        Ok(())
    }

    #[test]
    fn direct_hash_reads_honor_field_expiry_and_reject_metadata_drift() -> Result<(), Box<dyn Error>>
    {
        let temporary = TestDirectory::create()?;
        let blobs = BlobStore::create(temporary.path())?;
        let mut pages = PageStore::create(temporary.page_file())?;
        let pool = BufferPool::new(32, 4)?;
        let incarnation = StructureIncarnation::from_mutation_index(TransactionId::new(604)?, 0)?;
        let tree = BTree::empty()
            .upsert(
                &mut pages,
                Csn::new(1)?,
                crate::STRUCTURE_FORMAT_KEY.to_vec(),
                STRUCTURE_FORMAT_VALUE_V3.to_vec(),
            )?
            .tree;
        let tree = seed_physical_hash(&mut pages, tree, incarnation)?;

        assert_eq!(
            hash_get_latest_at_v3(&pages, &blobs, &pool, tree, b"record", b"beta", 550,)?,
            None
        );
        assert_eq!(
            hash_get_latest_at_v3(&pages, &blobs, &pool, tree, b"record", b"gamma", 550,)?,
            Some(b"three".to_vec())
        );
        assert_eq!(
            hash_len_latest_at_v3(&pages, &blobs, &pool, tree, b"record", 550)?,
            2
        );
        let any_pattern = assert_direct_hash_ranges_v3(&pages, &blobs, &pool, tree)?;

        let forged_metadata = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
            incarnation,
            state: CollectionStateV3::Hash {
                field_count: 4,
                field_expiry_count: 2,
                expires_at_micros: None,
            },
        })?;
        let forged = tree
            .upsert(
                &mut pages,
                Csn::new(9)?,
                structure_hash_meta_key(b"record"),
                forged_metadata,
            )?
            .tree;
        assert!(matches!(
            hash_len_latest_at_v3(&pages, &blobs, &pool, forged, b"record", 550),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        assert!(matches!(
            hash_scan_latest_at_v3(
                &pages,
                &blobs,
                &pool,
                forged,
                CollectionScanRequestV3::new(b"record", None, 4, 550),
            ),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        assert!(matches!(
            hash_scan_reverse_latest_at_v3(
                &pages,
                &blobs,
                &pool,
                forged,
                CollectionScanRequestV3::new(b"record", None, 4, 550),
            ),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        assert!(matches!(
            hash_pattern_scan_latest_at_v3(
                &pages,
                &blobs,
                &pool,
                forged,
                b"record",
                &any_pattern,
                550,
            ),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        Ok(())
    }

    fn assert_direct_hash_ranges_v3(
        pages: &PageStore,
        blobs: &BlobStore,
        pool: &BufferPool,
        tree: BTree,
    ) -> Result<HashPatternScanRequest, Box<dyn Error>> {
        let scan = hash_scan_latest_at_v3(
            pages,
            blobs,
            pool,
            tree,
            CollectionScanRequestV3::new(b"record", None, 3, 550),
        )?;
        assert_eq!(scan.declared_field_count, 3);
        assert_eq!(
            scan.entries,
            vec![
                HashFieldEntry::new(b"alpha".to_vec(), b"one".to_vec()),
                HashFieldEntry::new(b"gamma".to_vec(), b"three".to_vec()),
            ]
        );
        let reverse = hash_scan_reverse_latest_at_v3(
            pages,
            blobs,
            pool,
            tree,
            CollectionScanRequestV3::new(b"record", None, 3, 550),
        )?;
        assert_eq!(reverse.declared_field_count, 3);
        assert_eq!(
            reverse.entries,
            vec![
                HashFieldEntry::new(b"gamma".to_vec(), b"three".to_vec()),
                HashFieldEntry::new(b"alpha".to_vec(), b"one".to_vec()),
            ]
        );
        let any_pattern = HashPatternScanRequest::try_new(b"*", None, 4, 4, 100)?;
        let pattern =
            hash_pattern_scan_latest_at_v3(pages, blobs, pool, tree, b"record", &any_pattern, 550)?;
        assert_eq!(
            pattern.entries(),
            &[
                HashFieldEntry::new(b"alpha".to_vec(), b"one".to_vec()),
                HashFieldEntry::new(b"gamma".to_vec(), b"three".to_vec()),
            ]
        );
        assert_eq!(pattern.stop(), HashPatternScanStop::Exhausted);
        assert_eq!(pattern.visited(), 3);
        Ok(any_pattern)
    }

    #[test]
    fn direct_set_scan_preserves_order_and_rejects_metadata_drift() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create()?;
        let blobs = BlobStore::create(temporary.path())?;
        let mut pages = PageStore::create(temporary.page_file())?;
        let pool = BufferPool::new(32, 4)?;
        let incarnation = StructureIncarnation::from_mutation_index(TransactionId::new(605)?, 0)?;
        let tree = BTree::empty()
            .upsert(
                &mut pages,
                Csn::new(1)?,
                crate::STRUCTURE_FORMAT_KEY.to_vec(),
                STRUCTURE_FORMAT_VALUE_V3.to_vec(),
            )?
            .tree;
        let tree = seed_physical_set(&mut pages, tree, incarnation)?;

        let scan = set_scan_latest_at_v3(
            &pages,
            &blobs,
            &pool,
            tree,
            CollectionScanRequestV3::new(b"members", Some(b"b"), 3, i64::MIN),
        )?;
        assert_eq!(scan.declared_member_count, 5);
        assert_eq!(
            scan.members,
            vec![b"c".to_vec(), b"d".to_vec(), b"e".to_vec()]
        );

        let forged_metadata = encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
            incarnation,
            state: CollectionStateV3::Set {
                member_count: 6,
                expires_at_micros: None,
            },
        })?;
        let forged = tree
            .upsert(
                &mut pages,
                Csn::new(9)?,
                structure_set_meta_key(b"members"),
                forged_metadata,
            )?
            .tree;
        assert!(matches!(
            set_scan_latest_at_v3(
                &pages,
                &blobs,
                &pool,
                forged,
                CollectionScanRequestV3::new(b"members", None, 6, i64::MIN),
            ),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        Ok(())
    }

    #[test]
    fn full_tree_validation_requires_hash_expiry_backlinks() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create()?;
        let blobs = BlobStore::create(temporary.path())?;
        let mut pages = PageStore::create(temporary.page_file())?;
        let incarnation = StructureIncarnation::from_mutation_index(TransactionId::new(604)?, 0)?;
        let tree = BTree::empty()
            .upsert(
                &mut pages,
                Csn::new(1)?,
                crate::STRUCTURE_FORMAT_KEY.to_vec(),
                STRUCTURE_FORMAT_VALUE_V3.to_vec(),
            )?
            .tree;
        let tree = seed_physical_hash(&mut pages, tree, incarnation)?;
        validate_structure_v3_tree(&pages, &blobs, tree)?;
        let expiry_key = encode_collection_expiry_key(
            crate::STRUCTURE_HASH_FIELD_EXPIRY_PREFIX,
            500,
            b"record",
            incarnation,
            b"beta",
        )?;
        let missing_backlink = tree
            .upsert(
                &mut pages,
                Csn::new(9)?,
                expiry_key,
                vec![STRUCTURE_EXPIRY_TOMBSTONE],
            )?
            .tree;
        assert!(matches!(
            validate_structure_v3_tree(&pages, &blobs, missing_backlink),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        Ok(())
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn full_tree_validation_and_compaction_cover_all_collection_families()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create()?;
        let blobs = BlobStore::create(temporary.path())?;
        let mut pages = PageStore::create(temporary.page_file())?;
        let incarnations = [
            StructureIncarnation::from_mutation_index(TransactionId::new(701)?, 0)?,
            StructureIncarnation::from_mutation_index(TransactionId::new(702)?, 0)?,
            StructureIncarnation::from_mutation_index(TransactionId::new(703)?, 0)?,
            StructureIncarnation::from_mutation_index(TransactionId::new(704)?, 0)?,
            StructureIncarnation::from_mutation_index(TransactionId::new(705)?, 0)?,
        ];
        let mut tree = BTree::empty()
            .upsert(
                &mut pages,
                Csn::new(1)?,
                crate::STRUCTURE_FORMAT_KEY.to_vec(),
                STRUCTURE_FORMAT_VALUE_V3.to_vec(),
            )?
            .tree;
        tree = seed_physical_hash(&mut pages, tree, incarnations[0])?;
        tree = seed_physical_set(&mut pages, tree, incarnations[1])?;
        tree = seed_physical_list(&mut pages, tree, incarnations[2])?;
        tree = seed_physical_stream(&mut pages, tree, incarnations[3])?;
        tree = seed_physical_sorted_set(&mut pages, tree, incarnations[4])?;
        let historical = tree;
        let validation = validate_structure_v3_tree(&pages, &blobs, tree)?;
        assert_eq!(validation.live_collections, 5);
        assert_eq!(validation.active_retirements, 0);
        let loaded = load_structure_state_v3(&pages, &blobs, tree)?;
        let hash = loaded
            .hashes
            .get(b"record".as_slice())
            .ok_or("missing hash")?;
        assert_eq!(hash.get(b"beta".as_slice()), Some(&b"two".to_vec()));
        assert_eq!(
            loaded.hash_field_expiries[&(b"record".to_vec(), b"beta".to_vec())],
            500
        );
        assert_eq!(
            loaded
                .sets
                .get(b"members".as_slice())
                .ok_or("missing set")?
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            retired_set_values()
        );
        assert_eq!(
            loaded
                .lists
                .get(b"queue".as_slice())
                .ok_or("missing list")?
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            retired_list_values()
        );
        assert_eq!(
            loaded
                .streams
                .get(b"events".as_slice())
                .ok_or("missing stream")?,
            &retired_stream_entries().into_iter().collect()
        );
        assert_eq!(
            loaded
                .sorted_sets
                .get(b"rank".as_slice())
                .ok_or("missing sorted set")?
                .len(),
            3
        );

        let (updated, _, _) = delete_hash_v3_in_tree(&mut pages, tree, Csn::new(20)?, b"record")?;
        tree = updated;
        let (updated, _, _) = delete_set_v3_in_tree(&mut pages, tree, Csn::new(21)?, b"members")?;
        tree = updated;
        let (updated, _, _) = delete_list_v3_in_tree(&mut pages, tree, Csn::new(22)?, b"queue")?;
        tree = updated;
        let (updated, _, _) = delete_stream_v3_in_tree(&mut pages, tree, Csn::new(23)?, b"events")?;
        tree = updated;
        let (updated, _, _) =
            delete_sorted_set_v3_in_tree(&mut pages, tree, Csn::new(24)?, b"rank")?;
        tree = updated;
        let validation = validate_structure_v3_tree(&pages, &blobs, tree)?;
        assert_eq!(validation.live_collections, 0);
        assert_eq!(validation.active_retirements, 5);

        let (compacted, compaction) =
            compact_structure_v3_tree(&mut pages, &blobs, tree, Csn::new(25)?)?;
        assert_eq!(compaction.active_retirements, 5);
        assert!(compaction.dropped_tombstones >= 5);
        validate_structure_v3_tree(&pages, &blobs, compacted)?;
        assert_eq!(
            read_hash_fields_v3(&pages, &blobs, historical, b"record")?,
            Some(retired_hash_values())
        );
        assert_eq!(
            read_set_members_v3(&pages, historical, b"members")?,
            Some(retired_set_values())
        );
        assert_eq!(
            read_list_values_v3(&pages, &blobs, historical, b"queue")?,
            Some(retired_list_values())
        );
        assert_eq!(
            read_stream_entries_v3(&pages, historical, b"events")?,
            Some(retired_stream_entries())
        );
        assert_eq!(
            read_sorted_set_members_v3(&pages, historical, b"rank")?,
            Some(retired_sorted_set_members()?)
        );
        Ok(())
    }

    #[test]
    fn scalar_mutations_reject_stale_values_and_expiry_backlinks_without_pages()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create()?;
        let mut pages = PageStore::create(temporary.page_file())?;
        let blob_references = BTreeMap::new();
        let mut tree = BTree::empty()
            .upsert(
                &mut pages,
                Csn::new(1)?,
                crate::STRUCTURE_FORMAT_KEY.to_vec(),
                STRUCTURE_FORMAT_VALUE_V3.to_vec(),
            )?
            .tree;
        tree = apply_scalar_mutation_v3(
            &mut pages,
            tree,
            Csn::new(2)?,
            &Mutation {
                engine: EngineKind::Structure,
                opcode: Opcode::SetValue,
                target: None,
                key: b"leased".to_vec(),
                value: b"value".to_vec(),
                expires_at_micros: Some(100),
            },
            &blob_references,
        )?;

        let pages_before_stale = pages.page_count();
        let entries_before_stale = tree.scan(&pages)?;
        assert!(matches!(
            apply_scalar_mutation_v3(
                &mut pages,
                tree,
                Csn::new(3)?,
                &Mutation {
                    engine: EngineKind::Structure,
                    opcode: Opcode::ExpireValue,
                    target: None,
                    key: b"leased".to_vec(),
                    value: b"stale".to_vec(),
                    expires_at_micros: Some(200),
                },
                &blob_references,
            ),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        assert_eq!(pages.page_count(), pages_before_stale);
        assert_eq!(tree.scan(&pages)?, entries_before_stale);

        let corrupt = tree
            .upsert(
                &mut pages,
                Csn::new(4)?,
                structure_expiry_key(100, b"leased")?,
                vec![STRUCTURE_EXPIRY_TOMBSTONE],
            )?
            .tree;
        let pages_before_backlink = pages.page_count();
        let entries_before_backlink = corrupt.scan(&pages)?;
        assert!(matches!(
            apply_scalar_mutation_v3(
                &mut pages,
                corrupt,
                Csn::new(5)?,
                &Mutation {
                    engine: EngineKind::Structure,
                    opcode: Opcode::SetValue,
                    target: None,
                    key: b"leased".to_vec(),
                    value: b"replacement".to_vec(),
                    expires_at_micros: Some(100),
                },
                &blob_references,
            ),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        assert_eq!(pages.page_count(), pages_before_backlink);
        assert_eq!(corrupt.scan(&pages)?, entries_before_backlink);
        Ok(())
    }

    #[test]
    fn scalar_mutations_reject_missing_and_collection_owned_state_without_pages()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create()?;
        let mut pages = PageStore::create(temporary.page_file())?;
        let blob_references = BTreeMap::new();
        let mut tree = BTree::empty()
            .upsert(
                &mut pages,
                Csn::new(1)?,
                crate::STRUCTURE_FORMAT_KEY.to_vec(),
                STRUCTURE_FORMAT_VALUE_V3.to_vec(),
            )?
            .tree;

        let pages_before_missing = pages.page_count();
        assert!(matches!(
            apply_scalar_mutation_v3(
                &mut pages,
                tree,
                Csn::new(2)?,
                &Mutation {
                    engine: EngineKind::Structure,
                    opcode: Opcode::DeleteValue,
                    target: None,
                    key: b"missing".to_vec(),
                    value: Vec::new(),
                    expires_at_micros: None,
                },
                &blob_references,
            ),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        assert_eq!(pages.page_count(), pages_before_missing);

        tree = create_set_v3_in_tree(
            &mut pages,
            tree,
            Csn::new(3)?,
            b"owned",
            StructureIncarnation::from_mutation_index(TransactionId::new(90)?, 0)?,
        )?;
        let pages_before_owned = pages.page_count();
        let entries_before_owned = tree.scan(&pages)?;
        assert!(matches!(
            apply_scalar_mutation_v3(
                &mut pages,
                tree,
                Csn::new(4)?,
                &Mutation {
                    engine: EngineKind::Structure,
                    opcode: Opcode::SetValue,
                    target: None,
                    key: b"owned".to_vec(),
                    value: b"invalid".to_vec(),
                    expires_at_micros: None,
                },
                &blob_references,
            ),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        assert_eq!(pages.page_count(), pages_before_owned);
        assert_eq!(tree.scan(&pages)?, entries_before_owned);
        Ok(())
    }

    struct FullLoadFailureGuard;

    impl FullLoadFailureGuard {
        fn install() -> Self {
            crate::FAIL_FULL_STATE_LOAD.set(true);
            crate::FAIL_FULL_STRUCTURE_STATE_LOAD.set(true);
            crate::FAIL_FULL_CATALOG_STATE_LOAD.set(true);
            Self
        }
    }

    impl Drop for FullLoadFailureGuard {
        fn drop(&mut self) {
            crate::FAIL_FULL_CATALOG_STATE_LOAD.set(false);
            crate::FAIL_FULL_STRUCTURE_STATE_LOAD.set(false);
            crate::FAIL_FULL_STATE_LOAD.set(false);
        }
    }

    fn empty_structure_v3_tree(pages: &mut PageStore) -> Result<BTree, Box<dyn Error>> {
        Ok(BTree::empty()
            .upsert(
                pages,
                Csn::new(1)?,
                crate::STRUCTURE_FORMAT_KEY.to_vec(),
                STRUCTURE_FORMAT_VALUE_V3.to_vec(),
            )?
            .tree)
    }

    #[test]
    fn delta_hash_field_point_preserves_raw_visibility_due_and_missing_state()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create()?;
        let mut pages = PageStore::create(temporary.page_file())?;
        let pool = BufferPool::new(32, 4)?;
        let incarnation = StructureIncarnation::from_mutation_index(TransactionId::new(901)?, 0)?;
        let tree = empty_structure_v3_tree(&mut pages)?;
        let tree = create_hash_v3_in_tree(&mut pages, tree, Csn::new(2)?, b"record", incarnation)?;
        let (tree, inserted) = put_hash_field_v3_in_tree(
            &mut pages,
            tree,
            Csn::new(3)?,
            HashFieldWriteV3 {
                key: b"record",
                field: b"visible",
                value: b"value",
                expires_at_micros: None,
            },
            &BTreeMap::new(),
        )?;
        assert!(inserted);
        let (tree, inserted) = put_hash_field_v3_in_tree(
            &mut pages,
            tree,
            Csn::new(4)?,
            HashFieldWriteV3 {
                key: b"record",
                field: b"due",
                value: b"stale",
                expires_at_micros: Some(50),
            },
            &BTreeMap::new(),
        )?;
        assert!(inserted);

        let retired_incarnation =
            StructureIncarnation::from_mutation_index(TransactionId::new(900)?, 0)?;
        let stale_field_key = encode_collection_child_key(
            STRUCTURE_HASH_FIELD_PREFIX,
            b"record",
            retired_incarnation,
            b"stale-only",
        )?;
        let tree = tree
            .upsert(
                &mut pages,
                Csn::new(5)?,
                stale_field_key,
                structure_storage_value(b"retired", None, &BTreeMap::new())?,
            )?
            .tree;

        let _guards = FullLoadFailureGuard::install();
        let visible =
            delta_hash_field_state_latest_at_v3(&pages, &pool, tree, b"record", b"visible")?
                .ok_or("missing hash")?;
        assert_eq!(visible.incarnation, incarnation);
        assert_eq!(visible.field_count, 2);
        assert_eq!(visible.field_expiry_count, 1);
        assert_eq!(visible.expires_at_micros, None);
        let visible_field = visible.field.ok_or("missing visible field")?;
        assert_eq!(visible_field.expires_at_micros, None);
        assert_eq!(visible_field.logical_value_bytes, 5);
        assert_eq!(visible_field.blob_reference, None);

        let due = delta_hash_field_state_latest_at_v3(&pages, &pool, tree, b"record", b"due")?
            .ok_or("missing hash")?
            .field
            .ok_or("missing due field")?;
        assert_eq!(due.expires_at_micros, Some(50));
        assert_eq!(due.logical_value_bytes, 5);

        assert!(
            delta_hash_field_state_latest_at_v3(&pages, &pool, tree, b"record", b"missing",)?
                .ok_or("missing hash")?
                .field
                .is_none()
        );
        assert!(
            delta_hash_field_state_latest_at_v3(&pages, &pool, tree, b"record", b"stale-only",)?
                .ok_or("missing hash")?
                .field
                .is_none()
        );
        assert!(
            delta_hash_field_state_latest_at_v3(&pages, &pool, tree, b"missing", b"field",)?
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn delta_hash_field_point_rejects_wrong_kind_malformed_and_missing_backlinks()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create()?;
        let mut pages = PageStore::create(temporary.page_file())?;
        let pool = BufferPool::new(32, 4)?;
        let incarnation = StructureIncarnation::from_mutation_index(TransactionId::new(902)?, 0)?;
        let tree = empty_structure_v3_tree(&mut pages)?;
        let wrong_kind =
            create_set_v3_in_tree(&mut pages, tree, Csn::new(2)?, b"wrong", incarnation)?;
        assert!(matches!(
            delta_hash_field_state_latest_at_v3(&pages, &pool, wrong_kind, b"wrong", b"field",),
            Err(NativeRuntimeError::StructureKindMismatch)
        ));

        let hash =
            create_hash_v3_in_tree(&mut pages, wrong_kind, Csn::new(3)?, b"record", incarnation)?;
        let (hash, inserted) = put_hash_field_v3_in_tree(
            &mut pages,
            hash,
            Csn::new(4)?,
            HashFieldWriteV3 {
                key: b"record",
                field: b"leased",
                value: b"value",
                expires_at_micros: Some(50),
            },
            &BTreeMap::new(),
        )?;
        assert!(inserted);
        let field_expiry_key = encode_collection_expiry_key(
            crate::STRUCTURE_HASH_FIELD_EXPIRY_PREFIX,
            50,
            b"record",
            incarnation,
            b"leased",
        )?;
        let missing_field_backlink = hash
            .upsert(
                &mut pages,
                Csn::new(5)?,
                field_expiry_key,
                vec![STRUCTURE_EXPIRY_TOMBSTONE],
            )?
            .tree;
        assert!(matches!(
            delta_hash_field_state_latest_at_v3(
                &pages,
                &pool,
                missing_field_backlink,
                b"record",
                b"leased",
            ),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));

        let malformed = hash
            .upsert(
                &mut pages,
                Csn::new(6)?,
                structure_hash_meta_key(b"record"),
                b"malformed".to_vec(),
            )?
            .tree;
        assert!(matches!(
            delta_hash_field_state_latest_at_v3(&pages, &pool, malformed, b"record", b"leased",),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));

        let expiring_hash = expire_collection_v3_in_tree(
            &mut pages,
            hash,
            Csn::new(7)?,
            b"record",
            StructureCollectionFamily::Hash,
            100,
        )?;
        let hash_expiry_key = encode_collection_expiry_key(
            crate::STRUCTURE_EXPIRY_PREFIX,
            100,
            b"record",
            incarnation,
            &[],
        )?;
        let missing_hash_backlink = expiring_hash
            .upsert(
                &mut pages,
                Csn::new(8)?,
                hash_expiry_key,
                vec![STRUCTURE_EXPIRY_TOMBSTONE],
            )?
            .tree;
        assert!(matches!(
            delta_hash_field_state_latest_at_v3(
                &pages,
                &pool,
                missing_hash_backlink,
                b"record",
                b"leased",
            ),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        Ok(())
    }

    #[test]
    fn delta_hash_field_point_rejects_persistent_field_when_all_fields_claim_expiry()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create()?;
        let mut pages = PageStore::create(temporary.page_file())?;
        let pool = BufferPool::new(32, 4)?;
        let incarnation = StructureIncarnation::from_mutation_index(TransactionId::new(904)?, 0)?;
        let tree = empty_structure_v3_tree(&mut pages)?;
        let tree = create_hash_v3_in_tree(&mut pages, tree, Csn::new(2)?, b"record", incarnation)?;
        let (tree, inserted) = put_hash_field_v3_in_tree(
            &mut pages,
            tree,
            Csn::new(3)?,
            HashFieldWriteV3 {
                key: b"record",
                field: b"persistent",
                value: b"value",
                expires_at_micros: None,
            },
            &BTreeMap::new(),
        )?;
        assert!(inserted);
        let impossible_metadata =
            encode_typed_collection_metadata(&TypedCollectionMetadataV3::Live {
                incarnation,
                state: CollectionStateV3::Hash {
                    field_count: 1,
                    field_expiry_count: 1,
                    expires_at_micros: None,
                },
            })?;
        let tree = tree
            .upsert(
                &mut pages,
                Csn::new(4)?,
                structure_hash_meta_key(b"record"),
                impossible_metadata,
            )?
            .tree;

        let _guards = FullLoadFailureGuard::install();
        assert!(matches!(
            delta_hash_field_state_latest_at_v3(&pages, &pool, tree, b"record", b"persistent",),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        Ok(())
    }

    #[test]
    fn delta_hash_field_point_preserves_blob_reference_without_reading_payload()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::create()?;
        let mut blobs = BlobStore::create(temporary.path())?;
        let mut pages = PageStore::create(temporary.page_file())?;
        let pool = BufferPool::new(32, 4)?;
        let incarnation = StructureIncarnation::from_mutation_index(TransactionId::new(903)?, 0)?;
        let large = vec![0x5a; crate::STRUCTURE_INLINE_VALUE_LIMIT + 1];
        let reference = blobs.put(&large, false)?;
        let references = BTreeMap::from([(*blake3::hash(&large).as_bytes(), reference)]);
        let tree = empty_structure_v3_tree(&mut pages)?;
        let tree = create_hash_v3_in_tree(&mut pages, tree, Csn::new(2)?, b"record", incarnation)?;
        let (tree, inserted) = put_hash_field_v3_in_tree(
            &mut pages,
            tree,
            Csn::new(3)?,
            HashFieldWriteV3 {
                key: b"record",
                field: b"large",
                value: &large,
                expires_at_micros: None,
            },
            &references,
        )?;
        assert!(inserted);

        let raw = delta_hash_field_state_latest_at_v3(&pages, &pool, tree, b"record", b"large")?
            .ok_or("missing hash")?
            .field
            .ok_or("missing blob field")?;
        assert_eq!(raw.blob_reference, Some(reference));
        assert_eq!(raw.logical_value_bytes, u64::try_from(large.len())?);
        assert_eq!(
            decode_structure_value(&raw.encoded, &blobs)?
                .ok_or("blob envelope decoded as tombstone")?
                .value,
            large
        );

        let mut malformed_blob = raw.encoded.clone();
        malformed_blob
            [crate::STRUCTURE_VALUE_HEADER_SIZE + 16..crate::STRUCTURE_VALUE_HEADER_SIZE + 24]
            .copy_from_slice(&1_u64.to_le_bytes());
        let field_key = encode_collection_child_key(
            STRUCTURE_HASH_FIELD_PREFIX,
            b"record",
            incarnation,
            b"large",
        )?;
        let malformed_tree = tree
            .upsert(&mut pages, Csn::new(4)?, field_key.clone(), malformed_blob)?
            .tree;
        assert!(matches!(
            delta_hash_field_state_latest_at_v3(&pages, &pool, malformed_tree, b"record", b"large",),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));

        let mut malformed_reference = raw.encoded;
        malformed_reference
            [crate::STRUCTURE_VALUE_HEADER_SIZE..crate::STRUCTURE_VALUE_HEADER_SIZE + 16]
            .fill(0);
        let malformed_tree = tree
            .upsert(&mut pages, Csn::new(5)?, field_key, malformed_reference)?
            .tree;
        assert!(matches!(
            delta_hash_field_state_latest_at_v3(&pages, &pool, malformed_tree, b"record", b"large",),
            Err(NativeRuntimeError::InvalidStructureTree)
        ));
        Ok(())
    }
}
