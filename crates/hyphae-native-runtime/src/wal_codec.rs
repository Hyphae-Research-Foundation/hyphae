// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;

use hyphae_native_types::{
    CatalogVersion, Csn, DurabilityClass, EngineKind, Lsn, ManifestGeneration, ObjectId,
    PageGeneration, PageId, TransactionId,
};
use hyphae_native_wal::{PendingRecord, RecordKind, WAL_RECORD_BODY_SIZE, WalRecord};
use thiserror::Error;

const BEGIN_MAGIC: &[u8; 8] = b"HYBGN001";
const MUTATION_MAGIC: &[u8; 8] = b"HYMUT001";
const COMMIT_MAGIC_V1: &[u8; 8] = b"HYCMT001";
const COMMIT_MAGIC_V2: &[u8; 8] = b"HYCMT002";
const COMMIT_V1_SIZE: usize = 124;
const COMMIT_V2_SIZE: usize = 140;
const ABORT_MAGIC: &[u8; 8] = b"HYABT001";
const CHECKPOINT_MAGIC: &[u8; 8] = b"HYCHK001";
const OUTCOME_MAGIC: &[u8; 8] = b"HYOUT001";
const OUTCOME_BODY_SIZE: usize = 120;
const ANN_DELTA_AUTHORITY_MAGIC_V1: &[u8; 8] = b"HYANNA01";
const ANN_DELTA_AUTHORITY_V1_SIZE: usize = 16;
const ANN_DELTA_AUTHORITY_MAGIC_V2: &[u8; 8] = b"HYANNA02";
pub(crate) const ANN_DELTA_AUTHORITY_V2_SIZE: usize = 184;
const ANN_DELTA_AUTHORITY_V2_M04_TO_M05: u8 = 1;
const ANN_DELTA_AUTHORITY_V2_MAX_OPERATIONS: u32 = 4_096;
const ANN_CONSOLIDATION_V1_SIZE: usize = 112;
const ANN_CONSOLIDATION_V2_SIZE: usize = 384;
const ROOT_COUNT: usize = 4;
const MUTATION_HAS_EXPIRY: u8 = 1;
const MUTATION_BODY_HEADER_SIZE: usize = 44;
const MAX_TRANSACTION_MUTATION_BYTES: u64 = 64 * 1_024 * 1_024;
const MAX_TRANSACTION_MUTATIONS: u64 = MAX_TRANSACTION_MUTATION_BYTES / 44;

#[derive(Debug, Error)]
pub(crate) enum WalSemanticError {
    #[error("native transaction WAL body has invalid magic or reserved bytes")]
    InvalidBody,
    #[error("native transaction WAL has an invalid sequence")]
    InvalidSequence,
    #[error("native transaction WAL digest or aggregate counts do not match")]
    ContentMismatch,
    #[error("native transaction WAL contains an invalid identity")]
    InvalidIdentity,
    #[error("native transaction WAL body length exceeds its canonical field")]
    LengthOverflow,
    #[error(transparent)]
    Frame(#[from] hyphae_native_wal::WalError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum Opcode {
    CreateTable = 1,
    InsertRow = 2,
    SetValue = 3,
    CreateIndex = 4,
    IndexDocument = 5,
    UpdateRow = 6,
    DeleteRow = 7,
    DeleteValue = 8,
    ExpireValue = 9,
    CreateHash = 10,
    SetHashField = 11,
    DeleteHashField = 12,
    CreateSecondaryIndex = 13,
    CreateSet = 14,
    AddSetMember = 15,
    DeleteSetMember = 16,
    CreateAnnIndex = 17,
    UpsertVector = 18,
    DeleteVector = 19,
    CreateList = 20,
    PushListHead = 21,
    PushListTail = 22,
    PopListHead = 23,
    PopListTail = 24,
    CreateSortedSet = 25,
    UpsertSortedSetMember = 26,
    DeleteSortedSetMember = 27,
    CompactStructure = 28,
    VacuumPageGeneration = 29,
    DeleteHash = 30,
    ExpireHash = 31,
    ExpireHashField = 32,
    ExpireSet = 33,
    DeleteSet = 34,
    DeleteList = 35,
    ExpireList = 36,
    ReplaceDocument = 37,
    DeleteDocument = 38,
    CompactSearch = 39,
    DropSecondaryIndex = 40,
    RenameTable = 41,
    MigrateStructureV3 = 42,
    DropTable = 43,
    CreateStream = 44,
    AppendStreamEntry = 45,
    DeleteStream = 46,
    ExpireStream = 47,
    DeleteSortedSet = 48,
    ExpireSortedSet = 49,
    ConsolidateAnn = 50,
    CreateCatalogObjectV2 = 51,
    CleanupStructureRetirementV3 = 52,
    PublishInitialAnnBulk = 53,
    MigrateCatalogV7 = 54,
    AnnDeltaAuthorityV1 = 55,
    AnnDeltaAuthorityV2 = 56,
    FenceVectorAbsence = 57,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Mutation {
    pub(crate) engine: EngineKind,
    pub(crate) opcode: Opcode,
    pub(crate) target: Option<ObjectId>,
    pub(crate) key: Vec<u8>,
    pub(crate) value: Vec<u8>,
    pub(crate) expires_at_micros: Option<i64>,
}

impl Mutation {
    fn encode(&self) -> Result<Vec<u8>, WalSemanticError> {
        let capacity = MUTATION_BODY_HEADER_SIZE
            .checked_add(self.key.len())
            .and_then(|bytes| bytes.checked_add(self.value.len()))
            .ok_or(WalSemanticError::LengthOverflow)?;
        let mut bytes = Vec::with_capacity(capacity);
        bytes.extend_from_slice(MUTATION_MAGIC);
        bytes.push(self.opcode as u8);
        bytes.push(self.engine as u8);
        bytes.push(u8::from(self.expires_at_micros.is_some()) * MUTATION_HAS_EXPIRY);
        bytes.push(0);
        bytes.extend_from_slice(&self.target.map_or(0, ObjectId::get).to_le_bytes());
        bytes.extend_from_slice(&self.expires_at_micros.unwrap_or(i64::MAX).to_le_bytes());
        put_len(&mut bytes, self.key.len())?;
        put_len(&mut bytes, self.value.len())?;
        bytes.extend_from_slice(&self.key);
        bytes.extend_from_slice(&self.value);
        Ok(bytes)
    }
}

#[cfg(test)]
pub(crate) fn ann_delta_authority_marker_v1(index: ObjectId) -> Mutation {
    let mut value = Vec::with_capacity(ANN_DELTA_AUTHORITY_V1_SIZE);
    value.extend_from_slice(ANN_DELTA_AUTHORITY_MAGIC_V1);
    value.extend_from_slice(&[0; 8]);
    Mutation {
        engine: EngineKind::Search,
        opcode: Opcode::AnnDeltaAuthorityV1,
        target: Some(index),
        key: Vec::new(),
        value,
        expires_at_micros: None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AnnDeltaAuthorityV2 {
    pub(crate) index: ObjectId,
    pub(crate) operation_count: u32,
    pub(crate) prior_view_identity: [u8; 32],
    pub(crate) result_view_identity: [u8; 32],
    pub(crate) prior_overlay_root: [u8; 32],
    pub(crate) result_overlay_root: [u8; 32],
    pub(crate) prior_next_sequence: u64,
    pub(crate) result_next_sequence: u64,
    pub(crate) m04_to_m05: bool,
}

pub(crate) fn ann_delta_authority_marker_v2(authority: AnnDeltaAuthorityV2) -> Mutation {
    let mut value = Vec::with_capacity(ANN_DELTA_AUTHORITY_V2_SIZE);
    value.extend_from_slice(ANN_DELTA_AUTHORITY_MAGIC_V2);
    value.push(u8::from(authority.m04_to_m05) * ANN_DELTA_AUTHORITY_V2_M04_TO_M05);
    value.extend_from_slice(&[0; 7]);
    value.extend_from_slice(&authority.index.get().to_be_bytes());
    value.extend_from_slice(&authority.operation_count.to_le_bytes());
    value.extend_from_slice(&[0; 4]);
    value.extend_from_slice(&authority.prior_view_identity);
    value.extend_from_slice(&authority.result_view_identity);
    value.extend_from_slice(&authority.prior_overlay_root);
    value.extend_from_slice(&authority.result_overlay_root);
    value.extend_from_slice(&authority.prior_next_sequence.to_le_bytes());
    value.extend_from_slice(&authority.result_next_sequence.to_le_bytes());
    Mutation {
        engine: EngineKind::Search,
        opcode: Opcode::AnnDeltaAuthorityV2,
        target: Some(authority.index),
        key: Vec::new(),
        value,
        expires_at_micros: None,
    }
}

pub(crate) fn ann_delta_authority_v2(
    mutation: &Mutation,
) -> Result<AnnDeltaAuthorityV2, WalSemanticError> {
    if mutation.engine != EngineKind::Search
        || mutation.opcode != Opcode::AnnDeltaAuthorityV2
        || !mutation.key.is_empty()
        || mutation.expires_at_micros.is_some()
    {
        return Err(WalSemanticError::InvalidBody);
    }
    decode_ann_delta_authority_v2(&mutation.value, mutation.target)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommitManifest {
    pub(crate) read_csn: Option<Csn>,
    pub(crate) commit_csn: Csn,
    pub(crate) catalog_version: CatalogVersion,
    pub(crate) blob_generation: u64,
    pub(crate) mutation_count: u32,
    pub(crate) mutation_bytes: u64,
    pub(crate) logical_time_micros: i64,
    pub(crate) mutation_digest: [u8; 32],
    pub(crate) roots: [PageId; ROOT_COUNT],
    pub(crate) page_generation: PageGeneration,
    pub(crate) retention_floor_csn: Csn,
}

impl CommitManifest {
    fn encode(&self) -> Vec<u8> {
        let is_v1 =
            self.page_generation == PageGeneration::FIRST && self.retention_floor_csn == Csn::FIRST;
        let mut bytes = Vec::with_capacity(if is_v1 {
            COMMIT_V1_SIZE
        } else {
            COMMIT_V2_SIZE
        });
        bytes.extend_from_slice(if is_v1 {
            COMMIT_MAGIC_V1
        } else {
            COMMIT_MAGIC_V2
        });
        bytes.extend_from_slice(&self.read_csn.map_or(0, Csn::get).to_le_bytes());
        bytes.extend_from_slice(&self.commit_csn.get().to_le_bytes());
        bytes.extend_from_slice(&self.catalog_version.get().to_le_bytes());
        bytes.extend_from_slice(&self.blob_generation.to_le_bytes());
        bytes.extend_from_slice(&self.mutation_count.to_le_bytes());
        bytes.extend_from_slice(&self.mutation_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.logical_time_micros.to_le_bytes());
        bytes.extend_from_slice(&self.mutation_digest);
        for root in self.roots {
            bytes.extend_from_slice(&root.get().to_le_bytes());
        }
        if !is_v1 {
            bytes.extend_from_slice(&self.page_generation.get().to_le_bytes());
            bytes.extend_from_slice(&self.retention_floor_csn.get().to_le_bytes());
        }
        bytes
    }

    fn decode(body: &[u8]) -> Result<Self, WalSemanticError> {
        let is_v1 =
            body.len() == COMMIT_V1_SIZE && body.get(..8) == Some(COMMIT_MAGIC_V1.as_slice());
        let is_v2 =
            body.len() == COMMIT_V2_SIZE && body.get(..8) == Some(COMMIT_MAGIC_V2.as_slice());
        if !is_v1 && !is_v2 {
            return Err(WalSemanticError::InvalidBody);
        }
        let read_csn = optional_csn(read_u64(&body[8..16]))?;
        let commit_csn =
            Csn::new(read_u64(&body[16..24])).map_err(|_| WalSemanticError::InvalidIdentity)?;
        let catalog_version = CatalogVersion::new(read_u64(&body[24..32]))
            .map_err(|_| WalSemanticError::InvalidIdentity)?;
        let blob_generation = read_u64(&body[32..40]);
        let mutation_count = read_u32(&body[40..44]);
        let mutation_bytes = read_u64(&body[44..52]);
        let logical_time_micros = read_i64(&body[52..60]);
        let mut mutation_digest = [0_u8; 32];
        mutation_digest.copy_from_slice(&body[60..92]);
        let mut roots =
            [PageId::new(1).map_err(|_| WalSemanticError::InvalidIdentity)?; ROOT_COUNT];
        for (index, root) in roots.iter_mut().enumerate() {
            let start = 92 + index * 8;
            *root = PageId::new(read_u64(&body[start..start + 8]))
                .map_err(|_| WalSemanticError::InvalidIdentity)?;
        }
        let (page_generation, retention_floor_csn) = if is_v1 {
            (PageGeneration::FIRST, Csn::FIRST)
        } else {
            (
                PageGeneration::new(read_u64(&body[124..132]))
                    .map_err(|_| WalSemanticError::InvalidIdentity)?,
                Csn::new(read_u64(&body[132..140]))
                    .map_err(|_| WalSemanticError::InvalidIdentity)?,
            )
        };
        validate_storage_state(page_generation, retention_floor_csn, commit_csn)?;
        Ok(Self {
            read_csn,
            commit_csn,
            catalog_version,
            blob_generation,
            mutation_count,
            mutation_bytes,
            logical_time_micros,
            mutation_digest,
            roots,
            page_generation,
            retention_floor_csn,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoveredCommit {
    pub(crate) transaction_id: TransactionId,
    pub(crate) commit_lsn: Lsn,
    pub(crate) durability: DurabilityClass,
    pub(crate) manifest: CommitManifest,
    pub(crate) mutations: Vec<Mutation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoveredCheckpoint {
    pub(crate) transaction_id: TransactionId,
    pub(crate) checkpoint_lsn: Lsn,
    pub(crate) visible_csn: Csn,
    pub(crate) manifest_generation: ManifestGeneration,
    pub(crate) manifest_digest: [u8; 32],
    pub(crate) previous_checkpoint_lsn: Option<Lsn>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RecoveredOutcome {
    pub(crate) resolution_id: TransactionId,
    pub(crate) runtime_transaction_id: TransactionId,
    pub(crate) principal_hash: [u8; 32],
    pub(crate) idempotency_token: [u8; 32],
    pub(crate) state: u8,
    pub(crate) commit_csn: Option<Csn>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RecoveredWal {
    pub(crate) commits: Vec<RecoveredCommit>,
    pub(crate) checkpoints: Vec<RecoveredCheckpoint>,
    pub(crate) outcomes: Vec<RecoveredOutcome>,
    pub(crate) dangling_transaction: Option<TransactionId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WalSemanticBase {
    pub(crate) visible_csn: Csn,
    pub(crate) checkpoint_lsn: Lsn,
    pub(crate) manifest_generation: ManifestGeneration,
}

pub(crate) struct TransactionPlan<'mutations> {
    pub(crate) transaction_id: TransactionId,
    pub(crate) read_csn: Option<Csn>,
    pub(crate) catalog_version: CatalogVersion,
    pub(crate) logical_time_micros: i64,
    pub(crate) durability: DurabilityClass,
    pub(crate) mutations: &'mutations [Mutation],
    pub(crate) commit_csn: Csn,
    pub(crate) roots: [PageId; ROOT_COUNT],
    pub(crate) blob_generation: u64,
    pub(crate) page_generation: PageGeneration,
    pub(crate) retention_floor_csn: Csn,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TransactionWalPreflight {
    mutation_count: u32,
    mutation_bytes: u64,
    peak_memory_bytes: u64,
}

impl TransactionWalPreflight {
    pub(crate) fn mutation_count(self) -> usize {
        usize::try_from(self.mutation_count).unwrap_or(usize::MAX)
    }

    pub(crate) const fn peak_memory_bytes(self) -> u64 {
        self.peak_memory_bytes
    }

    #[cfg(test)]
    const fn mutation_bytes(self) -> u64 {
        self.mutation_bytes
    }
}

#[derive(Default)]
pub(crate) struct TransactionWalPreflightBuilder {
    mutation_count: u32,
    mutation_bytes: u64,
    cloned_payload_bytes: u64,
}

impl TransactionWalPreflightBuilder {
    pub(crate) fn push(
        &mut self,
        key_length: usize,
        value_length: usize,
    ) -> Result<(), WalSemanticError> {
        u32::try_from(key_length).map_err(|_| WalSemanticError::LengthOverflow)?;
        u32::try_from(value_length).map_err(|_| WalSemanticError::LengthOverflow)?;
        let body_length = MUTATION_BODY_HEADER_SIZE
            .checked_add(key_length)
            .and_then(|length| length.checked_add(value_length))
            .ok_or(WalSemanticError::LengthOverflow)?;
        if body_length > WAL_RECORD_BODY_SIZE {
            return Err(WalSemanticError::LengthOverflow);
        }
        self.mutation_count = self
            .mutation_count
            .checked_add(1)
            .ok_or(WalSemanticError::LengthOverflow)?;
        self.mutation_bytes = self
            .mutation_bytes
            .checked_add(u64::try_from(body_length).map_err(|_| WalSemanticError::LengthOverflow)?)
            .ok_or(WalSemanticError::LengthOverflow)?;
        self.cloned_payload_bytes = self
            .cloned_payload_bytes
            .checked_add(u64::try_from(key_length).map_err(|_| WalSemanticError::LengthOverflow)?)
            .and_then(|bytes| bytes.checked_add(u64::try_from(value_length).ok()?))
            .ok_or(WalSemanticError::LengthOverflow)?;
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<TransactionWalPreflight, WalSemanticError> {
        validate_transaction_bounds(self.mutation_count, self.mutation_bytes)?;
        let count = u64::from(self.mutation_count);
        let mutation_slots = count
            .checked_mul(
                u64::try_from(std::mem::size_of::<Mutation>())
                    .map_err(|_| WalSemanticError::LengthOverflow)?,
            )
            .ok_or(WalSemanticError::LengthOverflow)?;
        let encoded_slots = count
            .checked_mul(
                u64::try_from(std::mem::size_of::<Vec<u8>>())
                    .map_err(|_| WalSemanticError::LengthOverflow)?,
            )
            .ok_or(WalSemanticError::LengthOverflow)?;
        let pending_slots = count
            .checked_add(2)
            .and_then(|records| {
                records.checked_mul(u64::try_from(std::mem::size_of::<PendingRecord>()).ok()?)
            })
            .ok_or(WalSemanticError::LengthOverflow)?;
        let peak_memory_bytes = self
            .cloned_payload_bytes
            .checked_add(mutation_slots)
            .and_then(|bytes| bytes.checked_add(self.mutation_bytes))
            .and_then(|bytes| bytes.checked_add(encoded_slots))
            .and_then(|bytes| bytes.checked_add(pending_slots))
            .and_then(|bytes| bytes.checked_add(52 + COMMIT_V2_SIZE as u64))
            .ok_or(WalSemanticError::LengthOverflow)?;
        Ok(TransactionWalPreflight {
            mutation_count: self.mutation_count,
            mutation_bytes: self.mutation_bytes,
            peak_memory_bytes,
        })
    }
}

pub(crate) fn encode_transaction(
    plan: &TransactionPlan<'_>,
) -> Result<Vec<PendingRecord>, WalSemanticError> {
    validate_storage_state(
        plan.page_generation,
        plan.retention_floor_csn,
        plan.commit_csn,
    )?;
    let mutation_count =
        u32::try_from(plan.mutations.len()).map_err(|_| WalSemanticError::LengthOverflow)?;
    let mutation_bytes = plan.mutations.iter().try_fold(0_u64, |total, mutation| {
        let body_length = MUTATION_BODY_HEADER_SIZE
            .checked_add(mutation.key.len())
            .and_then(|length| length.checked_add(mutation.value.len()))
            .ok_or(WalSemanticError::LengthOverflow)?;
        if body_length > WAL_RECORD_BODY_SIZE {
            return Err(WalSemanticError::LengthOverflow);
        }
        u32::try_from(mutation.key.len()).map_err(|_| WalSemanticError::LengthOverflow)?;
        u32::try_from(mutation.value.len()).map_err(|_| WalSemanticError::LengthOverflow)?;
        total
            .checked_add(u64::try_from(body_length).map_err(|_| WalSemanticError::LengthOverflow)?)
            .ok_or(WalSemanticError::LengthOverflow)
    })?;
    validate_transaction_bounds(mutation_count, mutation_bytes)?;
    validate_ann_delta_authority_markers(plan.mutations.iter())?;
    let encoded_mutations = plan
        .mutations
        .iter()
        .map(Mutation::encode)
        .collect::<Result<Vec<_>, _>>()?;
    let digest = mutation_digest(
        plan.mutations
            .iter()
            .zip(encoded_mutations.iter())
            .map(|(mutation, body)| (mutation.engine, body.as_slice())),
    )?;
    let begin = encode_begin(
        plan.read_csn,
        plan.catalog_version,
        plan.logical_time_micros,
        plan.durability,
        mutation_count,
        mutation_bytes,
    );
    let manifest = CommitManifest {
        read_csn: plan.read_csn,
        commit_csn: plan.commit_csn,
        catalog_version: plan.catalog_version,
        blob_generation: plan.blob_generation,
        mutation_count,
        mutation_bytes,
        logical_time_micros: plan.logical_time_micros,
        mutation_digest: digest,
        roots: plan.roots,
        page_generation: plan.page_generation,
        retention_floor_csn: plan.retention_floor_csn,
    };
    let mut records = Vec::with_capacity(plan.mutations.len() + 2);
    records.push(PendingRecord::new(
        RecordKind::Begin,
        EngineKind::Kernel,
        0,
        plan.transaction_id,
        begin,
    )?);
    for (mutation, body) in plan.mutations.iter().zip(encoded_mutations) {
        records.push(PendingRecord::new(
            RecordKind::Mutation,
            mutation.engine,
            0,
            plan.transaction_id,
            body,
        )?);
    }
    records.push(PendingRecord::new(
        RecordKind::Commit,
        EngineKind::Kernel,
        0,
        plan.transaction_id,
        manifest.encode(),
    )?);
    Ok(records)
}

fn validate_storage_state(
    page_generation: PageGeneration,
    retention_floor_csn: Csn,
    commit_csn: Csn,
) -> Result<(), WalSemanticError> {
    let first_generation = page_generation == PageGeneration::FIRST;
    let first_floor = retention_floor_csn == Csn::FIRST;
    if retention_floor_csn > commit_csn || first_generation != first_floor {
        return Err(WalSemanticError::InvalidSequence);
    }
    Ok(())
}

fn validate_transaction_bounds(
    mutation_count: u32,
    mutation_bytes: u64,
) -> Result<(), WalSemanticError> {
    let minimum_bytes = u64::from(mutation_count)
        .checked_mul(
            u64::try_from(MUTATION_BODY_HEADER_SIZE)
                .map_err(|_| WalSemanticError::LengthOverflow)?,
        )
        .ok_or(WalSemanticError::LengthOverflow)?;
    let maximum_bytes = u64::from(mutation_count)
        .checked_mul(
            u64::try_from(WAL_RECORD_BODY_SIZE).map_err(|_| WalSemanticError::LengthOverflow)?,
        )
        .ok_or(WalSemanticError::LengthOverflow)?;
    let decoded_mutation_slots = u64::from(mutation_count)
        .checked_mul(
            u64::try_from(std::mem::size_of::<Mutation>())
                .map_err(|_| WalSemanticError::LengthOverflow)?,
        )
        .ok_or(WalSemanticError::LengthOverflow)?;
    let decoded_mutation_memory = mutation_bytes.saturating_add(decoded_mutation_slots);
    if mutation_count == 0
        || u64::from(mutation_count) > MAX_TRANSACTION_MUTATIONS
        || mutation_bytes < minimum_bytes
        || mutation_bytes > maximum_bytes
        || mutation_bytes > MAX_TRANSACTION_MUTATION_BYTES
        || decoded_mutation_memory > MAX_TRANSACTION_MUTATION_BYTES
    {
        return Err(WalSemanticError::InvalidSequence);
    }
    Ok(())
}

pub(crate) fn encode_checkpoint(
    transaction_id: TransactionId,
    visible_csn: Csn,
    manifest_generation: ManifestGeneration,
    manifest_digest: [u8; 32],
    previous_checkpoint_lsn: Option<Lsn>,
) -> Result<PendingRecord, WalSemanticError> {
    let mut body = Vec::with_capacity(64);
    body.extend_from_slice(CHECKPOINT_MAGIC);
    body.extend_from_slice(&visible_csn.get().to_le_bytes());
    body.extend_from_slice(&manifest_generation.get().to_le_bytes());
    body.extend_from_slice(&manifest_digest);
    body.extend_from_slice(&previous_checkpoint_lsn.map_or(0, Lsn::get).to_le_bytes());
    Ok(PendingRecord::new(
        RecordKind::Checkpoint,
        EngineKind::Kernel,
        0,
        transaction_id,
        body,
    )?)
}

pub(crate) fn encode_abort(
    transaction_id: TransactionId,
) -> Result<PendingRecord, WalSemanticError> {
    Ok(PendingRecord::new(
        RecordKind::Abort,
        EngineKind::Kernel,
        0,
        transaction_id,
        ABORT_MAGIC.to_vec(),
    )?)
}

pub(crate) fn encode_outcome(
    resolution_id: TransactionId,
    runtime_transaction_id: TransactionId,
    principal_hash: [u8; 32],
    idempotency_token: [u8; 32],
    state: u8,
    commit_csn: Option<Csn>,
) -> Result<PendingRecord, WalSemanticError> {
    if !(1..=3).contains(&state) || (state == 1) != commit_csn.is_some() {
        return Err(WalSemanticError::InvalidSequence);
    }
    let mut body = Vec::with_capacity(OUTCOME_BODY_SIZE);
    body.extend_from_slice(OUTCOME_MAGIC);
    body.push(state);
    body.extend_from_slice(&[0; 7]);
    body.extend_from_slice(&resolution_id.get().to_le_bytes());
    body.extend_from_slice(&runtime_transaction_id.get().to_le_bytes());
    body.extend_from_slice(&principal_hash);
    body.extend_from_slice(&idempotency_token);
    body.extend_from_slice(&commit_csn.map_or(0, Csn::get).to_le_bytes());
    Ok(PendingRecord::new(
        RecordKind::Catalog,
        EngineKind::Kernel,
        0,
        runtime_transaction_id,
        body,
    )?)
}

#[cfg(test)]
pub(crate) fn recover_wal(records: &[WalRecord]) -> Result<RecoveredWal, WalSemanticError> {
    recover_wal_after(records, None)
}

pub(crate) fn recover_wal_after(
    records: &[WalRecord],
    base: Option<WalSemanticBase>,
) -> Result<RecoveredWal, WalSemanticError> {
    let mut recovered = RecoveredWal::default();
    let mut active: Option<ActiveTransaction> = None;
    for record in records {
        match record.kind() {
            RecordKind::Begin => {
                if active.is_some() || record.engine() != EngineKind::Kernel {
                    return Err(WalSemanticError::InvalidSequence);
                }
                let begin = decode_begin(record.body())?;
                let mutation_capacity = usize::try_from(begin.mutation_count)
                    .map_err(|_| WalSemanticError::LengthOverflow)?;
                active = Some(ActiveTransaction {
                    transaction_id: record.transaction_id(),
                    begin,
                    mutations: Vec::with_capacity(mutation_capacity),
                    mutation_bytes: 0,
                    mutation_hasher: mutation_digest_hasher(),
                });
            }
            RecordKind::Mutation => {
                let transaction = active.as_mut().ok_or(WalSemanticError::InvalidSequence)?;
                if transaction.transaction_id != record.transaction_id() {
                    return Err(WalSemanticError::InvalidSequence);
                }
                transaction.admit_mutation_body(record.engine(), record.body())?;
                let mutation = decode_mutation(record.engine(), record.body())?;
                transaction.mutations.push(mutation);
            }
            RecordKind::Commit => {
                let transaction = active.take().ok_or(WalSemanticError::InvalidSequence)?;
                if transaction.transaction_id != record.transaction_id()
                    || record.engine() != EngineKind::Kernel
                {
                    return Err(WalSemanticError::InvalidSequence);
                }
                let manifest = CommitManifest::decode(record.body())?;
                transaction.validate(&manifest)?;
                if recovered
                    .commits
                    .last()
                    .is_some_and(|prior: &RecoveredCommit| {
                        prior.manifest.commit_csn >= manifest.commit_csn
                    })
                {
                    return Err(WalSemanticError::InvalidSequence);
                }
                recovered.commits.push(RecoveredCommit {
                    transaction_id: record.transaction_id(),
                    commit_lsn: record.lsn(),
                    durability: transaction.begin.durability,
                    manifest,
                    mutations: transaction.mutations,
                });
            }
            RecordKind::Abort => {
                let transaction = active.take().ok_or(WalSemanticError::InvalidSequence)?;
                if transaction.transaction_id != record.transaction_id()
                    || record.engine() != EngineKind::Kernel
                    || record.body() != ABORT_MAGIC
                {
                    return Err(WalSemanticError::InvalidSequence);
                }
            }
            RecordKind::Checkpoint => {
                recover_checkpoint(record, active.is_some(), &mut recovered, base)?;
            }
            RecordKind::Catalog => {
                recover_outcome(record, active.is_some(), &mut recovered)?;
            }
        }
    }
    recovered.dangling_transaction = active.map(|transaction| transaction.transaction_id);
    Ok(recovered)
}

fn recover_outcome(
    record: &WalRecord,
    transaction_active: bool,
    recovered: &mut RecoveredWal,
) -> Result<(), WalSemanticError> {
    if transaction_active || record.engine() != EngineKind::Kernel || record.flags() != 0 {
        return Err(WalSemanticError::InvalidSequence);
    }
    let outcome = decode_outcome(record)?;
    let prior = recovered.outcomes.iter().rev().find(|prior| {
        prior.resolution_id == outcome.resolution_id
            || (prior.principal_hash == outcome.principal_hash
                && prior.idempotency_token == outcome.idempotency_token)
    });
    if prior.is_some_and(|prior| {
        prior.resolution_id != outcome.resolution_id
            || prior.runtime_transaction_id != outcome.runtime_transaction_id
            || prior.principal_hash != outcome.principal_hash
            || prior.idempotency_token != outcome.idempotency_token
            || prior.state != 3
    }) {
        return Err(WalSemanticError::InvalidSequence);
    }
    if outcome.runtime_transaction_id != record.transaction_id()
        || (outcome.state == 1
            && !recovered.commits.iter().any(|commit| {
                commit.transaction_id == outcome.runtime_transaction_id
                    && Some(commit.manifest.commit_csn) == outcome.commit_csn
            }))
    {
        return Err(WalSemanticError::InvalidSequence);
    }
    if let Some(position) = recovered
        .outcomes
        .iter()
        .position(|prior| prior.resolution_id == outcome.resolution_id)
    {
        recovered.outcomes[position] = outcome;
    } else {
        recovered.outcomes.push(outcome);
    }
    Ok(())
}

fn decode_outcome(record: &WalRecord) -> Result<RecoveredOutcome, WalSemanticError> {
    let body = record.body();
    if body.len() != OUTCOME_BODY_SIZE
        || body.get(..8) != Some(OUTCOME_MAGIC.as_slice())
        || body[9..16].iter().any(|byte| *byte != 0)
    {
        return Err(WalSemanticError::InvalidBody);
    }
    let state = body[8];
    if !(1..=3).contains(&state) {
        return Err(WalSemanticError::InvalidBody);
    }
    let resolution_id = TransactionId::new(read_u128(&body[16..32]))
        .map_err(|_| WalSemanticError::InvalidIdentity)?;
    let runtime_transaction_id = TransactionId::new(read_u128(&body[32..48]))
        .map_err(|_| WalSemanticError::InvalidIdentity)?;
    let mut principal_hash = [0; 32];
    principal_hash.copy_from_slice(&body[48..80]);
    let mut idempotency_token = [0; 32];
    idempotency_token.copy_from_slice(&body[80..112]);
    if principal_hash == [0; 32] || idempotency_token == [0; 32] {
        return Err(WalSemanticError::InvalidIdentity);
    }
    let commit_csn = optional_csn(read_u64(&body[112..120]))?;
    if (state == 1) != commit_csn.is_some() {
        return Err(WalSemanticError::InvalidSequence);
    }
    Ok(RecoveredOutcome {
        resolution_id,
        runtime_transaction_id,
        principal_hash,
        idempotency_token,
        state,
        commit_csn,
    })
}

fn recover_checkpoint(
    record: &WalRecord,
    transaction_active: bool,
    recovered: &mut RecoveredWal,
    base: Option<WalSemanticBase>,
) -> Result<(), WalSemanticError> {
    if transaction_active || record.engine() != EngineKind::Kernel || record.flags() != 0 {
        return Err(WalSemanticError::InvalidSequence);
    }
    let checkpoint = decode_checkpoint(record)?;
    let committed = recovered
        .commits
        .iter()
        .any(|commit| commit.manifest.commit_csn == checkpoint.visible_csn)
        || base.is_some_and(|base| base.visible_csn == checkpoint.visible_csn);
    if !committed {
        return Err(WalSemanticError::InvalidSequence);
    }
    if let Some(previous) = recovered.checkpoints.last() {
        if checkpoint.previous_checkpoint_lsn != Some(previous.checkpoint_lsn)
            || checkpoint.manifest_generation <= previous.manifest_generation
            || checkpoint.visible_csn < previous.visible_csn
        {
            return Err(WalSemanticError::InvalidSequence);
        }
    } else if let Some(base) = base {
        if checkpoint.previous_checkpoint_lsn != Some(base.checkpoint_lsn)
            || checkpoint.manifest_generation <= base.manifest_generation
            || checkpoint.visible_csn < base.visible_csn
        {
            return Err(WalSemanticError::InvalidSequence);
        }
    } else if checkpoint.previous_checkpoint_lsn.is_some() {
        return Err(WalSemanticError::InvalidSequence);
    }
    recovered.checkpoints.push(checkpoint);
    Ok(())
}

fn decode_checkpoint(record: &WalRecord) -> Result<RecoveredCheckpoint, WalSemanticError> {
    let body = record.body();
    if body.len() != 64 || body.get(..8) != Some(CHECKPOINT_MAGIC.as_slice()) {
        return Err(WalSemanticError::InvalidBody);
    }
    let visible_csn =
        Csn::new(read_u64(&body[8..16])).map_err(|_| WalSemanticError::InvalidIdentity)?;
    let manifest_generation = ManifestGeneration::new(read_u64(&body[16..24]))
        .map_err(|_| WalSemanticError::InvalidIdentity)?;
    let mut manifest_digest = [0_u8; 32];
    manifest_digest.copy_from_slice(&body[24..56]);
    if manifest_digest == [0; 32] {
        return Err(WalSemanticError::InvalidIdentity);
    }
    let previous_checkpoint_lsn = optional_lsn(read_u64(&body[56..64]))?;
    Ok(RecoveredCheckpoint {
        transaction_id: record.transaction_id(),
        checkpoint_lsn: record.lsn(),
        visible_csn,
        manifest_generation,
        manifest_digest,
        previous_checkpoint_lsn,
    })
}

fn encode_begin(
    read_csn: Option<Csn>,
    catalog_version: CatalogVersion,
    logical_time_micros: i64,
    durability: DurabilityClass,
    mutation_count: u32,
    mutation_bytes: u64,
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(52);
    bytes.extend_from_slice(BEGIN_MAGIC);
    bytes.extend_from_slice(&read_csn.map_or(0, Csn::get).to_le_bytes());
    bytes.extend_from_slice(&catalog_version.get().to_le_bytes());
    bytes.extend_from_slice(&logical_time_micros.to_le_bytes());
    bytes.push(durability as u8);
    bytes.extend_from_slice(&[0; 7]);
    bytes.extend_from_slice(&mutation_count.to_le_bytes());
    bytes.extend_from_slice(&mutation_bytes.to_le_bytes());
    bytes
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Begin {
    read_csn: Option<Csn>,
    catalog_version: CatalogVersion,
    logical_time_micros: i64,
    durability: DurabilityClass,
    mutation_count: u32,
    mutation_bytes: u64,
}

fn decode_begin(body: &[u8]) -> Result<Begin, WalSemanticError> {
    if body.len() != 52
        || body.get(..8) != Some(BEGIN_MAGIC.as_slice())
        || body[33..40].iter().any(|byte| *byte != 0)
    {
        return Err(WalSemanticError::InvalidBody);
    }
    let durability = match body[32] {
        1 => DurabilityClass::Strict,
        2 => DurabilityClass::Group,
        3 => DurabilityClass::Memory,
        _ => return Err(WalSemanticError::InvalidBody),
    };
    let begin = Begin {
        read_csn: optional_csn(read_u64(&body[8..16]))?,
        catalog_version: CatalogVersion::new(read_u64(&body[16..24]))
            .map_err(|_| WalSemanticError::InvalidIdentity)?,
        logical_time_micros: read_i64(&body[24..32]),
        durability,
        mutation_count: read_u32(&body[40..44]),
        mutation_bytes: read_u64(&body[44..52]),
    };
    validate_transaction_bounds(begin.mutation_count, begin.mutation_bytes)?;
    Ok(begin)
}

impl ActiveTransaction {
    fn admit_mutation_body(
        &mut self,
        engine: EngineKind,
        body: &[u8],
    ) -> Result<(), WalSemanticError> {
        let next_count = u64::try_from(self.mutations.len())
            .map_err(|_| WalSemanticError::LengthOverflow)?
            .checked_add(1)
            .ok_or(WalSemanticError::LengthOverflow)?;
        let next_bytes = self
            .mutation_bytes
            .checked_add(u64::try_from(body.len()).map_err(|_| WalSemanticError::LengthOverflow)?)
            .ok_or(WalSemanticError::LengthOverflow)?;
        if next_count > u64::from(self.begin.mutation_count)
            || next_bytes > self.begin.mutation_bytes
            || next_count > MAX_TRANSACTION_MUTATIONS
            || next_bytes > MAX_TRANSACTION_MUTATION_BYTES
        {
            return Err(WalSemanticError::ContentMismatch);
        }
        let body_length =
            u32::try_from(body.len()).map_err(|_| WalSemanticError::LengthOverflow)?;
        self.mutation_hasher.update(&[engine as u8]);
        self.mutation_hasher.update(&body_length.to_le_bytes());
        self.mutation_hasher.update(body);
        self.mutation_bytes = next_bytes;
        Ok(())
    }

    fn validate(&self, commit: &CommitManifest) -> Result<(), WalSemanticError> {
        if self.mutations.is_empty() {
            return Err(WalSemanticError::InvalidSequence);
        }
        validate_ann_delta_authority_markers(self.mutations.iter())?;
        let count =
            u32::try_from(self.mutations.len()).map_err(|_| WalSemanticError::LengthOverflow)?;
        let bytes = self.mutation_bytes;
        let digest = *self.mutation_hasher.clone().finalize().as_bytes();
        if self.begin.read_csn != commit.read_csn
            || self.begin.catalog_version != commit.catalog_version
            || self.begin.logical_time_micros != commit.logical_time_micros
            || self.begin.mutation_count != count
            || self.begin.mutation_bytes != bytes
            || commit.mutation_count != count
            || commit.mutation_bytes != bytes
            || commit.mutation_digest != digest
        {
            return Err(WalSemanticError::ContentMismatch);
        }
        Ok(())
    }
}

struct ActiveTransaction {
    transaction_id: TransactionId,
    begin: Begin,
    mutations: Vec<Mutation>,
    mutation_bytes: u64,
    mutation_hasher: blake3::Hasher,
}

#[allow(clippy::too_many_lines)]
fn decode_opcode(value: u8) -> Result<(Opcode, EngineKind), WalSemanticError> {
    Ok(match value {
        value if value == Opcode::CreateTable as u8 => {
            (Opcode::CreateTable, EngineKind::Relational)
        }
        value if value == Opcode::InsertRow as u8 => (Opcode::InsertRow, EngineKind::Relational),
        value if value == Opcode::UpdateRow as u8 => (Opcode::UpdateRow, EngineKind::Relational),
        value if value == Opcode::DeleteRow as u8 => (Opcode::DeleteRow, EngineKind::Relational),
        value if value == Opcode::CreateSecondaryIndex as u8 => {
            (Opcode::CreateSecondaryIndex, EngineKind::Relational)
        }
        value if value == Opcode::DropSecondaryIndex as u8 => {
            (Opcode::DropSecondaryIndex, EngineKind::Relational)
        }
        value if value == Opcode::RenameTable as u8 => {
            (Opcode::RenameTable, EngineKind::Relational)
        }
        value if value == Opcode::DropTable as u8 => (Opcode::DropTable, EngineKind::Relational),
        value if value == Opcode::CreateStream as u8 => {
            (Opcode::CreateStream, EngineKind::Structure)
        }
        value if value == Opcode::AppendStreamEntry as u8 => {
            (Opcode::AppendStreamEntry, EngineKind::Structure)
        }
        value if value == Opcode::DeleteStream as u8 => {
            (Opcode::DeleteStream, EngineKind::Structure)
        }
        value if value == Opcode::ExpireStream as u8 => {
            (Opcode::ExpireStream, EngineKind::Structure)
        }
        value if value == Opcode::DeleteSortedSet as u8 => {
            (Opcode::DeleteSortedSet, EngineKind::Structure)
        }
        value if value == Opcode::ExpireSortedSet as u8 => {
            (Opcode::ExpireSortedSet, EngineKind::Structure)
        }
        value if value == Opcode::SetValue as u8 => (Opcode::SetValue, EngineKind::Structure),
        value if value == Opcode::DeleteValue as u8 => (Opcode::DeleteValue, EngineKind::Structure),
        value if value == Opcode::ExpireValue as u8 => (Opcode::ExpireValue, EngineKind::Structure),
        value if value == Opcode::CreateHash as u8 => (Opcode::CreateHash, EngineKind::Structure),
        value if value == Opcode::SetHashField as u8 => {
            (Opcode::SetHashField, EngineKind::Structure)
        }
        value if value == Opcode::DeleteHashField as u8 => {
            (Opcode::DeleteHashField, EngineKind::Structure)
        }
        value if value == Opcode::DeleteHash as u8 => (Opcode::DeleteHash, EngineKind::Structure),
        value if value == Opcode::ExpireHash as u8 => (Opcode::ExpireHash, EngineKind::Structure),
        value if value == Opcode::ExpireHashField as u8 => {
            (Opcode::ExpireHashField, EngineKind::Structure)
        }
        value if value == Opcode::CreateSet as u8 => (Opcode::CreateSet, EngineKind::Structure),
        value if value == Opcode::AddSetMember as u8 => {
            (Opcode::AddSetMember, EngineKind::Structure)
        }
        value if value == Opcode::DeleteSetMember as u8 => {
            (Opcode::DeleteSetMember, EngineKind::Structure)
        }
        value if value == Opcode::ExpireSet as u8 => (Opcode::ExpireSet, EngineKind::Structure),
        value if value == Opcode::DeleteSet as u8 => (Opcode::DeleteSet, EngineKind::Structure),
        value if value == Opcode::CreateList as u8 => (Opcode::CreateList, EngineKind::Structure),
        value if value == Opcode::DeleteList as u8 => (Opcode::DeleteList, EngineKind::Structure),
        value if value == Opcode::ExpireList as u8 => (Opcode::ExpireList, EngineKind::Structure),
        value if value == Opcode::PushListHead as u8 => {
            (Opcode::PushListHead, EngineKind::Structure)
        }
        value if value == Opcode::PushListTail as u8 => {
            (Opcode::PushListTail, EngineKind::Structure)
        }
        value if value == Opcode::PopListHead as u8 => (Opcode::PopListHead, EngineKind::Structure),
        value if value == Opcode::PopListTail as u8 => (Opcode::PopListTail, EngineKind::Structure),
        value if value == Opcode::CreateSortedSet as u8 => {
            (Opcode::CreateSortedSet, EngineKind::Structure)
        }
        value if value == Opcode::UpsertSortedSetMember as u8 => {
            (Opcode::UpsertSortedSetMember, EngineKind::Structure)
        }
        value if value == Opcode::DeleteSortedSetMember as u8 => {
            (Opcode::DeleteSortedSetMember, EngineKind::Structure)
        }
        value if value == Opcode::CompactStructure as u8 => {
            (Opcode::CompactStructure, EngineKind::Structure)
        }
        value if value == Opcode::MigrateStructureV3 as u8 => {
            (Opcode::MigrateStructureV3, EngineKind::Structure)
        }
        value if value == Opcode::CleanupStructureRetirementV3 as u8 => {
            (Opcode::CleanupStructureRetirementV3, EngineKind::Structure)
        }
        value if value == Opcode::VacuumPageGeneration as u8 => {
            (Opcode::VacuumPageGeneration, EngineKind::Kernel)
        }
        value if value == Opcode::CreateIndex as u8 => (Opcode::CreateIndex, EngineKind::Search),
        value if value == Opcode::IndexDocument as u8 => {
            (Opcode::IndexDocument, EngineKind::Search)
        }
        value if value == Opcode::ReplaceDocument as u8 => {
            (Opcode::ReplaceDocument, EngineKind::Search)
        }
        value if value == Opcode::DeleteDocument as u8 => {
            (Opcode::DeleteDocument, EngineKind::Search)
        }
        value if value == Opcode::CompactSearch as u8 => {
            (Opcode::CompactSearch, EngineKind::Search)
        }
        value if value == Opcode::CreateAnnIndex as u8 => {
            (Opcode::CreateAnnIndex, EngineKind::Search)
        }
        value if value == Opcode::UpsertVector as u8 => (Opcode::UpsertVector, EngineKind::Search),
        value if value == Opcode::DeleteVector as u8 => (Opcode::DeleteVector, EngineKind::Search),
        value if value == Opcode::ConsolidateAnn as u8 => {
            (Opcode::ConsolidateAnn, EngineKind::Search)
        }
        value if value == Opcode::PublishInitialAnnBulk as u8 => {
            (Opcode::PublishInitialAnnBulk, EngineKind::Search)
        }
        value if value == Opcode::CreateCatalogObjectV2 as u8 => {
            (Opcode::CreateCatalogObjectV2, EngineKind::Kernel)
        }
        value if value == Opcode::MigrateCatalogV7 as u8 => {
            (Opcode::MigrateCatalogV7, EngineKind::Kernel)
        }
        value if value == Opcode::AnnDeltaAuthorityV1 as u8 => {
            (Opcode::AnnDeltaAuthorityV1, EngineKind::Search)
        }
        value if value == Opcode::AnnDeltaAuthorityV2 as u8 => {
            (Opcode::AnnDeltaAuthorityV2, EngineKind::Search)
        }
        value if value == Opcode::FenceVectorAbsence as u8 => {
            (Opcode::FenceVectorAbsence, EngineKind::Search)
        }
        _ => return Err(WalSemanticError::InvalidBody),
    })
}

fn decode_mutation(engine: EngineKind, body: &[u8]) -> Result<Mutation, WalSemanticError> {
    if body.len() < 44
        || body.get(..8) != Some(MUTATION_MAGIC.as_slice())
        || body[9] != engine as u8
        || body[10] & !MUTATION_HAS_EXPIRY != 0
        || body[11] != 0
    {
        return Err(WalSemanticError::InvalidBody);
    }
    let (opcode, opcode_engine) = decode_opcode(body[8])?;
    if opcode_engine != engine {
        return Err(WalSemanticError::InvalidBody);
    }
    let key_length =
        usize::try_from(read_u32(&body[36..40])).map_err(|_| WalSemanticError::LengthOverflow)?;
    let value_length =
        usize::try_from(read_u32(&body[40..44])).map_err(|_| WalSemanticError::LengthOverflow)?;
    let expected = 44_usize
        .checked_add(key_length)
        .and_then(|size| size.checked_add(value_length))
        .ok_or(WalSemanticError::LengthOverflow)?;
    if expected != body.len() {
        return Err(WalSemanticError::InvalidBody);
    }
    let raw_target = read_u128(&body[12..28]);
    let target = if raw_target == 0 {
        None
    } else {
        Some(ObjectId::new(raw_target).map_err(|_| WalSemanticError::InvalidIdentity)?)
    };
    let raw_expiry = read_i64(&body[28..36]);
    let expires_at_micros = if body[10] == MUTATION_HAS_EXPIRY {
        Some(raw_expiry)
    } else {
        (raw_expiry != i64::MAX).then_some(raw_expiry)
    };
    let key_start = 44;
    let value_start = key_start + key_length;
    let key = &body[key_start..value_start];
    validate_mutation_shape(
        opcode,
        target.is_some(),
        value_length,
        expires_at_micros,
        key,
    )?;
    let value = &body[value_start..expected];
    if opcode == Opcode::CleanupStructureRetirementV3 {
        let entry_budget = u32::from_le_bytes(
            value
                .try_into()
                .map_err(|_| WalSemanticError::InvalidBody)?,
        );
        if !(2..=1_024).contains(&entry_budget) {
            return Err(WalSemanticError::InvalidBody);
        }
    }
    if opcode == Opcode::ConsolidateAnn && !valid_ann_consolidation(value, target) {
        return Err(WalSemanticError::InvalidBody);
    }
    if opcode == Opcode::PublishInitialAnnBulk && !valid_initial_ann_bulk_publication(value) {
        return Err(WalSemanticError::InvalidBody);
    }
    if opcode == Opcode::AnnDeltaAuthorityV1 && !valid_ann_delta_authority_v1(value) {
        return Err(WalSemanticError::InvalidBody);
    }
    if opcode == Opcode::AnnDeltaAuthorityV2 {
        decode_ann_delta_authority_v2(value, target)?;
    }
    Ok(Mutation {
        engine,
        opcode,
        target,
        key: key.to_vec(),
        value: value.to_vec(),
        expires_at_micros,
    })
}

#[allow(clippy::too_many_lines)]
fn validate_mutation_shape(
    opcode: Opcode,
    has_target: bool,
    value_length: usize,
    expires_at_micros: Option<i64>,
    key: &[u8],
) -> Result<(), WalSemanticError> {
    if matches!(
        opcode,
        Opcode::AnnDeltaAuthorityV1 | Opcode::AnnDeltaAuthorityV2
    ) {
        return validate_targeted_ann_maintenance_shape(
            has_target,
            value_length,
            if opcode == Opcode::AnnDeltaAuthorityV1 {
                ANN_DELTA_AUTHORITY_V1_SIZE
            } else {
                ANN_DELTA_AUTHORITY_V2_SIZE
            },
            expires_at_micros,
            key,
        );
    }
    if opcode == Opcode::ConsolidateAnn {
        return if has_target
            && key.is_empty()
            && matches!(
                value_length,
                ANN_CONSOLIDATION_V1_SIZE | ANN_CONSOLIDATION_V2_SIZE
            )
            && expires_at_micros.is_none()
        {
            Ok(())
        } else {
            Err(WalSemanticError::InvalidBody)
        };
    }
    if let Some(expected_length) = targeted_ann_maintenance_length(opcode) {
        return validate_targeted_ann_maintenance_shape(
            has_target,
            value_length,
            expected_length,
            expires_at_micros,
            key,
        );
    }
    if opcode == Opcode::CreateCatalogObjectV2 {
        return validate_catalog_v2_shape(has_target, value_length, expires_at_micros, key);
    }
    if opcode == Opcode::CleanupStructureRetirementV3 {
        if has_target || key.is_empty() || value_length != 4 || expires_at_micros.is_some() {
            return Err(WalSemanticError::InvalidBody);
        }
        return Ok(());
    }
    if matches!(
        opcode,
        Opcode::CompactStructure
            | Opcode::MigrateStructureV3
            | Opcode::VacuumPageGeneration
            | Opcode::CompactSearch
            | Opcode::MigrateCatalogV7
    ) {
        return validate_empty_maintenance_shape(has_target, value_length, expires_at_micros, key);
    }
    validate_mutation_target_shape(opcode, has_target)?;
    match opcode {
        Opcode::RenameTable | Opcode::AppendStreamEntry
            if key.is_empty() || value_length == 0 || expires_at_micros.is_some() =>
        {
            return Err(WalSemanticError::InvalidBody);
        }
        Opcode::DropSecondaryIndex | Opcode::DropTable
            if value_length != 0 || !key.is_empty() || expires_at_micros.is_some() =>
        {
            return Err(WalSemanticError::InvalidBody);
        }
        Opcode::DeleteRow
        | Opcode::DeleteVector
        | Opcode::FenceVectorAbsence
        | Opcode::DeleteDocument
            if value_length != 0 =>
        {
            return Err(WalSemanticError::InvalidBody);
        }
        Opcode::DeleteValue
        | Opcode::CreateStream
        | Opcode::DeleteStream
        | Opcode::CreateHash
        | Opcode::DeleteHash
        | Opcode::DeleteHashField
        | Opcode::CreateSet
        | Opcode::AddSetMember
        | Opcode::DeleteSetMember
        | Opcode::DeleteSet
        | Opcode::CreateList
        | Opcode::DeleteList
        | Opcode::CreateSortedSet
        | Opcode::DeleteSortedSet
        | Opcode::DeleteSortedSetMember
            if value_length != 0 || expires_at_micros.is_some() =>
        {
            return Err(WalSemanticError::InvalidBody);
        }
        Opcode::ExpireValue if expires_at_micros.is_none() => {
            return Err(WalSemanticError::InvalidBody);
        }
        Opcode::ExpireHash
        | Opcode::ExpireHashField
        | Opcode::ExpireSet
        | Opcode::ExpireList
        | Opcode::ExpireStream
        | Opcode::ExpireSortedSet
            if value_length != 0 || expires_at_micros.is_none() =>
        {
            return Err(WalSemanticError::InvalidBody);
        }
        Opcode::AppendStreamEntry
        | Opcode::PushListHead
        | Opcode::PushListTail
        | Opcode::PopListHead
        | Opcode::PopListTail
        | Opcode::SetHashField
        | Opcode::UpsertSortedSetMember
        | Opcode::CreateTable
        | Opcode::InsertRow
        | Opcode::UpdateRow
        | Opcode::DeleteRow
        | Opcode::CreateSecondaryIndex
        | Opcode::IndexDocument
        | Opcode::ReplaceDocument
        | Opcode::DeleteDocument
        | Opcode::CreateAnnIndex
        | Opcode::UpsertVector
        | Opcode::DeleteVector
        | Opcode::FenceVectorAbsence
            if expires_at_micros.is_some() =>
        {
            return Err(WalSemanticError::InvalidBody);
        }
        _ => {}
    }
    validate_mutation_identity(opcode, value_length, key)
}

fn targeted_ann_maintenance_length(opcode: Opcode) -> Option<usize> {
    match opcode {
        Opcode::PublishInitialAnnBulk => Some(160),
        _ => None,
    }
}

fn validate_catalog_v2_shape(
    has_target: bool,
    value_length: usize,
    expires_at_micros: Option<i64>,
    key: &[u8],
) -> Result<(), WalSemanticError> {
    if has_target && !key.is_empty() && value_length != 0 && expires_at_micros.is_none() {
        Ok(())
    } else {
        Err(WalSemanticError::InvalidBody)
    }
}

fn validate_targeted_ann_maintenance_shape(
    has_target: bool,
    value_length: usize,
    expected_length: usize,
    expires_at_micros: Option<i64>,
    key: &[u8],
) -> Result<(), WalSemanticError> {
    if has_target
        && key.is_empty()
        && value_length == expected_length
        && expires_at_micros.is_none()
    {
        Ok(())
    } else {
        Err(WalSemanticError::InvalidBody)
    }
}

fn valid_ann_consolidation(value: &[u8], target: Option<ObjectId>) -> bool {
    if value.len() == ANN_CONSOLIDATION_V1_SIZE && value.get(..8) == Some(b"HYANNC01") {
        return value[8..40].iter().any(|byte| *byte != 0)
            && value[40..72].iter().any(|byte| *byte != 0)
            && value[72..104].iter().any(|byte| *byte != 0)
            && matches!(read_u64(&value[104..112]), 1..=4_096);
    }
    if value.len() != ANN_CONSOLIDATION_V2_SIZE
        || value.get(..8) != Some(b"HYANNC02")
        || value[26..32].iter().any(|byte| *byte != 0)
        || value[376..384].iter().any(|byte| *byte != 0)
        || !matches!(value[24], 4 | 5)
        || value[25] != 5
        || value[32..320]
            .chunks_exact(32)
            .any(|field| field.iter().all(|byte| *byte == 0))
    {
        return false;
    }
    let Ok(index_bytes) = <[u8; 16]>::try_from(&value[8..24]) else {
        return false;
    };
    let Ok(index) = ObjectId::new(u128::from_be_bytes(index_bytes)) else {
        return false;
    };
    let captured_next_sequence = read_u64(&value[320..328]);
    let captured_count = read_u64(&value[328..336]);
    let prior_next_sequence = read_u64(&value[336..344]);
    let result_next_sequence = read_u64(&value[344..352]);
    let consumed_count = read_u64(&value[352..360]);
    let preserved_count = read_u64(&value[360..368]);
    let effective_count = read_u64(&value[368..376]);
    target == Some(index)
        && (1..=4_096).contains(&captured_count)
        && consumed_count <= captured_count
        && preserved_count <= 4_096
        && effective_count <= 1_000_000
        && captured_next_sequence != 0
        && prior_next_sequence >= captured_next_sequence
        && result_next_sequence == prior_next_sequence
}

fn valid_initial_ann_bulk_publication(value: &[u8]) -> bool {
    if value.len() != 160 || value.get(..8) != Some(b"HYANNP01") {
        return false;
    }
    let identities_are_nonzero = value[8..136]
        .chunks_exact(32)
        .all(|identity| identity.iter().any(|byte| *byte != 0));
    let partition_count = read_u64(&value[136..144]);
    let vector_count = read_u64(&value[144..152]);
    identities_are_nonzero
        && partition_count != 0
        && partition_count <= vector_count
        && Csn::new(read_u64(&value[152..160])).is_ok()
}

fn valid_ann_delta_authority_v1(value: &[u8]) -> bool {
    value.len() == ANN_DELTA_AUTHORITY_V1_SIZE
        && value.get(..8) == Some(ANN_DELTA_AUTHORITY_MAGIC_V1.as_slice())
        && value[8..].iter().all(|byte| *byte == 0)
}

fn decode_ann_delta_authority_v2(
    value: &[u8],
    target: Option<ObjectId>,
) -> Result<AnnDeltaAuthorityV2, WalSemanticError> {
    if value.len() != ANN_DELTA_AUTHORITY_V2_SIZE
        || value.get(..8) != Some(ANN_DELTA_AUTHORITY_MAGIC_V2.as_slice())
        || value[8] & !ANN_DELTA_AUTHORITY_V2_M04_TO_M05 != 0
        || value[9..16].iter().any(|byte| *byte != 0)
        || value[36..40].iter().any(|byte| *byte != 0)
    {
        return Err(WalSemanticError::InvalidBody);
    }
    let index = ObjectId::new(u128::from_be_bytes(
        value[16..32]
            .try_into()
            .map_err(|_| WalSemanticError::InvalidBody)?,
    ))
    .map_err(|_| WalSemanticError::InvalidIdentity)?;
    let operation_count = read_u32(&value[32..36]);
    let prior_view_identity = value[40..72]
        .try_into()
        .map_err(|_| WalSemanticError::InvalidBody)?;
    let result_view_identity = value[72..104]
        .try_into()
        .map_err(|_| WalSemanticError::InvalidBody)?;
    let prior_overlay_root = value[104..136]
        .try_into()
        .map_err(|_| WalSemanticError::InvalidBody)?;
    let result_overlay_root = value[136..168]
        .try_into()
        .map_err(|_| WalSemanticError::InvalidBody)?;
    let prior_next_sequence = read_u64(&value[168..176]);
    let result_next_sequence = read_u64(&value[176..184]);
    if target != Some(index)
        || !(1..=ANN_DELTA_AUTHORITY_V2_MAX_OPERATIONS).contains(&operation_count)
        || [
            prior_view_identity,
            result_view_identity,
            prior_overlay_root,
            result_overlay_root,
        ]
        .contains(&[0; 32])
        || prior_next_sequence == 0
        || prior_next_sequence.checked_add(u64::from(operation_count)) != Some(result_next_sequence)
        || prior_view_identity == result_view_identity
        || prior_overlay_root == result_overlay_root
    {
        return Err(WalSemanticError::InvalidBody);
    }
    Ok(AnnDeltaAuthorityV2 {
        index,
        operation_count,
        prior_view_identity,
        result_view_identity,
        prior_overlay_root,
        result_overlay_root,
        prior_next_sequence,
        result_next_sequence,
        m04_to_m05: value[8] == ANN_DELTA_AUTHORITY_V2_M04_TO_M05,
    })
}

#[allow(clippy::too_many_lines)]
fn validate_ann_delta_authority_markers<'a>(
    mutations: impl IntoIterator<Item = &'a Mutation>,
) -> Result<(), WalSemanticError> {
    let mut upsert_counts = std::collections::BTreeMap::<ObjectId, u32>::new();
    let mut vector_mutation_counts = std::collections::BTreeMap::<ObjectId, u32>::new();
    let mut absence_fence_indexes = BTreeSet::new();
    let mut absence_fence_count = 0_u32;
    let mut created_indexes = BTreeSet::new();
    let mut legacy_mutation_indexes = BTreeSet::new();
    let mut authority_v1_indexes = BTreeSet::new();
    let mut authority_v2_indexes = BTreeSet::new();
    let mut markers_started = false;
    let mut previous_marker = None;
    let mut marker_version = None;
    for mutation in mutations {
        if matches!(
            mutation.opcode,
            Opcode::AnnDeltaAuthorityV1 | Opcode::AnnDeltaAuthorityV2
        ) {
            markers_started = true;
            let index = mutation.target.ok_or(WalSemanticError::InvalidBody)?;
            let version = if mutation.opcode == Opcode::AnnDeltaAuthorityV1 {
                1
            } else {
                2
            };
            if marker_version
                .replace(version)
                .is_some_and(|prior| prior != version)
                || mutation.engine != EngineKind::Search
                || !mutation.key.is_empty()
                || mutation.expires_at_micros.is_some()
                || previous_marker.is_some_and(|previous| previous >= index)
            {
                return Err(WalSemanticError::InvalidBody);
            }
            if version == 1 {
                if !valid_ann_delta_authority_v1(&mutation.value)
                    || !authority_v1_indexes.insert(index)
                {
                    return Err(WalSemanticError::InvalidBody);
                }
            } else {
                let authority = ann_delta_authority_v2(mutation)?;
                if authority.operation_count
                    != vector_mutation_counts.get(&index).copied().unwrap_or(0)
                    || created_indexes.contains(&index)
                    || legacy_mutation_indexes.contains(&index)
                    || !authority_v2_indexes.insert(index)
                {
                    return Err(WalSemanticError::InvalidSequence);
                }
            }
            previous_marker = Some(index);
        } else {
            if markers_started {
                return Err(WalSemanticError::InvalidSequence);
            }
            if matches!(mutation.opcode, Opcode::UpsertVector | Opcode::DeleteVector) {
                let count = vector_mutation_counts
                    .entry(mutation.target.ok_or(WalSemanticError::InvalidBody)?)
                    .or_insert(0);
                *count = count
                    .checked_add(1)
                    .ok_or(WalSemanticError::LengthOverflow)?;
                if mutation.opcode == Opcode::UpsertVector {
                    let count = upsert_counts
                        .entry(mutation.target.ok_or(WalSemanticError::InvalidBody)?)
                        .or_insert(0);
                    *count = count
                        .checked_add(1)
                        .ok_or(WalSemanticError::LengthOverflow)?;
                }
            } else if mutation.opcode == Opcode::FenceVectorAbsence {
                absence_fence_count = absence_fence_count
                    .checked_add(1)
                    .ok_or(WalSemanticError::LengthOverflow)?;
                if absence_fence_count > ANN_DELTA_AUTHORITY_V2_MAX_OPERATIONS {
                    return Err(WalSemanticError::InvalidSequence);
                }
                absence_fence_indexes.insert(mutation.target.ok_or(WalSemanticError::InvalidBody)?);
            } else if mutation.opcode == Opcode::CreateAnnIndex {
                created_indexes.insert(mutation.target.ok_or(WalSemanticError::InvalidBody)?);
            } else if matches!(
                mutation.opcode,
                Opcode::ConsolidateAnn | Opcode::PublishInitialAnnBulk
            ) {
                legacy_mutation_indexes
                    .insert(mutation.target.ok_or(WalSemanticError::InvalidBody)?);
            }
        }
    }
    if marker_version == Some(1) {
        let vector_indexes = upsert_counts.keys().copied().collect::<BTreeSet<_>>();
        if vector_indexes != authority_v1_indexes || !absence_fence_indexes.is_empty() {
            return Err(WalSemanticError::InvalidSequence);
        }
    } else if marker_version == Some(2) {
        let expected = vector_mutation_counts
            .keys()
            .filter(|index| {
                !created_indexes.contains(index) && !legacy_mutation_indexes.contains(index)
            })
            .copied()
            .collect::<BTreeSet<_>>();
        if expected != authority_v2_indexes {
            return Err(WalSemanticError::InvalidSequence);
        }
    }
    Ok(())
}

fn validate_mutation_target_shape(
    opcode: Opcode,
    has_target: bool,
) -> Result<(), WalSemanticError> {
    let forbids_target = matches!(
        opcode,
        Opcode::SetValue
            | Opcode::CreateStream
            | Opcode::AppendStreamEntry
            | Opcode::DeleteStream
            | Opcode::ExpireStream
            | Opcode::DeleteValue
            | Opcode::ExpireValue
            | Opcode::CreateHash
            | Opcode::DeleteHash
            | Opcode::ExpireHash
            | Opcode::ExpireHashField
            | Opcode::SetHashField
            | Opcode::DeleteHashField
            | Opcode::CreateSet
            | Opcode::AddSetMember
            | Opcode::DeleteSetMember
            | Opcode::ExpireSet
            | Opcode::DeleteSet
            | Opcode::CreateList
            | Opcode::DeleteList
            | Opcode::ExpireList
            | Opcode::PushListHead
            | Opcode::PushListTail
            | Opcode::PopListHead
            | Opcode::PopListTail
            | Opcode::CreateSortedSet
            | Opcode::DeleteSortedSet
            | Opcode::ExpireSortedSet
            | Opcode::UpsertSortedSetMember
            | Opcode::DeleteSortedSetMember
    );
    let requires_target = matches!(
        opcode,
        Opcode::CreateTable
            | Opcode::InsertRow
            | Opcode::CreateSecondaryIndex
            | Opcode::DropSecondaryIndex
            | Opcode::RenameTable
            | Opcode::DropTable
            | Opcode::CreateIndex
            | Opcode::IndexDocument
            | Opcode::ReplaceDocument
            | Opcode::DeleteDocument
            | Opcode::CreateAnnIndex
            | Opcode::UpsertVector
            | Opcode::DeleteVector
            | Opcode::FenceVectorAbsence
            | Opcode::ConsolidateAnn
            | Opcode::PublishInitialAnnBulk
            | Opcode::AnnDeltaAuthorityV1
            | Opcode::AnnDeltaAuthorityV2
            | Opcode::CreateCatalogObjectV2
            | Opcode::UpdateRow
            | Opcode::DeleteRow
    );
    if (forbids_target && has_target) || (requires_target && !has_target) {
        Err(WalSemanticError::InvalidBody)
    } else {
        Ok(())
    }
}

fn validate_mutation_identity(
    opcode: Opcode,
    value_length: usize,
    key: &[u8],
) -> Result<(), WalSemanticError> {
    if matches!(
        opcode,
        Opcode::SetHashField
            | Opcode::DeleteHashField
            | Opcode::ExpireHashField
            | Opcode::AddSetMember
            | Opcode::DeleteSetMember
            | Opcode::UpsertSortedSetMember
            | Opcode::DeleteSortedSetMember
    ) && !valid_collection_member_identity(key)
    {
        return Err(WalSemanticError::InvalidBody);
    }
    if matches!(
        opcode,
        Opcode::UpsertVector | Opcode::DeleteVector | Opcode::FenceVectorAbsence
    ) && key.len() != 16
    {
        return Err(WalSemanticError::InvalidBody);
    }
    if opcode == Opcode::UpsertVector && (value_length == 0 || !value_length.is_multiple_of(4)) {
        return Err(WalSemanticError::InvalidBody);
    }
    if opcode == Opcode::UpsertSortedSetMember && value_length != 8 {
        return Err(WalSemanticError::InvalidBody);
    }
    if opcode == Opcode::CreateAnnIndex && value_length < 20 {
        return Err(WalSemanticError::InvalidBody);
    }
    Ok(())
}

fn validate_empty_maintenance_shape(
    has_target: bool,
    value_length: usize,
    expires_at_micros: Option<i64>,
    key: &[u8],
) -> Result<(), WalSemanticError> {
    if has_target || !key.is_empty() || value_length != 0 || expires_at_micros.is_some() {
        Err(WalSemanticError::InvalidBody)
    } else {
        Ok(())
    }
}

fn valid_collection_member_identity(encoded: &[u8]) -> bool {
    let Some(length_bytes) = encoded.get(..4) else {
        return false;
    };
    let mut length = [0_u8; 4];
    length.copy_from_slice(length_bytes);
    let Ok(collection_key_length) = usize::try_from(u32::from_be_bytes(length)) else {
        return false;
    };
    4_usize
        .checked_add(collection_key_length)
        .is_some_and(|member_start| member_start <= encoded.len())
}

fn mutation_digest<'body>(
    mutations: impl Iterator<Item = (EngineKind, &'body [u8])>,
) -> Result<[u8; 32], WalSemanticError> {
    let mut hasher = mutation_digest_hasher();
    for (engine, body) in mutations {
        hasher.update(&[engine as u8]);
        let length = u32::try_from(body.len()).map_err(|_| WalSemanticError::LengthOverflow)?;
        hasher.update(&length.to_le_bytes());
        hasher.update(body);
    }
    Ok(*hasher.finalize().as_bytes())
}

fn mutation_digest_hasher() -> blake3::Hasher {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-native-mutation-set-v1");
    hasher
}

fn put_len(bytes: &mut Vec<u8>, value: usize) -> Result<(), WalSemanticError> {
    let value = u32::try_from(value).map_err(|_| WalSemanticError::LengthOverflow)?;
    bytes.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

fn optional_csn(value: u64) -> Result<Option<Csn>, WalSemanticError> {
    if value == 0 {
        Ok(None)
    } else {
        Csn::new(value)
            .map(Some)
            .map_err(|_| WalSemanticError::InvalidIdentity)
    }
}

fn optional_lsn(value: u64) -> Result<Option<Lsn>, WalSemanticError> {
    if value == 0 {
        Ok(None)
    } else {
        Lsn::new(value)
            .map(Some)
            .map_err(|_| WalSemanticError::InvalidIdentity)
    }
}

fn read_u32(bytes: &[u8]) -> u32 {
    let mut value = [0_u8; 4];
    value.copy_from_slice(bytes);
    u32::from_le_bytes(value)
}

fn read_u64(bytes: &[u8]) -> u64 {
    let mut value = [0_u8; 8];
    value.copy_from_slice(bytes);
    u64::from_le_bytes(value)
}

fn read_u128(bytes: &[u8]) -> u128 {
    let mut value = [0_u8; 16];
    value.copy_from_slice(bytes);
    u128::from_le_bytes(value)
}

fn read_i64(bytes: &[u8]) -> i64 {
    let mut value = [0_u8; 8];
    value.copy_from_slice(bytes);
    i64::from_le_bytes(value)
}

#[cfg(test)]
mod tests {
    use hyphae_native_types::{
        CatalogVersion, Csn, DurabilityClass, EngineKind, ManifestGeneration, ObjectId,
        PageGeneration, PageId, TransactionId,
    };
    use hyphae_native_wal::{PendingRecord, RecordKind, WAL_RECORD_BODY_SIZE, WalBlock};

    use super::{
        ANN_CONSOLIDATION_V1_SIZE, ANN_CONSOLIDATION_V2_SIZE, ANN_DELTA_AUTHORITY_V2_SIZE,
        AnnDeltaAuthorityV2, CommitManifest, Mutation, Opcode, TransactionPlan,
        TransactionWalPreflightBuilder, WalSemanticError, ann_delta_authority_marker_v1,
        ann_delta_authority_marker_v2, ann_delta_authority_v2, decode_begin, decode_mutation,
        decode_opcode, encode_begin, encode_checkpoint, encode_transaction, recover_wal,
        validate_mutation_shape,
    };

    fn mutation(
        engine: EngineKind,
        opcode: Opcode,
        target: Option<ObjectId>,
        key: &[u8],
        value: &[u8],
        expires_at_micros: Option<i64>,
    ) -> Mutation {
        Mutation {
            engine,
            opcode,
            target,
            key: key.to_vec(),
            value: value.to_vec(),
            expires_at_micros,
        }
    }

    fn structure_mutation(
        opcode: Opcode,
        key: &[u8],
        value: &[u8],
        expires_at_micros: Option<i64>,
    ) -> Mutation {
        mutation(
            EngineKind::Structure,
            opcode,
            None,
            key,
            value,
            expires_at_micros,
        )
    }

    fn complete_transaction_mutations()
    -> Result<Vec<Mutation>, hyphae_native_types::NativeTypeError> {
        let mut hash_field = 4_u32.to_be_bytes().to_vec();
        hash_field.extend_from_slice(b"hashfield");
        let mut set_member = 3_u32.to_be_bytes().to_vec();
        set_member.extend_from_slice(b"setmember");
        let mut sorted_set_member = 6_u32.to_be_bytes().to_vec();
        sorted_set_member.extend_from_slice(b"sortedmember");
        Ok(vec![
            mutation(
                EngineKind::Relational,
                Opcode::InsertRow,
                Some(ObjectId::new(1)?),
                b"pk",
                b"row",
                None,
            ),
            structure_mutation(Opcode::SetValue, b"key", b"value", Some(50)),
            structure_mutation(Opcode::ExpireValue, b"key", b"value", Some(i64::MAX)),
            structure_mutation(Opcode::DeleteValue, b"old-key", b"", None),
            structure_mutation(Opcode::CreateHash, b"hash", b"", None),
            structure_mutation(Opcode::ExpireHash, b"hash", b"", Some(i64::MIN)),
            structure_mutation(Opcode::DeleteHash, b"retired-hash", b"", None),
            structure_mutation(Opcode::SetHashField, &hash_field, b"value", None),
            structure_mutation(Opcode::ExpireHashField, &hash_field, b"", Some(i64::MAX)),
            structure_mutation(Opcode::DeleteHashField, &hash_field, b"", None),
            structure_mutation(Opcode::CreateSet, b"set", b"", None),
            structure_mutation(Opcode::AddSetMember, &set_member, b"", None),
            structure_mutation(Opcode::DeleteSetMember, &set_member, b"", None),
            structure_mutation(Opcode::ExpireSet, b"set", b"", Some(42)),
            structure_mutation(Opcode::DeleteSet, b"retired-set", b"", None),
            structure_mutation(Opcode::CreateList, b"list", b"", None),
            structure_mutation(Opcode::ExpireList, b"list", b"", Some(43)),
            structure_mutation(Opcode::DeleteList, b"retired-list", b"", None),
            structure_mutation(Opcode::PushListHead, b"list", b"head", None),
            structure_mutation(Opcode::PushListTail, b"list", b"tail", None),
            structure_mutation(Opcode::PopListHead, b"list", b"head", None),
            structure_mutation(Opcode::PopListTail, b"list", b"tail", None),
            structure_mutation(Opcode::CreateSortedSet, b"sorted", b"", None),
            structure_mutation(
                Opcode::UpsertSortedSetMember,
                &sorted_set_member,
                &20.0_f64.to_bits().to_be_bytes(),
                None,
            ),
            structure_mutation(Opcode::DeleteSortedSetMember, &sorted_set_member, b"", None),
            structure_mutation(Opcode::CompactStructure, b"", b"", None),
            mutation(
                EngineKind::Search,
                Opcode::IndexDocument,
                Some(ObjectId::new(2)?),
                b"doc",
                b"native search",
                None,
            ),
            mutation(
                EngineKind::Search,
                Opcode::ReplaceDocument,
                Some(ObjectId::new(2)?),
                b"doc",
                b"replacement",
                None,
            ),
            mutation(
                EngineKind::Search,
                Opcode::DeleteDocument,
                Some(ObjectId::new(2)?),
                b"retired-doc",
                b"",
                None,
            ),
            mutation(
                EngineKind::Search,
                Opcode::CreateAnnIndex,
                Some(ObjectId::new(3)?),
                b"vectors",
                b"HYANNIDX00000000000000",
                None,
            ),
            mutation(
                EngineKind::Search,
                Opcode::UpsertVector,
                Some(ObjectId::new(3)?),
                &ObjectId::new(4)?.get().to_be_bytes(),
                &[0, 0, 128, 63, 0, 0, 0, 0],
                None,
            ),
            mutation(
                EngineKind::Search,
                Opcode::DeleteVector,
                Some(ObjectId::new(3)?),
                &ObjectId::new(5)?.get().to_be_bytes(),
                b"",
                None,
            ),
        ])
    }

    fn commit_manifest(
        page_generation: PageGeneration,
        retention_floor_csn: Csn,
    ) -> Result<CommitManifest, hyphae_native_types::NativeTypeError> {
        Ok(CommitManifest {
            read_csn: Some(Csn::new(7)?),
            commit_csn: Csn::new(9)?,
            catalog_version: CatalogVersion::new(3)?,
            blob_generation: 4,
            mutation_count: 2,
            mutation_bytes: 17,
            logical_time_micros: 123,
            mutation_digest: [0xa5; 32],
            roots: [
                PageId::new(11)?,
                PageId::new(12)?,
                PageId::new(13)?,
                PageId::new(14)?,
            ],
            page_generation,
            retention_floor_csn,
        })
    }

    fn test_roots() -> Result<[PageId; super::ROOT_COUNT], hyphae_native_types::NativeTypeError> {
        Ok([
            PageId::new(1)?,
            PageId::new(2)?,
            PageId::new(3)?,
            PageId::new(4)?,
        ])
    }

    fn encode_test_transaction(
        mutations: &[Mutation],
    ) -> Result<Vec<hyphae_native_wal::PendingRecord>, Box<dyn std::error::Error>> {
        Ok(encode_transaction(&TransactionPlan {
            transaction_id: TransactionId::new(1)?,
            read_csn: None,
            catalog_version: CatalogVersion::new(1)?,
            logical_time_micros: 10,
            durability: DurabilityClass::Strict,
            mutations,
            commit_csn: Csn::FIRST,
            roots: test_roots()?,
            blob_generation: 0,
            page_generation: PageGeneration::FIRST,
            retention_floor_csn: Csn::FIRST,
        })?)
    }

    #[test]
    fn hostile_begin_counts_and_body_totals_fail_before_transaction_allocation()
    -> Result<(), Box<dyn std::error::Error>> {
        let valid = encode_begin(
            None,
            CatalogVersion::new(1)?,
            1,
            DurabilityClass::Strict,
            1,
            u64::try_from(super::MUTATION_BODY_HEADER_SIZE)?,
        );
        assert!(decode_begin(&valid).is_ok());

        for (count, bytes) in [
            (0, 0),
            (u32::MAX, u64::MAX),
            (1, u64::try_from(super::MUTATION_BODY_HEADER_SIZE - 1)?),
            (1, super::MAX_TRANSACTION_MUTATION_BYTES + 1),
        ] {
            let mut hostile = valid.clone();
            hostile[40..44].copy_from_slice(&count.to_le_bytes());
            hostile[44..52].copy_from_slice(&bytes.to_le_bytes());
            assert!(matches!(
                decode_begin(&hostile),
                Err(WalSemanticError::InvalidSequence)
            ));
        }
        let decoded_slot = u64::try_from(std::mem::size_of::<Mutation>())?;
        let decoded_overflow_count = u32::try_from(
            super::MAX_TRANSACTION_MUTATION_BYTES
                .checked_div(u64::try_from(super::MUTATION_BODY_HEADER_SIZE)? + decoded_slot)
                .ok_or("zero decoded mutation divisor")?
                + 1,
        )?;
        let mut decoded_overflow = valid;
        decoded_overflow[40..44].copy_from_slice(&decoded_overflow_count.to_le_bytes());
        decoded_overflow[44..52].copy_from_slice(
            &(u64::from(decoded_overflow_count) * u64::try_from(super::MUTATION_BODY_HEADER_SIZE)?)
                .to_le_bytes(),
        );
        assert!(matches!(
            decode_begin(&decoded_overflow),
            Err(WalSemanticError::InvalidSequence)
        ));
        Ok(())
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn recovery_rejects_zero_mutations_and_declared_prefix_overruns_incrementally()
    -> Result<(), Box<dyn std::error::Error>> {
        let transaction_id = TransactionId::new(1)?;
        let zero = PendingRecord::new(
            RecordKind::Begin,
            EngineKind::Kernel,
            0,
            transaction_id,
            encode_begin(
                None,
                CatalogVersion::new(1)?,
                1,
                DurabilityClass::Strict,
                0,
                0,
            ),
        )?;
        let block = WalBlock::build(1, [0; 32], vec![zero])?;
        let decoded = WalBlock::decode(1, [0; 32], &block.encode()?)?;
        assert!(matches!(
            recover_wal(decoded.records()),
            Err(WalSemanticError::InvalidSequence)
        ));
        assert!(matches!(
            encode_test_transaction(&[]),
            Err(error) if matches!(error.downcast_ref::<WalSemanticError>(),
                Some(WalSemanticError::InvalidSequence))
        ));

        let empty_commit = CommitManifest {
            read_csn: None,
            commit_csn: Csn::FIRST,
            catalog_version: CatalogVersion::new(1)?,
            blob_generation: 0,
            mutation_count: 1,
            mutation_bytes: u64::try_from(super::MUTATION_BODY_HEADER_SIZE)?,
            logical_time_micros: 1,
            mutation_digest: [1; 32],
            roots: test_roots()?,
            page_generation: PageGeneration::FIRST,
            retention_floor_csn: Csn::FIRST,
        };
        let empty = vec![
            PendingRecord::new(
                RecordKind::Begin,
                EngineKind::Kernel,
                0,
                transaction_id,
                encode_begin(
                    None,
                    CatalogVersion::new(1)?,
                    1,
                    DurabilityClass::Strict,
                    1,
                    u64::try_from(super::MUTATION_BODY_HEADER_SIZE)?,
                ),
            )?,
            PendingRecord::new(
                RecordKind::Commit,
                EngineKind::Kernel,
                0,
                transaction_id,
                empty_commit.encode(),
            )?,
        ];
        let block = WalBlock::build(1, [0; 32], empty)?;
        let decoded = WalBlock::decode(1, [0; 32], &block.encode()?)?;
        assert!(matches!(
            recover_wal(decoded.records()),
            Err(WalSemanticError::InvalidSequence)
        ));

        let mutation = structure_mutation(Opcode::SetValue, b"k", b"v", None);
        let body = mutation.encode()?;
        let body_bytes = u64::try_from(body.len())?;
        let catalog_version = CatalogVersion::new(1)?;
        let begin = |count, bytes| {
            PendingRecord::new(
                RecordKind::Begin,
                EngineKind::Kernel,
                0,
                transaction_id,
                encode_begin(
                    None,
                    catalog_version,
                    1,
                    DurabilityClass::Strict,
                    count,
                    bytes,
                ),
            )
        };
        let record = || {
            PendingRecord::new(
                RecordKind::Mutation,
                EngineKind::Structure,
                0,
                transaction_id,
                body.clone(),
            )
        };
        for pending in [
            vec![
                begin(1, body_bytes.saturating_mul(2))?,
                record()?,
                record()?,
            ],
            vec![
                begin(1, u64::try_from(super::MUTATION_BODY_HEADER_SIZE)?)?,
                record()?,
            ],
        ] {
            let block = WalBlock::build(1, [0; 32], pending)?;
            let decoded = WalBlock::decode(1, [0; 32], &block.encode()?)?;
            assert!(matches!(
                recover_wal(decoded.records()),
                Err(WalSemanticError::ContentMismatch)
            ));
        }
        Ok(())
    }

    #[test]
    fn ann_delta_authority_marker_is_append_only_fixed_and_bounded()
    -> Result<(), Box<dyn std::error::Error>> {
        let index = ObjectId::new(7)?;
        let marker = ann_delta_authority_marker_v1(index);
        assert_eq!(Opcode::AnnDeltaAuthorityV1 as u8, 55);
        assert_eq!(
            decode_opcode(55)?,
            (Opcode::AnnDeltaAuthorityV1, EngineKind::Search)
        );
        assert_eq!(marker.value.len(), super::ANN_DELTA_AUTHORITY_V1_SIZE);
        let encoded = marker.encode()?;
        assert_eq!(decode_mutation(EngineKind::Search, &encoded)?, marker);

        for invalid_value in [
            marker.value[..marker.value.len() - 1].to_vec(),
            [marker.value.as_slice(), &[0]].concat(),
            {
                let mut reserved = marker.value.clone();
                reserved[8] = 1;
                reserved
            },
        ] {
            let mut invalid = marker.clone();
            invalid.value = invalid_value;
            assert!(matches!(
                decode_mutation(EngineKind::Search, &invalid.encode()?),
                Err(WalSemanticError::InvalidBody)
            ));
        }
        for invalid in [
            validate_mutation_shape(
                Opcode::AnnDeltaAuthorityV1,
                false,
                super::ANN_DELTA_AUTHORITY_V1_SIZE,
                None,
                b"",
            ),
            validate_mutation_shape(
                Opcode::AnnDeltaAuthorityV1,
                true,
                super::ANN_DELTA_AUTHORITY_V1_SIZE + 1,
                None,
                b"",
            ),
            validate_mutation_shape(
                Opcode::AnnDeltaAuthorityV1,
                true,
                super::ANN_DELTA_AUTHORITY_V1_SIZE,
                Some(1),
                b"",
            ),
            validate_mutation_shape(
                Opcode::AnnDeltaAuthorityV1,
                true,
                super::ANN_DELTA_AUTHORITY_V1_SIZE,
                None,
                b"key",
            ),
        ] {
            assert!(matches!(invalid, Err(WalSemanticError::InvalidBody)));
        }
        Ok(())
    }

    #[test]
    fn ann_delta_authority_markers_are_canonical_and_cover_exactly_the_vector_indexes()
    -> Result<(), Box<dyn std::error::Error>> {
        let first = ObjectId::new(7)?;
        let second = ObjectId::new(8)?;
        let upsert = |index, object: u128| {
            mutation(
                EngineKind::Search,
                Opcode::UpsertVector,
                Some(index),
                &object.to_be_bytes(),
                &[0, 0, 128, 63],
                None,
            )
        };
        let valid = vec![
            upsert(first, 1),
            upsert(second, 2),
            ann_delta_authority_marker_v1(first),
            ann_delta_authority_marker_v1(second),
        ];
        let block = WalBlock::build(1, [0; 32], encode_test_transaction(&valid)?)?;
        let decoded = WalBlock::decode(1, [0; 32], &block.encode()?)?;
        assert_eq!(recover_wal(decoded.records())?.commits[0].mutations, valid);

        for invalid in [
            vec![ann_delta_authority_marker_v1(first)],
            vec![ann_delta_authority_marker_v1(first), upsert(first, 1)],
            vec![
                upsert(first, 1),
                upsert(second, 2),
                ann_delta_authority_marker_v1(first),
            ],
            vec![
                upsert(first, 1),
                ann_delta_authority_marker_v1(first),
                ann_delta_authority_marker_v1(first),
            ],
            vec![
                upsert(first, 1),
                upsert(second, 2),
                ann_delta_authority_marker_v1(second),
                ann_delta_authority_marker_v1(first),
            ],
        ] {
            assert!(encode_test_transaction(&invalid).is_err());
        }
        Ok(())
    }

    fn authority_v2(index: ObjectId, operation_count: u32) -> AnnDeltaAuthorityV2 {
        AnnDeltaAuthorityV2 {
            index,
            operation_count,
            prior_view_identity: [1; 32],
            result_view_identity: [2; 32],
            prior_overlay_root: [3; 32],
            result_overlay_root: [4; 32],
            prior_next_sequence: 9,
            result_next_sequence: 9 + u64::from(operation_count),
            m04_to_m05: true,
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn ann_delta_authority_v2_has_fixed_golden_bytes_and_strict_fields()
    -> Result<(), Box<dyn std::error::Error>> {
        let index = ObjectId::new(7)?;
        let authority = authority_v2(index, 2);
        let marker = ann_delta_authority_marker_v2(authority);
        assert_eq!(Opcode::AnnDeltaAuthorityV2 as u8, 56);
        assert_eq!(
            decode_opcode(56)?,
            (Opcode::AnnDeltaAuthorityV2, EngineKind::Search)
        );
        assert_eq!(marker.value.len(), super::ANN_DELTA_AUTHORITY_V2_SIZE);
        assert_eq!(&marker.value[..8], b"HYANNA02");
        assert_eq!(marker.value[8], 1);
        assert_eq!(&marker.value[9..16], &[0; 7]);
        assert_eq!(&marker.value[16..32], &7_u128.to_be_bytes());
        assert_eq!(&marker.value[32..36], &2_u32.to_le_bytes());
        assert_eq!(&marker.value[36..40], &[0; 4]);
        assert_eq!(&marker.value[40..72], &[1; 32]);
        assert_eq!(&marker.value[72..104], &[2; 32]);
        assert_eq!(&marker.value[104..136], &[3; 32]);
        assert_eq!(&marker.value[136..168], &[4; 32]);
        assert_eq!(&marker.value[168..176], &9_u64.to_le_bytes());
        assert_eq!(&marker.value[176..184], &11_u64.to_le_bytes());
        assert_eq!(
            marker.value,
            [
                b"HYANNA02".as_slice(),
                &[1, 0, 0, 0, 0, 0, 0, 0],
                &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7],
                &[2, 0, 0, 0, 0, 0, 0, 0],
                &[1; 32],
                &[2; 32],
                &[3; 32],
                &[4; 32],
                &[9, 0, 0, 0, 0, 0, 0, 0],
                &[11, 0, 0, 0, 0, 0, 0, 0],
            ]
            .concat()
        );
        let encoded = marker.encode()?;
        assert_eq!(encoded.len(), 44 + super::ANN_DELTA_AUTHORITY_V2_SIZE);
        assert_eq!(encoded[8], 56);
        assert_eq!(decode_mutation(EngineKind::Search, &encoded)?, marker);
        assert_eq!(ann_delta_authority_v2(&marker)?, authority);

        for invalid_value in [
            marker.value[..183].to_vec(),
            [marker.value.as_slice(), &[0]].concat(),
            {
                let mut value = marker.value.clone();
                value[8] = 2;
                value
            },
            {
                let mut value = marker.value.clone();
                value[9] = 1;
                value
            },
            {
                let mut value = marker.value.clone();
                value[36] = 1;
                value
            },
            {
                let mut value = marker.value.clone();
                value[16..32].fill(0);
                value
            },
            {
                let mut value = marker.value.clone();
                value[32..36].fill(0);
                value
            },
            {
                let mut value = marker.value.clone();
                value[32..36].copy_from_slice(&4_097_u32.to_le_bytes());
                value
            },
            {
                let mut value = marker.value.clone();
                value[40..72].fill(0);
                value
            },
            {
                let mut value = marker.value.clone();
                value[136..168].copy_from_slice(&[3; 32]);
                value
            },
            {
                let mut value = marker.value.clone();
                value[176..184].copy_from_slice(&12_u64.to_le_bytes());
                value
            },
        ] {
            let mut invalid = marker.clone();
            invalid.value = invalid_value;
            assert!(matches!(
                decode_mutation(EngineKind::Search, &invalid.encode()?),
                Err(WalSemanticError::InvalidBody | WalSemanticError::InvalidIdentity)
            ));
        }
        for range in [40..72, 72..104, 104..136, 136..168] {
            let mut invalid = marker.clone();
            invalid.value[range].fill(0);
            assert!(decode_mutation(EngineKind::Search, &invalid.encode()?).is_err());
        }
        for range in [168..176, 176..184] {
            let mut invalid = marker.clone();
            invalid.value[range].fill(0);
            assert!(decode_mutation(EngineKind::Search, &invalid.encode()?).is_err());
        }
        let mut wrong_target = marker.clone();
        wrong_target.target = Some(ObjectId::new(8)?);
        assert!(decode_mutation(EngineKind::Search, &wrong_target.encode()?).is_err());
        Ok(())
    }

    #[test]
    fn ann_delta_authority_v2_is_trailing_sorted_unique_exact_and_never_mixes_v1()
    -> Result<(), Box<dyn std::error::Error>> {
        let first = ObjectId::new(7)?;
        let second = ObjectId::new(8)?;
        let upsert = |index, object: u128| {
            mutation(
                EngineKind::Search,
                Opcode::UpsertVector,
                Some(index),
                &object.to_be_bytes(),
                &[0, 0, 128, 63],
                None,
            )
        };
        let valid = vec![
            upsert(first, 1),
            upsert(first, 2),
            upsert(second, 3),
            ann_delta_authority_marker_v2(authority_v2(first, 2)),
            ann_delta_authority_marker_v2(authority_v2(second, 1)),
        ];
        encode_test_transaction(&valid)?;
        for invalid in [
            vec![
                upsert(first, 1),
                ann_delta_authority_marker_v2(authority_v2(first, 1)),
                upsert(second, 2),
            ],
            vec![
                upsert(first, 1),
                ann_delta_authority_marker_v2(authority_v2(first, 2)),
            ],
            vec![
                upsert(first, 1),
                ann_delta_authority_marker_v2(authority_v2(first, 1)),
                ann_delta_authority_marker_v2(authority_v2(first, 1)),
            ],
            vec![
                upsert(first, 1),
                upsert(second, 2),
                ann_delta_authority_marker_v2(authority_v2(second, 1)),
                ann_delta_authority_marker_v2(authority_v2(first, 1)),
            ],
            vec![
                upsert(first, 1),
                ann_delta_authority_marker_v1(first),
                ann_delta_authority_marker_v2(authority_v2(first, 1)),
            ],
        ] {
            assert!(encode_test_transaction(&invalid).is_err());
        }
        Ok(())
    }

    #[test]
    fn vector_absence_fence_has_fixed_golden_and_strict_shape()
    -> Result<(), Box<dyn std::error::Error>> {
        let index = ObjectId::new(7)?;
        let object = ObjectId::new(9)?;
        let fence = mutation(
            EngineKind::Search,
            Opcode::FenceVectorAbsence,
            Some(index),
            &object.get().to_be_bytes(),
            &[],
            None,
        );
        assert_eq!(Opcode::FenceVectorAbsence as u8, 57);
        assert_eq!(
            decode_opcode(57)?,
            (Opcode::FenceVectorAbsence, EngineKind::Search)
        );
        assert_eq!(
            fence.encode()?,
            [
                b"HYMUT001".as_slice(),
                &[57, 3, 0, 0],
                &[7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                &[255, 255, 255, 255, 255, 255, 255, 127],
                &[16, 0, 0, 0],
                &[0, 0, 0, 0],
                &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9],
            ]
            .concat()
        );
        encode_test_transaction(std::slice::from_ref(&fence))?;
        for invalid in [
            Mutation {
                target: None,
                ..fence.clone()
            },
            Mutation {
                key: vec![0; 15],
                ..fence.clone()
            },
            Mutation {
                value: vec![1],
                ..fence.clone()
            },
            Mutation {
                expires_at_micros: Some(1),
                ..fence.clone()
            },
            Mutation {
                engine: EngineKind::Structure,
                ..fence
            },
        ] {
            assert!(decode_mutation(invalid.engine, &invalid.encode()?).is_err());
        }
        Ok(())
    }

    #[test]
    fn delete_vector_and_absence_fence_have_distinct_marker_rules()
    -> Result<(), Box<dyn std::error::Error>> {
        let index = ObjectId::new(7)?;
        let delete = |object: u128| {
            mutation(
                EngineKind::Search,
                Opcode::DeleteVector,
                Some(index),
                &object.to_be_bytes(),
                &[],
                None,
            )
        };
        let fence = |object: u128| {
            mutation(
                EngineKind::Search,
                Opcode::FenceVectorAbsence,
                Some(index),
                &object.to_be_bytes(),
                &[],
                None,
            )
        };
        encode_test_transaction(&[delete(1)])?;
        encode_test_transaction(&[fence(1)])?;
        encode_test_transaction(&[
            delete(1),
            ann_delta_authority_marker_v2(authority_v2(index, 1)),
        ])?;
        encode_test_transaction(&[
            delete(1),
            fence(2),
            ann_delta_authority_marker_v2(authority_v2(index, 1)),
        ])?;
        encode_test_transaction(&[delete(1), fence(2)])?;
        assert!(
            encode_test_transaction(&[
                delete(1),
                delete(2),
                ann_delta_authority_marker_v2(authority_v2(index, 1)),
            ])
            .is_err()
        );
        assert!(
            encode_test_transaction(&[
                fence(1),
                ann_delta_authority_marker_v2(authority_v2(index, 1)),
            ])
            .is_err()
        );
        assert!(
            encode_test_transaction(&[ann_delta_authority_marker_v2(authority_v2(index, 1))])
                .is_err()
        );
        assert!(
            encode_test_transaction(&[
                delete(1),
                ann_delta_authority_marker_v2(authority_v2(index, 2)),
            ])
            .is_err()
        );
        let mut bounded_fences = (1..=super::ANN_DELTA_AUTHORITY_V2_MAX_OPERATIONS)
            .map(|object| fence(u128::from(object)))
            .collect::<Vec<_>>();
        encode_test_transaction(&bounded_fences)?;
        bounded_fences.push(fence(
            u128::from(super::ANN_DELTA_AUTHORITY_V2_MAX_OPERATIONS) + 1,
        ));
        assert!(encode_test_transaction(&bounded_fences).is_err());
        Ok(())
    }

    #[test]
    fn transaction_preflight_streams_ordinary_and_generated_ann_bodies()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut preflight = TransactionWalPreflightBuilder::default();
        preflight.push(16, 0)?;
        preflight.push(0, ANN_DELTA_AUTHORITY_V2_SIZE)?;
        preflight.push(0, ANN_CONSOLIDATION_V2_SIZE)?;
        let preflight = preflight.finish()?;
        assert_eq!(preflight.mutation_count(), 3);
        assert_eq!(preflight.mutation_bytes(), 60 + 228 + 428);
        assert!(preflight.peak_memory_bytes() > preflight.mutation_bytes());

        let mut oversized = TransactionWalPreflightBuilder::default();
        for _ in 0..8_200 {
            oversized.push(1, 8_216)?;
        }
        assert!(matches!(
            oversized.finish(),
            Err(WalSemanticError::InvalidSequence)
        ));

        let mut record_too_large = TransactionWalPreflightBuilder::default();
        assert!(matches!(
            record_too_large.push(0, WAL_RECORD_BODY_SIZE),
            Err(WalSemanticError::LengthOverflow)
        ));
        Ok(())
    }

    #[test]
    fn stream_opcodes_are_stable_engine_bound_and_shape_checked()
    -> Result<(), Box<dyn std::error::Error>> {
        for (byte, opcode) in [
            (44, Opcode::CreateStream),
            (45, Opcode::AppendStreamEntry),
            (46, Opcode::DeleteStream),
        ] {
            assert_eq!(opcode as u8, byte);
            assert_eq!(decode_opcode(byte)?, (opcode, EngineKind::Structure));
        }
        assert!(validate_mutation_shape(Opcode::CreateStream, false, 0, None, b"s").is_ok());
        assert!(validate_mutation_shape(Opcode::DeleteStream, false, 0, None, b"s").is_ok());
        assert!(validate_mutation_shape(Opcode::AppendStreamEntry, false, 1, None, b"s").is_ok());
        assert!(validate_mutation_shape(Opcode::AppendStreamEntry, false, 0, None, b"s").is_err());
        assert!(validate_mutation_shape(Opcode::AppendStreamEntry, true, 1, None, b"s").is_err());
        Ok(())
    }

    #[test]
    fn drop_secondary_index_opcode_is_stable_and_engine_bound()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(Opcode::DropSecondaryIndex as u8, 40);
        assert_eq!(
            decode_opcode(40)?,
            (Opcode::DropSecondaryIndex, EngineKind::Relational)
        );
        assert_eq!(
            decode_opcode(41)?,
            (Opcode::RenameTable, EngineKind::Relational)
        );
        assert_eq!(
            decode_opcode(42)?,
            (Opcode::MigrateStructureV3, EngineKind::Structure)
        );
        Ok(())
    }

    #[test]
    fn structure_v3_migration_opcode_is_append_only_and_strict()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(Opcode::MigrateStructureV3 as u8, 42);
        let migration = structure_mutation(Opcode::MigrateStructureV3, b"", b"", None);
        let encoded = migration.encode()?;
        assert_eq!(encoded[8], 42);
        assert_eq!(decode_mutation(EngineKind::Structure, &encoded)?, migration);
        for invalid in [
            validate_mutation_shape(Opcode::MigrateStructureV3, true, 0, None, b""),
            validate_mutation_shape(Opcode::MigrateStructureV3, false, 1, None, b""),
            validate_mutation_shape(Opcode::MigrateStructureV3, false, 0, None, b"key"),
            validate_mutation_shape(Opcode::MigrateStructureV3, false, 0, Some(1), b""),
        ] {
            assert!(matches!(invalid, Err(WalSemanticError::InvalidBody)));
        }
        Ok(())
    }

    #[test]
    fn structure_v3_retirement_cleanup_opcode_is_append_only_and_bounded()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(Opcode::CleanupStructureRetirementV3 as u8, 52);
        let cleanup = structure_mutation(
            Opcode::CleanupStructureRetirementV3,
            b"retirement",
            &16_u32.to_le_bytes(),
            None,
        );
        let encoded = cleanup.encode()?;
        assert_eq!(encoded[8], 52);
        assert_eq!(decode_mutation(EngineKind::Structure, &encoded)?, cleanup);
        for invalid in [
            validate_mutation_shape(
                Opcode::CleanupStructureRetirementV3,
                true,
                4,
                None,
                b"retirement",
            ),
            validate_mutation_shape(
                Opcode::CleanupStructureRetirementV3,
                false,
                0,
                None,
                b"retirement",
            ),
            validate_mutation_shape(Opcode::CleanupStructureRetirementV3, false, 4, None, b""),
            validate_mutation_shape(
                Opcode::CleanupStructureRetirementV3,
                false,
                4,
                Some(1),
                b"retirement",
            ),
        ] {
            assert!(matches!(invalid, Err(WalSemanticError::InvalidBody)));
        }
        for invalid_budget in [1_u32, 1_025] {
            let invalid = structure_mutation(
                Opcode::CleanupStructureRetirementV3,
                b"retirement",
                &invalid_budget.to_le_bytes(),
                None,
            )
            .encode()?;
            assert!(matches!(
                decode_mutation(EngineKind::Structure, &invalid),
                Err(WalSemanticError::InvalidBody)
            ));
        }
        Ok(())
    }

    #[test]
    fn generation_one_commit_manifest_keeps_the_v1_golden_encoding()
    -> Result<(), Box<dyn std::error::Error>> {
        let manifest = commit_manifest(PageGeneration::FIRST, Csn::FIRST)?;
        let encoded = manifest.encode();
        assert_eq!(encoded.len(), 124);
        assert_eq!(&encoded[..8], b"HYCMT001");
        assert_eq!(
            blake3::hash(&encoded).to_hex().as_str(),
            "c113774534f936d83b929b9f07cb3d60990f0a1accc17fe886a335c2cb60b36b"
        );
        assert_eq!(CommitManifest::decode(&encoded)?, manifest);
        Ok(())
    }

    #[test]
    fn vacuum_commit_manifest_round_trips_v2_storage_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let manifest = commit_manifest(PageGeneration::new(2)?, Csn::new(8)?)?;
        let encoded = manifest.encode();
        assert_eq!(encoded.len(), 140);
        assert_eq!(&encoded[..8], b"HYCMT002");
        assert_eq!(&encoded[124..132], &2_u64.to_le_bytes());
        assert_eq!(&encoded[132..140], &8_u64.to_le_bytes());
        assert_eq!(CommitManifest::decode(&encoded)?, manifest);
        Ok(())
    }

    #[test]
    fn commit_manifest_rejects_invalid_storage_state() -> Result<(), Box<dyn std::error::Error>> {
        let mut zero_generation = commit_manifest(PageGeneration::new(2)?, Csn::new(8)?)?.encode();
        zero_generation[124..132].fill(0);
        assert!(matches!(
            CommitManifest::decode(&zero_generation),
            Err(WalSemanticError::InvalidIdentity)
        ));

        let mut future_floor = commit_manifest(PageGeneration::new(2)?, Csn::new(8)?)?.encode();
        future_floor[132..140].copy_from_slice(&10_u64.to_le_bytes());
        assert!(matches!(
            CommitManifest::decode(&future_floor),
            Err(WalSemanticError::InvalidSequence)
        ));

        let invalid_v1_state = [Mutation {
            engine: EngineKind::Structure,
            opcode: Opcode::SetValue,
            target: None,
            key: b"key".to_vec(),
            value: b"value".to_vec(),
            expires_at_micros: None,
        }];
        assert!(matches!(
            encode_transaction(&TransactionPlan {
                transaction_id: TransactionId::new(1)?,
                read_csn: Some(Csn::new(8)?),
                catalog_version: CatalogVersion::new(3)?,
                logical_time_micros: 123,
                durability: DurabilityClass::Strict,
                mutations: &invalid_v1_state,
                commit_csn: Csn::new(9)?,
                roots: [
                    PageId::new(11)?,
                    PageId::new(12)?,
                    PageId::new(13)?,
                    PageId::new(14)?,
                ],
                blob_generation: 4,
                page_generation: PageGeneration::FIRST,
                retention_floor_csn: Csn::new(8)?,
            }),
            Err(WalSemanticError::InvalidSequence)
        ));
        Ok(())
    }

    #[test]
    fn complete_transaction_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let mutations = complete_transaction_mutations()?;
        let roots = test_roots()?;
        let pending = encode_transaction(&TransactionPlan {
            transaction_id: TransactionId::new(1)?,
            read_csn: None,
            catalog_version: CatalogVersion::new(1)?,
            logical_time_micros: 10,
            durability: DurabilityClass::Strict,
            mutations: &mutations,
            commit_csn: Csn::new(1)?,
            roots,
            blob_generation: 0,
            page_generation: PageGeneration::FIRST,
            retention_floor_csn: Csn::FIRST,
        })?;
        let block = WalBlock::build(1, [0; 32], pending)?;
        let decoded = WalBlock::decode(1, [0; 32], &block.encode()?)?;
        let recovered = recover_wal(decoded.records())?;
        assert_eq!(recovered.commits.len(), 1);
        assert_eq!(recovered.commits[0].manifest.roots, roots);
        assert_eq!(
            recovered.commits[0].manifest.mutation_count,
            u32::try_from(mutations.len())?
        );
        assert_eq!(recovered.commits[0].mutations, mutations);
        Ok(())
    }

    #[test]
    fn whole_hash_delete_rejects_target_value_and_expiry() {
        assert!(validate_mutation_shape(Opcode::DeleteHash, false, 0, None, b"hash").is_ok());
        assert!(matches!(
            validate_mutation_shape(Opcode::DeleteHash, true, 0, None, b"hash"),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::DeleteHash, false, 1, None, b"hash"),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::DeleteHash, false, 0, Some(10), b"hash"),
            Err(WalSemanticError::InvalidBody)
        ));
    }

    #[test]
    fn whole_hash_expiry_requires_only_an_explicit_expiry() {
        assert!(
            validate_mutation_shape(Opcode::ExpireHash, false, 0, Some(i64::MIN), b"hash").is_ok()
        );
        assert!(matches!(
            validate_mutation_shape(Opcode::ExpireHash, true, 0, Some(10), b"hash"),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::ExpireHash, false, 1, Some(10), b"hash"),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::ExpireHash, false, 0, None, b"hash"),
            Err(WalSemanticError::InvalidBody)
        ));
    }

    #[test]
    fn hash_field_expiry_requires_compound_identity_and_explicit_expiry() {
        let mut field = 4_u32.to_be_bytes().to_vec();
        field.extend_from_slice(b"hashfield");
        assert!(
            validate_mutation_shape(Opcode::ExpireHashField, false, 0, Some(i64::MIN), &field,)
                .is_ok()
        );
        assert!(matches!(
            validate_mutation_shape(Opcode::ExpireHashField, true, 0, Some(10), &field),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::ExpireHashField, false, 1, Some(10), &field),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::ExpireHashField, false, 0, None, &field),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::ExpireHashField, false, 0, Some(10), b"\0"),
            Err(WalSemanticError::InvalidBody)
        ));
    }

    #[test]
    fn set_lifecycle_mutations_require_canonical_shapes() {
        assert!(
            validate_mutation_shape(Opcode::ExpireSet, false, 0, Some(i64::MIN), b"set").is_ok()
        );
        assert!(validate_mutation_shape(Opcode::DeleteSet, false, 0, None, b"set").is_ok());
        for result in [
            validate_mutation_shape(Opcode::ExpireSet, true, 0, Some(10), b"set"),
            validate_mutation_shape(Opcode::ExpireSet, false, 1, Some(10), b"set"),
            validate_mutation_shape(Opcode::ExpireSet, false, 0, None, b"set"),
            validate_mutation_shape(Opcode::DeleteSet, true, 0, None, b"set"),
            validate_mutation_shape(Opcode::DeleteSet, false, 1, None, b"set"),
            validate_mutation_shape(Opcode::DeleteSet, false, 0, Some(10), b"set"),
        ] {
            assert!(matches!(result, Err(WalSemanticError::InvalidBody)));
        }
    }

    #[test]
    fn list_mutations_reject_targets_expiry_and_nonempty_creation() {
        assert_eq!(Opcode::DeleteList as u8, 35);
        assert_eq!(Opcode::ExpireList as u8, 36);
        assert!(validate_mutation_shape(Opcode::DeleteList, false, 0, None, b"list").is_ok());
        assert!(
            validate_mutation_shape(Opcode::ExpireList, false, 0, Some(i64::MIN), b"list").is_ok()
        );
        assert!(matches!(
            validate_mutation_shape(Opcode::CreateList, false, 1, None, b"list"),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::CreateList, true, 0, None, b"list"),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::PushListHead, false, 1, Some(10), b"list"),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::PopListTail, true, 0, None, b"list"),
            Err(WalSemanticError::InvalidBody)
        ));
        for result in [
            validate_mutation_shape(Opcode::DeleteList, true, 0, None, b"list"),
            validate_mutation_shape(Opcode::DeleteList, false, 1, None, b"list"),
            validate_mutation_shape(Opcode::DeleteList, false, 0, Some(10), b"list"),
            validate_mutation_shape(Opcode::ExpireList, true, 0, Some(10), b"list"),
            validate_mutation_shape(Opcode::ExpireList, false, 1, Some(10), b"list"),
            validate_mutation_shape(Opcode::ExpireList, false, 0, None, b"list"),
        ] {
            assert!(matches!(result, Err(WalSemanticError::InvalidBody)));
        }
    }

    #[test]
    fn sorted_set_mutations_reject_noncanonical_shapes() {
        let mut member = 3_u32.to_be_bytes().to_vec();
        member.extend_from_slice(b"setmember");
        assert!(matches!(
            validate_mutation_shape(Opcode::CreateSortedSet, false, 1, None, b"sorted"),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::UpsertSortedSetMember, false, 7, None, &member),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::UpsertSortedSetMember, false, 8, Some(10), &member),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::DeleteSortedSetMember, false, 1, None, &member),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::DeleteSortedSetMember, true, 0, None, &member),
            Err(WalSemanticError::InvalidBody)
        ));
    }

    #[test]
    fn structure_compaction_requires_an_empty_structure_maintenance_body() {
        assert!(validate_mutation_shape(Opcode::CompactStructure, false, 0, None, b"").is_ok());
        assert!(matches!(
            validate_mutation_shape(Opcode::CompactStructure, true, 0, None, b""),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::CompactStructure, false, 0, None, b"key"),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::CompactStructure, false, 1, None, b""),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::CompactStructure, false, 0, Some(10), b""),
            Err(WalSemanticError::InvalidBody)
        ));
    }

    #[test]
    fn search_compaction_requires_an_empty_search_maintenance_body() {
        assert!(validate_mutation_shape(Opcode::CompactSearch, false, 0, None, b"").is_ok());
        assert!(matches!(
            validate_mutation_shape(Opcode::CompactSearch, true, 0, None, b""),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::CompactSearch, false, 0, None, b"key"),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::CompactSearch, false, 1, None, b""),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::CompactSearch, false, 0, Some(10), b""),
            Err(WalSemanticError::InvalidBody)
        ));
    }

    #[test]
    fn search_compaction_has_stable_bytes() -> Result<(), Box<dyn std::error::Error>> {
        let compaction = mutation(
            EngineKind::Search,
            Opcode::CompactSearch,
            None,
            b"",
            b"",
            None,
        );
        let encoded = compaction.encode()?;
        assert_eq!(
            encoded,
            [
                b'H', b'Y', b'M', b'U', b'T', b'0', b'0', b'1', 39, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f, 0, 0, 0,
                0, 0, 0, 0, 0,
            ]
        );
        assert_eq!(decode_mutation(EngineKind::Search, &encoded)?, compaction);
        Ok(())
    }

    #[test]
    fn ann_consolidation_opcode_is_append_only_and_strict() -> Result<(), Box<dyn std::error::Error>>
    {
        assert_eq!(Opcode::ConsolidateAnn as u8, 50);
        assert_eq!(
            decode_opcode(50)?,
            (Opcode::ConsolidateAnn, EngineKind::Search)
        );
        let index = ObjectId::new(3)?;
        let mut value = Vec::with_capacity(ANN_CONSOLIDATION_V2_SIZE);
        value.extend_from_slice(b"HYANNC02");
        value.extend_from_slice(&index.get().to_be_bytes());
        value.extend_from_slice(&[4, 5]);
        value.extend_from_slice(&[0; 6]);
        for byte in 1..=9 {
            value.extend_from_slice(&[byte; 32]);
        }
        value.extend_from_slice(&8_u64.to_le_bytes());
        value.extend_from_slice(&1_u64.to_le_bytes());
        value.extend_from_slice(&9_u64.to_le_bytes());
        value.extend_from_slice(&9_u64.to_le_bytes());
        value.extend_from_slice(&1_u64.to_le_bytes());
        value.extend_from_slice(&0_u64.to_le_bytes());
        value.extend_from_slice(&2_u64.to_le_bytes());
        value.extend_from_slice(&[0; 8]);
        assert_eq!(value.len(), ANN_CONSOLIDATION_V2_SIZE);
        let consolidation = mutation(
            EngineKind::Search,
            Opcode::ConsolidateAnn,
            Some(index),
            b"",
            &value,
            None,
        );
        let encoded = consolidation.encode()?;
        assert_eq!(encoded[8], 50);
        assert_eq!(
            decode_mutation(EngineKind::Search, &encoded)?,
            consolidation
        );

        for invalid in [
            validate_mutation_shape(Opcode::ConsolidateAnn, false, value.len(), None, b""),
            validate_mutation_shape(
                Opcode::ConsolidateAnn,
                true,
                ANN_CONSOLIDATION_V2_SIZE - 1,
                None,
                b"",
            ),
            validate_mutation_shape(Opcode::ConsolidateAnn, true, value.len(), None, b"key"),
            validate_mutation_shape(Opcode::ConsolidateAnn, true, value.len(), Some(1), b""),
        ] {
            assert!(matches!(invalid, Err(WalSemanticError::InvalidBody)));
        }
        let mut bad_magic = encoded.clone();
        bad_magic[44] ^= 1;
        assert!(matches!(
            decode_mutation(EngineKind::Search, &bad_magic),
            Err(WalSemanticError::InvalidBody)
        ));
        for offset in [24_usize, 25] {
            for format in 1..=3 {
                let mut historical = value.clone();
                historical[offset] = format;
                let rejected = mutation(
                    EngineKind::Search,
                    Opcode::ConsolidateAnn,
                    Some(index),
                    b"",
                    &historical,
                    None,
                );
                let rejected = rejected.encode()?;
                assert!(matches!(
                    decode_mutation(EngineKind::Search, &rejected),
                    Err(WalSemanticError::InvalidBody)
                ));
            }
        }

        let mut legacy = Vec::with_capacity(ANN_CONSOLIDATION_V1_SIZE);
        legacy.extend_from_slice(b"HYANNC01");
        legacy.extend_from_slice(&[1; 32]);
        legacy.extend_from_slice(&[2; 32]);
        legacy.extend_from_slice(&[3; 32]);
        legacy.extend_from_slice(&1_u64.to_le_bytes());
        let legacy = mutation(
            EngineKind::Search,
            Opcode::ConsolidateAnn,
            Some(index),
            b"",
            &legacy,
            None,
        );
        let encoded = legacy.encode()?;
        assert_eq!(decode_mutation(EngineKind::Search, &encoded)?, legacy);
        let mut zero_count = legacy;
        zero_count.value[104..112].fill(0);
        assert!(matches!(
            decode_mutation(EngineKind::Search, &zero_count.encode()?),
            Err(WalSemanticError::InvalidBody)
        ));
        Ok(())
    }

    #[test]
    fn initial_ann_bulk_publication_opcode_and_payload_are_strict()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(Opcode::PublishInitialAnnBulk as u8, 53);
        assert_eq!(
            decode_opcode(53)?,
            (Opcode::PublishInitialAnnBulk, EngineKind::Search)
        );
        let mut value = Vec::with_capacity(160);
        value.extend_from_slice(b"HYANNP01");
        value.extend_from_slice(&[1; 32]);
        value.extend_from_slice(&[2; 32]);
        value.extend_from_slice(&[3; 32]);
        value.extend_from_slice(&[4; 32]);
        value.extend_from_slice(&8_u64.to_le_bytes());
        value.extend_from_slice(&1_000_000_u64.to_le_bytes());
        value.extend_from_slice(&99_u64.to_le_bytes());
        let mutation = mutation(
            EngineKind::Search,
            Opcode::PublishInitialAnnBulk,
            Some(ObjectId::new(3)?),
            b"",
            &value,
            None,
        );
        let encoded = mutation.encode()?;
        assert_eq!(encoded[8], 53);
        assert_eq!(decode_mutation(EngineKind::Search, &encoded)?, mutation);

        for invalid in [
            validate_mutation_shape(Opcode::PublishInitialAnnBulk, false, 160, None, b""),
            validate_mutation_shape(Opcode::PublishInitialAnnBulk, true, 159, None, b""),
            validate_mutation_shape(Opcode::PublishInitialAnnBulk, true, 160, None, b"key"),
            validate_mutation_shape(Opcode::PublishInitialAnnBulk, true, 160, Some(1), b""),
        ] {
            assert!(matches!(invalid, Err(WalSemanticError::InvalidBody)));
        }

        let mut bad_magic = encoded.clone();
        bad_magic[44] ^= 1;
        assert!(matches!(
            decode_mutation(EngineKind::Search, &bad_magic),
            Err(WalSemanticError::InvalidBody)
        ));
        for range in [52..84, 84..116, 116..148, 148..180] {
            let mut invalid_identity = encoded.clone();
            invalid_identity[range].fill(0);
            assert!(matches!(
                decode_mutation(EngineKind::Search, &invalid_identity),
                Err(WalSemanticError::InvalidBody)
            ));
        }
        for (range, invalid_count) in [
            (180..188, 0_u64),
            (188..196, 0),
            (180..188, 1_000_001),
            (196..204, 0),
        ] {
            let mut invalid_field = encoded.clone();
            invalid_field[range].copy_from_slice(&invalid_count.to_le_bytes());
            assert!(matches!(
                decode_mutation(EngineKind::Search, &invalid_field),
                Err(WalSemanticError::InvalidBody)
            ));
        }
        Ok(())
    }

    #[test]
    fn logical_catalog_create_uses_the_next_append_only_opcode()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(Opcode::CreateCatalogObjectV2 as u8, 51);
        assert_eq!(
            decode_opcode(51)?,
            (Opcode::CreateCatalogObjectV2, EngineKind::Kernel)
        );
        let mutation = mutation(
            EngineKind::Kernel,
            Opcode::CreateCatalogObjectV2,
            Some(ObjectId::new(7)?),
            b"qualified-name",
            b"HYCOBJ02definition",
            None,
        );
        let encoded = mutation.encode()?;
        assert_eq!(encoded[8], 51);
        assert_eq!(decode_mutation(EngineKind::Kernel, &encoded)?, mutation);
        assert!(
            validate_mutation_shape(Opcode::CreateCatalogObjectV2, false, 1, None, b"").is_err()
        );
        assert!(
            validate_mutation_shape(
                Opcode::CreateCatalogObjectV2,
                true,
                0,
                None,
                b"qualified-name"
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn page_vacuum_requires_an_empty_kernel_maintenance_body() {
        assert!(validate_mutation_shape(Opcode::VacuumPageGeneration, false, 0, None, b"").is_ok());
        assert!(matches!(
            validate_mutation_shape(Opcode::VacuumPageGeneration, true, 0, None, b""),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::VacuumPageGeneration, false, 0, None, b"key"),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::VacuumPageGeneration, false, 1, None, b""),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::VacuumPageGeneration, false, 0, Some(10), b""),
            Err(WalSemanticError::InvalidBody)
        ));
    }

    #[test]
    fn lexical_lifecycle_mutations_have_stable_shapes_and_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        let index = ObjectId::new(2)?;
        let replacement = mutation(
            EngineKind::Search,
            Opcode::ReplaceDocument,
            Some(index),
            b"doc",
            b"x",
            None,
        );
        let encoded_replacement = replacement.encode()?;
        let mut expected_replacement = Vec::new();
        expected_replacement.extend_from_slice(b"HYMUT001");
        expected_replacement.extend_from_slice(&[37, 3, 0, 0]);
        expected_replacement.extend_from_slice(&2_u128.to_le_bytes());
        expected_replacement.extend_from_slice(&i64::MAX.to_le_bytes());
        expected_replacement.extend_from_slice(&3_u32.to_le_bytes());
        expected_replacement.extend_from_slice(&1_u32.to_le_bytes());
        expected_replacement.extend_from_slice(b"docx");
        assert_eq!(encoded_replacement, expected_replacement);
        assert_eq!(
            decode_mutation(EngineKind::Search, &encoded_replacement)?,
            replacement
        );

        let deletion = mutation(
            EngineKind::Search,
            Opcode::DeleteDocument,
            Some(index),
            b"doc",
            b"",
            None,
        );
        let encoded_deletion = deletion.encode()?;
        let mut expected_deletion = Vec::new();
        expected_deletion.extend_from_slice(b"HYMUT001");
        expected_deletion.extend_from_slice(&[38, 3, 0, 0]);
        expected_deletion.extend_from_slice(&2_u128.to_le_bytes());
        expected_deletion.extend_from_slice(&i64::MAX.to_le_bytes());
        expected_deletion.extend_from_slice(&3_u32.to_le_bytes());
        expected_deletion.extend_from_slice(&0_u32.to_le_bytes());
        expected_deletion.extend_from_slice(b"doc");
        assert_eq!(encoded_deletion, expected_deletion);
        assert_eq!(
            decode_mutation(EngineKind::Search, &encoded_deletion)?,
            deletion
        );

        assert!(matches!(
            validate_mutation_shape(Opcode::ReplaceDocument, false, 1, None, b"doc"),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::ReplaceDocument, true, 1, Some(1), b"doc"),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::DeleteDocument, true, 1, None, b"doc"),
            Err(WalSemanticError::InvalidBody)
        ));
        assert!(matches!(
            validate_mutation_shape(Opcode::DeleteDocument, true, 0, Some(1), b"doc"),
            Err(WalSemanticError::InvalidBody)
        ));
        Ok(())
    }

    #[test]
    fn vector_mutations_reject_noncanonical_object_identities()
    -> Result<(), Box<dyn std::error::Error>> {
        let mutations = [mutation(
            EngineKind::Search,
            Opcode::UpsertVector,
            Some(ObjectId::new(3)?),
            &[1; 15],
            &[0, 0, 128, 63],
            None,
        )];
        let pending = encode_transaction(&TransactionPlan {
            transaction_id: TransactionId::new(1)?,
            read_csn: None,
            catalog_version: CatalogVersion::new(1)?,
            logical_time_micros: 10,
            durability: DurabilityClass::Strict,
            mutations: &mutations,
            commit_csn: Csn::new(1)?,
            roots: [
                PageId::new(1)?,
                PageId::new(2)?,
                PageId::new(3)?,
                PageId::new(4)?,
            ],
            blob_generation: 0,
            page_generation: PageGeneration::FIRST,
            retention_floor_csn: Csn::FIRST,
        })?;
        let block = WalBlock::build(1, [0; 32], pending)?;
        let decoded = WalBlock::decode(1, [0; 32], &block.encode()?)?;
        assert!(matches!(
            recover_wal(decoded.records()),
            Err(WalSemanticError::InvalidBody)
        ));
        Ok(())
    }

    #[test]
    fn set_member_mutations_reject_truncated_compound_identities()
    -> Result<(), Box<dyn std::error::Error>> {
        let mutations = [structure_mutation(
            Opcode::AddSetMember,
            &[0, 0, 0, 8, 1],
            b"",
            None,
        )];
        let pending = encode_transaction(&TransactionPlan {
            transaction_id: TransactionId::new(1)?,
            read_csn: None,
            catalog_version: CatalogVersion::new(1)?,
            logical_time_micros: 10,
            durability: DurabilityClass::Strict,
            mutations: &mutations,
            commit_csn: Csn::new(1)?,
            roots: [
                PageId::new(1)?,
                PageId::new(2)?,
                PageId::new(3)?,
                PageId::new(4)?,
            ],
            blob_generation: 0,
            page_generation: PageGeneration::FIRST,
            retention_floor_csn: Csn::FIRST,
        })?;
        let block = WalBlock::build(1, [0; 32], pending)?;
        let decoded = WalBlock::decode(1, [0; 32], &block.encode()?)?;
        assert!(matches!(
            recover_wal(decoded.records()),
            Err(WalSemanticError::InvalidBody)
        ));
        Ok(())
    }

    #[test]
    fn checkpoint_record_anchors_one_committed_manifest() -> Result<(), Box<dyn std::error::Error>>
    {
        let transaction_id = TransactionId::new(1)?;
        let mutations = vec![Mutation {
            engine: EngineKind::Structure,
            opcode: Opcode::SetValue,
            target: None,
            key: b"key".to_vec(),
            value: b"value".to_vec(),
            expires_at_micros: None,
        }];
        let roots = [
            PageId::new(1)?,
            PageId::new(2)?,
            PageId::new(3)?,
            PageId::new(4)?,
        ];
        let mut pending = encode_transaction(&TransactionPlan {
            transaction_id,
            read_csn: None,
            catalog_version: CatalogVersion::new(1)?,
            logical_time_micros: 10,
            durability: DurabilityClass::Strict,
            mutations: &mutations,
            commit_csn: Csn::new(1)?,
            roots,
            blob_generation: 0,
            page_generation: PageGeneration::FIRST,
            retention_floor_csn: Csn::FIRST,
        })?;
        let commit_block = WalBlock::build(1, [0; 32], pending)?;
        let commit_decoded = WalBlock::decode(1, [0; 32], &commit_block.encode()?)?;
        let commit_lsn = commit_decoded
            .records()
            .last()
            .ok_or("missing commit")?
            .lsn();
        let checkpoint = encode_checkpoint(
            TransactionId::new(2)?,
            Csn::new(1)?,
            ManifestGeneration::new(1)?,
            [7; 32],
            None,
        )?;
        pending = vec![checkpoint];
        let checkpoint_block = WalBlock::build(2, commit_block.digest(), pending)?;
        let checkpoint_decoded =
            WalBlock::decode(2, commit_block.digest(), &checkpoint_block.encode()?)?;
        let mut records = commit_decoded.records().to_vec();
        records.extend_from_slice(checkpoint_decoded.records());
        let recovered = recover_wal(&records)?;
        assert_eq!(recovered.commits[0].commit_lsn, commit_lsn);
        assert_eq!(recovered.checkpoints.len(), 1);
        assert_eq!(recovered.checkpoints[0].manifest_generation.get(), 1);
        assert_eq!(recovered.checkpoints[0].manifest_digest, [7; 32]);
        Ok(())
    }
}
