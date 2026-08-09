// SPDX-License-Identifier: Apache-2.0

//! All three v2 SDKs against one real native daemon and HTTP server.

#![allow(clippy::too_many_lines)]

use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use hyphae_client::v2::{
    CancellationToken, ClientError, HyphaeClient, ProductOperation, ProductResponse, ProductValue,
    RequestOptions,
};
use hyphae_native_daemon::{NativeDaemon, NativeDaemonConfig};
use hyphae_native_product::{
    NativeProduct, NativeProductService, NativeProductServiceConfig, ProductAuthorization,
    ProductErrorCode,
};
use hyphae_server::{BearerToken, NativeHttpV2Config, NativeHttpV2Server};
use uuid::Uuid;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const DENIED_IDENTITY: &str = "hyphae-sdk-acceptance-denied";

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let path = std::env::temp_dir().join(format!("hyphae-sdk-v2-real-{}", Uuid::now_v7()));
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.0);
    }
}

fn options(request_id: u64) -> RequestOptions {
    RequestOptions {
        request_id: Some(request_id),
        ..RequestOptions::default()
    }
}

fn python_command() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

fn npm_command() -> &'static str {
    if cfg!(windows) { "npm.cmd" } else { "npm" }
}

fn near_deadline() -> Result<i64, Box<dyn std::error::Error>> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros();
    Ok(i64::try_from(now)?.saturating_add(5_000))
}

fn proof_operation() -> ProductOperation {
    ProductOperation::ExecuteSql {
        statement: "SELECT label FROM proof_items WHERE id = ?".into(),
        parameters: vec![ProductValue::Signed(7)],
    }
}

fn proof_limits() -> hyphae_native_product::proof::NativeProofGenerationLimits {
    let mut limits = hyphae_native_product::proof::NativeProofGenerationLimits::default();
    limits.witness.max_witness_bytes = 16 * 1024 * 1024;
    limits.witness.max_file_bytes = 16 * 1024 * 1024;
    limits.witness.max_total_file_bytes = 16 * 1024 * 1024;
    limits.witness.max_decoded_bytes = 16 * 1024 * 1024;
    limits
}

fn error_fields(
    error: ClientError,
) -> Result<BTreeMap<&'static str, String>, Box<dyn std::error::Error>> {
    let ClientError::Product(error) = error else {
        return Err("SDK result was not a typed ProductError".into());
    };
    Ok(BTreeMap::from([
        ("code", error.code().as_str().to_owned()),
        ("category", error.category().as_str().to_owned()),
        ("retry", error.retry().as_str().to_owned()),
        ("message", error.message().to_owned()),
        (
            "request_id",
            error
                .request_id()
                .map_or_else(String::new, |value| value.to_string()),
        ),
        (
            "trace_id",
            error
                .trace_id()
                .map_or_else(String::new, |value| value.to_string()),
        ),
        (
            "object_id",
            error
                .object_id()
                .map_or_else(String::new, |value| value.get().to_string()),
        ),
        (
            "transaction_state",
            error.transaction_state().as_str().to_owned(),
        ),
        (
            "transaction_id",
            error
                .details()
                .transaction_id()
                .map_or_else(String::new, |value| value.get().to_string()),
        ),
        ("limit", format!("{:?}", error.limit())),
        ("source_span", format!("{:?}", error.source_span())),
        ("details", format!("{:?}", error.details())),
    ]))
}

fn required_error(
    result: Result<ProductResponse, ClientError>,
    message: &'static str,
) -> Result<ClientError, Box<dyn std::error::Error>> {
    result.err().ok_or_else(|| message.into())
}

fn assert_expected(error: &BTreeMap<&str, String>, code: ProductErrorCode, request_id: u64) {
    assert_eq!(error["code"], code.as_str());
    assert_eq!(error["request_id"], request_id.to_string());
    assert!(!error["category"].is_empty());
    assert!(!error["retry"].is_empty());
    assert!(!error["message"].is_empty());
}

async fn rust_transport_acceptance(
    endpoint: &str,
    local: &HyphaeClient,
    denied_local: &HyphaeClient,
    http: &HyphaeClient,
    denied_http: &HyphaeClient,
) -> Result<Vec<(Vec<u8>, Vec<u8>, [u8; 32])>, Box<dyn std::error::Error>> {
    for (offset, (code, operation, configure)) in [
        (
            ProductErrorCode::SqlInvalidSyntax,
            ProductOperation::ExecuteSql {
                statement: "SELEC bad".into(),
                parameters: vec![],
            },
            None,
        ),
        (
            ProductErrorCode::CatalogObjectNotFound,
            ProductOperation::CatalogObject {
                id: hyphae_native_product::ObjectId::new(999)?,
            },
            None,
        ),
        (
            ProductErrorCode::LimitExceeded,
            ProductOperation::ExecuteSql {
                statement: "SELECT id FROM proof_items".into(),
                parameters: vec![],
            },
            Some("limit"),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let request_id = 10_100 + u64::try_from(offset)?;
        let mut request = options(request_id);
        if configure == Some("limit") {
            request.limits.max_request_bytes = 1;
        }
        let local_error = error_fields(required_error(
            local.execute(operation.clone(), request.clone()).await,
            "local failure accepted",
        )?)?;
        let http_error = error_fields(required_error(
            http.execute(operation, request).await,
            "HTTP failure accepted",
        )?)?;
        assert_eq!(local_error, http_error);
        assert_expected(&local_error, code, request_id);
    }

    let expired_id = 10_110;
    let mut expired = options(expired_id);
    expired.deadline_micros = Some(near_deadline()?);
    let deadline_local = HyphaeClient::local(endpoint)?;
    let local_error = error_fields(required_error(
        deadline_local
            .prove(proof_operation(), proof_limits(), expired.clone())
            .await,
        "expired local request accepted",
    )?)?;
    expired.deadline_micros = Some(near_deadline()?);
    let http_error = error_fields(required_error(
        http.prove(proof_operation(), proof_limits(), expired).await,
        "expired HTTP request accepted",
    )?)?;
    assert_eq!(local_error, http_error);
    assert_expected(&local_error, ProductErrorCode::DeadlineExceeded, expired_id);

    let cancelled_id = 10_111;
    let local_token = CancellationToken::new();
    let cancel_local = local_token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(5)).await;
        cancel_local.cancel();
    });
    let cancelled = RequestOptions {
        request_id: Some(cancelled_id),
        cancellation: local_token,
        ..RequestOptions::default()
    };
    let cancellation_local = HyphaeClient::local(endpoint)?;
    let local_error = error_fields(required_error(
        cancellation_local
            .prove(proof_operation(), proof_limits(), cancelled)
            .await,
        "cancelled local request accepted",
    )?)?;
    let http_token = CancellationToken::new();
    let cancel_http = http_token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(5)).await;
        cancel_http.cancel();
    });
    let cancelled = RequestOptions {
        request_id: Some(cancelled_id),
        cancellation: http_token,
        ..RequestOptions::default()
    };
    let http_error = error_fields(required_error(
        http.prove(proof_operation(), proof_limits(), cancelled)
            .await,
        "cancelled HTTP request accepted",
    )?)?;
    assert_eq!(local_error, http_error);
    assert_expected(&local_error, ProductErrorCode::Cancelled, cancelled_id);

    let authorization_id = 10_112;
    let local_error = error_fields(required_error(
        denied_local
            .structure_get(b"denied".to_vec(), options(authorization_id))
            .await,
        "denied local request accepted",
    )?)?;
    let http_error = error_fields(required_error(
        denied_http
            .structure_get(b"denied".to_vec(), options(authorization_id))
            .await,
        "denied HTTP request accepted",
    )?)?;
    assert_eq!(local_error, http_error);
    assert_expected(
        &local_error,
        ProductErrorCode::AuthorizationDenied,
        authorization_id,
    );

    let mut artifacts = Vec::new();
    let proven = local
        .prove(proof_operation(), proof_limits(), options(10_120))
        .await?;
    let ProductResponse::Proven { artifact, .. } = proven else {
        return Err("local proof response missing".into());
    };
    assert!(artifact.proof_bytes.starts_with(b"HYNPRF02"));
    assert!(artifact.witness_bytes.starts_with(b"HYNWIT02"));
    let artifact = (
        artifact.proof_bytes,
        artifact.witness_bytes,
        artifact.trusted_anchor.digest(),
    );
    let verified = http
        .verify_proof(
            artifact.0.clone(),
            artifact.1.clone(),
            artifact.2,
            options(10_121),
        )
        .await?;
    assert!(
        matches!(verified, ProductResponse::ProofVerification(report) if report.semantic_reexecution_performed)
    );
    artifacts.push(artifact);

    let proven = http
        .prove(proof_operation(), proof_limits(), options(10_122))
        .await?;
    let ProductResponse::Proven { artifact, .. } = proven else {
        return Err("HTTP proof response missing".into());
    };
    assert!(artifact.proof_bytes.starts_with(b"HYNPRF02"));
    assert!(artifact.witness_bytes.starts_with(b"HYNWIT02"));
    let artifact = (
        artifact.proof_bytes,
        artifact.witness_bytes,
        artifact.trusted_anchor.digest(),
    );
    let proof_local = HyphaeClient::local(endpoint)?;
    let verified = proof_local
        .verify_proof(
            artifact.0.clone(),
            artifact.1.clone(),
            artifact.2,
            options(10_123),
        )
        .await?;
    assert!(
        matches!(verified, ProductResponse::ProofVerification(report) if report.semantic_reexecution_performed)
    );
    artifacts.push(artifact);
    Ok(artifacts)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "G6 hosted suite installs the TypeScript toolchain before running this test"]
async fn all_sdks_match_typed_errors_and_verify_origin_free_native_proofs()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = TestDirectory::new()?;
    let data = temporary.0.join("origin");
    let socket = std::env::temp_dir().join(format!("hy-sdk-{}.sock", Uuid::now_v7()));
    let mut product = NativeProduct::create(&data)?;
    let mut session = hyphae_native_product::ProductSession::new(
        hyphae_native_product::ProductSessionId::new(1).ok_or("session")?,
        hyphae_native_product::ProductPrincipal::new("sdk-seed").ok_or("principal")?,
        ProductAuthorization::ALL,
    );
    for (request_id, statement, parameters) in [
        (
            1,
            "CREATE TABLE proof_items (id BIGINT PRIMARY KEY, label TEXT NOT NULL)",
            vec![],
        ),
        (
            2,
            "INSERT INTO proof_items (id, label) VALUES (?, ?)",
            vec![ProductValue::Signed(7), ProductValue::Text("seven".into())],
        ),
    ] {
        let context = hyphae_native_product::ProductRequestContext::new(
            request_id,
            session.id(),
            0,
            session.principal().clone(),
            session.authorization(),
        );
        product.dispatch(
            &mut session,
            &context,
            ProductOperation::ExecuteSql {
                statement: statement.into(),
                parameters,
            },
        )?;
    }
    let load_context = hyphae_native_product::ProductRequestContext::new(
        3,
        session.id(),
        0,
        session.principal().clone(),
        session.authorization(),
    );
    product.dispatch(
        &mut session,
        &load_context,
        ProductOperation::StructureSet {
            key: b"transport-cancellation-load".to_vec(),
            value: vec![0x5a; 64 * 1024],
            expires_at_micros: None,
        },
    )?;
    drop(session);
    let service = NativeProductService::start(product, NativeProductServiceConfig::default())?;
    let handle = service.handle();
    let daemon = NativeDaemon::start_with_service_for_acceptance(
        service,
        socket.to_string_lossy(),
        NativeDaemonConfig::default(),
        DENIED_IDENTITY,
    )?;
    let bound = NativeHttpV2Server::new(
        handle,
        NativeHttpV2Config {
            bind: "127.0.0.1:0".parse()?,
            bearer_token: Some(BearerToken::new(TOKEN)?),
            ..NativeHttpV2Config::default()
        },
    )?
    .bind()
    .await?;
    let origin = format!("http://{}", bound.local_addr());
    let (shutdown, receive) = tokio::sync::oneshot::channel::<()>();
    let http_server = tokio::spawn(bound.run_with_shutdown(async move {
        let _ignored = receive.await;
    }));

    let local = HyphaeClient::local(socket.to_string_lossy())?;
    let denied_local =
        HyphaeClient::local_with_identity(socket.to_string_lossy(), DENIED_IDENTITY)?;
    let origin_free_local = HyphaeClient::local(socket.to_string_lossy())?;
    let _ready = origin_free_local.capabilities(options(10_127)).await?;
    let http =
        HyphaeClient::new(hyphae_client::v2::HttpTransport::new(&origin)?.bearer_token(TOKEN)?);
    let denied_http = HyphaeClient::http(&origin)?;
    let rust_artifacts = rust_transport_acceptance(
        &socket.to_string_lossy(),
        &local,
        &denied_local,
        &http,
        &denied_http,
    )
    .await?;

    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let python_artifact = temporary.0.join("python-proof.bin");
    let python_script = include_str!("fixtures/sdk_v2_acceptance.py");
    let python_output = Command::new(python_command())
        .arg("-c")
        .arg(python_script)
        .env("PYTHONPATH", workspace.join("sdks/python/src"))
        .env("HYPHAE_SOCKET", &socket)
        .env("HYPHAE_ORIGIN", &origin)
        .env("HYPHAE_TOKEN", TOKEN)
        .env("HYPHAE_DENIED_IDENTITY", DENIED_IDENTITY)
        .env("HYPHAE_PYTHON_ARTIFACT", &python_artifact)
        .output()?;
    assert!(
        python_output.status.success(),
        "Python SDK acceptance failed: {}",
        String::from_utf8_lossy(&python_output.stderr)
    );

    let typescript = workspace.join("sdks/typescript");
    let typescript_artifact = temporary.0.join("typescript-proof.bin");
    let build = Command::new(npm_command())
        .arg("run")
        .arg("build")
        .current_dir(&typescript)
        .output()?;
    assert!(
        build.status.success(),
        "TypeScript SDK build failed: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let node_output = Command::new("node")
        .arg("test/sdk-v2-acceptance.mjs")
        .current_dir(&typescript)
        .env("HYPHAE_SOCKET", &socket)
        .env("HYPHAE_ORIGIN", &origin)
        .env("HYPHAE_TOKEN", TOKEN)
        .env("HYPHAE_DENIED_IDENTITY", DENIED_IDENTITY)
        .env("HYPHAE_TYPESCRIPT_ARTIFACT", &typescript_artifact)
        .output()?;
    assert!(
        node_output.status.success(),
        "TypeScript SDK acceptance failed: {}",
        String::from_utf8_lossy(&node_output.stderr)
    );

    #[cfg(unix)]
    let removed_origin = temporary.0.join("origin-removed");
    #[cfg(unix)]
    fs::rename(&data, &removed_origin)?;
    for (offset, (proof, witness, anchor)) in rust_artifacts.into_iter().enumerate() {
        let verified = http
            .verify_proof(
                proof.clone(),
                witness.clone(),
                anchor,
                options(10_124 + u64::try_from(offset)?),
            )
            .await?;
        assert!(
            matches!(verified, ProductResponse::ProofVerification(report) if report.semantic_reexecution_performed)
        );
        if offset == 0 {
            let verified = origin_free_local
                .verify_proof(proof, witness, anchor, options(10_128))
                .await?;
            assert!(
                matches!(verified, ProductResponse::ProofVerification(report) if report.semantic_reexecution_performed)
            );
        }
    }
    for (offset, path) in [python_artifact, typescript_artifact]
        .into_iter()
        .enumerate()
    {
        let encoded = fs::read(path)?;
        let mut cursor = 0_usize;
        let mut take = || -> Result<Vec<u8>, Box<dyn std::error::Error>> {
            let length = usize::try_from(u64::from_le_bytes(
                encoded
                    .get(cursor..cursor + 8)
                    .ok_or("artifact length")?
                    .try_into()?,
            ))?;
            cursor += 8;
            let value = encoded
                .get(cursor..cursor + length)
                .ok_or("artifact value")?
                .to_vec();
            cursor += length;
            Ok(value)
        };
        let proof = take()?;
        let witness = take()?;
        let anchor: [u8; 32] = take()?.try_into().map_err(|_| "artifact anchor")?;
        let consumed = cursor;
        assert_eq!(consumed, encoded.len());
        let verified = http
            .verify_proof(
                proof.clone(),
                witness.clone(),
                anchor,
                options(10_126 + u64::try_from(offset)?),
            )
            .await?;
        assert!(
            matches!(verified, ProductResponse::ProofVerification(report) if report.semantic_reexecution_performed)
        );
    }
    let python_origin_free = Command::new(python_command())
        .arg("-c")
        .arg(include_str!("fixtures/sdk_v2_origin_free.py"))
        .env("PYTHONPATH", workspace.join("sdks/python/src"))
        .env("HYPHAE_ORIGIN", &origin)
        .env("HYPHAE_TOKEN", TOKEN)
        .env("HYPHAE_ARTIFACT", temporary.0.join("python-proof.bin"))
        .output()?;
    assert!(
        python_origin_free.status.success(),
        "Python origin-free verification failed: {}",
        String::from_utf8_lossy(&python_origin_free.stderr)
    );
    let typescript_origin_free = Command::new("node")
        .arg("test/sdk-v2-origin-free.mjs")
        .current_dir(&typescript)
        .env("HYPHAE_ORIGIN", &origin)
        .env("HYPHAE_TOKEN", TOKEN)
        .env("HYPHAE_ARTIFACT", temporary.0.join("typescript-proof.bin"))
        .output()?;
    assert!(
        typescript_origin_free.status.success(),
        "TypeScript origin-free verification failed: {}",
        String::from_utf8_lossy(&typescript_origin_free.stderr)
    );
    #[cfg(unix)]
    fs::rename(&removed_origin, &data)?;

    let blob_stage = data.join("tmp/blobs");
    fs::remove_dir(&blob_stage)?;
    fs::write(&blob_stage, b"force real blob-stage I/O failure")?;

    let unknown_script = include_str!("fixtures/sdk_v2_unknown_commit.py");
    let python_unknown = Command::new(python_command())
        .arg("-c")
        .arg(unknown_script)
        .env("PYTHONPATH", workspace.join("sdks/python/src"))
        .env("HYPHAE_SOCKET", &socket)
        .env("HYPHAE_ORIGIN", &origin)
        .env("HYPHAE_TOKEN", TOKEN)
        .output()?;
    assert!(
        python_unknown.status.success(),
        "Python unknown-commit acceptance failed: {}",
        String::from_utf8_lossy(&python_unknown.stderr)
    );

    let unknown_local = HyphaeClient::local(socket.to_string_lossy())?;
    let unknown_http =
        HyphaeClient::new(hyphae_client::v2::HttpTransport::new(&origin)?.bearer_token(TOKEN)?);
    let mut rust_unknown = Vec::new();
    for (client, row_id) in [(&unknown_local, 8_i64), (&unknown_http, 13_i64)] {
        let error = error_fields(required_error(
            client
                .structure_set(
                    format!("unknown-{row_id}").into_bytes(),
                    vec![b'r'; 9_000],
                    None,
                    options(10_132),
                )
                .await,
            "Rust unknown commit acknowledged",
        )?)?;
        assert_expected(&error, ProductErrorCode::UnknownCommit, 10_132);
        assert_eq!(error["transaction_state"], "outcome-unknown");
        assert!(!error["transaction_id"].is_empty());
        rust_unknown.push(error);
    }
    for field in [
        "code",
        "category",
        "retry",
        "message",
        "request_id",
        "transaction_state",
        "limit",
        "source_span",
    ] {
        assert_eq!(rust_unknown[0][field], rust_unknown[1][field]);
    }
    drop(unknown_local);
    drop(unknown_http);

    let node_unknown = Command::new("node")
        .arg("test/sdk-v2-unknown-commit.mjs")
        .current_dir(&typescript)
        .env("HYPHAE_SOCKET", &socket)
        .env("HYPHAE_ORIGIN", &origin)
        .env("HYPHAE_TOKEN", TOKEN)
        .output()?;
    assert!(
        node_unknown.status.success(),
        "TypeScript unknown-commit acceptance failed: {}",
        String::from_utf8_lossy(&node_unknown.stderr)
    );

    let _ignored = shutdown.send(());
    http_server.await??;
    drop(local);
    drop(denied_local);
    drop(origin_free_local);
    drop(http);
    drop(denied_http);
    tokio::time::sleep(Duration::from_millis(20)).await;
    let _product = daemon.shutdown().await?;
    let _ignored = fs::remove_file(socket);
    Ok(())
}
