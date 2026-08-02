// SPDX-License-Identifier: Apache-2.0

use hyphae_native_types::{
    CatalogVersion, Csn, DurabilityClass, EngineKind, Lsn, ManifestGeneration, ObjectId, PageId,
    TransactionId,
};
use hyphae_native_wal::{PendingRecord, RecordKind, WalRecord};
use thiserror::Error;

const BEGIN_MAGIC: &[u8; 8] = b"HYBGN001";
const MUTATION_MAGIC: &[u8; 8] = b"HYMUT001";
const COMMIT_MAGIC: &[u8; 8] = b"HYCMT001";
const ABORT_MAGIC: &[u8; 8] = b"HYABT001";
const CHECKPOINT_MAGIC: &[u8; 8] = b"HYCHK001";
const ROOT_COUNT: usize = 4;
const MUTATION_HAS_EXPIRY: u8 = 1;

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
        let mut bytes = Vec::new();
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
}

impl CommitManifest {
    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(124);
        bytes.extend_from_slice(COMMIT_MAGIC);
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
        bytes
    }

    fn decode(body: &[u8]) -> Result<Self, WalSemanticError> {
        if body.len() != 124 || body.get(..8) != Some(COMMIT_MAGIC.as_slice()) {
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
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoveredCommit {
    pub(crate) transaction_id: TransactionId,
    pub(crate) commit_lsn: Lsn,
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RecoveredWal {
    pub(crate) commits: Vec<RecoveredCommit>,
    pub(crate) checkpoints: Vec<RecoveredCheckpoint>,
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
}

pub(crate) fn encode_transaction(
    plan: &TransactionPlan<'_>,
) -> Result<Vec<PendingRecord>, WalSemanticError> {
    if plan.mutations.is_empty() {
        return Err(WalSemanticError::InvalidSequence);
    }
    let encoded_mutations = plan
        .mutations
        .iter()
        .map(Mutation::encode)
        .collect::<Result<Vec<_>, _>>()?;
    let mutation_bytes = encoded_mutations.iter().try_fold(0_u64, |total, body| {
        total
            .checked_add(u64::try_from(body.len()).map_err(|_| WalSemanticError::LengthOverflow)?)
            .ok_or(WalSemanticError::LengthOverflow)
    })?;
    let digest = mutation_digest(
        plan.mutations
            .iter()
            .zip(encoded_mutations.iter())
            .map(|(mutation, body)| (mutation.engine, body.as_slice())),
    )?;
    let mutation_count =
        u32::try_from(plan.mutations.len()).map_err(|_| WalSemanticError::LengthOverflow)?;
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

pub(crate) fn recover_wal(records: &[WalRecord]) -> Result<RecoveredWal, WalSemanticError> {
    let mut recovered = RecoveredWal::default();
    let mut active: Option<ActiveTransaction> = None;
    for record in records {
        match record.kind() {
            RecordKind::Begin => {
                if active.is_some() || record.engine() != EngineKind::Kernel {
                    return Err(WalSemanticError::InvalidSequence);
                }
                active = Some(ActiveTransaction {
                    transaction_id: record.transaction_id(),
                    begin: decode_begin(record.body())?,
                    mutations: Vec::new(),
                });
            }
            RecordKind::Mutation => {
                let transaction = active.as_mut().ok_or(WalSemanticError::InvalidSequence)?;
                if transaction.transaction_id != record.transaction_id() {
                    return Err(WalSemanticError::InvalidSequence);
                }
                let mutation = decode_mutation(record.engine(), record.body())?;
                transaction
                    .mutations
                    .push((mutation, record.body().to_vec()));
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
                    manifest,
                    mutations: transaction
                        .mutations
                        .into_iter()
                        .map(|(mutation, _)| mutation)
                        .collect(),
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
                if active.is_some() || record.engine() != EngineKind::Kernel || record.flags() != 0
                {
                    return Err(WalSemanticError::InvalidSequence);
                }
                let checkpoint = decode_checkpoint(record)?;
                let committed = recovered
                    .commits
                    .iter()
                    .any(|commit| commit.manifest.commit_csn == checkpoint.visible_csn);
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
                } else if checkpoint.previous_checkpoint_lsn.is_some() {
                    return Err(WalSemanticError::InvalidSequence);
                }
                recovered.checkpoints.push(checkpoint);
            }
            RecordKind::Catalog => {
                return Err(WalSemanticError::InvalidSequence);
            }
        }
    }
    Ok(recovered)
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
    Ok(Begin {
        read_csn: optional_csn(read_u64(&body[8..16]))?,
        catalog_version: CatalogVersion::new(read_u64(&body[16..24]))
            .map_err(|_| WalSemanticError::InvalidIdentity)?,
        logical_time_micros: read_i64(&body[24..32]),
        durability,
        mutation_count: read_u32(&body[40..44]),
        mutation_bytes: read_u64(&body[44..52]),
    })
}

impl ActiveTransaction {
    fn validate(&self, commit: &CommitManifest) -> Result<(), WalSemanticError> {
        let count =
            u32::try_from(self.mutations.len()).map_err(|_| WalSemanticError::LengthOverflow)?;
        let bytes = self.mutations.iter().try_fold(0_u64, |total, (_, body)| {
            total
                .checked_add(
                    u64::try_from(body.len()).map_err(|_| WalSemanticError::LengthOverflow)?,
                )
                .ok_or(WalSemanticError::LengthOverflow)
        })?;
        let digest = mutation_digest(
            self.mutations
                .iter()
                .map(|(mutation, body)| (mutation.engine, body.as_slice())),
        )?;
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
    mutations: Vec<(Mutation, Vec<u8>)>,
}

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
        value if value == Opcode::CreateSet as u8 => (Opcode::CreateSet, EngineKind::Structure),
        value if value == Opcode::AddSetMember as u8 => {
            (Opcode::AddSetMember, EngineKind::Structure)
        }
        value if value == Opcode::DeleteSetMember as u8 => {
            (Opcode::DeleteSetMember, EngineKind::Structure)
        }
        value if value == Opcode::CreateList as u8 => (Opcode::CreateList, EngineKind::Structure),
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
        value if value == Opcode::CreateIndex as u8 => (Opcode::CreateIndex, EngineKind::Search),
        value if value == Opcode::IndexDocument as u8 => {
            (Opcode::IndexDocument, EngineKind::Search)
        }
        value if value == Opcode::CreateAnnIndex as u8 => {
            (Opcode::CreateAnnIndex, EngineKind::Search)
        }
        value if value == Opcode::UpsertVector as u8 => (Opcode::UpsertVector, EngineKind::Search),
        value if value == Opcode::DeleteVector as u8 => (Opcode::DeleteVector, EngineKind::Search),
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
    Ok(Mutation {
        engine,
        opcode,
        target,
        key: key.to_vec(),
        value: body[value_start..expected].to_vec(),
        expires_at_micros,
    })
}

fn validate_mutation_shape(
    opcode: Opcode,
    has_target: bool,
    value_length: usize,
    expires_at_micros: Option<i64>,
    key: &[u8],
) -> Result<(), WalSemanticError> {
    if opcode == Opcode::CompactStructure {
        return validate_structure_compaction_shape(
            has_target,
            value_length,
            expires_at_micros,
            key,
        );
    }
    match opcode {
        Opcode::SetValue
        | Opcode::DeleteValue
        | Opcode::ExpireValue
        | Opcode::CreateHash
        | Opcode::SetHashField
        | Opcode::DeleteHashField
        | Opcode::CreateSet
        | Opcode::AddSetMember
        | Opcode::DeleteSetMember
        | Opcode::CreateList
        | Opcode::PushListHead
        | Opcode::PushListTail
        | Opcode::PopListHead
        | Opcode::PopListTail
        | Opcode::CreateSortedSet
        | Opcode::UpsertSortedSetMember
        | Opcode::DeleteSortedSetMember
            if has_target =>
        {
            return Err(WalSemanticError::InvalidBody);
        }
        Opcode::CreateTable
        | Opcode::InsertRow
        | Opcode::CreateSecondaryIndex
        | Opcode::CreateIndex
        | Opcode::IndexDocument
        | Opcode::CreateAnnIndex
        | Opcode::UpsertVector
        | Opcode::DeleteVector
        | Opcode::UpdateRow
        | Opcode::DeleteRow
            if !has_target =>
        {
            return Err(WalSemanticError::InvalidBody);
        }
        Opcode::DeleteRow | Opcode::DeleteVector if value_length != 0 => {
            return Err(WalSemanticError::InvalidBody);
        }
        Opcode::DeleteValue
        | Opcode::CreateHash
        | Opcode::DeleteHashField
        | Opcode::CreateSet
        | Opcode::AddSetMember
        | Opcode::DeleteSetMember
        | Opcode::CreateList
        | Opcode::CreateSortedSet
        | Opcode::DeleteSortedSetMember
            if value_length != 0 || expires_at_micros.is_some() =>
        {
            return Err(WalSemanticError::InvalidBody);
        }
        Opcode::ExpireValue if expires_at_micros.is_none() => {
            return Err(WalSemanticError::InvalidBody);
        }
        Opcode::PushListHead
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
        | Opcode::CreateAnnIndex
        | Opcode::UpsertVector
        | Opcode::DeleteVector
            if expires_at_micros.is_some() =>
        {
            return Err(WalSemanticError::InvalidBody);
        }
        _ => {}
    }
    validate_mutation_identity(opcode, value_length, key)
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
            | Opcode::AddSetMember
            | Opcode::DeleteSetMember
            | Opcode::UpsertSortedSetMember
            | Opcode::DeleteSortedSetMember
    ) && !valid_collection_member_identity(key)
    {
        return Err(WalSemanticError::InvalidBody);
    }
    if matches!(opcode, Opcode::UpsertVector | Opcode::DeleteVector) && key.len() != 16 {
        return Err(WalSemanticError::InvalidBody);
    }
    if opcode == Opcode::UpsertVector && (value_length == 0 || !value_length.is_multiple_of(4)) {
        return Err(WalSemanticError::InvalidBody);
    }
    if opcode == Opcode::UpsertSortedSetMember && value_length != 8 {
        return Err(WalSemanticError::InvalidBody);
    }
    Ok(())
}

fn validate_structure_compaction_shape(
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
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-native-mutation-set-v1");
    for (engine, body) in mutations {
        hasher.update(&[engine as u8]);
        let length = u32::try_from(body.len()).map_err(|_| WalSemanticError::LengthOverflow)?;
        hasher.update(&length.to_le_bytes());
        hasher.update(body);
    }
    Ok(*hasher.finalize().as_bytes())
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
        CatalogVersion, Csn, DurabilityClass, EngineKind, ManifestGeneration, ObjectId, PageId,
        TransactionId,
    };
    use hyphae_native_wal::WalBlock;

    use super::{
        Mutation, Opcode, TransactionPlan, WalSemanticError, encode_checkpoint, encode_transaction,
        recover_wal, validate_mutation_shape,
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

    #[test]
    fn complete_transaction_round_trips_through_physical_wal()
    -> Result<(), Box<dyn std::error::Error>> {
        let transaction_id = TransactionId::new(1)?;
        let mut hash_field = 4_u32.to_be_bytes().to_vec();
        hash_field.extend_from_slice(b"hashfield");
        let mut set_member = 3_u32.to_be_bytes().to_vec();
        set_member.extend_from_slice(b"setmember");
        let mut sorted_set_member = 6_u32.to_be_bytes().to_vec();
        sorted_set_member.extend_from_slice(b"sortedmember");
        let mutations = vec![
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
            structure_mutation(Opcode::SetHashField, &hash_field, b"value", None),
            structure_mutation(Opcode::DeleteHashField, &hash_field, b"", None),
            structure_mutation(Opcode::CreateSet, b"set", b"", None),
            structure_mutation(Opcode::AddSetMember, &set_member, b"", None),
            structure_mutation(Opcode::DeleteSetMember, &set_member, b"", None),
            structure_mutation(Opcode::CreateList, b"list", b"", None),
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
                Opcode::CreateAnnIndex,
                Some(ObjectId::new(3)?),
                b"",
                b"catalog-definition",
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
        ];
        let roots = [
            PageId::new(1)?,
            PageId::new(2)?,
            PageId::new(3)?,
            PageId::new(4)?,
        ];
        let pending = encode_transaction(&TransactionPlan {
            transaction_id,
            read_csn: None,
            catalog_version: CatalogVersion::new(1)?,
            logical_time_micros: 10,
            durability: DurabilityClass::Strict,
            mutations: &mutations,
            commit_csn: Csn::new(1)?,
            roots,
            blob_generation: 0,
        })?;
        let block = WalBlock::build(1, [0; 32], pending)?;
        let decoded = WalBlock::decode(1, [0; 32], &block.encode()?)?;
        let recovered = recover_wal(decoded.records())?;
        assert_eq!(recovered.commits.len(), 1);
        assert_eq!(recovered.commits[0].manifest.roots, roots);
        assert_eq!(recovered.commits[0].manifest.mutation_count, 23);
        assert_eq!(recovered.commits[0].mutations, mutations);
        Ok(())
    }

    #[test]
    fn list_mutations_reject_targets_expiry_and_nonempty_creation() {
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
