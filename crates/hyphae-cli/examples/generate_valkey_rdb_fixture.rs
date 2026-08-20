// SPDX-License-Identifier: Apache-2.0

//! Generates the immutable Valkey/Redis RDB v11 compatibility fixture.
//!
//! The fixture is one deterministic RDB payload exercising every decoded
//! encoding: raw and integer strings, a hashtable hash, a listpack set, an
//! intset set, a quicklist2 list, a binary-double sorted set, and a stream
//! with one consumer group, across two databases with one absolute expiry.
//! The builder here is deliberately self-contained so the fixture bytes stay
//! independent of the parser under test, and the embedded test proves the
//! checked-in fixture is byte-exact forever.

use std::env;
use std::error::Error;
use std::fs;

use serde_json::json;

const RDB_VERSION: u32 = 11;
const FAR_FUTURE_EXPIRY_MS: u64 = 4_102_444_800_000;
const STREAM_ID_MS: u64 = 1_700_000_000_000;

fn main() -> Result<(), Box<dyn Error>> {
    let fixture = generate();
    match env::args().nth(1) {
        Some(path) => {
            fs::write(
                &path,
                format!("{}\n", serde_json::to_string_pretty(&fixture)?),
            )?;
            println!("wrote {path}");
        }
        None => println!("{}", serde_json::to_string_pretty(&fixture)?),
    }
    Ok(())
}

fn generate() -> serde_json::Value {
    let bytes = fixture_rdb_bytes();
    json!({
        "fixture_version": 1,
        "purpose": "Immutable Valkey/Redis RDB v11 source exercising every decoded encoding; the migration inspect/run/verify/promote cycle against these exact bytes must stay reproducible forever.",
        "rdb_version": RDB_VERSION,
        "rdb_hex": encode_hex(&bytes),
        "blake3_hex": blake3::hash(&bytes).to_hex().to_string(),
        "expected": {
            "key_count": 10,
            "database_count": 2,
            "required_waivers": ["stream-consumer-groups", "streams"],
            "families": {
                "strings": 4,
                "hashes": 1,
                "sets": 2,
                "lists": 1,
                "sorted_sets": 1,
                "streams": 1
            }
        }
    })
}

/// Builds the complete fixture payload with a valid CRC-64 trailer.
fn fixture_rdb_bytes() -> Vec<u8> {
    let mut builder = RdbBuilder::new(RDB_VERSION);
    builder
        .aux(b"redis-ver", b"7.2.5")
        .select_db(0)
        .string_record(b"greeting", b"hola")
        .int_string_record(b"answer", 42)
        .expire_ms(FAR_FUTURE_EXPIRY_MS)
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
            STREAM_ID_MS,
            &[(b"kind", b"created")],
            Some(b"workers"),
        )
        .select_db(1)
        .string_record(b"other", b"db");
    builder.finish()
}

/// Incrementally builds one RDB payload.
struct RdbBuilder {
    bytes: Vec<u8>,
}

#[allow(clippy::cast_possible_truncation)]
impl RdbBuilder {
    fn new(version: u32) -> Self {
        let mut bytes = b"REDIS".to_vec();
        bytes.extend_from_slice(format!("{version:04}").as_bytes());
        Self { bytes }
    }

    fn length(&mut self, value: u64) -> &mut Self {
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

    fn string(&mut self, value: &[u8]) -> &mut Self {
        self.length(value.len() as u64);
        self.bytes.extend_from_slice(value);
        self
    }

    fn select_db(&mut self, index: u64) -> &mut Self {
        self.bytes.push(0xfe);
        self.length(index)
    }

    fn aux(&mut self, name: &[u8], value: &[u8]) -> &mut Self {
        self.bytes.push(0xfa);
        self.string(name);
        self.string(value)
    }

    fn expire_ms(&mut self, at: u64) -> &mut Self {
        self.bytes.push(0xfc);
        self.bytes.extend_from_slice(&at.to_le_bytes());
        self
    }

    fn string_record(&mut self, key: &[u8], value: &[u8]) -> &mut Self {
        self.bytes.push(0);
        self.string(key);
        self.string(value)
    }

    fn int_string_record(&mut self, key: &[u8], value: i32) -> &mut Self {
        self.bytes.push(0);
        self.string(key);
        self.bytes.push(0xc2);
        self.bytes.extend_from_slice(&value.to_le_bytes());
        self
    }

    fn hash_record(&mut self, key: &[u8], entries: &[(&[u8], &[u8])]) -> &mut Self {
        self.bytes.push(4);
        self.string(key);
        self.length(entries.len() as u64);
        for (field, value) in entries {
            self.string(field);
            self.string(value);
        }
        self
    }

    fn set_listpack_record(&mut self, key: &[u8], members: &[&[u8]]) -> &mut Self {
        self.bytes.push(20);
        self.string(key);
        let payload = listpack(members);
        self.string(&payload)
    }

    fn list_quicklist2_record(&mut self, key: &[u8], members: &[&[u8]]) -> &mut Self {
        self.bytes.push(18);
        self.string(key);
        self.length(1);
        self.length(2);
        let payload = listpack(members);
        self.string(&payload)
    }

    fn zset2_record(&mut self, key: &[u8], members: &[(&[u8], f64)]) -> &mut Self {
        self.bytes.push(5);
        self.string(key);
        self.length(members.len() as u64);
        for (member, score) in members {
            self.string(member);
            self.bytes.extend_from_slice(&score.to_le_bytes());
        }
        self
    }

    fn set_intset_record(&mut self, key: &[u8], members: &[i32]) -> &mut Self {
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

    fn stream_record(
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

    fn finish(mut self) -> Vec<u8> {
        self.bytes.push(0xff);
        let checksum = crc64(0, &self.bytes);
        self.bytes.extend_from_slice(&checksum.to_le_bytes());
        self.bytes
    }
}

/// Encodes one listpack payload of raw string elements.
#[allow(clippy::cast_possible_truncation)]
fn listpack(elements: &[&[u8]]) -> Vec<u8> {
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

/// Folds `bytes` into a running CRC-64/Jones value.
fn crc64(crc: u64, bytes: &[u8]) -> u64 {
    const fn build_table() -> [u64; 256] {
        const POLYNOMIAL: u64 = 0x95ac_9329_ac4b_c9b5;
        let mut table = [0_u64; 256];
        let mut index = 0_usize;
        while index < 256 {
            let mut value = index as u64;
            let mut bit = 0_u8;
            while bit < 8 {
                value = if value & 1 == 1 {
                    (value >> 1) ^ POLYNOMIAL
                } else {
                    value >> 1
                };
                bit += 1;
            }
            table[index] = value;
            index += 1;
        }
        table
    }
    const TABLE: [u64; 256] = build_table();
    let mut value = crc;
    for byte in bytes {
        let low = u8::try_from(value & 0xff).unwrap_or(0);
        value = TABLE[usize::from(low ^ byte)] ^ (value >> 8);
    }
    value
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::generate;

    #[test]
    fn checked_in_valkey_fixture_is_current() -> Result<(), Box<dyn Error>> {
        let generated = generate();
        // The packaged mirror is byte-identical to compatibility/valkey/
        // rdb-v11.json; the release mirror audit enforces the equality.
        let checked_in: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/valkey-rdb-v11.json"))?;
        assert_eq!(generated, checked_in);
        Ok(())
    }
}
