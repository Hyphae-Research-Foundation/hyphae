// SPDX-License-Identifier: Apache-2.0

use hyphae_native_types::{
    CatalogVersion, Csn, DurabilityClass, EngineKind, Lsn, ObjectId, PageId, TransactionId,
};
use hyphae_native_wal::{PendingRecord, RecordKind, WalRecord};
use thiserror::Error;

const BEGIN_MAGIC: &[u8; 8] = b"HYBGN001";
const MUTATION_MAGIC: &[u8; 8] = b"HYMUT001";
const COMMIT_MAGIC: &[u8; 8] = b"HYCMT001";
const ABORT_MAGIC: &[u8; 8] = b"HYABT001";
const ROOT_COUNT: usize = 4;

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
        bytes.extend_from_slice(&0_u16.to_le_bytes());
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
    pub(crate) mutation_count: u32,
    pub(crate) mutation_bytes: u64,
    pub(crate) logical_time_micros: i64,
    pub(crate) mutation_digest: [u8; 32],
    pub(crate) roots: [PageId; ROOT_COUNT],
}

impl CommitManifest {
    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(116);
        bytes.extend_from_slice(COMMIT_MAGIC);
        bytes.extend_from_slice(&self.read_csn.map_or(0, Csn::get).to_le_bytes());
        bytes.extend_from_slice(&self.commit_csn.get().to_le_bytes());
        bytes.extend_from_slice(&self.catalog_version.get().to_le_bytes());
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
        if body.len() != 116 || body.get(..8) != Some(COMMIT_MAGIC.as_slice()) {
            return Err(WalSemanticError::InvalidBody);
        }
        let read_csn = optional_csn(read_u64(&body[8..16]))?;
        let commit_csn =
            Csn::new(read_u64(&body[16..24])).map_err(|_| WalSemanticError::InvalidIdentity)?;
        let catalog_version = CatalogVersion::new(read_u64(&body[24..32]))
            .map_err(|_| WalSemanticError::InvalidIdentity)?;
        let mutation_count = read_u32(&body[32..36]);
        let mutation_bytes = read_u64(&body[36..44]);
        let logical_time_micros = read_i64(&body[44..52]);
        let mut mutation_digest = [0_u8; 32];
        mutation_digest.copy_from_slice(&body[52..84]);
        let mut roots =
            [PageId::new(1).map_err(|_| WalSemanticError::InvalidIdentity)?; ROOT_COUNT];
        for (index, root) in roots.iter_mut().enumerate() {
            let start = 84 + index * 8;
            *root = PageId::new(read_u64(&body[start..start + 8]))
                .map_err(|_| WalSemanticError::InvalidIdentity)?;
        }
        Ok(Self {
            read_csn,
            commit_csn,
            catalog_version,
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

pub(crate) fn recover_commits(
    records: &[WalRecord],
) -> Result<Vec<RecoveredCommit>, WalSemanticError> {
    let mut recovered = Vec::new();
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
                validate_mutation(record.engine(), record.body())?;
                transaction
                    .mutations
                    .push((record.engine(), record.body().to_vec()));
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
                if recovered.last().is_some_and(|prior: &RecoveredCommit| {
                    prior.manifest.commit_csn >= manifest.commit_csn
                }) {
                    return Err(WalSemanticError::InvalidSequence);
                }
                recovered.push(RecoveredCommit {
                    transaction_id: record.transaction_id(),
                    commit_lsn: record.lsn(),
                    manifest,
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
            RecordKind::Checkpoint | RecordKind::Catalog => {
                return Err(WalSemanticError::InvalidSequence);
            }
        }
    }
    Ok(recovered)
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
                .map(|(engine, body)| (*engine, body.as_slice())),
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
    mutations: Vec<(EngineKind, Vec<u8>)>,
}

fn validate_mutation(engine: EngineKind, body: &[u8]) -> Result<(), WalSemanticError> {
    if body.len() < 44
        || body.get(..8) != Some(MUTATION_MAGIC.as_slice())
        || body[9] != engine as u8
        || body[10..12].iter().any(|byte| *byte != 0)
    {
        return Err(WalSemanticError::InvalidBody);
    }
    let opcode_engine = match body[8] {
        value if value == Opcode::CreateTable as u8 || value == Opcode::InsertRow as u8 => {
            EngineKind::Relational
        }
        value if value == Opcode::SetValue as u8 => EngineKind::Structure,
        value if value == Opcode::CreateIndex as u8 || value == Opcode::IndexDocument as u8 => {
            EngineKind::Search
        }
        _ => return Err(WalSemanticError::InvalidBody),
    };
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
    Ok(())
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

fn read_i64(bytes: &[u8]) -> i64 {
    let mut value = [0_u8; 8];
    value.copy_from_slice(bytes);
    i64::from_le_bytes(value)
}

#[cfg(test)]
mod tests {
    use hyphae_native_types::{
        CatalogVersion, Csn, DurabilityClass, EngineKind, ObjectId, PageId, TransactionId,
    };
    use hyphae_native_wal::WalBlock;

    use super::{Mutation, Opcode, TransactionPlan, encode_transaction, recover_commits};

    #[test]
    fn complete_transaction_round_trips_through_physical_wal()
    -> Result<(), Box<dyn std::error::Error>> {
        let transaction_id = TransactionId::new(1)?;
        let mutations = vec![
            Mutation {
                engine: EngineKind::Relational,
                opcode: Opcode::InsertRow,
                target: Some(ObjectId::new(1)?),
                key: b"pk".to_vec(),
                value: b"row".to_vec(),
                expires_at_micros: None,
            },
            Mutation {
                engine: EngineKind::Structure,
                opcode: Opcode::SetValue,
                target: None,
                key: b"key".to_vec(),
                value: b"value".to_vec(),
                expires_at_micros: Some(50),
            },
            Mutation {
                engine: EngineKind::Search,
                opcode: Opcode::IndexDocument,
                target: Some(ObjectId::new(2)?),
                key: b"doc".to_vec(),
                value: b"native search".to_vec(),
                expires_at_micros: None,
            },
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
        })?;
        let block = WalBlock::build(1, [0; 32], pending)?;
        let decoded = WalBlock::decode(1, [0; 32], &block.encode()?)?;
        let commits = recover_commits(decoded.records())?;
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].manifest.roots, roots);
        assert_eq!(commits[0].manifest.mutation_count, 3);
        Ok(())
    }
}
