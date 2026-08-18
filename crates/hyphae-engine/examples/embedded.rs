// SPDX-License-Identifier: Apache-2.0

//! End-to-end embedded engine, query, proof creation, and offline verification.

use std::{collections::BTreeMap, error::Error, io, path::PathBuf};

use hyphae_engine::{
    HyphaeEngine, ProvenResult, VerificationLimits, verify_result_proof, write_result_proof,
};
use hyphae_query::{
    ExecutionLimits, FieldPath, Filter, NullPlacement, Query, Record, SortDirection, SortField,
    Value,
};
use uuid::Uuid;

fn main() -> Result<(), Box<dyn Error>> {
    let data_dir = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "usage: embedded DATA_DIR"))?;

    let mut opened = HyphaeEngine::open(&data_dir)?;
    let record = Record::new(
        b"alpha",
        Value::Object(BTreeMap::from([
            ("group".to_owned(), Value::String("x".to_owned())),
            ("score".to_owned(), Value::Integer(10)),
        ])),
    );
    let receipt = opened.engine.put_record(Uuid::now_v7(), &record)?;

    let query = Query {
        filter: Filter::MatchAll,
        sort: vec![SortField {
            path: FieldPath::field("score"),
            direction: SortDirection::Descending,
            nulls: NullPlacement::Last,
        }],
        cursor: None,
        limit: 100,
        aggregation: None,
    };
    let artifact = opened
        .engine
        .query_with_proof(&query, &ExecutionLimits::default())?;
    let row_count = match artifact.proof.result() {
        ProvenResult::Query(result) => result.rows.len(),
        ProvenResult::Get(_) => {
            return Err(io::Error::other("query produced a get proof").into());
        }
    };
    let anchor = artifact.proof.anchor_digest();
    let proof_path = std::env::temp_dir().join(format!(
        "hyphae-embedded-example-{}.hyproof",
        Uuid::now_v7()
    ));
    write_result_proof(&proof_path, &artifact.proof)?;
    let report = verify_result_proof(
        &proof_path,
        &artifact.snapshot.path,
        anchor,
        &VerificationLimits::default(),
    )?;
    std::fs::remove_file(proof_path)?;

    println!("data_dir={}", data_dir.display());
    println!("commit={receipt:?}");
    println!("rows={row_count}");
    println!("anchor={}", encode_hex(&report.anchor_digest));
    println!("proof={}", encode_hex(&report.proof_digest));
    Ok(())
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}
