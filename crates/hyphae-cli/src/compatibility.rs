// SPDX-License-Identifier: Apache-2.0

//! Explicit shipped format-2 CLI and `/v1` compatibility paths.
//!
//! These functions never participate in native command execution. They open
//! `HyphaeEngine` or `HyphaeServer` only for the shipped format-2 contracts.

use std::{
    env,
    error::Error,
    fs,
    io::{BufWriter, Read, Write, stdin, stdout},
    net::SocketAddr,
    path::{Path, PathBuf},
};

use clap::{Subcommand, ValueEnum};
use hyphae_client::HyphaeClient;
use hyphae_contracts::v1::{
    DefineLexicalIndexRequestV1, DefineVectorSpaceRequestV1, DeleteRequestV1,
    DeleteVectorsRequestV1, ExactRetrievalRequestV1, GetRequestV1, HybridRetrievalRequestV1,
    LexicalRetrievalRequestV1, ProofV1, PutRequestV1, PutVectorsRequestV1, QueryRequestV1,
};
use hyphae_engine::{
    HyphaeEngine, OpenedEngine, ProvenResult, ResultProofArtifact, RetrievalProofAnchor,
    RetrievalVerificationLimits, StorageLimits, VerificationLimits, verify_exact_retrieval_proof,
    verify_hybrid_retrieval_proof, verify_lexical_retrieval_proof, verify_result_proof,
    write_result_proof,
};
use hyphae_query::{
    Cursor, ExecutionLimits, FieldPath, Filter, MetricValue, NullPlacement, Query, Record,
    SortDirection, SortField,
};
use hyphae_server::{BearerToken, HyphaeServer, ServerConfig, ServerLimits};
use hyphae_storage::{AppendOutcome, CommitReceipt, CompactionOutcome, SnapshotInfo};
use serde_json::json;
use thiserror::Error;
use uuid::Uuid;

use crate::json_value::{decode_hex, encode_hex, parse_json, to_json};

const MAX_BEARER_TOKEN_BYTES: u64 = 4_096;
const MAX_BEARER_TOKEN_INPUT_BYTES: u64 = MAX_BEARER_TOKEN_BYTES + 2;
const INITIAL_READ_CAPACITY: u64 = 64 * 1024;

#[derive(Debug, Error, Eq, PartialEq)]
enum CompatibilityError {
    #[error("field path must contain nonempty dot-separated segments")]
    InvalidFieldPath,
    #[error("result proof contains an unexpected operation/result variant")]
    UnexpectedProofResult,
    #[error("bearer token environment value is not valid Unicode")]
    InvalidBearerTokenEncoding,
    #[error("bearer token contains an embedded newline")]
    BearerTokenContainsNewline,
    #[cfg(unix)]
    #[error("bearer token file must not grant permissions to group or other users")]
    InsecureBearerTokenFile,
    #[error("{input} is at least {actual} bytes, exceeding the local CLI limit {maximum}")]
    InputTooLarge {
        input: &'static str,
        actual: u64,
        maximum: u64,
    },
    #[error("{input} changed length while it was read")]
    InputLengthChanged { input: &'static str },
    #[error("format-2 compatibility command cannot open a native data directory")]
    NativeDirectory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DirectoryFamily {
    Native,
    Format2,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BackupFamily {
    Format2,
    Other,
}

#[derive(Debug, Subcommand)]
pub(crate) enum RemoteCommand {
    /// Print public API capabilities and effective limits.
    Capabilities,
    /// Print process liveness.
    Liveness,
    /// Print engine readiness.
    Readiness,
    /// Submit a typed JSON `PutRequestV1`.
    Put {
        #[arg(long)]
        request: PathBuf,
    },
    /// Submit a typed JSON `GetRequestV1`.
    Get {
        #[arg(long)]
        request: PathBuf,
    },
    /// Submit a typed JSON `DeleteRequestV1`.
    Delete {
        #[arg(long)]
        request: PathBuf,
    },
    /// Submit a typed JSON `QueryRequestV1`.
    Query {
        #[arg(long)]
        request: PathBuf,
    },
    /// Submit a typed JSON `DefineVectorSpaceRequestV1`.
    DefineVectorSpace {
        #[arg(long)]
        request: PathBuf,
    },
    /// Submit a typed JSON `PutVectorsRequestV1`.
    PutVectors {
        #[arg(long)]
        request: PathBuf,
    },
    /// Submit a typed JSON `DeleteVectorsRequestV1`.
    DeleteVectors {
        #[arg(long)]
        request: PathBuf,
    },
    /// Submit a typed JSON `ExactRetrievalRequestV1`.
    RetrieveExact {
        #[arg(long)]
        request: PathBuf,
    },
    /// Submit a typed JSON `DefineLexicalIndexRequestV1`.
    DefineLexicalIndex {
        #[arg(long)]
        request: PathBuf,
    },
    /// Submit a typed JSON `LexicalRetrievalRequestV1`.
    RetrieveLexical {
        #[arg(long)]
        request: PathBuf,
    },
    /// Submit a typed JSON `HybridRetrievalRequestV1`.
    RetrieveHybrid {
        #[arg(long)]
        request: PathBuf,
    },
    /// Download the canonical witness referenced by proof JSON.
    Witness {
        #[arg(long)]
        proof: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum RetrievalKind {
    Exact,
    Lexical,
    Hybrid,
}

pub(crate) struct QueryArguments {
    pub(crate) field: Option<String>,
    pub(crate) equals: Option<String>,
    pub(crate) sort: Option<String>,
    pub(crate) descending: bool,
    pub(crate) nulls_first: bool,
    pub(crate) limit: usize,
    pub(crate) proof_out: Option<PathBuf>,
}

pub(crate) fn directory_family(path: &Path) -> Result<DirectoryFamily, Box<dyn Error>> {
    let marker = path.join("FORMAT");
    let marker = match fs::read(marker) {
        Ok(marker) => marker,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DirectoryFamily::Other);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => {
            return Ok(DirectoryFamily::Other);
        }
        Err(error) => return Err(error.into()),
    };
    if marker.starts_with(b"hyphae-native-format=") {
        Ok(DirectoryFamily::Native)
    } else if marker.starts_with(b"hyphae-disk-format=") {
        Ok(DirectoryFamily::Format2)
    } else {
        Ok(DirectoryFamily::Other)
    }
}

pub(crate) fn backup_family(path: &Path) -> Result<BackupFamily, Box<dyn Error>> {
    if path.join("NATIVE_BACKUP.json").try_exists()? {
        Ok(BackupFamily::Other)
    } else if path.join("BACKUP.json").try_exists()? {
        Ok(BackupFamily::Format2)
    } else {
        Ok(BackupFamily::Other)
    }
}

pub(crate) async fn remote(
    base_url: &str,
    bearer_token_file: Option<&Path>,
    operation: RemoteCommand,
) -> Result<(), Box<dyn Error>> {
    let mut builder = HyphaeClient::builder(base_url)?;
    if let Some(token) = load_remote_bearer_token(bearer_token_file)? {
        builder = builder.bearer_token(&token)?;
    }
    let client = builder.build()?;
    match operation {
        RemoteCommand::Capabilities => print_serializable(&client.capabilities().await?.value),
        RemoteCommand::Liveness => print_serializable(&client.liveness().await?.value),
        RemoteCommand::Readiness => print_serializable(&client.readiness().await?.value),
        RemoteCommand::Put { request } => {
            let request: PutRequestV1 = read_json_request(&request)?;
            print_serializable(&client.put(&request).await?.value)
        }
        RemoteCommand::Get { request } => {
            let request: GetRequestV1 = read_json_request(&request)?;
            print_serializable(&client.get(&request).await?.value)
        }
        RemoteCommand::Delete { request } => {
            let request: DeleteRequestV1 = read_json_request(&request)?;
            print_serializable(&client.delete(&request).await?.value)
        }
        RemoteCommand::Query { request } => {
            let request: QueryRequestV1 = read_json_request(&request)?;
            print_serializable(&client.query(&request).await?.value)
        }
        RemoteCommand::DefineVectorSpace { request } => {
            let request: DefineVectorSpaceRequestV1 = read_json_request(&request)?;
            print_serializable(&client.define_vector_space(&request).await?.value)
        }
        RemoteCommand::PutVectors { request } => {
            let request: PutVectorsRequestV1 = read_json_request(&request)?;
            print_serializable(&client.put_vectors(&request).await?.value)
        }
        RemoteCommand::DeleteVectors { request } => {
            let request: DeleteVectorsRequestV1 = read_json_request(&request)?;
            print_serializable(&client.delete_vectors(&request).await?.value)
        }
        RemoteCommand::RetrieveExact { request } => {
            let request: ExactRetrievalRequestV1 = read_json_request(&request)?;
            print_serializable(&client.retrieve_exact(&request).await?.value)
        }
        RemoteCommand::DefineLexicalIndex { request } => {
            let request: DefineLexicalIndexRequestV1 = read_json_request(&request)?;
            print_serializable(&client.define_lexical_index(&request).await?.value)
        }
        RemoteCommand::RetrieveLexical { request } => {
            let request: LexicalRetrievalRequestV1 = read_json_request(&request)?;
            print_serializable(&client.retrieve_lexical(&request).await?.value)
        }
        RemoteCommand::RetrieveHybrid { request } => {
            let request: HybridRetrievalRequestV1 = read_json_request(&request)?;
            print_serializable(&client.retrieve_hybrid(&request).await?.value)
        }
        RemoteCommand::Witness { proof, out } => {
            let encoded = read_proof_json_value(&proof)?;
            let witness = match serde_json::from_value::<ProofV1>(encoded.clone()) {
                Ok(proof) => client.download_witness(&proof).await?.value,
                Err(result_error) => {
                    match serde_json::from_value::<hyphae_contracts::v1::RetrievalProofV1>(encoded)
                    {
                        Ok(proof) => client.download_retrieval_witness(&proof).await?.value,
                        Err(retrieval_error) => {
                            return Err(format!(
                                "proof is neither ProofV1 ({result_error}) nor RetrievalProofV1 ({retrieval_error})"
                            )
                            .into());
                        }
                    }
                }
            };
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&out)?;
            output.write_all(&witness)?;
            output.sync_all()?;
            print_json(&json!({ "path": out, "file_bytes": witness.len() }))
        }
    }
}

pub(crate) async fn serve(
    data_dir: PathBuf,
    bind: Option<SocketAddr>,
    bearer_token_file: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    let mut config = ServerConfig::new(&data_dir);
    if let Some(bind) = bind {
        config.bind = bind;
    }
    config.bearer_token = load_bearer_token(bearer_token_file)?;
    let bound = HyphaeServer::open(config)?.bind().await?;
    eprintln!(
        "hyphae serving format-2 /v1 on {} with data directory {}",
        bound.local_addr(),
        data_dir.display()
    );
    bound
        .run_with_shutdown(async {
            let _signal_result = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

pub(crate) fn put(
    data_dir: &Path,
    key: String,
    encoded_json: &str,
    transaction_id: Option<Uuid>,
) -> Result<(), Box<dyn Error>> {
    let value = parse_json(encoded_json)?;
    let mut opened = open_engine(data_dir)?;
    let outcome = opened.engine.put_record(
        transaction_id.unwrap_or_else(Uuid::now_v7),
        &Record::new(key.into_bytes(), value),
    )?;
    print_json(&receipt_json(outcome))
}

pub(crate) fn get(
    data_dir: &Path,
    key: &[u8],
    proof_out: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    let opened = open_engine(data_dir)?;
    let (record, proof) = if let Some(proof_path) = proof_out {
        let artifact = opened.engine.get_record_with_proof(key)?;
        write_result_proof(proof_path, &artifact.proof)?;
        let ProvenResult::Get(record) = artifact.proof.result() else {
            return Err(CompatibilityError::UnexpectedProofResult.into());
        };
        (record.clone(), Some(proof_json(proof_path, &artifact)))
    } else {
        (opened.engine.get_record(key)?, None)
    };
    let value = record.as_ref().map(record_json);
    print_json(&json!({ "found": value.is_some(), "record": value, "proof": proof }))
}

pub(crate) fn delete(
    data_dir: &Path,
    key: &[u8],
    transaction_id: Option<Uuid>,
) -> Result<(), Box<dyn Error>> {
    let mut opened = open_engine(data_dir)?;
    let outcome = opened
        .engine
        .delete_record(transaction_id.unwrap_or_else(Uuid::now_v7), key)?;
    print_json(&receipt_json(outcome))
}

pub(crate) fn query(data_dir: &Path, arguments: QueryArguments) -> Result<(), Box<dyn Error>> {
    let filter = match (arguments.field, arguments.equals) {
        (Some(field), Some(equals)) => Filter::Compare {
            path: parse_field_path(&field)?,
            operator: hyphae_query::CompareOperator::Equal,
            value: parse_json(&equals)?,
        },
        (None, None) => Filter::MatchAll,
        _ => return Err(CompatibilityError::InvalidFieldPath.into()),
    };
    let sort = arguments
        .sort
        .map(|field| {
            Ok::<SortField, CompatibilityError>(SortField {
                path: parse_field_path(&field)?,
                direction: if arguments.descending {
                    SortDirection::Descending
                } else {
                    SortDirection::Ascending
                },
                nulls: if arguments.nulls_first {
                    NullPlacement::First
                } else {
                    NullPlacement::Last
                },
            })
        })
        .transpose()?
        .into_iter()
        .collect();
    let request = Query {
        filter,
        sort,
        cursor: None,
        limit: arguments.limit,
        aggregation: None,
    };
    let opened = open_engine(data_dir)?;
    let (result, proof) = if let Some(proof_path) = arguments.proof_out.as_deref() {
        let artifact = opened
            .engine
            .query_with_proof(&request, &ExecutionLimits::default())?;
        write_result_proof(proof_path, &artifact.proof)?;
        let ProvenResult::Query(result) = artifact.proof.result() else {
            return Err(CompatibilityError::UnexpectedProofResult.into());
        };
        (result.clone(), Some(proof_json(proof_path, &artifact)))
    } else {
        (
            opened.engine.query(&request, &ExecutionLimits::default())?,
            None,
        )
    };
    print_json(&query_result_json(&result, proof.as_ref()))
}

pub(crate) fn verify(
    proof_path: &Path,
    snapshot_path: &Path,
    encoded_anchor: &str,
) -> Result<(), Box<dyn Error>> {
    let expected_anchor = decode_hex::<32>(encoded_anchor)?;
    let report = verify_result_proof(
        proof_path,
        snapshot_path,
        expected_anchor,
        &VerificationLimits::default(),
    )?;
    print_json(&json!({
        "status": "verified",
        "anchor_digest": encode_hex(&report.anchor_digest),
        "proof_digest": encode_hex(&report.proof_digest),
        "checkpoint_sequence": report.anchor.checkpoint_sequence,
        "checkpoint_digest": report.anchor.checkpoint_digest.map(|digest| encode_hex(&digest)),
        "snapshot_digest": encode_hex(&report.anchor.snapshot_digest),
        "result": proven_result_json(&report.result),
    }))
}

pub(crate) fn verify_retrieval(
    kind: RetrievalKind,
    proof_path: &Path,
    snapshot_path: &Path,
    encoded_anchor: &str,
) -> Result<(), Box<dyn Error>> {
    let expected_anchor = decode_hex::<32>(encoded_anchor)?;
    let limits = RetrievalVerificationLimits::default();
    let value = match kind {
        RetrievalKind::Exact => {
            let report =
                verify_exact_retrieval_proof(proof_path, snapshot_path, expected_anchor, &limits)?;
            retrieval_verification_json(
                "exact",
                &report.anchor,
                report.anchor_digest,
                report.proof_digest,
            )
        }
        RetrievalKind::Lexical => {
            let report = verify_lexical_retrieval_proof(
                proof_path,
                snapshot_path,
                expected_anchor,
                &limits,
            )?;
            retrieval_verification_json(
                "lexical",
                &report.anchor,
                report.anchor_digest,
                report.proof_digest,
            )
        }
        RetrievalKind::Hybrid => {
            let report =
                verify_hybrid_retrieval_proof(proof_path, snapshot_path, expected_anchor, &limits)?;
            retrieval_verification_json(
                "hybrid",
                &report.anchor,
                report.anchor_digest,
                report.proof_digest,
            )
        }
    };
    print_json(&value)
}

pub(crate) fn snapshot(data_dir: &Path) -> Result<(), Box<dyn Error>> {
    let opened = open_engine(data_dir)?;
    print_json(&snapshot_json(&opened.engine.snapshot()?))
}

pub(crate) fn compact(data_dir: &Path) -> Result<(), Box<dyn Error>> {
    let mut opened = open_engine(data_dir)?;
    let value = match opened.engine.compact()? {
        CompactionOutcome::NoChanges { snapshot } => {
            json!({ "status": "no_changes", "snapshot": snapshot_json(&snapshot) })
        }
        CompactionOutcome::Compacted(report) => json!({
            "status": "compacted",
            "generation": report.generation,
            "snapshot": snapshot_json(&report.snapshot),
            "retired_segment": report.retired_segment,
            "retired_segment_removed": report.retired_segment_removed,
        }),
    };
    print_json(&value)
}

pub(crate) fn backup(data_dir: &Path, destination: &Path) -> Result<(), Box<dyn Error>> {
    let opened = open_engine(data_dir)?;
    let backup = opened.engine.backup(destination)?;
    print_json(&json!({
        "status": "created",
        "backup_path": backup.path,
        "snapshot": snapshot_json(&backup.snapshot),
    }))
}

pub(crate) fn backup_verify(path: &Path) -> Result<(), Box<dyn Error>> {
    let backup = HyphaeEngine::verify_backup(path)?;
    print_json(&json!({
        "status": "verified",
        "backup_path": backup.path,
        "snapshot": snapshot_json(&backup.snapshot),
    }))
}

pub(crate) fn restore(backup: &Path, data_dir: &Path) -> Result<(), Box<dyn Error>> {
    let restored = HyphaeEngine::restore_backup(backup, data_dir)?;
    print_json(&json!({
        "status": "restored",
        "data_path": restored.data_path,
        "snapshot": snapshot_json(&restored.snapshot),
    }))
}

pub(crate) fn doctor(data_dir: &Path) -> Result<(), Box<dyn Error>> {
    let opened = open_engine(data_dir)?;
    let snapshot = opened.engine.snapshot()?;
    let log = &opened.recovery.log;
    print_json(&json!({
        "status": "healthy",
        "data_path": data_dir,
        "recovery": {
            "base_sequence": log.base_sequence,
            "base_digest": encode_hex(&log.base_digest),
            "recovered_transactions": log.transactions.len(),
            "ignored_uncommitted_transactions": log.ignored_uncommitted_transactions,
            "duplicate_commits": log.duplicate_commits,
            "truncated_tail_bytes": log.truncated_tail_bytes,
            "valid_bytes": log.valid_bytes,
            "last_sequence": log.last_sequence,
            "last_digest": encode_hex(&log.last_digest),
            "replayed_transactions": opened.recovery.replayed_transactions,
        },
        "snapshot": snapshot_json(&snapshot),
    }))
}

fn open_engine(data_dir: &Path) -> Result<OpenedEngine, Box<dyn Error>> {
    if directory_family(data_dir)? == DirectoryFamily::Native {
        return Err(CompatibilityError::NativeDirectory.into());
    }
    HyphaeEngine::open_with_limits(data_dir, StorageLimits::default()).map_err(Into::into)
}

fn load_bearer_token(path: Option<&Path>) -> Result<Option<BearerToken>, Box<dyn Error>> {
    let Some(mut encoded) = load_bearer_token_bytes(path)? else {
        return Ok(None);
    };
    trim_terminal_newline(&mut encoded);
    if encoded.contains(&b'\n') || encoded.contains(&b'\r') {
        return Err(CompatibilityError::BearerTokenContainsNewline.into());
    }
    Ok(Some(BearerToken::new(encoded)?))
}

fn load_remote_bearer_token(path: Option<&Path>) -> Result<Option<String>, Box<dyn Error>> {
    let Some(mut encoded) = load_bearer_token_bytes(path)? else {
        return Ok(None);
    };
    trim_terminal_newline(&mut encoded);
    if encoded.contains(&b'\n') || encoded.contains(&b'\r') {
        return Err(CompatibilityError::BearerTokenContainsNewline.into());
    }
    enforce_input_limit(encoded.len(), MAX_BEARER_TOKEN_BYTES, "bearer token")?;
    String::from_utf8(encoded)
        .map(Some)
        .map_err(|_| CompatibilityError::InvalidBearerTokenEncoding.into())
}

fn trim_terminal_newline(encoded: &mut Vec<u8>) {
    if encoded.last() == Some(&b'\n') {
        encoded.pop();
        if encoded.last() == Some(&b'\r') {
            encoded.pop();
        }
    }
}

fn load_bearer_token_bytes(path: Option<&Path>) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
    if let Some(path) = path {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            if fs::metadata(path)?.permissions().mode() & 0o077 != 0 {
                return Err(CompatibilityError::InsecureBearerTokenFile.into());
            }
        }
        return read_bounded_file(path, MAX_BEARER_TOKEN_INPUT_BYTES, "bearer token input")
            .map(Some);
    }
    let Some(value) = env::var_os("HYPHAE_BEARER_TOKEN") else {
        return Ok(None);
    };
    let encoded = value
        .into_string()
        .map(String::into_bytes)
        .map_err(|_| CompatibilityError::InvalidBearerTokenEncoding)?;
    enforce_input_limit(
        encoded.len(),
        MAX_BEARER_TOKEN_INPUT_BYTES,
        "bearer token input",
    )?;
    Ok(Some(encoded))
}

fn read_json_request<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, Box<dyn Error>> {
    let maximum = u64::try_from(ServerLimits::default().request_body_bytes).unwrap_or(u64::MAX);
    Ok(serde_json::from_value(read_json_value(
        path,
        maximum,
        "JSON request",
    )?)?)
}

fn read_proof_json_value(path: &Path) -> Result<serde_json::Value, Box<dyn Error>> {
    let maximum = u64::try_from(ServerLimits::default().response_bytes).unwrap_or(u64::MAX);
    read_json_value(path, maximum, "proof JSON")
}

fn read_json_value(
    path: &Path,
    maximum: u64,
    input: &'static str,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let encoded = if path == Path::new("-") {
        read_bounded(stdin().lock(), maximum, input)?
    } else {
        read_bounded_file(path, maximum, input)?
    };
    Ok(serde_json::from_slice(&encoded)?)
}

fn read_bounded_file(
    path: &Path,
    maximum: u64,
    input: &'static str,
) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    if metadata.len() > maximum {
        return Err(CompatibilityError::InputTooLarge {
            input,
            actual: metadata.len(),
            maximum,
        }
        .into());
    }
    let encoded = read_bounded(&mut file, maximum, input)?;
    if metadata.is_file() {
        let final_length = file.metadata()?.len();
        let observed_length = u64::try_from(encoded.len()).unwrap_or(u64::MAX);
        if final_length != metadata.len() || observed_length != final_length {
            return Err(CompatibilityError::InputLengthChanged { input }.into());
        }
    }
    Ok(encoded)
}

fn read_bounded(
    input_reader: impl Read,
    maximum: u64,
    input: &'static str,
) -> Result<Vec<u8>, Box<dyn Error>> {
    let initial_capacity =
        usize::try_from(maximum.min(INITIAL_READ_CAPACITY)).unwrap_or(usize::MAX);
    let mut encoded = Vec::with_capacity(initial_capacity);
    input_reader
        .take(maximum.saturating_add(1))
        .read_to_end(&mut encoded)?;
    enforce_input_limit(encoded.len(), maximum, input)?;
    Ok(encoded)
}

fn enforce_input_limit(
    length: usize,
    maximum: u64,
    input: &'static str,
) -> Result<(), CompatibilityError> {
    let actual = u64::try_from(length).unwrap_or(u64::MAX);
    if actual > maximum {
        Err(CompatibilityError::InputTooLarge {
            input,
            actual,
            maximum,
        })
    } else {
        Ok(())
    }
}

fn parse_field_path(path: &str) -> Result<FieldPath, CompatibilityError> {
    let segments = path.split('.').collect::<Vec<_>>();
    if segments.is_empty() || segments.iter().any(|segment| segment.is_empty()) {
        return Err(CompatibilityError::InvalidFieldPath);
    }
    Ok(FieldPath::new(segments))
}

fn proof_json(path: &Path, artifact: &ResultProofArtifact) -> serde_json::Value {
    json!({
        "path": path,
        "snapshot_path": artifact.snapshot.path,
        "checkpoint_sequence": artifact.proof.anchor().checkpoint_sequence,
        "checkpoint_digest": artifact.proof.anchor().checkpoint_digest.map(|digest| encode_hex(&digest)),
        "snapshot_digest": encode_hex(&artifact.proof.anchor().snapshot_digest),
        "anchor_digest": encode_hex(&artifact.proof.anchor_digest()),
        "proof_digest": encode_hex(&artifact.proof.proof_digest()),
    })
}

fn record_json(record: &Record) -> serde_json::Value {
    json!({ "key_hex": encode_hex(&record.key), "value": to_json(&record.value) })
}

fn query_result_json(
    result: &hyphae_query::QueryResult,
    proof: Option<&serde_json::Value>,
) -> serde_json::Value {
    json!({
        "rows": result.rows.iter().map(record_json).collect::<Vec<_>>(),
        "next_cursor": result.next_cursor.as_ref().map(cursor_json),
        "aggregation": result.aggregation.as_ref().map(aggregation_json),
        "scanned_records": result.scanned_records,
        "matched_records": result.matched_records,
        "proof": proof,
    })
}

fn proven_result_json(result: &ProvenResult) -> serde_json::Value {
    match result {
        ProvenResult::Get(record) => json!({
            "type": "get",
            "found": record.is_some(),
            "record": record.as_ref().map(record_json),
        }),
        ProvenResult::Query(result) => {
            json!({ "type": "query", "result": query_result_json(result, None) })
        }
    }
}

fn receipt_json(outcome: AppendOutcome) -> serde_json::Value {
    let (status, receipt) = match outcome {
        AppendOutcome::Committed(receipt) => ("committed", receipt),
        AppendOutcome::Existing(receipt) => ("existing", receipt),
    };
    commit_receipt_json(status, receipt)
}

fn commit_receipt_json(status: &str, receipt: CommitReceipt) -> serde_json::Value {
    json!({
        "status": status,
        "transaction_id": receipt.transaction_id,
        "commit_sequence": receipt.commit_sequence,
        "commit_digest": encode_hex(&receipt.commit_digest),
        "transaction_digest": encode_hex(&receipt.transaction_digest),
    })
}

fn cursor_json(cursor: &Cursor) -> serde_json::Value {
    json!({
        "sort_values": cursor.sort_values.iter().map(|value| value.as_ref().map_or(serde_json::Value::Null, to_json)).collect::<Vec<_>>(),
        "key_hex": encode_hex(&cursor.key),
    })
}

fn aggregation_json(aggregation: &hyphae_query::AggregationResult) -> serde_json::Value {
    json!({
        "grouped": aggregation.grouped,
        "groups": aggregation.groups.iter().map(|group| json!({
            "key": group.key.iter().map(|value| value.as_ref().map_or(serde_json::Value::Null, to_json)).collect::<Vec<_>>(),
            "metrics": group.metrics.iter().map(|metric| json!({
                "name": metric.name,
                "value": metric_json(&metric.value),
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

fn metric_json(metric: &MetricValue) -> serde_json::Value {
    match metric {
        MetricValue::Count(value) => json!(value),
        MetricValue::Integer(None) | MetricValue::Value(None) => serde_json::Value::Null,
        MetricValue::Integer(Some(value)) => i64::try_from(*value).map_or_else(
            |_| serde_json::Value::String(value.to_string()),
            |value| json!(value),
        ),
        MetricValue::Value(Some(value)) => to_json(value),
    }
}

fn snapshot_json(snapshot: &SnapshotInfo) -> serde_json::Value {
    json!({
        "path": snapshot.path,
        "checkpoint_sequence": snapshot.checkpoint_sequence,
        "checkpoint_digest": snapshot.checkpoint_digest.map(|digest| encode_hex(&digest)),
        "entry_count": snapshot.entry_count,
        "receipt_count": snapshot.receipt_count,
        "snapshot_digest": encode_hex(&snapshot.snapshot_digest),
        "file_bytes": snapshot.file_bytes,
    })
}

fn retrieval_verification_json(
    operation: &str,
    anchor: &RetrievalProofAnchor,
    anchor_digest: [u8; 32],
    proof_digest: [u8; 32],
) -> serde_json::Value {
    json!({
        "status": "verified",
        "operation": operation,
        "anchor_digest": encode_hex(&anchor_digest),
        "proof_digest": encode_hex(&proof_digest),
        "checkpoint_sequence": anchor.checkpoint_sequence,
        "checkpoint_digest": anchor.checkpoint_digest.map(|digest| encode_hex(&digest)),
        "snapshot_digest": encode_hex(&anchor.snapshot_digest),
    })
}

fn print_serializable(value: &impl serde::Serialize) -> Result<(), Box<dyn Error>> {
    print_json(&serde_json::to_value(value)?)
}

fn print_json(value: &serde_json::Value) -> Result<(), Box<dyn Error>> {
    let mut output = BufWriter::new(stdout().lock());
    serde_json::to_writer_pretty(&mut output, value)?;
    writeln!(output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{CompatibilityError, parse_field_path, read_bounded};

    #[test]
    fn compatibility_inputs_remain_bounded_and_strict() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            parse_field_path("nested.value").map(|path| path.segments().len()),
            Ok(2)
        );
        assert!(matches!(
            parse_field_path("nested..value"),
            Err(CompatibilityError::InvalidFieldPath)
        ));
        assert_eq!(read_bounded(Cursor::new(b"1234"), 4, "test")?, b"1234");
        assert!(read_bounded(Cursor::new(b"12345"), 4, "test").is_err());
        Ok(())
    }
}
