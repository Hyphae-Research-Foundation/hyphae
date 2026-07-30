// SPDX-License-Identifier: Apache-2.0

//! Command-line entry point for the single Hyphae executable.

mod json_value;
mod mcp;

use std::{
    env,
    error::Error,
    fs,
    io::{BufWriter, Read, Write, stdin, stdout},
    net::SocketAddr,
    path::{Path, PathBuf},
};

use clap::{Parser, Subcommand, ValueEnum};
use hyphae_client::HyphaeClient;
use hyphae_contracts::v1::{
    DefineLexicalIndexRequestV1, DefineVectorSpaceRequestV1, DeleteRequestV1,
    DeleteVectorsRequestV1, ExactRetrievalRequestV1, GetRequestV1, HybridRetrievalRequestV1,
    LexicalRetrievalRequestV1, ProofV1, PutRequestV1, PutVectorsRequestV1, QueryRequestV1,
};
use hyphae_core::current_version;
use hyphae_engine::{
    EngineError, HyphaeEngine, OpenedEngine, ProvenResult, ResultProofArtifact,
    RetrievalProofAnchor, RetrievalVerificationLimits, StorageLimits, VerificationLimits,
    verify_exact_retrieval_proof, verify_hybrid_retrieval_proof, verify_lexical_retrieval_proof,
    verify_result_proof, write_result_proof,
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
enum CliError {
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
}

#[derive(Debug, Parser)]
#[command(
    name = "hyphae",
    version,
    about = "Autonomous, embeddable, and verifiable data engine"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print independently versioned product surfaces.
    Version {
        /// Emit a machine-readable JSON object.
        #[arg(long)]
        json: bool,
    },
    /// Atomically store one structured JSON document.
    Put {
        /// Owned Hyphae data directory.
        #[arg(long, env = "HYPHAE_DATA_DIR")]
        data_dir: PathBuf,
        /// UTF-8 key; the Rust API also accepts arbitrary binary keys.
        #[arg(long)]
        key: String,
        /// JSON value. Numbers must be signed 64-bit integers.
        #[arg(long = "json")]
        value: String,
        /// Optional idempotency UUID; defaults to a new `UUIDv7`.
        #[arg(long)]
        transaction_id: Option<Uuid>,
    },
    /// Read and verify one structured document.
    Get {
        /// Owned Hyphae data directory.
        #[arg(long, env = "HYPHAE_DATA_DIR")]
        data_dir: PathBuf,
        /// UTF-8 key.
        #[arg(long)]
        key: String,
        /// Write a portable result proof to a new file.
        #[arg(long)]
        proof_out: Option<PathBuf>,
    },
    /// Atomically delete one structured document.
    Delete {
        /// Owned Hyphae data directory.
        #[arg(long, env = "HYPHAE_DATA_DIR")]
        data_dir: PathBuf,
        /// UTF-8 key.
        #[arg(long)]
        key: String,
        /// Optional idempotency UUID; defaults to a new `UUIDv7`.
        #[arg(long)]
        transaction_id: Option<Uuid>,
    },
    /// Execute deterministic structured query without AI.
    Query {
        /// Owned Hyphae data directory.
        #[arg(long, env = "HYPHAE_DATA_DIR")]
        data_dir: PathBuf,
        /// Dot-separated exact object path for equality filtering.
        #[arg(long, requires = "equals")]
        field: Option<String>,
        /// JSON equality literal; requires `--field`.
        #[arg(long, requires = "field")]
        equals: Option<String>,
        /// Dot-separated exact object path for sorting.
        #[arg(long)]
        sort: Option<String>,
        /// Sort non-null values descending.
        #[arg(long)]
        descending: bool,
        /// Place missing and null before non-null values.
        #[arg(long)]
        nulls_first: bool,
        /// Final page size after global ordering.
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Write a portable result proof to a new file.
        #[arg(long)]
        proof_out: Option<PathBuf>,
    },
    /// Create or reuse a verified logical snapshot.
    Snapshot {
        /// Owned Hyphae data directory.
        #[arg(long, env = "HYPHAE_DATA_DIR")]
        data_dir: PathBuf,
    },
    /// Commit an anchored compaction generation.
    Compact {
        /// Owned Hyphae data directory.
        #[arg(long, env = "HYPHAE_DATA_DIR")]
        data_dir: PathBuf,
    },
    /// Create a portable backup at one locked logical checkpoint.
    Backup {
        /// Owned Hyphae data directory.
        #[arg(long, env = "HYPHAE_DATA_DIR")]
        data_dir: PathBuf,
        /// New backup directory; existing paths are never replaced.
        #[arg(long)]
        out: PathBuf,
    },
    /// Verify a portable backup without restoring it.
    BackupVerify {
        /// Backup directory containing `BACKUP.json` and `snapshot.hysnap`.
        #[arg(long)]
        backup: PathBuf,
    },
    /// Restore a verified backup to a new atomically activated data directory.
    Restore {
        /// Backup directory to verify and restore.
        #[arg(long)]
        backup: PathBuf,
        /// New destination data directory; existing paths are never replaced.
        #[arg(long, env = "HYPHAE_DATA_DIR")]
        data_dir: PathBuf,
    },
    /// Verify and report the complete local data-directory health state.
    Doctor {
        /// Owned Hyphae data directory.
        #[arg(long, env = "HYPHAE_DATA_DIR")]
        data_dir: PathBuf,
    },
    /// Verify a result proof completely offline.
    Verify {
        /// Canonical `.hyproof` file.
        #[arg(long)]
        proof: PathBuf,
        /// Canonical logical snapshot witness referenced by the proof.
        #[arg(long)]
        snapshot: PathBuf,
        /// Trusted 32-byte anchor digest as hexadecimal.
        #[arg(long)]
        anchor: String,
    },
    /// Verify an exact, lexical, or hybrid retrieval proof completely offline.
    VerifyRetrieval {
        /// Retrieval operation encoded by the canonical `.hyrproof` file.
        #[arg(long, value_enum)]
        kind: RetrievalKind,
        /// Canonical `.hyrproof` file.
        #[arg(long)]
        proof: PathBuf,
        /// Canonical format-2 logical snapshot witness referenced by the proof.
        #[arg(long)]
        snapshot: PathBuf,
        /// Trusted 32-byte retrieval-anchor digest as hexadecimal.
        #[arg(long)]
        anchor: String,
    },
    /// Start the optional secure version 1 HTTP server.
    Serve {
        /// Exclusively owned Hyphae data directory.
        #[arg(long, env = "HYPHAE_DATA_DIR")]
        data_dir: PathBuf,
        /// Listener address; non-loopback requires bearer authentication.
        #[arg(long, default_value = "127.0.0.1:8787")]
        bind: SocketAddr,
        /// Restricted file containing the bearer token. Alternatively set
        /// `HYPHAE_BEARER_TOKEN` without placing the secret in argv.
        #[arg(long, env = "HYPHAE_BEARER_TOKEN_FILE")]
        bearer_token_file: Option<PathBuf>,
    },
    /// Call a running Hyphae instance through only the public version 1 API.
    Remote {
        /// Root HTTP(S) origin. The token is never accepted on argv.
        #[arg(long, env = "HYPHAE_BASE_URL")]
        base_url: String,
        /// Restricted bearer-token file. Alternatively set
        /// `HYPHAE_BEARER_TOKEN`.
        #[arg(long, env = "HYPHAE_BEARER_TOKEN_FILE")]
        bearer_token_file: Option<PathBuf>,
        #[command(subcommand)]
        operation: RemoteCommand,
    },
    /// Run the optional MCP 2025-11-25 stdio adapter over the public API.
    Mcp {
        /// Root HTTP(S) Hyphae origin. MCP never opens a data directory.
        #[arg(long, env = "HYPHAE_BASE_URL")]
        base_url: String,
        /// Restricted bearer-token file. Alternatively set
        /// `HYPHAE_BEARER_TOKEN`.
        #[arg(long, env = "HYPHAE_BEARER_TOKEN_FILE")]
        bearer_token_file: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum RemoteCommand {
    /// Print public API capabilities and effective limits.
    Capabilities,
    /// Print process liveness.
    Liveness,
    /// Print engine readiness.
    Readiness,
    /// Submit a typed JSON `PutRequestV1` from a file or standard input (`-`).
    Put {
        #[arg(long)]
        request: PathBuf,
    },
    /// Submit a typed JSON `GetRequestV1` from a file or standard input (`-`).
    Get {
        #[arg(long)]
        request: PathBuf,
    },
    /// Submit a typed JSON `DeleteRequestV1` from a file or standard input (`-`).
    Delete {
        #[arg(long)]
        request: PathBuf,
    },
    /// Submit a typed JSON `QueryRequestV1` from a file or standard input (`-`).
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
    /// Download the canonical witness referenced by a result or retrieval proof JSON file.
    Witness {
        #[arg(long)]
        proof: PathBuf,
        /// New destination file; existing files are never replaced.
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum RetrievalKind {
    Exact,
    Lexical,
    Hybrid,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    match Cli::parse().command {
        Command::Version { json } => print_version(json),
        Command::Put {
            data_dir,
            key,
            value,
            transaction_id,
        } => put(&data_dir, key, &value, transaction_id),
        Command::Get {
            data_dir,
            key,
            proof_out,
        } => get(&data_dir, key.as_bytes(), proof_out.as_deref()),
        Command::Delete {
            data_dir,
            key,
            transaction_id,
        } => delete(&data_dir, key.as_bytes(), transaction_id),
        Command::Query {
            data_dir,
            field,
            equals,
            sort,
            descending,
            nulls_first,
            limit,
            proof_out,
        } => query(
            &data_dir,
            QueryArguments {
                field,
                equals,
                sort,
                descending,
                nulls_first,
                limit,
                proof_out,
            },
        ),
        Command::Snapshot { data_dir } => snapshot(&data_dir),
        Command::Compact { data_dir } => compact(&data_dir),
        Command::Backup { data_dir, out } => backup(&data_dir, &out),
        Command::BackupVerify { backup } => backup_verify(&backup),
        Command::Restore { backup, data_dir } => restore(&backup, &data_dir),
        Command::Doctor { data_dir } => doctor(&data_dir),
        Command::Verify {
            proof,
            snapshot,
            anchor,
        } => verify(&proof, &snapshot, &anchor),
        Command::VerifyRetrieval {
            kind,
            proof,
            snapshot,
            anchor,
        } => verify_retrieval(kind, &proof, &snapshot, &anchor),
        Command::Serve {
            data_dir,
            bind,
            bearer_token_file,
        } => serve(data_dir, bind, bearer_token_file.as_deref()).await,
        Command::Remote {
            base_url,
            bearer_token_file,
            operation,
        } => remote(&base_url, bearer_token_file.as_deref(), operation).await,
        Command::Mcp {
            base_url,
            bearer_token_file,
        } => {
            let token = load_remote_bearer_token(bearer_token_file.as_deref())?;
            mcp::run(&base_url, token.as_deref()).await
        }
    }
}

async fn remote(
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
                                "proof is neither ProofV1 ({result_error}) nor \
                                 RetrievalProofV1 ({retrieval_error})"
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

async fn serve(
    data_dir: PathBuf,
    bind: SocketAddr,
    bearer_token_file: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    let mut config = ServerConfig::new(&data_dir);
    config.bind = bind;
    config.bearer_token = load_bearer_token(bearer_token_file)?;
    let bound = HyphaeServer::open(config)?.bind().await?;
    eprintln!(
        "hyphae serving {} with data directory {}",
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

fn load_bearer_token(path: Option<&Path>) -> Result<Option<BearerToken>, Box<dyn Error>> {
    let Some(mut encoded) = load_bearer_token_bytes(path)? else {
        return Ok(None);
    };
    if encoded.last() == Some(&b'\n') {
        encoded.pop();
        if encoded.last() == Some(&b'\r') {
            encoded.pop();
        }
    }
    if encoded.contains(&b'\n') || encoded.contains(&b'\r') {
        return Err(CliError::BearerTokenContainsNewline.into());
    }
    Ok(Some(BearerToken::new(encoded)?))
}

fn load_remote_bearer_token(path: Option<&Path>) -> Result<Option<String>, Box<dyn Error>> {
    let Some(mut encoded) = load_bearer_token_bytes(path)? else {
        return Ok(None);
    };
    if encoded.last() == Some(&b'\n') {
        encoded.pop();
        if encoded.last() == Some(&b'\r') {
            encoded.pop();
        }
    }
    if encoded.contains(&b'\n') || encoded.contains(&b'\r') {
        return Err(CliError::BearerTokenContainsNewline.into());
    }
    enforce_input_limit(encoded.len(), MAX_BEARER_TOKEN_BYTES, "bearer token")?;
    String::from_utf8(encoded)
        .map(Some)
        .map_err(|_| CliError::InvalidBearerTokenEncoding.into())
}

fn load_bearer_token_bytes(path: Option<&Path>) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
    if let Some(path) = path {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let metadata = fs::metadata(path)?;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(CliError::InsecureBearerTokenFile.into());
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
        .map_err(|_| CliError::InvalidBearerTokenEncoding)?;
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
    if path == Path::new("-") {
        let encoded = read_bounded(stdin().lock(), maximum, input)?;
        Ok(serde_json::from_slice(&encoded)?)
    } else {
        let encoded = read_bounded_file(path, maximum, input)?;
        Ok(serde_json::from_slice(&encoded)?)
    }
}

fn read_bounded_file(
    path: &Path,
    maximum: u64,
    input: &'static str,
) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    if metadata.len() > maximum {
        return Err(CliError::InputTooLarge {
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
            return Err(CliError::InputLengthChanged { input }.into());
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

fn enforce_input_limit(length: usize, maximum: u64, input: &'static str) -> Result<(), CliError> {
    let actual = u64::try_from(length).unwrap_or(u64::MAX);
    if actual > maximum {
        Err(CliError::InputTooLarge {
            input,
            actual,
            maximum,
        })
    } else {
        Ok(())
    }
}

fn print_serializable(value: &impl serde::Serialize) -> Result<(), Box<dyn Error>> {
    print_json(&serde_json::to_value(value)?)
}

fn print_version(json_output: bool) -> Result<(), Box<dyn Error>> {
    let version = current_version();
    if json_output {
        print_json(&json!({
            "product": version.product,
            "engine_version": version.engine,
            "api_version": version.api,
            "disk_format_version": version.disk_format,
        }))
    } else {
        let mut output = BufWriter::new(stdout().lock());
        writeln!(
            output,
            "{} {} (api {}, disk format {})",
            version.product, version.engine, version.api, version.disk_format
        )?;
        Ok(())
    }
}

fn open_engine(data_dir: &Path) -> Result<OpenedEngine, EngineError> {
    HyphaeEngine::open_with_limits(data_dir, StorageLimits::default())
}

fn put(
    data_dir: &Path,
    key: String,
    encoded_json: &str,
    transaction_id: Option<Uuid>,
) -> Result<(), Box<dyn Error>> {
    let value = parse_json(encoded_json)?;
    let mut opened = open_engine(data_dir)?;
    let transaction_id = transaction_id.unwrap_or_else(Uuid::now_v7);
    let outcome = opened
        .engine
        .put_record(transaction_id, &Record::new(key.into_bytes(), value))?;
    print_json(&receipt_json(outcome))
}

fn get(data_dir: &Path, key: &[u8], proof_out: Option<&Path>) -> Result<(), Box<dyn Error>> {
    let opened = open_engine(data_dir)?;
    let (record, proof) = if let Some(proof_path) = proof_out {
        let artifact = opened.engine.get_record_with_proof(key)?;
        write_result_proof(proof_path, &artifact.proof)?;
        let ProvenResult::Get(record) = artifact.proof.result() else {
            return Err(CliError::UnexpectedProofResult.into());
        };
        (record.clone(), Some(proof_json(proof_path, &artifact)))
    } else {
        (opened.engine.get_record(key)?, None)
    };
    let value = record.as_ref().map(record_json);
    print_json(&json!({
        "found": value.is_some(),
        "record": value,
        "proof": proof,
    }))
}

fn delete(data_dir: &Path, key: &[u8], transaction_id: Option<Uuid>) -> Result<(), Box<dyn Error>> {
    let mut opened = open_engine(data_dir)?;
    let outcome = opened
        .engine
        .delete_record(transaction_id.unwrap_or_else(Uuid::now_v7), key)?;
    print_json(&receipt_json(outcome))
}

struct QueryArguments {
    field: Option<String>,
    equals: Option<String>,
    sort: Option<String>,
    descending: bool,
    nulls_first: bool,
    limit: usize,
    proof_out: Option<PathBuf>,
}

fn query(data_dir: &Path, arguments: QueryArguments) -> Result<(), Box<dyn Error>> {
    let filter = match (arguments.field, arguments.equals) {
        (Some(field), Some(equals)) => Filter::Compare {
            path: parse_field_path(&field)?,
            operator: hyphae_query::CompareOperator::Equal,
            value: parse_json(&equals)?,
        },
        (None, None) => Filter::MatchAll,
        _ => return Err(CliError::InvalidFieldPath.into()),
    };
    let sort = arguments
        .sort
        .map(|field| {
            Ok::<SortField, CliError>(SortField {
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
            return Err(CliError::UnexpectedProofResult.into());
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

fn verify(
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

fn verify_retrieval(
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

fn snapshot(data_dir: &Path) -> Result<(), Box<dyn Error>> {
    let opened = open_engine(data_dir)?;
    print_json(&snapshot_json(&opened.engine.snapshot()?))
}

fn compact(data_dir: &Path) -> Result<(), Box<dyn Error>> {
    let mut opened = open_engine(data_dir)?;
    let value = match opened.engine.compact()? {
        CompactionOutcome::NoChanges { snapshot } => json!({
            "status": "no_changes",
            "snapshot": snapshot_json(&snapshot),
        }),
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

fn backup(data_dir: &Path, destination: &Path) -> Result<(), Box<dyn Error>> {
    let opened = open_engine(data_dir)?;
    let backup = opened.engine.backup(destination)?;
    print_json(&json!({
        "status": "created",
        "backup_path": backup.path,
        "snapshot": snapshot_json(&backup.snapshot),
    }))
}

fn backup_verify(path: &Path) -> Result<(), Box<dyn Error>> {
    let backup = HyphaeEngine::verify_backup(path)?;
    print_json(&json!({
        "status": "verified",
        "backup_path": backup.path,
        "snapshot": snapshot_json(&backup.snapshot),
    }))
}

fn restore(backup: &Path, data_dir: &Path) -> Result<(), Box<dyn Error>> {
    let restored = HyphaeEngine::restore_backup(backup, data_dir)?;
    print_json(&json!({
        "status": "restored",
        "data_path": restored.data_path,
        "snapshot": snapshot_json(&restored.snapshot),
    }))
}

fn doctor(data_dir: &Path) -> Result<(), Box<dyn Error>> {
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

fn parse_field_path(path: &str) -> Result<FieldPath, CliError> {
    let segments = path.split('.').collect::<Vec<_>>();
    if segments.is_empty() || segments.iter().any(|segment| segment.is_empty()) {
        return Err(CliError::InvalidFieldPath);
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
    json!({
        "key_hex": encode_hex(&record.key),
        "value": to_json(&record.value),
    })
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
        ProvenResult::Query(result) => json!({
            "type": "query",
            "result": query_result_json(result, None),
        }),
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
        "sort_values": cursor.sort_values.iter().map(|value| {
            value.as_ref().map_or(serde_json::Value::Null, to_json)
        }).collect::<Vec<_>>(),
        "key_hex": encode_hex(&cursor.key),
    })
}

fn aggregation_json(aggregation: &hyphae_query::AggregationResult) -> serde_json::Value {
    json!({
        "grouped": aggregation.grouped,
        "groups": aggregation.groups.iter().map(|group| json!({
            "key": group.key.iter().map(|value| {
                value.as_ref().map_or(serde_json::Value::Null, to_json)
            }).collect::<Vec<_>>(),
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

fn print_json(value: &serde_json::Value) -> Result<(), Box<dyn Error>> {
    let mut output = BufWriter::new(stdout().lock());
    serde_json::to_writer_pretty(&mut output, value)?;
    writeln!(output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{error::Error, fs, io::Cursor};

    use uuid::Uuid;

    use super::{
        CliError, MAX_BEARER_TOKEN_BYTES, MAX_BEARER_TOKEN_INPUT_BYTES, load_bearer_token,
        load_remote_bearer_token, parse_field_path, read_bounded,
    };

    #[test]
    fn cli_field_paths_reject_empty_segments() {
        assert_eq!(
            parse_field_path("nested.value").map(|path| path.segments().len()),
            Ok(2)
        );
        assert!(matches!(
            parse_field_path("nested..value"),
            Err(CliError::InvalidFieldPath)
        ));
    }

    #[test]
    fn bearer_token_file_accepts_one_terminal_newline() -> Result<(), Box<dyn Error>> {
        let path = std::env::temp_dir().join(format!("hyphae-token-{}", Uuid::now_v7()));
        fs::write(&path, b"0123456789abcdef0123456789abcdef\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        let token = load_bearer_token(Some(&path));
        let _ignored = fs::remove_file(path);
        assert!(token?.is_some());
        Ok(())
    }

    #[test]
    fn bounded_input_accepts_exact_limit_and_rejects_next_byte() -> Result<(), Box<dyn Error>> {
        let exact = read_bounded(Cursor::new(b"12345678"), 8, "test input")?;
        assert_eq!(exact, b"12345678");

        let Err(error) = read_bounded(Cursor::new(b"123456789"), 8, "test input") else {
            return Err(std::io::Error::other("one byte above the limit was accepted").into());
        };
        assert!(matches!(
            error.downcast_ref::<CliError>(),
            Some(CliError::InputTooLarge {
                input: "test input",
                actual: 9,
                maximum: 8,
            })
        ));
        Ok(())
    }

    #[test]
    fn bearer_token_file_rejects_oversized_input_before_validation() -> Result<(), Box<dyn Error>> {
        let path = std::env::temp_dir().join(format!("hyphae-token-{}", Uuid::now_v7()));
        fs::write(
            &path,
            vec![b'x'; usize::try_from(MAX_BEARER_TOKEN_INPUT_BYTES + 1)?],
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        let result = load_bearer_token(Some(&path));
        let _ignored = fs::remove_file(path);
        let Err(error) = result else {
            return Err(std::io::Error::other("oversized bearer token input was accepted").into());
        };
        assert!(matches!(
            error.downcast_ref::<CliError>(),
            Some(CliError::InputTooLarge {
                input: "bearer token input",
                actual: 4_099,
                maximum: MAX_BEARER_TOKEN_INPUT_BYTES,
            })
        ));
        Ok(())
    }

    #[test]
    fn remote_bearer_token_enforces_canonical_limit_after_terminal_crlf()
    -> Result<(), Box<dyn Error>> {
        let path = std::env::temp_dir().join(format!("hyphae-token-{}", Uuid::now_v7()));
        let mut exact_with_crlf = vec![b'x'; usize::try_from(MAX_BEARER_TOKEN_BYTES)?];
        exact_with_crlf.extend_from_slice(b"\r\n");
        fs::write(&path, exact_with_crlf)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        let exact = load_remote_bearer_token(Some(&path))?;
        assert_eq!(
            exact.as_deref().map(str::len),
            Some(usize::try_from(MAX_BEARER_TOKEN_BYTES)?)
        );

        fs::write(
            &path,
            vec![b'x'; usize::try_from(MAX_BEARER_TOKEN_BYTES + 1)?],
        )?;
        let result = load_remote_bearer_token(Some(&path));
        let _ignored = fs::remove_file(path);
        let Err(error) = result else {
            return Err(std::io::Error::other("oversized remote bearer token was accepted").into());
        };
        assert!(matches!(
            error.downcast_ref::<CliError>(),
            Some(CliError::InputTooLarge {
                input: "bearer token",
                actual: 4_097,
                maximum: MAX_BEARER_TOKEN_BYTES,
            })
        ));
        Ok(())
    }
}
