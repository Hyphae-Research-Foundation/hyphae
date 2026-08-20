// SPDX-License-Identifier: Apache-2.0

//! Fail-closed import of one inspected Valkey/Redis RDB source into a
//! pending Native target, with sealed receipt evidence, deterministic
//! read-back verification at the pinned import time, and explicit
//! promotion.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use hyphae_native_catalog::CatalogObjectV2;
use hyphae_native_product::{
    CanonicalF64, CatalogName, LogicalCatalogObject, NativeProduct, ObjectId, ProductHashEntry,
    ProductListSide, ProductStructureKey, ProductStructureMutation, ProductStructureReadRequest,
    ProductStructureReadResult, ProductTtl, QualifiedName, StructureKind,
};
use hyphae_native_runtime::{
    ConstructClassification, EXTERNAL_MIGRATION_RECEIPT_KIND, EXTERNAL_MIGRATION_RECEIPT_VERSION,
    ExternalConsistencyPoint, ExternalMigrationReceipt, ExternalMigrationReceiptLimits,
    ExternalSourceIdentity, ExternalTargetState, FidelityClass, MappingDecision, OperatorWaiver,
    TargetKeyspace,
};

use crate::exit::CliFailure;

use super::rdb::{RdbReadLimits, RdbRecord, RdbValue};
use super::{ValkeyInventory, hex_encode, inspect_valkey_rdb};

/// Domain tag binding the logical digest to this migration path.
const LOGICAL_DIGEST_DOMAIN: &[u8] = b"hyphae-external-migration-logical-v1";
/// Maximum mutations per strict native import commit.
const IMPORT_BATCH_LIMIT: usize = 1024;
/// Honest statement of what the equivalence evidence covers.
const CONSISTENCY_STATEMENT: &str = "the destination corresponds to this RDB file at its \
                                     point-in-time capture, not to what clients last observed \
                                     on the live source";

/// Outcome of one completed pending import.
pub(crate) struct ValkeyImportOutcome {
    /// The sealed receipt written to the manifest path.
    pub receipt: ExternalMigrationReceipt,
    /// Keys imported into the pending target.
    pub imported_keys: u64,
    /// Keys skipped because they were already expired at import time.
    pub skipped_expired: u64,
}

/// Outcome of one offline verification.
pub(crate) struct ValkeyVerifyOutcome {
    /// The validated receipt.
    pub receipt: ExternalMigrationReceipt,
    /// Whether the target is still pending promotion.
    pub pending: bool,
}

type KeyspaceMap = BTreeMap<(u32, &'static str), (String, ObjectId)>;
type LiveRecords<'a> = Vec<(&'a RdbRecord, Option<i64>)>;

fn now_micros() -> Result<i64, CliFailure> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CliFailure::internal())?;
    i64::try_from(elapsed.as_micros()).map_err(|_| CliFailure::internal())
}

fn family_kind(family: &str) -> Result<StructureKind, CliFailure> {
    Ok(match family {
        "strings" => StructureKind::String,
        "lists" => StructureKind::List,
        "sets" => StructureKind::Set,
        "hashes" => StructureKind::Hash,
        "sorted_sets" => StructureKind::SortedSet,
        "streams" => StructureKind::Stream,
        _ => return Err(CliFailure::internal()),
    })
}

fn family_tag(family: &str) -> u8 {
    match family {
        "strings" => 0,
        "lists" => 1,
        "sets" => 2,
        "hashes" => 3,
        "sorted_sets" => 4,
        _ => 5,
    }
}

fn keyspace_name(db_index: u32, family: &str) -> String {
    format!("valkey_db{db_index}_{family}")
}

fn record_expiry_micros(record: &RdbRecord) -> Result<Option<i64>, CliFailure> {
    record
        .expires_at_ms
        .map(|ms| {
            i64::try_from(ms)
                .ok()
                .and_then(|ms| ms.checked_mul(1000))
                .ok_or_else(CliFailure::invalid)
        })
        .transpose()
}

fn push_bytes(payload: &mut Vec<u8>, bytes: &[u8]) {
    payload.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    payload.extend_from_slice(bytes);
}

fn push_count(payload: &mut Vec<u8>, count: usize) {
    payload.extend_from_slice(&(count as u64).to_le_bytes());
}

fn record_prefix(record: &RdbRecord, expiry: Option<i64>) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&record.db_index.to_le_bytes());
    payload.push(family_tag(record.value.family()));
    push_bytes(&mut payload, &record.key);
    match expiry {
        Some(micros) => {
            payload.push(1);
            payload.extend_from_slice(&micros.to_le_bytes());
        }
        None => payload.push(0),
    }
    payload
}

/// Serializes one live record into its canonical logical byte form: sets
/// ascending, hash fields ascending, sorted sets ascending by member with
/// canonical score bits, lists and stream entries in source order, stream
/// identifiers excluded because the import remaps them.
fn record_logical_bytes(record: &RdbRecord, expiry: Option<i64>) -> Vec<u8> {
    let mut payload = record_prefix(record, expiry);
    match &record.value {
        RdbValue::String(value) => push_bytes(&mut payload, value),
        RdbValue::List(members) => {
            push_count(&mut payload, members.len());
            for member in members {
                push_bytes(&mut payload, member);
            }
        }
        RdbValue::Set(members) => {
            let mut sorted = members.clone();
            sorted.sort();
            sorted.dedup();
            push_count(&mut payload, sorted.len());
            for member in &sorted {
                push_bytes(&mut payload, member);
            }
        }
        RdbValue::Hash(fields) => {
            let mut sorted = fields.clone();
            sorted.sort();
            push_count(&mut payload, sorted.len());
            for (field, value) in &sorted {
                push_bytes(&mut payload, field);
                push_bytes(&mut payload, value);
            }
        }
        RdbValue::SortedSet(entries) => {
            let mut sorted = entries.clone();
            sorted.sort_by(|left, right| left.0.cmp(&right.0));
            push_count(&mut payload, sorted.len());
            for (member, score) in &sorted {
                push_bytes(&mut payload, member);
                payload.extend_from_slice(&CanonicalF64::new(*score).get().to_bits().to_le_bytes());
            }
        }
        RdbValue::Stream(stream) => {
            push_count(&mut payload, stream.entries.len());
            for entry in &stream.entries {
                push_count(&mut payload, entry.fields.len());
                for (field, value) in &entry.fields {
                    push_bytes(&mut payload, field);
                    push_bytes(&mut payload, value);
                }
            }
        }
    }
    payload
}

fn logical_digest(chunks: &[Vec<u8>]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(LOGICAL_DIGEST_DOMAIN);
    hasher.update(&(chunks.len() as u64).to_le_bytes());
    for chunk in chunks {
        hasher.update(&(chunk.len() as u64).to_le_bytes());
        hasher.update(chunk);
    }
    hex_encode(hasher.finalize().as_bytes())
}

fn structure_key(keyspace: ObjectId, key: &[u8]) -> ProductStructureKey {
    ProductStructureKey {
        keyspace,
        key: key.to_vec(),
    }
}

fn collection_mutations(
    record: &RdbRecord,
    key: &ProductStructureKey,
    mutations: &mut Vec<ProductStructureMutation>,
) -> Result<(), CliFailure> {
    match &record.value {
        RdbValue::String(_) => return Err(CliFailure::internal()),
        RdbValue::List(members) => {
            for member in members {
                mutations.push(ProductStructureMutation::ListPush {
                    key: key.clone(),
                    side: ProductListSide::Right,
                    value: member.clone(),
                });
            }
        }
        RdbValue::Set(members) => {
            for member in members {
                mutations.push(ProductStructureMutation::SetAdd {
                    key: key.clone(),
                    member: member.clone(),
                });
            }
        }
        RdbValue::Hash(fields) => {
            for (field, value) in fields {
                mutations.push(ProductStructureMutation::HashSet {
                    key: key.clone(),
                    field: field.clone(),
                    value: value.clone(),
                });
            }
        }
        RdbValue::SortedSet(entries) => {
            for (member, score) in entries {
                mutations.push(ProductStructureMutation::SortedSetAdd {
                    key: key.clone(),
                    member: member.clone(),
                    score: CanonicalF64::new(*score),
                });
            }
        }
        RdbValue::Stream(stream) => {
            for entry in &stream.entries {
                if entry.fields.is_empty() {
                    return Err(CliFailure::invalid());
                }
                mutations.push(ProductStructureMutation::StreamAdd {
                    key: key.clone(),
                    fields: entry
                        .fields
                        .iter()
                        .map(|(field, value)| ProductHashEntry {
                            field: field.clone(),
                            value: value.clone(),
                        })
                        .collect(),
                });
            }
        }
    }
    Ok(())
}

/// Emits the ordered mutations for one live record: Create before members,
/// Expire last.
fn record_mutations(
    record: &RdbRecord,
    keyspace: ObjectId,
    expiry: Option<i64>,
) -> Result<Vec<ProductStructureMutation>, CliFailure> {
    let key = structure_key(keyspace, &record.key);
    if let RdbValue::String(value) = &record.value {
        return Ok(vec![ProductStructureMutation::StringSet {
            key,
            value: value.clone(),
            expires_at_micros: expiry,
        }]);
    }
    let family = family_kind(record.value.family())?;
    let mut mutations = vec![ProductStructureMutation::Create {
        key: key.clone(),
        family,
    }];
    collection_mutations(record, &key, &mut mutations)?;
    if let Some(micros) = expiry {
        mutations.push(ProductStructureMutation::Expire {
            key,
            family,
            expires_at_micros: micros,
        });
    }
    Ok(mutations)
}

fn read_one(
    product: &NativeProduct,
    import_time_micros: i64,
    request: ProductStructureReadRequest,
) -> Result<ProductStructureReadResult, CliFailure> {
    let mut results = product.migration_read_structures(import_time_micros, vec![request])?;
    results.pop().ok_or_else(CliFailure::internal)
}

fn read_ttl_bytes(
    product: &NativeProduct,
    import_time_micros: i64,
    key: ProductStructureKey,
    family: StructureKind,
    payload: &mut Vec<u8>,
) -> Result<(), CliFailure> {
    let ttl = read_one(
        product,
        import_time_micros,
        ProductStructureReadRequest::Ttl { key, family },
    )?;
    match ttl {
        ProductStructureReadResult::Ttl(ProductTtl::Persistent) => payload.push(0),
        ProductStructureReadResult::Ttl(ProductTtl::RemainingMicros(remaining)) => {
            payload.push(1);
            let absolute = import_time_micros
                .checked_add(remaining)
                .ok_or_else(CliFailure::invalid)?;
            payload.extend_from_slice(&absolute.to_le_bytes());
        }
        _ => return Err(CliFailure::invalid()),
    }
    Ok(())
}

fn read_back_value(
    product: &NativeProduct,
    import_time_micros: i64,
    record: &RdbRecord,
    keyspace: ObjectId,
    payload: &mut Vec<u8>,
) -> Result<(), CliFailure> {
    let key = structure_key(keyspace, &record.key);
    match &record.value {
        RdbValue::String(_) => {
            let ProductStructureReadResult::Value(Some(value)) = read_one(
                product,
                import_time_micros,
                ProductStructureReadRequest::StringGet { key },
            )?
            else {
                return Err(CliFailure::invalid());
            };
            push_bytes(payload, &value);
        }
        RdbValue::List(_) => {
            let ProductStructureReadResult::Values(values) = read_one(
                product,
                import_time_micros,
                ProductStructureReadRequest::ListRange {
                    key,
                    start: 0,
                    stop: -1,
                },
            )?
            else {
                return Err(CliFailure::invalid());
            };
            push_count(payload, values.len());
            for value in &values {
                push_bytes(payload, value);
            }
        }
        RdbValue::Set(members) => {
            let ProductStructureReadResult::Values(values) = read_one(
                product,
                import_time_micros,
                ProductStructureReadRequest::SetMembers {
                    key,
                    start_after: None,
                    limit: members.len().saturating_add(1),
                },
            )?
            else {
                return Err(CliFailure::invalid());
            };
            push_count(payload, values.len());
            for value in &values {
                push_bytes(payload, value);
            }
        }
        RdbValue::Hash(fields) => {
            let ProductStructureReadResult::HashEntries(entries) = read_one(
                product,
                import_time_micros,
                ProductStructureReadRequest::HashScan {
                    key,
                    start_after: None,
                    limit: fields.len().saturating_add(1),
                },
            )?
            else {
                return Err(CliFailure::invalid());
            };
            push_count(payload, entries.len());
            for entry in &entries {
                push_bytes(payload, &entry.field);
                push_bytes(payload, &entry.value);
            }
        }
        RdbValue::SortedSet(entries) => {
            read_back_sorted_set(
                product,
                import_time_micros,
                keyspace,
                record,
                entries,
                payload,
            )?;
        }
        RdbValue::Stream(stream) => {
            read_back_stream(
                product,
                import_time_micros,
                key,
                stream.entries.len(),
                payload,
            )?;
        }
    }
    Ok(())
}

fn read_back_stream(
    product: &NativeProduct,
    import_time_micros: i64,
    key: ProductStructureKey,
    entry_count: usize,
    payload: &mut Vec<u8>,
) -> Result<(), CliFailure> {
    let ProductStructureReadResult::StreamEntries(entries) = read_one(
        product,
        import_time_micros,
        ProductStructureReadRequest::StreamRange {
            key,
            start: 0,
            end: u64::MAX,
            limit: entry_count.saturating_add(1),
        },
    )?
    else {
        return Err(CliFailure::invalid());
    };
    push_count(payload, entries.len());
    for entry in &entries {
        push_count(payload, entry.fields.len());
        for field in &entry.fields {
            push_bytes(payload, &field.field);
            push_bytes(payload, &field.value);
        }
    }
    Ok(())
}

fn read_back_sorted_set(
    product: &NativeProduct,
    import_time_micros: i64,
    keyspace: ObjectId,
    record: &RdbRecord,
    entries: &[(Vec<u8>, f64)],
    payload: &mut Vec<u8>,
) -> Result<(), CliFailure> {
    let mut members = entries.iter().map(|(member, _)| member).collect::<Vec<_>>();
    members.sort();
    push_count(payload, members.len());
    for member in members {
        let ProductStructureReadResult::SortedSetScore(Some(score)) = read_one(
            product,
            import_time_micros,
            ProductStructureReadRequest::SortedSetScore {
                key: structure_key(keyspace, &record.key),
                member: member.clone(),
            },
        )?
        else {
            return Err(CliFailure::invalid());
        };
        push_bytes(payload, member);
        payload.extend_from_slice(&score.get().to_bits().to_le_bytes());
    }
    Ok(())
}

/// Rebuilds one record's canonical logical bytes purely from target reads at
/// the pinned import time.
fn read_back_record(
    product: &NativeProduct,
    import_time_micros: i64,
    record: &RdbRecord,
    keyspace: ObjectId,
) -> Result<Vec<u8>, CliFailure> {
    let family = family_kind(record.value.family())?;
    let mut payload = Vec::new();
    payload.extend_from_slice(&record.db_index.to_le_bytes());
    payload.push(family_tag(record.value.family()));
    push_bytes(&mut payload, &record.key);
    read_ttl_bytes(
        product,
        import_time_micros,
        structure_key(keyspace, &record.key),
        family,
        &mut payload,
    )?;
    read_back_value(product, import_time_micros, record, keyspace, &mut payload)?;
    Ok(payload)
}

/// Fails closed unless every required waiver was explicitly granted and
/// every granted waiver names a required construct.
fn check_waivers(inventory: &ValkeyInventory, waived: &[String]) -> Result<(), CliFailure> {
    for construct in waived {
        if !inventory.required_waivers.contains(construct) {
            eprintln!("waiver names no waivable construct: {construct}");
            return Err(CliFailure::invalid());
        }
    }
    let unwaived = inventory
        .required_waivers
        .iter()
        .filter(|construct| !waived.contains(construct))
        .collect::<Vec<_>>();
    if !unwaived.is_empty() {
        eprintln!("unwaived constructs: {unwaived:?}");
        return Err(CliFailure::invalid());
    }
    Ok(())
}

fn receipt_classifications(inventory: &ValkeyInventory) -> Vec<ConstructClassification> {
    inventory
        .classifications
        .iter()
        .map(|row| ConstructClassification {
            construct: row.construct.clone(),
            class: row.class,
            detail: row.detail.clone(),
            count: row.count,
        })
        .collect()
}

fn receipt_waivers(inventory: &ValkeyInventory) -> Vec<OperatorWaiver> {
    inventory
        .classifications
        .iter()
        .filter(|row| inventory.required_waivers.contains(&row.construct))
        .map(|row| OperatorWaiver {
            construct: row.construct.clone(),
            action: if row.class == FidelityClass::DeclaredDegraded {
                "degrade".to_owned()
            } else {
                "skip".to_owned()
            },
            keys_affected: row.count,
        })
        .collect()
}

fn receipt_mappings() -> Vec<MappingDecision> {
    vec![
        MappingDecision {
            construct: "numbered-databases".to_owned(),
            decision: "keyspace-per-database".to_owned(),
            detail: "each source database and family maps to one catalogued keyspace".to_owned(),
        },
        MappingDecision {
            construct: "streams".to_owned(),
            decision: "stream-id-remap".to_owned(),
            detail: "entry order and field maps are preserved; identifiers are remapped".to_owned(),
        },
        MappingDecision {
            construct: "strings".to_owned(),
            decision: "integer-decode".to_owned(),
            detail: "integer-encoded strings decode to decimal bytes".to_owned(),
        },
        MappingDecision {
            construct: "ttl".to_owned(),
            decision: "absolute-expiry-micros".to_owned(),
            detail: "absolute millisecond expiry converts to absolute microseconds; keys \
                     already expired at import time are skipped"
                .to_owned(),
        },
    ]
}

fn source_identity(inventory: &ValkeyInventory) -> ExternalSourceIdentity {
    ExternalSourceIdentity {
        kind: "valkey-rdb".to_owned(),
        format_version: inventory.version,
        source_digest: inventory.source_digest.clone(),
        source_bytes: inventory.source_bytes,
        source_checksum: inventory.source_checksum.clone(),
        aux_fields: inventory.aux_fields.clone(),
        database_count: inventory.database_count,
        key_count: inventory.key_count,
    }
}

/// Partitions live records and expiry state at one pinned import time.
fn live_records(
    inventory: &ValkeyInventory,
    import_time_micros: i64,
) -> Result<(LiveRecords<'_>, u64), CliFailure> {
    let mut live = Vec::new();
    let mut skipped = 0_u64;
    for record in &inventory.file.records {
        let expiry = record_expiry_micros(record)?;
        if expiry.is_some_and(|micros| micros <= import_time_micros) {
            skipped += 1;
            continue;
        }
        live.push((record, expiry));
    }
    Ok((live, skipped))
}

/// Creates one catalogued keyspace per (database, family) pair present among
/// the live records and returns the id map.
fn create_keyspaces(
    product: &mut NativeProduct,
    live: &[(&RdbRecord, Option<i64>)],
) -> Result<KeyspaceMap, CliFailure> {
    let mut pairs = BTreeSet::new();
    for (record, _) in live {
        pairs.insert((record.db_index, record.value.family()));
    }
    if pairs.is_empty() {
        return Err(CliFailure::invalid());
    }
    let requests = pairs
        .iter()
        .map(|(db, family)| Ok((keyspace_name(*db, family), family_kind(family)?)))
        .collect::<Result<Vec<_>, CliFailure>>()?;
    let created = product.migration_create_structure_keyspaces(&requests)?;
    if created.len() != pairs.len() {
        return Err(CliFailure::internal());
    }
    Ok(pairs.into_iter().zip(created).collect())
}

/// Stores every live record in bounded strict batches and rebuilds the
/// canonical logical bytes from target reads, failing on any divergence.
fn import_live_records(
    product: &mut NativeProduct,
    live: &[(&RdbRecord, Option<i64>)],
    import_time_micros: i64,
) -> Result<(KeyspaceMap, Vec<Vec<u8>>), CliFailure> {
    let keyspaces = create_keyspaces(product, live)?;
    let mut buffer: Vec<ProductStructureMutation> = Vec::new();
    for (record, expiry) in live {
        let (_, keyspace) = keyspaces
            .get(&(record.db_index, record.value.family()))
            .ok_or_else(CliFailure::internal)?;
        for mutation in record_mutations(record, *keyspace, *expiry)? {
            if buffer.len() == IMPORT_BATCH_LIMIT {
                product.migration_store_structures(std::mem::take(&mut buffer))?;
            }
            buffer.push(mutation);
        }
    }
    if !buffer.is_empty() {
        product.migration_store_structures(buffer)?;
    }
    let mut target_chunks = Vec::with_capacity(live.len());
    for (record, expiry) in live {
        let (_, keyspace) = keyspaces
            .get(&(record.db_index, record.value.family()))
            .ok_or_else(CliFailure::internal)?;
        let source_bytes = record_logical_bytes(record, *expiry);
        let target_bytes = read_back_record(product, import_time_micros, record, *keyspace)?;
        if source_bytes != target_bytes {
            eprintln!(
                "read-back verification diverged for key {:?}",
                String::from_utf8_lossy(&record.key)
            );
            return Err(CliFailure::invalid());
        }
        target_chunks.push(target_bytes);
    }
    Ok((keyspaces, target_chunks))
}

fn build_receipt(
    inventory: &ValkeyInventory,
    product: &NativeProduct,
    import_time_micros: i64,
    keyspaces: &KeyspaceMap,
    live: &[(&RdbRecord, Option<i64>)],
    logical_digest: String,
) -> ExternalMigrationReceipt {
    let mut entry_counts: BTreeMap<(u32, &'static str), u64> = BTreeMap::new();
    for (record, _) in live {
        *entry_counts
            .entry((record.db_index, record.value.family()))
            .or_insert(0) += 1;
    }
    let mut target_keyspaces = keyspaces
        .iter()
        .map(|(pair, (name, id))| TargetKeyspace {
            name: format!("main.public.{name}"),
            object_id: id.get().to_string(),
            family: pair.1.to_owned(),
            entry_count: entry_counts.get(pair).copied().unwrap_or(0),
        })
        .collect::<Vec<_>>();
    target_keyspaces.sort();
    let identity = product.directory_identity();
    ExternalMigrationReceipt {
        version: EXTERNAL_MIGRATION_RECEIPT_VERSION,
        kind: EXTERNAL_MIGRATION_RECEIPT_KIND.to_owned(),
        source: source_identity(inventory),
        consistency: ExternalConsistencyPoint {
            kind: "rdb-file-point-in-time".to_owned(),
            statement: CONSISTENCY_STATEMENT.to_owned(),
            clock_skew_micros: None,
        },
        import_time_micros,
        classifications: receipt_classifications(inventory),
        waivers: receipt_waivers(inventory),
        mappings: receipt_mappings(),
        target: ExternalTargetState {
            directory_id: identity.directory_id().to_owned(),
            history_epoch: identity.history_epoch(),
            keyspaces: target_keyspaces,
            logical_digest,
        },
        content_digest: String::new(),
    }
}

fn abort_pending(product: NativeProduct, target: &Path, error: CliFailure) -> CliFailure {
    drop(product);
    let _ignored = fs::remove_dir_all(target);
    error
}

/// Imports one inspected RDB source into a fresh pending target, verifies it
/// value by value at the pinned import time, and writes the sealed receipt.
pub(crate) fn run_valkey_rdb(
    source: &Path,
    target: &Path,
    manifest: &Path,
    waived: &[String],
) -> Result<ValkeyImportOutcome, CliFailure> {
    let inventory = inspect_valkey_rdb(source, &RdbReadLimits::default()).map_err(|error| {
        eprintln!("valkey-rdb inspection failed: {error}");
        CliFailure::from(error)
    })?;
    check_waivers(&inventory, waived)?;
    let import_time_micros = now_micros()?;
    let (live, skipped_expired) = live_records(&inventory, import_time_micros)?;
    if live.is_empty() {
        eprintln!("the source carries no live keys at import time");
        return Err(CliFailure::invalid());
    }
    let mut product = NativeProduct::create_pending(target)?;
    let (keyspaces, target_chunks) =
        match import_live_records(&mut product, &live, import_time_micros) {
            Ok(state) => state,
            Err(error) => return Err(abort_pending(product, target, error)),
        };
    let digest = logical_digest(&target_chunks);
    let mut receipt = build_receipt(
        &inventory,
        &product,
        import_time_micros,
        &keyspaces,
        &live,
        digest,
    );
    if receipt.seal().is_err() {
        return Err(abort_pending(product, target, CliFailure::internal()));
    }
    let encoded = match receipt.encode() {
        Ok(encoded) => encoded,
        Err(error) => {
            eprintln!("receipt validation failed: {error}");
            return Err(abort_pending(product, target, CliFailure::internal()));
        }
    };
    if let Err(error) = crate::write_new_file(manifest, &encoded) {
        return Err(abort_pending(product, target, error));
    }
    Ok(ValkeyImportOutcome {
        receipt,
        imported_keys: live.len() as u64,
        skipped_expired,
    })
}

/// Resolves and validates every receipt keyspace against the live pairs and
/// the target catalog; missing or extra keyspaces fail closed.
fn resolve_receipt_keyspaces(
    product: &NativeProduct,
    receipt: &ExternalMigrationReceipt,
    live: &[(&RdbRecord, Option<i64>)],
) -> Result<KeyspaceMap, CliFailure> {
    let mut expected: BTreeMap<String, (u32, &'static str)> = BTreeMap::new();
    let mut entry_counts: BTreeMap<(u32, &'static str), u64> = BTreeMap::new();
    for (record, _) in live {
        let pair = (record.db_index, record.value.family());
        expected.insert(
            format!("main.public.{}", keyspace_name(pair.0, pair.1)),
            pair,
        );
        *entry_counts.entry(pair).or_insert(0) += 1;
    }
    if expected.len() != receipt.target.keyspaces.len() {
        eprintln!("receipt keyspaces differ from the live source keyspaces");
        return Err(CliFailure::invalid());
    }
    let mut resolved = KeyspaceMap::new();
    for keyspace in &receipt.target.keyspaces {
        let Some(pair) = expected.get(&keyspace.name) else {
            eprintln!("receipt names an unexpected keyspace: {}", keyspace.name);
            return Err(CliFailure::invalid());
        };
        if keyspace.family != pair.1
            || keyspace.entry_count != entry_counts.get(pair).copied().unwrap_or(0)
        {
            eprintln!("receipt keyspace metadata differs: {}", keyspace.name);
            return Err(CliFailure::invalid());
        }
        let id = keyspace
            .object_id
            .parse::<u128>()
            .ok()
            .and_then(|value| ObjectId::new(value).ok())
            .ok_or_else(CliFailure::invalid)?;
        let expected_name = QualifiedName::new(
            CatalogName::unquoted("main").map_err(|_| CliFailure::internal())?,
            CatalogName::unquoted("public").map_err(|_| CliFailure::internal())?,
            CatalogName::unquoted(keyspace_name(pair.0, pair.1))
                .map_err(|_| CliFailure::internal())?,
        );
        let snapshot = product.catalog_snapshot()?;
        let Some(LogicalCatalogObject::V2(CatalogObjectV2::Keyspace(definition))) =
            product.catalog_resolve(&snapshot, &expected_name)?
        else {
            eprintln!("receipt keyspace is not catalogued: {}", keyspace.name);
            return Err(CliFailure::invalid());
        };
        if definition.header.id != id
            || definition.header.name != expected_name
            || definition.kind != family_kind(pair.1)?
        {
            eprintln!("catalogued keyspace differs: {}", keyspace.name);
            return Err(CliFailure::invalid());
        }
        resolved.insert(*pair, (keyspace_name(pair.0, pair.1), id));
    }
    Ok(resolved)
}

/// Verifies one sealed receipt against the untouched source and the pending
/// or promoted target, recomputing the logical digest from both sides.
pub(crate) fn verify_valkey_rdb(
    source: &Path,
    target: &Path,
    manifest: &Path,
) -> Result<ValkeyVerifyOutcome, CliFailure> {
    let inventory = inspect_valkey_rdb(source, &RdbReadLimits::default()).map_err(|error| {
        eprintln!("valkey-rdb inspection failed: {error}");
        CliFailure::from(error)
    })?;
    let encoded = fs::read(manifest)?;
    let receipt =
        ExternalMigrationReceipt::decode(&encoded, &ExternalMigrationReceiptLimits::default())
            .map_err(|error| {
                eprintln!("receipt validation failed: {error}");
                CliFailure::invalid()
            })?;
    if receipt.source != source_identity(&inventory) {
        eprintln!("receipt source identity differs from the inspected source");
        return Err(CliFailure::invalid());
    }
    let pending = target
        .join("FORMAT.pending")
        .try_exists()
        .map_err(|_| CliFailure::io())?;
    let product = if pending {
        NativeProduct::open_pending(target)?
    } else {
        NativeProduct::open(target)?
    };
    let identity = product.directory_identity();
    if receipt.target.directory_id != identity.directory_id()
        || receipt.target.history_epoch != identity.history_epoch()
    {
        eprintln!("receipt target identity differs from the target directory");
        return Err(CliFailure::invalid());
    }
    let (live, _skipped) = live_records(&inventory, receipt.import_time_micros)?;
    let keyspaces = resolve_receipt_keyspaces(&product, &receipt, &live)?;
    let mut source_chunks = Vec::with_capacity(live.len());
    let mut target_chunks = Vec::with_capacity(live.len());
    for (record, expiry) in &live {
        let (_, keyspace) = keyspaces
            .get(&(record.db_index, record.value.family()))
            .ok_or_else(CliFailure::invalid)?;
        source_chunks.push(record_logical_bytes(record, *expiry));
        target_chunks.push(read_back_record(
            &product,
            receipt.import_time_micros,
            record,
            *keyspace,
        )?);
    }
    if source_chunks != target_chunks {
        eprintln!("target values differ from the source values");
        return Err(CliFailure::invalid());
    }
    let source_digest = logical_digest(&source_chunks);
    let target_digest = logical_digest(&target_chunks);
    if source_digest != receipt.target.logical_digest
        || target_digest != receipt.target.logical_digest
    {
        eprintln!("logical digest differs between the source, target, and receipt");
        return Err(CliFailure::invalid());
    }
    Ok(ValkeyVerifyOutcome { receipt, pending })
}

/// Verifies then promotes one pending target.
pub(crate) fn promote_valkey_rdb(
    source: &Path,
    target: &Path,
    manifest: &Path,
) -> Result<ExternalMigrationReceipt, CliFailure> {
    let outcome = verify_valkey_rdb(source, target, manifest)?;
    if !outcome.pending {
        eprintln!("the target is already promoted");
        return Err(CliFailure::invalid());
    }
    let mut product = NativeProduct::open_pending(target)?;
    product.promote_pending()?;
    Ok(outcome.receipt)
}
