// SPDX-License-Identifier: Apache-2.0

//! Offline Valkey/Redis migration source support.
//!
//! The module classifies every construct an RDB file carries into the four
//! external-migration fidelity classes, reports the waivers a migration
//! would require, and exposes the parsed records to the importer. Inspection
//! never mutates anything.

pub(crate) mod crc64;
pub(crate) mod import;
pub(crate) mod lzf;
pub(crate) mod rdb;

use std::collections::BTreeMap;
use std::path::Path;

use hyphae_native_runtime::FidelityClass;

use rdb::{RdbFile, RdbReadLimits};

/// One classified construct in an inspection inventory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InventoryClassification {
    /// Stable construct identifier.
    pub construct: String,
    /// Assigned fidelity class.
    pub class: FidelityClass,
    /// Human-readable detail.
    pub detail: String,
    /// Number of source items covered.
    pub count: u64,
}

/// Complete inspection inventory for one RDB source.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ValkeyInventory {
    /// Declared RDB format version.
    pub version: u32,
    /// Lowercase BLAKE3 hex digest of the complete file bytes.
    pub source_digest: String,
    /// Exact file length in bytes.
    pub source_bytes: u64,
    /// Verified trailer checksum as lowercase hex, when present.
    pub source_checksum: Option<String>,
    /// Auxiliary fields in strict ascending order.
    pub aux_fields: Vec<(String, String)>,
    /// Number of logical databases with at least one record.
    pub database_count: u32,
    /// Number of keys before expiry filtering.
    pub key_count: u64,
    /// Per-database, per-family record counts.
    pub family_counts: BTreeMap<u32, BTreeMap<&'static str, u64>>,
    /// Every classification in strict ascending construct order.
    pub classifications: Vec<InventoryClassification>,
    /// Constructs that require an explicit operator waiver before a run.
    pub required_waivers: Vec<String>,
    /// The parsed file, retained for the importer.
    pub file: RdbFile,
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        encoded.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    encoded
}

fn hex_digest(bytes: &[u8]) -> String {
    hex_encode(blake3::hash(bytes).as_bytes())
}

fn push_classification(
    classifications: &mut Vec<InventoryClassification>,
    construct: &str,
    class: FidelityClass,
    detail: &str,
    count: u64,
) {
    if count > 0 {
        classifications.push(InventoryClassification {
            construct: construct.to_owned(),
            class,
            detail: detail.to_owned(),
            count,
        });
    }
}

fn family_detail(family: &'static str) -> &'static str {
    match family {
        "strings" => "raw, integer, and LZF encodings decode to exact byte values",
        "hashes" => "field/value pairs migrate byte-exactly",
        "lists" => "member order and bytes are preserved",
        "sets" => "members migrate byte-exactly; intset integers decode to decimal bytes",
        "sorted_sets" => "members and canonical finite scores are preserved",
        _ => {
            "entry order and field maps are preserved; identifiers are remapped and consumer groups are dropped"
        }
    }
}

fn classify_encountered(
    classifications: &mut Vec<InventoryClassification>,
    encountered: &BTreeMap<&'static str, u64>,
) {
    for (construct, count) in encountered {
        let (class, detail) = match *construct {
            "checksum-absent" => (
                FidelityClass::Rejected,
                "the RDB trailer carries no checksum, so file integrity is unverifiable",
            ),
            "stream-consumer-groups" => (
                FidelityClass::DeclaredDegraded,
                "consumer groups and pending entries are dropped",
            ),
            "functions" => (
                FidelityClass::Rejected,
                "server-side functions have no mapping",
            ),
            "cluster-slot-info" => (
                FidelityClass::Rejected,
                "cluster metadata is outside the product boundary",
            ),
            "zipmap" => (
                FidelityClass::Rejected,
                "the pre-2.6 zipmap encoding is not decoded",
            ),
            _ => (FidelityClass::Rejected, "no mapping exists"),
        };
        push_classification(classifications, construct, class, detail, *count);
    }
}

/// Builds the classified inventory for one parsed RDB file.
pub(crate) fn build_inventory(bytes: &[u8], file: RdbFile) -> ValkeyInventory {
    let mut family_totals: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut family_counts: BTreeMap<u32, BTreeMap<&'static str, u64>> = BTreeMap::new();
    let mut ttl_count = 0_u64;
    for record in &file.records {
        let family = record.value.family();
        *family_totals.entry(family).or_insert(0) += 1;
        *family_counts
            .entry(record.db_index)
            .or_default()
            .entry(family)
            .or_insert(0) += 1;
        if record.expires_at_ms.is_some() {
            ttl_count += 1;
        }
    }
    let mut classifications = Vec::new();
    for (family, count) in &family_totals {
        let class = if *family == "streams" {
            FidelityClass::DeclaredDegraded
        } else {
            FidelityClass::Exact
        };
        push_classification(
            &mut classifications,
            family,
            class,
            family_detail(family),
            *count,
        );
    }
    push_classification(
        &mut classifications,
        "ttl",
        FidelityClass::Equivalent,
        "expiry migrates as an absolute instant; keys already expired at import are skipped",
        ttl_count,
    );
    let database_count = u32::try_from(family_counts.len()).unwrap_or(u32::MAX);
    if database_count > 1 {
        push_classification(
            &mut classifications,
            "numbered-databases",
            FidelityClass::Equivalent,
            "each source database maps to its own keyspace namespace",
            u64::from(database_count),
        );
    }
    classify_encountered(&mut classifications, &file.encountered);
    classifications.sort_by(|left, right| left.construct.cmp(&right.construct));
    classifications.dedup_by(|left, right| left.construct == right.construct);
    let required_waivers = classifications
        .iter()
        .filter(|classification| {
            matches!(
                classification.class,
                FidelityClass::DeclaredDegraded | FidelityClass::Rejected
            ) && classification.count > 0
        })
        .map(|classification| classification.construct.clone())
        .collect();
    let mut aux_fields = file.aux_fields.clone();
    aux_fields.sort();
    aux_fields.dedup();
    ValkeyInventory {
        version: file.version,
        source_digest: hex_digest(bytes),
        source_bytes: bytes.len() as u64,
        source_checksum: file
            .checksum_present
            .then(|| format!("{:016x}", file.checksum)),
        aux_fields,
        database_count,
        key_count: file.records.len() as u64,
        family_counts,
        classifications,
        required_waivers,
        file,
    }
}

/// Parses and classifies one RDB file without mutating anything.
pub(crate) fn inspect_valkey_rdb(
    path: &Path,
    limits: &RdbReadLimits,
) -> Result<ValkeyInventory, rdb::RdbError> {
    let metadata = std::fs::metadata(path).map_err(|_| rdb::RdbError::Truncated { offset: 0 })?;
    if metadata.len() > limits.max_file_bytes {
        return Err(rdb::RdbError::Limit {
            field: "file_bytes",
            maximum: limits.max_file_bytes,
        });
    }
    let bytes = std::fs::read(path).map_err(|_| rdb::RdbError::Truncated { offset: 0 })?;
    let file = rdb::parse(&bytes, limits)?;
    Ok(build_inventory(&bytes, file))
}

/// Renders one inventory as the stable inspection JSON document.
pub(crate) fn inventory_json(inventory: &ValkeyInventory) -> serde_json::Value {
    serde_json::json!({
        "status": "inspected",
        "source_kind": "valkey-rdb",
        "rdb_version": inventory.version,
        "source_digest": inventory.source_digest,
        "source_bytes": inventory.source_bytes,
        "source_checksum": inventory.source_checksum,
        "aux_fields": inventory
            .aux_fields
            .iter()
            .map(|(name, value)| serde_json::json!({"name": name, "value": value}))
            .collect::<Vec<_>>(),
        "database_count": inventory.database_count,
        "key_count": inventory.key_count,
        "databases": inventory
            .family_counts
            .iter()
            .map(|(db_index, families)| {
                serde_json::json!({
                    "index": db_index,
                    "families": families
                        .iter()
                        .map(|(family, count)| serde_json::json!({
                            "family": family,
                            "count": count,
                        }))
                        .collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>(),
        "classifications": inventory
            .classifications
            .iter()
            .map(|classification| serde_json::json!({
                "construct": classification.construct,
                "class": match classification.class {
                    FidelityClass::Exact => "exact",
                    FidelityClass::Equivalent => "equivalent",
                    FidelityClass::DeclaredDegraded => "declared-degraded",
                    FidelityClass::Rejected => "rejected",
                },
                "detail": classification.detail,
                "count": classification.count,
            }))
            .collect::<Vec<_>>(),
        "required_waivers": inventory.required_waivers,
    })
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
pub(crate) mod test_support {
    //! Deterministic RDB byte construction for tests and fixtures.

    /// Incrementally builds one RDB payload.
    pub(crate) struct RdbBuilder {
        bytes: Vec<u8>,
    }

    impl RdbBuilder {
        /// Starts one payload at the given RDB version.
        pub(crate) fn new(version: u32) -> Self {
            let mut bytes = b"REDIS".to_vec();
            bytes.extend_from_slice(format!("{version:04}").as_bytes());
            Self { bytes }
        }

        /// Appends one raw length prefix.
        pub(crate) fn length(&mut self, value: u64) -> &mut Self {
            if value < 64 {
                self.bytes.push(u8::try_from(value).unwrap_or(0));
            } else if value < 16_384 {
                self.bytes
                    .push(0x40 | u8::try_from(value >> 8).unwrap_or(0));
                self.bytes.push(u8::try_from(value & 0xff).unwrap_or(0));
            } else {
                self.bytes.push(0x81);
                self.bytes.extend_from_slice(&value.to_be_bytes());
            }
            self
        }

        /// Appends one raw string.
        pub(crate) fn string(&mut self, value: &[u8]) -> &mut Self {
            self.length(value.len() as u64);
            self.bytes.extend_from_slice(value);
            self
        }

        /// Appends one SELECTDB opcode.
        pub(crate) fn select_db(&mut self, index: u64) -> &mut Self {
            self.bytes.push(0xfe);
            self.length(index)
        }

        /// Appends one auxiliary field.
        pub(crate) fn aux(&mut self, name: &[u8], value: &[u8]) -> &mut Self {
            self.bytes.push(0xfa);
            self.string(name);
            self.string(value)
        }

        /// Appends one absolute millisecond expiry for the next record.
        pub(crate) fn expire_ms(&mut self, at: u64) -> &mut Self {
            self.bytes.push(0xfc);
            self.bytes.extend_from_slice(&at.to_le_bytes());
            self
        }

        /// Appends one raw string record.
        pub(crate) fn string_record(&mut self, key: &[u8], value: &[u8]) -> &mut Self {
            self.bytes.push(0);
            self.string(key);
            self.string(value)
        }

        /// Appends one integer-encoded string record.
        pub(crate) fn int_string_record(&mut self, key: &[u8], value: i32) -> &mut Self {
            self.bytes.push(0);
            self.string(key);
            self.bytes.push(0xc2);
            self.bytes.extend_from_slice(&value.to_le_bytes());
            self
        }

        /// Appends one plain hashtable hash record.
        pub(crate) fn hash_record(&mut self, key: &[u8], entries: &[(&[u8], &[u8])]) -> &mut Self {
            self.bytes.push(4);
            self.string(key);
            self.length(entries.len() as u64);
            for (field, value) in entries {
                self.string(field);
                self.string(value);
            }
            self
        }

        /// Appends one listpack-encoded set record.
        pub(crate) fn set_listpack_record(&mut self, key: &[u8], members: &[&[u8]]) -> &mut Self {
            self.bytes.push(20);
            self.string(key);
            let payload = listpack(members);
            self.string(&payload)
        }

        /// Appends one quicklist2 list record with one packed node.
        pub(crate) fn list_quicklist2_record(
            &mut self,
            key: &[u8],
            members: &[&[u8]],
        ) -> &mut Self {
            self.bytes.push(18);
            self.string(key);
            self.length(1);
            self.length(2);
            let payload = listpack(members);
            self.string(&payload)
        }

        /// Appends one binary-double sorted-set record.
        pub(crate) fn zset2_record(&mut self, key: &[u8], members: &[(&[u8], f64)]) -> &mut Self {
            self.bytes.push(5);
            self.string(key);
            self.length(members.len() as u64);
            for (member, score) in members {
                self.string(member);
                self.bytes.extend_from_slice(&score.to_le_bytes());
            }
            self
        }

        /// Appends one intset set record.
        pub(crate) fn set_intset_record(&mut self, key: &[u8], members: &[i32]) -> &mut Self {
            self.bytes.push(11);
            self.string(key);
            let mut payload = Vec::new();
            payload.extend_from_slice(&4_u32.to_le_bytes());
            payload.extend_from_slice(&(members.len() as u32).to_le_bytes());
            for member in members {
                payload.extend_from_slice(&member.to_le_bytes());
            }
            self.string(&payload)
        }

        /// Appends one single-entry stream record with one consumer group.
        pub(crate) fn stream_record(
            &mut self,
            key: &[u8],
            id_ms: u64,
            fields: &[(&[u8], &[u8])],
            group: Option<&[u8]>,
        ) -> &mut Self {
            self.bytes.push(21);
            self.string(key);
            self.length(1);
            let mut master_key = Vec::new();
            master_key.extend_from_slice(&id_ms.to_be_bytes());
            master_key.extend_from_slice(&0_u64.to_be_bytes());
            self.string(&master_key);
            let mut elements: Vec<Vec<u8>> = vec![
                b"1".to_vec(),
                b"0".to_vec(),
                (fields.len() as u64).to_string().into_bytes(),
            ];
            for (name, _) in fields {
                elements.push((*name).to_vec());
            }
            elements.push(b"0".to_vec());
            elements.push(b"2".to_vec());
            elements.push(b"0".to_vec());
            elements.push(b"0".to_vec());
            for (_, value) in fields {
                elements.push((*value).to_vec());
            }
            elements.push(b"0".to_vec());
            let owned: Vec<&[u8]> = elements.iter().map(Vec::as_slice).collect();
            let payload = listpack(&owned);
            self.string(&payload);
            self.length(1);
            self.length(id_ms);
            self.length(0);
            self.length(id_ms);
            self.length(0);
            self.length(0);
            self.length(0);
            self.length(1);
            match group {
                Some(name) => {
                    self.length(1);
                    self.string(name);
                    self.length(id_ms);
                    self.length(0);
                    self.length(0);
                    self.length(0);
                    self.length(0);
                }
                None => {
                    self.length(0);
                }
            }
            self
        }

        /// Terminates the payload with a valid CRC-64 trailer.
        pub(crate) fn finish(mut self) -> Vec<u8> {
            self.bytes.push(0xff);
            let checksum = super::crc64::update(0, &self.bytes);
            self.bytes.extend_from_slice(&checksum.to_le_bytes());
            self.bytes
        }

        /// Terminates the payload with a zeroed checksum trailer.
        pub(crate) fn finish_without_checksum(mut self) -> Vec<u8> {
            self.bytes.push(0xff);
            self.bytes.extend_from_slice(&0_u64.to_le_bytes());
            self.bytes
        }
    }

    /// Encodes one listpack payload of raw string elements.
    pub(crate) fn listpack(elements: &[&[u8]]) -> Vec<u8> {
        let mut body = Vec::new();
        for element in elements {
            let start = body.len();
            if element.len() < 64 {
                body.push(0x80 | u8::try_from(element.len()).unwrap_or(0));
                body.extend_from_slice(element);
            } else {
                body.push(0xf0);
                body.extend_from_slice(&(element.len() as u32).to_le_bytes());
                body.extend_from_slice(element);
            }
            let consumed = body.len() - start;
            if consumed < 128 {
                body.push(u8::try_from(consumed).unwrap_or(0));
            } else {
                body.push(u8::try_from(consumed & 0x7f).unwrap_or(0) | 0x80);
                body.push(u8::try_from(consumed >> 7).unwrap_or(0));
            }
        }
        body.push(0xff);
        let mut payload = Vec::with_capacity(body.len() + 6);
        payload.extend_from_slice(&((body.len() + 6) as u32).to_le_bytes());
        payload.extend_from_slice(&(elements.len() as u16).to_le_bytes());
        payload.extend_from_slice(&body);
        payload
    }
}

#[cfg(test)]
mod tests {
    use super::rdb::{self, RdbError, RdbReadLimits, RdbValue};
    use super::test_support::RdbBuilder;
    use super::{FidelityClass, build_inventory};

    fn sample() -> Vec<u8> {
        let mut builder = RdbBuilder::new(11);
        builder
            .aux(b"redis-ver", b"7.2.5")
            .select_db(0)
            .string_record(b"greeting", b"hola")
            .int_string_record(b"answer", 42)
            .expire_ms(4_102_444_800_000)
            .string_record(b"session", b"active")
            .hash_record(
                b"note:1",
                &[(b"author", b"mario"), (b"state", b"published")],
            )
            .set_listpack_record(b"tags", &[b"alpha", b"beta"])
            .list_quicklist2_record(b"queue", &[b"first", b"second", b"third"])
            .zset2_record(b"ranking", &[(b"note:1", 9.5)])
            .set_intset_record(b"codes", &[7, 11])
            .stream_record(
                b"events",
                1_700_000_000_000,
                &[(b"kind", b"created")],
                Some(b"workers"),
            )
            .select_db(1)
            .string_record(b"other", b"db");
        builder.finish()
    }

    #[test]
    fn sample_parses_with_verified_checksum() -> Result<(), RdbError> {
        let bytes = sample();
        let file = rdb::parse(&bytes, &RdbReadLimits::default())?;
        assert_eq!(file.version, 11);
        assert!(file.checksum_present);
        assert_eq!(file.records.len(), 10);
        assert_eq!(file.records[0].value, RdbValue::String(b"hola".to_vec()));
        assert_eq!(file.records[1].value, RdbValue::String(b"42".to_vec()));
        assert_eq!(file.records[2].expires_at_ms, Some(4_102_444_800_000));
        assert_eq!(
            file.records[3].value,
            RdbValue::Hash(vec![
                (b"author".to_vec(), b"mario".to_vec()),
                (b"state".to_vec(), b"published".to_vec()),
            ])
        );
        assert_eq!(
            file.records[5].value,
            RdbValue::List(vec![
                b"first".to_vec(),
                b"second".to_vec(),
                b"third".to_vec()
            ])
        );
        assert_eq!(
            file.records[7].value,
            RdbValue::Set(vec![b"7".to_vec(), b"11".to_vec()])
        );
        let RdbValue::Stream(stream) = &file.records[8].value else {
            return Err(RdbError::Encoding("stream record is missing"));
        };
        assert_eq!(stream.entries.len(), 1);
        assert_eq!(stream.entries[0].id_ms, 1_700_000_000_000);
        assert_eq!(
            stream.entries[0].fields,
            vec![(b"kind".to_vec(), b"created".to_vec())]
        );
        assert_eq!(stream.group_count, 1);
        assert_eq!(file.records[8].db_index, 0);
        Ok(())
    }

    #[test]
    fn inventory_classifies_and_requires_waivers() -> Result<(), RdbError> {
        let bytes = sample();
        let file = rdb::parse(&bytes, &RdbReadLimits::default())?;
        let inventory = build_inventory(&bytes, file);
        assert_eq!(inventory.database_count, 2);
        assert_eq!(inventory.key_count, 10);
        assert!(
            inventory
                .classifications
                .iter()
                .any(|row| row.construct == "strings" && row.class == FidelityClass::Exact)
        );
        assert!(
            inventory
                .classifications
                .iter()
                .any(|row| row.construct == "ttl" && row.class == FidelityClass::Equivalent)
        );
        assert!(inventory.required_waivers.contains(&"streams".to_owned()));
        assert!(
            inventory
                .required_waivers
                .contains(&"stream-consumer-groups".to_owned())
        );
        assert!(!inventory.required_waivers.contains(&"strings".to_owned()));
        Ok(())
    }

    #[test]
    fn zeroed_checksum_is_a_waivable_construct() -> Result<(), RdbError> {
        let mut builder = RdbBuilder::new(11);
        builder.select_db(0).string_record(b"k", b"v");
        let bytes = builder.finish_without_checksum();
        let file = rdb::parse(&bytes, &RdbReadLimits::default())?;
        assert!(!file.checksum_present);
        let inventory = build_inventory(&bytes, file);
        assert!(
            inventory
                .required_waivers
                .contains(&"checksum-absent".to_owned())
        );
        Ok(())
    }

    #[test]
    fn corrupted_checksum_fails_closed() {
        let mut bytes = sample();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x55;
        assert!(matches!(
            rdb::parse(&bytes, &RdbReadLimits::default()),
            Err(RdbError::Checksum { .. })
        ));
    }

    #[test]
    fn truncation_at_every_byte_fails_closed() {
        let bytes = sample();
        for cut in 1..bytes.len() {
            assert!(
                rdb::parse(&bytes[..cut], &RdbReadLimits::default()).is_err(),
                "truncation at {cut} parsed successfully"
            );
        }
    }

    #[test]
    fn unknown_value_types_and_versions_fail_closed() {
        let mut builder = RdbBuilder::new(11);
        builder.select_db(0);
        let template = builder.finish();
        // Rewrite the trailer into an unknown opcode (>= 0x80).
        let mut bytes = template.clone();
        bytes.truncate(bytes.len() - 9);
        bytes.push(200);
        assert!(matches!(
            rdb::parse(&bytes, &RdbReadLimits::default()),
            Err(RdbError::UnknownOpcode { opcode: 200, .. })
        ));

        // Rewrite the trailer into an unknown value type (< 0x80).
        let mut bytes = template;
        bytes.truncate(bytes.len() - 9);
        bytes.push(60);
        bytes.push(1);
        bytes.push(b'k');
        assert!(matches!(
            rdb::parse(&bytes, &RdbReadLimits::default()),
            Err(RdbError::UnknownValueType { value_type: 60, .. })
        ));

        let old = RdbBuilder::new(7).finish();
        assert!(matches!(
            rdb::parse(&old, &RdbReadLimits::default()),
            Err(RdbError::Version { found: 7 })
        ));
    }

    #[test]
    fn limits_fail_closed() {
        let bytes = sample();
        let limits = RdbReadLimits {
            max_total_keys: 2,
            ..RdbReadLimits::default()
        };
        assert!(matches!(
            rdb::parse(&bytes, &limits),
            Err(RdbError::Limit {
                field: "total_keys",
                ..
            })
        ));
    }
}
