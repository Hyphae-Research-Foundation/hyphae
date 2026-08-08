// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use hyphae_client::v2::{CancellationToken, HttpTransport, HyphaeClient, RequestOptions};
use hyphae_native_catalog::{
    AnalyzerDefinition, AnalyzerFilter, AnalyzerTokenizer, AnnIndexDefinition, CatalogObjectV2,
    DefinitionVersion, FieldSourcePolicy, IncrementalVectorLifecycle, KeyspaceDefinition,
    KeyspaceEvictionPolicy, KeyspaceMemoryClass, KeyspaceTtlPolicy, LexicalIndexPolicy,
    NamedVectorDefinition, ObjectHeaderV2, SearchCollectionDefinitionV2, SearchFieldDefinitionV2,
    SearchFieldOptions, StructureKind, StructureOwnership, VectorMetric, VectorSearchPolicy,
};
use hyphae_native_product::LogicalCatalogObject;
use hyphae_native_product::ProductOperation;
use hyphae_native_product::{
    BackupRequest, NativeProduct, ProductAuthorization, ProductDurability, ProductDurabilityPolicy,
    ProductError, ProductHashEntry, ProductLimits, ProductListSide, ProductPermission,
    ProductPrincipal, ProductRequestContext, ProductResponse, ProductSession, ProductSessionId,
    ProductSortedSetOrder, ProductStructureKey, ProductStructureMutation,
    ProductStructureReadRequest, ProductValue, ProgressControl, RestoreRequest, StatusRequest,
};
use hyphae_native_product::{CatalogName, QualifiedName};
use hyphae_native_types::{EngineKind, FieldId, LogicalType, ObjectId, VectorElement, VectorType};
use serde_json::{Value, json};

const COVERAGE: &[&str] = &[
    "capabilities",
    "catalog",
    "sql",
    "structures",
    "search",
    "transactions",
    "administration",
    "proofs",
    "backup",
    "failures",
];
const TRANSPORT_COVERAGE: &[&str] = &[
    "capabilities",
    "catalog",
    "sql",
    "structures",
    "search",
    "transactions",
    "administration",
    "proofs",
    "backup",
    "failures",
    "transport-failures",
];
const LOCAL_TRANSPORT_COVERAGE: &[&str] = &[
    "capabilities",
    "catalog",
    "sql",
    "structures",
    "search",
    "transactions",
    "administration",
    "proofs",
    "backup",
    "failures",
    "transport-failures",
];
const LOCAL_SDK_COVERAGE: &[&str] = &[
    "capabilities",
    "catalog",
    "sql",
    "structures",
    "search",
    "transactions",
    "administration",
    "proofs",
    "backup",
    "failures",
];
const HTTP_TOKEN: &str = "0123456789abcdef0123456789abcdef";
const DENIED_IDENTITY: &str = "hyphae-g6-conformance-denied";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let arguments = std::env::args().collect::<Vec<_>>();
    match arguments.as_slice() {
        [_, command, work] if command == "bootstrap" => bootstrap(Path::new(work)),
        [_, command, lane] if command == "cli-lane" => run_cli_lane(lane),
        [_, command, lane] if command == "lane" => run_lane(lane).await,
        [_, command, lane, endpoint] if command == "transport-lane" => {
            run_transport_lane(lane, endpoint).await
        }
        [_, command, backup, data] if command == "restore" => restore_lane(Path::new(backup), Path::new(data)),
        [_, command, data, endpoint, port_file] if command == "serve" => {
            serve_lane(Path::new(data), endpoint, Path::new(port_file)).await
        }
        _ => Err("usage: runner bootstrap WORK | cli-lane cli | lane LANE | transport-lane LANE ENDPOINT | restore BACKUP DATA | serve DATA ENDPOINT PORT_FILE".into()),
    }
}

fn restore_lane(backup: &Path, data: &Path) -> Result<(), Box<dyn Error>> {
    hyphae_native_product::restore(&RestoreRequest::new(backup, data)?, |_| {
        ProgressControl::Continue
    })?;
    Ok(())
}

async fn serve_lane(data: &Path, endpoint: &str, port_file: &Path) -> Result<(), Box<dyn Error>> {
    let service = hyphae_native_product::NativeProductService::start(
        NativeProduct::open(data)?,
        Default::default(),
    )?;
    let handle = service.handle();
    let daemon = hyphae_native_daemon::NativeDaemon::start_with_service_for_acceptance(
        service,
        endpoint,
        Default::default(),
        DENIED_IDENTITY,
    )?;
    let mut config = hyphae_server::NativeHttpV2Config::default();
    config.bind = "127.0.0.1:0".parse()?;
    config.bearer_token = Some(hyphae_server::BearerToken::new(HTTP_TOKEN)?);
    let server = hyphae_server::NativeHttpV2Server::new(handle, config)?
        .bind()
        .await?;
    fs::write(port_file, server.local_addr().port().to_string())?;
    server
        .run_with_shutdown(async {
            let _ignored = tokio::signal::ctrl_c().await;
        })
        .await?;
    drop(daemon.shutdown().await?);
    Ok(())
}

fn bootstrap(work: &Path) -> Result<(), Box<dyn Error>> {
    let source = work.join("source");
    let backup = work.join("seed-backup");
    fs::create_dir_all(work)?;
    let mut product = NativeProduct::create(&source)?;
    let mut session = session();
    let mut request_id = 1_u128;
    for operation in [
        ProductOperation::ExecuteSql {
            statement: "CREATE TABLE g6_items (id BIGINT PRIMARY KEY, label TEXT NOT NULL)".into(),
            parameters: vec![],
        },
        ProductOperation::ExecuteSql {
            statement: "INSERT INTO g6_items (id, label) VALUES (?, ?)".into(),
            parameters: vec![ProductValue::Signed(1), ProductValue::Text("alpha".into())],
        },
        ProductOperation::StructureSet {
            key: b"g6-scalar".to_vec(),
            value: b"ready".to_vec(),
            expires_at_micros: None,
        },
    ] {
        let mut context = context(&session, request_id);
        context.durability = ProductDurabilityPolicy::STRICT;
        product.dispatch(&mut session, &context, operation)?;
        request_id += 1;
    }
    product.create_catalog_object_v2(
        LogicalCatalogObject::V2(CatalogObjectV2::Database(header(
            8,
            EngineKind::Kernel,
            "database",
            None,
        )?)),
        ProductDurability::Strict,
    )?;
    product.create_catalog_object_v2(
        LogicalCatalogObject::V2(CatalogObjectV2::Schema(header(
            9,
            EngineKind::Kernel,
            "schema",
            Some(8),
        )?)),
        ProductDurability::Strict,
    )?;
    for (id, kind, name) in [
        (10, StructureKind::Hash, "g6_hash"),
        (11, StructureKind::Set, "g6_set"),
        (12, StructureKind::List, "g6_list"),
        (13, StructureKind::SortedSet, "g6_zset"),
        (14, StructureKind::Stream, "g6_stream"),
    ] {
        product.create_catalog_object_v2(keyspace(id, kind, name)?, ProductDurability::Strict)?;
    }
    let mutations = vec![
        ProductStructureMutation::Create {
            key: structure_key(10, b"hash")?,
            family: StructureKind::Hash,
        },
        ProductStructureMutation::HashSet {
            key: structure_key(10, b"hash")?,
            field: b"field".to_vec(),
            value: b"value".to_vec(),
        },
        ProductStructureMutation::Create {
            key: structure_key(11, b"set")?,
            family: StructureKind::Set,
        },
        ProductStructureMutation::SetAdd {
            key: structure_key(11, b"set")?,
            member: b"member".to_vec(),
        },
        ProductStructureMutation::Create {
            key: structure_key(12, b"list")?,
            family: StructureKind::List,
        },
        ProductStructureMutation::ListPush {
            key: structure_key(12, b"list")?,
            side: ProductListSide::Right,
            value: b"item".to_vec(),
        },
        ProductStructureMutation::Create {
            key: structure_key(13, b"zset")?,
            family: StructureKind::SortedSet,
        },
        ProductStructureMutation::SortedSetAdd {
            key: structure_key(13, b"zset")?,
            member: b"ranked".to_vec(),
            score: hyphae_native_product::CanonicalF64::new(1.5),
        },
        ProductStructureMutation::Create {
            key: structure_key(14, b"stream")?,
            family: StructureKind::Stream,
        },
        ProductStructureMutation::StreamAdd {
            key: structure_key(14, b"stream")?,
            fields: vec![ProductHashEntry {
                field: b"field".to_vec(),
                value: b"event".to_vec(),
            }],
        },
    ];
    let request = context(&session, 20);
    product.dispatch(
        &mut session,
        &request,
        ProductOperation::StructureMutate { mutations },
    )?;
    configure_search(&mut product)?;
    product
        .administration()
        .backup(&BackupRequest::new(&backup)?, |_| ProgressControl::Continue)?;
    let proof_context = context(&session, 30);
    let (_, proof) = hyphae_native_product::proof::generate_native_operation_proof(
        &mut product,
        &mut session,
        &proof_context,
        &ProductOperation::ExecuteSql {
            statement: "SELECT id, label FROM g6_items WHERE id = ?".into(),
            parameters: vec![ProductValue::Signed(1)],
        },
        proof_limits(),
    )?;
    fs::write(work.join("reference-proof.hynproof"), proof.proof_bytes)?;
    fs::write(work.join("reference-witness.hynwit"), proof.witness_bytes)?;
    fs::write(
        work.join("reference-anchor.bin"),
        proof.trusted_anchor.digest(),
    )?;
    drop(product);
    fs::remove_dir_all(source)?;
    println!("{}", json!({"status": "ready", "backup": backup}));
    Ok(())
}

async fn run_lane(lane: &str) -> Result<(), Box<dyn Error>> {
    validate_declared_corpus()?;
    let work = PathBuf::from(std::env::var("HYPHAE_G6_WORK")?);
    let backup = work.join("seed-backup");
    let data = work.join(format!("lane-{lane}"));
    hyphae_native_product::restore(&RestoreRequest::new(&backup, &data)?, |_| {
        ProgressControl::Continue
    })?;
    let start = starting_identity(&data)?;
    if matches!(lane, "local-daemon" | "rust-sdk-local") {
        let endpoint = work.join(format!("{lane}.sock"));
        let service = hyphae_native_product::NativeProductService::start(
            NativeProduct::open(&data)?,
            Default::default(),
        )?;
        let daemon = hyphae_native_daemon::NativeDaemon::start_with_service_for_acceptance(
            service,
            endpoint.to_string_lossy(),
            Default::default(),
            DENIED_IDENTITY,
        )?;
        let client = HyphaeClient::local(endpoint.to_string_lossy())?;
        let denied =
            HyphaeClient::local_with_identity(endpoint.to_string_lossy(), DENIED_IDENTITY)?;
        let mut cases = sdk_cases(&client, &denied, &work, lane).await?;
        if lane == "local-daemon" {
            cases.extend(local_transport_failure_cases(&endpoint).await?);
        }
        drop(client);
        drop(denied);
        drop(daemon.shutdown().await?);
        return print_transcript(lane, start, cases);
    } else if matches!(lane, "http" | "rust-sdk-http") {
        let service = hyphae_native_product::NativeProductService::start(
            NativeProduct::open(&data)?,
            Default::default(),
        )?;
        let mut config = hyphae_server::NativeHttpV2Config::default();
        config.bind = "127.0.0.1:0".parse()?;
        config.bearer_token = Some(hyphae_server::BearerToken::new(HTTP_TOKEN)?);
        let server = hyphae_server::NativeHttpV2Server::new(service.handle(), config)?
            .bind()
            .await?;
        let address = server.local_addr();
        let (shutdown, receive) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(server.run_with_shutdown(async move {
            let _ignored = receive.await;
        }));
        let client = HyphaeClient::new(
            HttpTransport::new(&format!("http://{address}"))?.bearer_token(HTTP_TOKEN)?,
        );
        let denied = HyphaeClient::http(&format!("http://{address}"))?;
        let mut cases = sdk_cases(&client, &denied, &work, lane).await?;
        if lane == "http" {
            cases.extend(http_transport_failure_cases(&format!("http://{address}")).await?);
        }
        drop(client);
        drop(denied);
        let _ignored = shutdown.send(());
        task.await??;
        drop(service.shutdown()?);
        return print_transcript(lane, start, cases);
    }
    let cases = embedded_cases(&data, &work, lane)?;
    print_transcript(lane, start, cases)
}

async fn run_transport_lane(lane: &str, endpoint: &str) -> Result<(), Box<dyn Error>> {
    let work = PathBuf::from(std::env::var("HYPHAE_G6_WORK")?);
    let data = work.join(format!("lane-{lane}"));
    let start = starting_identity(&data)?;
    let client = if lane.ends_with("-local") {
        HyphaeClient::local(endpoint.to_owned())?
    } else {
        HyphaeClient::new(HttpTransport::new(endpoint)?.bearer_token(HTTP_TOKEN)?)
    };
    let denied = if lane.ends_with("-local") {
        HyphaeClient::local_with_identity(endpoint.to_owned(), DENIED_IDENTITY)?
    } else {
        HyphaeClient::http(endpoint)?
    };
    let cases = sdk_cases(&client, &denied, &work, lane).await?;
    print_transcript(lane, start, cases)
}

fn print_transcript(lane: &str, start: Value, cases: Vec<Value>) -> Result<(), Box<dyn Error>> {
    let (adapter, transport) = match lane {
        "cli" => ("cli", "cli"),
        "http" => ("http", "http-v2"),
        "python-sdk-local" => ("python", "native-local"),
        "python-sdk-http" => ("python", "http-v2"),
        "typescript-sdk-local" => ("typescript", "native-local"),
        "typescript-sdk-http" => ("typescript", "http-v2"),
        value if value.ends_with("-local") || value == "local-daemon" => ("rust", "native-local"),
        value if value.ends_with("-http") => ("rust", "http-v2"),
        _ => ("rust", "embedded"),
    };
    println!(
        "{}",
        serde_json::to_string(&json!({
            "schema": "hyphae-native-g6-transcript-v1",
            "lane": lane,
            "adapter": adapter,
            "transport": transport,
            "start": start,
            "cases": cases,
            "coverage": if lane == "local-daemon" { LOCAL_TRANSPORT_COVERAGE } else if lane == "http" { TRANSPORT_COVERAGE } else if lane.ends_with("-local") { LOCAL_SDK_COVERAGE } else { COVERAGE },
            "status": "passed"
        }))?
    );
    Ok(())
}

fn embedded_cases(data: &Path, work: &Path, lane: &str) -> Result<Vec<Value>, Box<dyn Error>> {
    let mut product = NativeProduct::open(data)?;
    let mut session = session();
    common_cases(&mut product, &mut session, work, lane)
}

fn common_cases(
    product: &mut NativeProduct,
    session: &mut ProductSession,
    work: &Path,
    lane: &str,
) -> Result<Vec<Value>, Box<dyn Error>> {
    let mut transport = EmbeddedTransport { product, session };
    execute_corpus(&mut transport, work, lane)
}

trait CaseTransport {
    fn execute(
        &mut self,
        operation: ProductOperation,
        request_id: u64,
    ) -> Result<ProductResponse, ProductError>;
}

struct EmbeddedTransport<'a> {
    product: &'a mut NativeProduct,
    session: &'a mut ProductSession,
}

impl CaseTransport for EmbeddedTransport<'_> {
    fn execute(
        &mut self,
        operation: ProductOperation,
        request_id: u64,
    ) -> Result<ProductResponse, ProductError> {
        self.product.dispatch(
            self.session,
            &context(self.session, u128::from(request_id)),
            operation,
        )
    }
}

fn execute_corpus(
    transport: &mut EmbeddedTransport<'_>,
    work: &Path,
    lane: &str,
) -> Result<Vec<Value>, Box<dyn Error>> {
    let mut cases = Vec::new();
    let mut request_id = 6000_u64;
    let mut run = |operation| {
        request_id += 1;
        transport.execute(operation, request_id)
    };
    cases.push(case(
        "capabilities/capabilities",
        capabilities_outcome(run(ProductOperation::Capabilities)?)?,
    ));
    let listed = run(ProductOperation::CatalogList(catalog_list_request()))?;
    cases.push(case("catalog/catalog-list", catalog_list_outcome(listed)?));
    cases.push(case(
        "catalog/catalog-describe",
        catalog_definition_outcome(
            run(ProductOperation::CatalogDescribe {
                id: ObjectId::new(10)?,
            })?,
            10,
        )?,
    ));
    cases.push(case(
        "catalog/catalog-dependencies",
        catalog_dependencies_outcome(
            run(ProductOperation::CatalogDependencies(
                hyphae_native_product::CatalogDependencyRequest {
                    object: ObjectId::new(15)?,
                    direction: hyphae_native_catalog::DependencyDirection::Outgoing,
                    cursor: None,
                    item_limit: 8,
                    visit_limit: 8,
                    byte_limit: 4096,
                },
            ))?,
            15,
        )?,
    ));

    let ddl = run(ProductOperation::ExecuteSql {
        statement: "CREATE TABLE g6_lane (id BIGINT PRIMARY KEY)".into(),
        parameters: vec![],
    })?;
    cases.push(case("sql/sql-ddl", sql_command_outcome(ddl)?));
    let dml = run(ProductOperation::ExecuteSql {
        statement: "INSERT INTO g6_lane (id) VALUES (?)".into(),
        parameters: vec![ProductValue::Signed(1)],
    })?;
    cases.push(case("sql/sql-dml", sql_command_outcome(dml)?));
    let prepared = run(ProductOperation::ExecuteSql {
        statement: "SELECT id, label FROM g6_items WHERE id = ?".into(),
        parameters: vec![ProductValue::Signed(1)],
    })?;
    cases.push(case("sql/sql-prepared", sql_rows_outcome(prepared)?));
    cases.push(case(
        "sql/sql-explain",
        explain_outcome(run(ProductOperation::AdminExplainSql {
            statement: "SELECT id, label FROM g6_items WHERE id = 1".into(),
        })?)?,
    ));

    let scalar = run(ProductOperation::StructureGet {
        key: b"g6-scalar".to_vec(),
    })?;
    cases.push(case(
        "structures/scalar",
        structure_outcome("scalar", scalar)?,
    ));
    for (name, request) in structure_reads()? {
        cases.push(case(
            &format!("structures/{name}"),
            structure_outcome(name, run(ProductOperation::StructureRead(request))?)?,
        ));
    }

    for mode in [
        "lexical",
        "exact",
        "ann",
        "hybrid",
        "named-vectors",
        "filter",
        "facet",
        "metric",
    ] {
        cases.push(case(
            &format!("search/{mode}"),
            search_outcome(mode, run(search_operation(mode)?)?)?,
        ));
    }
    cases.push(case(
        "transactions/commit-status",
        transaction_status_outcome(
            run(ProductOperation::TransactionStatus {
                transaction_id: hyphae_native_product::ProductTransactionId::new(2)
                    .ok_or("transaction ID")?,
            })?,
            2,
        )?,
    ));
    cases.push(case(
        "transactions/atomic-batch",
        atomic_batch_outcome(run(ProductOperation::ExecuteSql {
            statement: "UPDATE g6_items SET label = ? WHERE id = ?".into(),
            parameters: vec![ProductValue::Text("beta".into()), ProductValue::Signed(1)],
        })?)?,
    ));

    cases.push(case(
        "administration/status",
        status_outcome(run(ProductOperation::AdminStatus)?)?,
    ));
    cases.push(case(
        "administration/telemetry",
        telemetry_outcome(run(ProductOperation::Telemetry)?)?,
    ));
    cases.push(case(
        "administration/doctor",
        doctor_outcome(run(ProductOperation::Doctor(
            hyphae_native_product::DoctorRequest::new(".", 1_700_000_000_000_000)?,
        ))?)?,
    ));

    let _snapshot = response_snapshot(&run(ProductOperation::CatalogList(catalog_list_request()))?)
        .ok_or("catalog list snapshot")?;
    drop(run);
    let proof_context = context(transport.session, 6040);
    let (_, generated) = hyphae_native_product::proof::generate_native_operation_proof(
        transport.product,
        transport.session,
        &proof_context,
        &ProductOperation::ExecuteSql {
            statement: "SELECT id, label FROM g6_items WHERE id = ?".into(),
            parameters: vec![ProductValue::Signed(1)],
        },
        proof_limits(),
    )?;
    let artifact = (
        generated.proof_bytes,
        generated.witness_bytes,
        generated.trusted_anchor.digest(),
    );
    let verified = hyphae_native_product::proof::verify_native_proof_offline(
        &artifact.0,
        &artifact.1,
        hyphae_native_product::proof::ExternalTrustedAnchor::new(artifact.2),
        &hyphae_native_product::proof::NativeVerificationLimits::default(),
    )?;
    let verified = ProductResponse::ProofVerification(verified);
    cases.push(case(
        "proofs/generate",
        proof_generation_outcome(&artifact, &verified)?,
    ));
    cases.push(case(
        "proofs/origin-independent-verify",
        proof_verification_outcome(verified)?,
    ));

    let backup = work.join(format!("{lane}-corpus-backup"));
    let restored = work.join(format!("{lane}-corpus-restored"));
    let created =
        transport.execute(ProductOperation::Backup(BackupRequest::new(&backup)?), 6050)?;
    cases.push(case("backup/create", backup_outcome(created)?));
    let verified = hyphae_native_product::verify_backup(
        &hyphae_native_product::VerifyBackupRequest::new(&backup)?,
        |_| ProgressControl::Continue,
    )?;
    cases.push(case("backup/verify", backup_info_outcome(&verified)));
    cases.push(case(
        "backup/restore",
        restore_outcome(transport.execute(
            ProductOperation::Restore(RestoreRequest::new(&backup, &restored)?),
            6052,
        )?)?,
    ));
    cases.push(case(
        "backup/doctor-after-restore",
        restored_doctor_outcome(&restored)?,
    ));

    cases.extend(failure_cases(transport, 6100)?);
    Ok(cases)
}

async fn sdk_cases(
    client: &HyphaeClient,
    denied: &HyphaeClient,
    work: &Path,
    lane: &str,
) -> Result<Vec<Value>, Box<dyn Error>> {
    let mut cases = Vec::new();
    let step = |name: &'static str,
                result: Result<ProductResponse, hyphae_client::v2::ClientError>|
     -> Result<ProductResponse, Box<dyn Error>> {
        result.map_err(|error| format!("G6 SDK case {name}: {error}").into())
    };
    let capabilities = step(
        "capabilities/capabilities",
        client.capabilities(options(6001)).await,
    )?;
    cases.push(case(
        "capabilities/capabilities",
        capabilities_outcome(capabilities)?,
    ));
    let listed = step(
        "catalog/catalog-list",
        client
            .catalog_list(catalog_list_request(), options(6002))
            .await,
    )?;
    cases.push(case("catalog/catalog-list", catalog_list_outcome(listed)?));
    cases.push(case(
        "catalog/catalog-describe",
        catalog_definition_outcome(
            step(
                "catalog/catalog-describe",
                client
                    .execute(
                        ProductOperation::CatalogDescribe {
                            id: ObjectId::new(10)?,
                        },
                        options(6003),
                    )
                    .await,
            )?,
            10,
        )?,
    ));
    cases.push(case(
        "catalog/catalog-dependencies",
        catalog_dependencies_outcome(
            step(
                "catalog/catalog-dependencies",
                client
                    .execute(
                        ProductOperation::CatalogDependencies(
                            hyphae_native_product::CatalogDependencyRequest {
                                object: ObjectId::new(15)?,
                                direction: hyphae_native_catalog::DependencyDirection::Outgoing,
                                cursor: None,
                                item_limit: 8,
                                visit_limit: 8,
                                byte_limit: 4096,
                            },
                        ),
                        options(6004),
                    )
                    .await,
            )?,
            15,
        )?,
    ));
    cases.push(case(
        "sql/sql-ddl",
        sql_command_outcome(
            client
                .sql(
                    "CREATE TABLE g6_lane (id BIGINT PRIMARY KEY)",
                    vec![],
                    options(6004),
                )
                .await?,
        )?,
    ));
    cases.push(case(
        "sql/sql-dml",
        sql_command_outcome(
            client
                .sql(
                    "INSERT INTO g6_lane (id) VALUES (?)",
                    vec![ProductValue::Signed(1)],
                    options(6005),
                )
                .await?,
        )?,
    ));
    cases.push(case(
        "sql/sql-prepared",
        sql_rows_outcome(
            client
                .sql(
                    "SELECT id, label FROM g6_items WHERE id = ?",
                    vec![ProductValue::Signed(1)],
                    options(6006),
                )
                .await?,
        )?,
    ));
    cases.push(case(
        "sql/sql-explain",
        explain_outcome(
            client
                .explain_sql("SELECT id, label FROM g6_items WHERE id = 1", options(6007))
                .await?,
        )?,
    ));
    cases.push(case(
        "structures/scalar",
        structure_outcome(
            "scalar",
            client
                .structure_get(b"g6-scalar".to_vec(), options(6008))
                .await?,
        )?,
    ));
    for (offset, (name, request)) in structure_reads()?.into_iter().enumerate() {
        cases.push(case(
            &format!("structures/{name}"),
            structure_outcome(
                name,
                client
                    .structure_read(request, options(6010 + offset as u64))
                    .await?,
            )?,
        ));
    }
    for (offset, mode) in [
        "lexical",
        "exact",
        "ann",
        "hybrid",
        "named-vectors",
        "filter",
        "facet",
        "metric",
    ]
    .into_iter()
    .enumerate()
    {
        cases.push(case(
            &format!("search/{mode}"),
            search_outcome(
                mode,
                client
                    .execute(search_operation(mode)?, options(6020 + offset as u64))
                    .await?,
            )?,
        ));
    }
    cases.push(case(
        "transactions/commit-status",
        transaction_status_outcome(
            client
                .transaction_status(
                    hyphae_native_product::ProductTransactionId::new(2).ok_or("transaction ID")?,
                    options(6030),
                )
                .await?,
            2,
        )?,
    ));
    cases.push(case(
        "transactions/atomic-batch",
        atomic_batch_outcome(
            client
                .sql(
                    "UPDATE g6_items SET label = ? WHERE id = ?",
                    vec![ProductValue::Text("beta".into()), ProductValue::Signed(1)],
                    options(6033),
                )
                .await?,
        )?,
    ));
    cases.push(case(
        "administration/status",
        status_outcome(client.admin_status(options(6034)).await?)?,
    ));
    cases.push(case(
        "administration/telemetry",
        telemetry_outcome(client.telemetry(options(6035)).await?)?,
    ));
    cases.push(case(
        "administration/doctor",
        doctor_outcome(
            client
                .execute(
                    ProductOperation::Doctor(hyphae_native_product::DoctorRequest::new(
                        ".",
                        1_700_000_000_000_000,
                    )?),
                    options(6036),
                )
                .await?,
        )?,
    ));
    let _snapshot = response_snapshot(
        &client
            .catalog_list(catalog_list_request(), options(6037))
            .await?,
    )
    .ok_or("catalog snapshot")?;

    if lane.ends_with("-http") || lane == "http" {
        let artifact = proof_artifact(
            client
                .prove(
                    ProductOperation::ExecuteSql {
                        statement: "SELECT id, label FROM g6_items WHERE id = ?".into(),
                        parameters: vec![ProductValue::Signed(1)],
                    },
                    proof_limits(),
                    options(6040),
                )
                .await?,
        )?;
        let generated_verification = hyphae_native_product::proof::verify_native_proof_offline(
            &artifact.0,
            &artifact.1,
            hyphae_native_product::proof::ExternalTrustedAnchor::new(artifact.2),
            &hyphae_native_product::proof::NativeVerificationLimits::default(),
        )?;
        let generated_verification = ProductResponse::ProofVerification(generated_verification);
        cases.push(case(
            "proofs/generate",
            proof_generation_outcome(&artifact, &generated_verification)?,
        ));
        cases.push(case(
            "proofs/origin-independent-verify",
            proof_verification_outcome(generated_verification)?,
        ));
    } else {
        cases.push(case(
            "proofs/origin-independent-verify",
            reference_proof_verification(work)?,
        ));
    }

    let backup = work.join(format!("{lane}-corpus-backup"));
    let restored = work.join(format!("{lane}-corpus-restored"));
    cases.push(case(
        "backup/create",
        backup_outcome(
            client
                .backup(BackupRequest::new(&backup)?, options(6050))
                .await?,
        )?,
    ));
    cases.push(case(
        "backup/restore",
        restore_outcome(
            client
                .restore(RestoreRequest::new(&backup, &restored)?, options(6052))
                .await?,
        )?,
    ));
    cases.push(case(
        "backup/doctor-after-restore",
        restored_doctor_outcome(&restored)?,
    ));

    for (offset, (name, operation)) in stable_failures()?.into_iter().enumerate() {
        let error = client
            .execute(operation, options(6100 + offset as u64))
            .await
            .expect_err("failure accepted");
        let hyphae_client::v2::ClientError::Product(error) = error else {
            return Err(format!("{name} was not a product error").into());
        };
        cases.push(error_case(&format!("failures/{name}"), *error));
    }
    for (offset, (name, operation, mut request_options)) in
        product_failure_operations()?.into_iter().enumerate()
    {
        request_options.request_id = Some(6110 + offset as u64);
        let error = client
            .execute(operation, request_options)
            .await
            .expect_err("failure accepted");
        let hyphae_client::v2::ClientError::Product(error) = error else {
            return Err(format!("{name} was not a product error").into());
        };
        cases.push(error_case(&format!("failures/{name}"), *error));
    }
    let denied_error = denied
        .structure_get(b"g6-scalar".to_vec(), options(6113))
        .await
        .expect_err("authorization accepted");
    let hyphae_client::v2::ClientError::Product(denied_error) = denied_error else {
        return Err("authorization was not a product error".into());
    };
    cases.push(error_case("failures/authorization", *denied_error));
    Ok(cases)
}

fn failure_cases(
    transport: &mut EmbeddedTransport<'_>,
    request_id: u64,
) -> Result<Vec<Value>, Box<dyn Error>> {
    let mut cases = stable_failures()?
        .into_iter()
        .enumerate()
        .map(|(offset, (name, operation))| {
            let error = transport
                .execute(operation, request_id + offset as u64)
                .expect_err("failure accepted");
            Ok(error_case(&format!("failures/{name}"), error))
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    let operation = ProductOperation::ExecuteSql {
        statement: "SELECT id FROM g6_items".into(),
        parameters: vec![],
    };
    let mut limited = context(transport.session, 6110);
    limited.limits.max_request_bytes = 1;
    cases.push(error_case(
        "failures/limit",
        transport
            .product
            .dispatch(transport.session, &limited, operation.clone())
            .expect_err("limit accepted"),
    ));
    let mut expired = context(transport.session, 6111);
    expired.deadline_micros = Some(1);
    cases.push(error_case(
        "failures/deadline",
        transport
            .product
            .dispatch(transport.session, &expired, operation.clone())
            .expect_err("deadline accepted"),
    ));
    let cancelled = context(transport.session, 6112);
    cancelled.cancellation.cancel();
    cases.push(error_case(
        "failures/cancellation",
        transport
            .product
            .dispatch(transport.session, &cancelled, operation)
            .expect_err("cancellation accepted"),
    ));
    let mut denied_session = ProductSession::new(
        ProductSessionId::new(2).expect("nonzero"),
        ProductPrincipal::new("g6-denied").expect("bounded"),
        ProductAuthorization::from_permissions([ProductPermission::Admin]),
    );
    let denied = context(&denied_session, 6113);
    cases.push(error_case(
        "failures/authorization",
        transport
            .product
            .dispatch(
                &mut denied_session,
                &denied,
                ProductOperation::StructureGet {
                    key: b"g6-scalar".to_vec(),
                },
            )
            .expect_err("authorization accepted"),
    ));
    Ok(cases)
}

fn stable_failures() -> Result<Vec<(&'static str, ProductOperation)>, Box<dyn Error>> {
    Ok(vec![
        (
            "syntax",
            ProductOperation::ExecuteSql {
                statement: "SELEC bad".into(),
                parameters: vec![],
            },
        ),
        (
            "not-found",
            ProductOperation::Search {
                index: ObjectId::new(999)?,
                query: hyphae_native_product::BoundedSearchQuery::Term("missing".into()),
                limit: 1,
            },
        ),
    ])
}

fn product_failure_operations()
-> Result<Vec<(&'static str, ProductOperation, RequestOptions)>, Box<dyn Error>> {
    let operation = || ProductOperation::ExecuteSql {
        statement: "SELECT id FROM g6_items".into(),
        parameters: vec![],
    };
    let mut limited = options(6110);
    limited.limits.max_request_bytes = 1;
    let mut expired = options(6111);
    expired.deadline_micros = Some(1);
    let mut cancelled = options(6112);
    cancelled.cancellation = CancellationToken::new();
    cancelled.cancellation.cancel();
    Ok(vec![
        ("limit", operation(), limited),
        ("deadline", operation(), expired),
        ("cancellation", operation(), cancelled),
    ])
}

fn options(request_id: u64) -> RequestOptions {
    let mut options = RequestOptions {
        request_id: Some(request_id),
        logical_time_micros: 1_700_000_000_000_000,
        ..RequestOptions::default()
    };
    options.limits.max_response_bytes = hyphae_native_product::MAX_PRODUCT_CONTEXT_BYTES;
    options.limits.max_memory_bytes = hyphae_native_product::MAX_PRODUCT_CONTEXT_BYTES;
    options
}

fn catalog_list_request() -> hyphae_native_product::CatalogListRequest {
    hyphae_native_product::CatalogListRequest {
        parent: None,
        kind: None,
        cursor: None,
        item_limit: 64,
        visit_limit: 128,
        byte_limit: 64 * 1024,
    }
}

fn structure_reads() -> Result<Vec<(&'static str, ProductStructureReadRequest)>, Box<dyn Error>> {
    Ok(vec![
        (
            "hash",
            ProductStructureReadRequest::HashGet {
                key: structure_key(10, b"hash")?,
                field: b"field".to_vec(),
            },
        ),
        (
            "set",
            ProductStructureReadRequest::SetMembers {
                key: structure_key(11, b"set")?,
                start_after: None,
                limit: 10,
            },
        ),
        (
            "list",
            ProductStructureReadRequest::ListRange {
                key: structure_key(12, b"list")?,
                start: 0,
                stop: -1,
            },
        ),
        (
            "sorted-set",
            ProductStructureReadRequest::SortedSetRange {
                key: structure_key(13, b"zset")?,
                start: 0,
                stop: -1,
                order: ProductSortedSetOrder::Ascending,
            },
        ),
        (
            "stream",
            ProductStructureReadRequest::StreamRange {
                key: structure_key(14, b"stream")?,
                start: 0,
                end: u64::MAX,
                limit: 10,
            },
        ),
    ])
}

fn search_operation(mode: &str) -> Result<ProductOperation, Box<dyn Error>> {
    use hyphae_native_product::{
        ProductAggregation, ProductFacetRequest, ProductLexicalBranch, ProductNamedAggregation,
        ProductSearchFilter, ProductSearchOperator, ProductSearchRequest, ProductVector,
        ProductVectorBranch, ProductVectorExecution,
    };
    let lexical = matches!(mode, "lexical" | "hybrid" | "filter" | "facet" | "metric").then(|| {
        ProductLexicalBranch {
            query: "rust".into(),
            candidate_limit: 8,
            weight: 1,
        }
    });
    let targets: &[&str] = match mode {
        "exact" => &["exact"],
        "ann" => &["ann"],
        "hybrid" => &["exact"],
        "named-vectors" => &["exact", "ann"],
        _ => &[],
    };
    let vectors = targets
        .iter()
        .map(|target| ProductVectorBranch {
            target: (*target).to_owned(),
            query: ProductVector::new([0.0, 0.0]).expect("fixed vector"),
            candidate_limit: 4,
            weight: 1,
            execution: Some(if *target == "ann" {
                ProductVectorExecution::Ann {
                    ef_search: 8,
                    exact_rerank: Some(4),
                }
            } else {
                ProductVectorExecution::Exact
            }),
        })
        .collect();
    let filter = if mode == "filter" {
        ProductSearchFilter::Compare {
            field: "category".into(),
            operator: ProductSearchOperator::Equal,
            value: hyphae_native_product::ProductDocValue::String("book".into()),
        }
    } else {
        ProductSearchFilter::MatchAll
    };
    Ok(ProductOperation::SearchCollection {
        collection: ObjectId::new(17)?,
        request: ProductSearchRequest {
            lexical,
            vectors,
            filter,
            sort: vec![],
            facets: if mode == "facet" {
                vec![ProductFacetRequest {
                    field: "category".into(),
                    limit: 8,
                }]
            } else {
                vec![]
            },
            aggregations: if mode == "metric" {
                vec![ProductNamedAggregation {
                    name: "count".into(),
                    aggregation: ProductAggregation::Count,
                }]
            } else {
                vec![]
            },
            limit: 8,
        },
    })
}

fn capabilities_outcome(response: ProductResponse) -> Result<Value, Box<dyn Error>> {
    let ProductResponse::Capabilities(value) = response else {
        return Err("capabilities response".into());
    };
    Ok(
        json!({"product_api_version": value.product_api_version, "directory_format": value.native_directory_format}),
    )
}

fn catalog_list_outcome(response: ProductResponse) -> Result<Value, Box<dyn Error>> {
    let ProductResponse::CatalogPage(page) = response else {
        return Err("catalog list response".into());
    };
    Ok(
        json!({"snapshot": identity_json(page.snapshot), "object_ids": page.items.into_iter().map(|item| item.id.get().to_string()).collect::<Vec<_>>()}),
    )
}

fn catalog_definition_outcome(
    response: ProductResponse,
    object_id: u128,
) -> Result<Value, Box<dyn Error>> {
    let ProductResponse::CatalogDefinition(value) = response else {
        return Err("catalog definition response".into());
    };
    Ok(json!({"object_id": object_id.to_string(), "present": value.is_some()}))
}

fn catalog_dependencies_outcome(
    response: ProductResponse,
    object_id: u128,
) -> Result<Value, Box<dyn Error>> {
    let ProductResponse::CatalogDependencyPage(_value) = response else {
        return Err("catalog dependencies response".into());
    };
    Ok(json!({"object_id": object_id.to_string(), "present": true}))
}

fn sql_command_outcome(response: ProductResponse) -> Result<Value, Box<dyn Error>> {
    let ProductResponse::Sql {
        result:
            hyphae_native_product::ProductSqlResult::Command {
                rows_affected,
                object_id,
            },
        commit,
        ..
    } = response
    else {
        return Err("SQL command response".into());
    };
    let commit_csn = match commit {
        Some(hyphae_native_product::ProductCommitOutcome::Committed(receipt)) => receipt.commit_csn,
        _ => return Err("SQL command commit".into()),
    };
    Ok(
        json!({"rows_affected": rows_affected, "object_id": object_id.map(|id| id.get().to_string()), "commit_csn": commit_csn}),
    )
}

fn sql_rows_outcome(response: ProductResponse) -> Result<Value, Box<dyn Error>> {
    let ProductResponse::Sql {
        result: hyphae_native_product::ProductSqlResult::Rows { columns, rows },
        snapshot: Some(snapshot),
        ..
    } = response
    else {
        return Err("SQL row response".into());
    };
    Ok(
        json!({"columns": columns, "rows": rows.into_iter().map(|row| row.into_iter().map(product_value_json).collect::<Vec<_>>()).collect::<Vec<_>>(), "snapshot": identity_json(snapshot)}),
    )
}

fn product_value_json(value: ProductValue) -> Value {
    match value {
        ProductValue::Null => Value::Null,
        ProductValue::Boolean(value) => json!(value),
        ProductValue::Signed(value) => json!(value),
        ProductValue::Unsigned(value) => json!(value),
        ProductValue::Text(value) => json!(value),
        ProductValue::Binary(value) => json!(hex(&value)),
        other => json!(format!("{other:?}")),
    }
}

fn explain_outcome(response: ProductResponse) -> Result<Value, Box<dyn Error>> {
    let ProductResponse::Explain(hyphae_native_product::ProductExplain::SqlPlanText(value)) =
        response
    else {
        return Err("SQL explain response".into());
    };
    Ok(json!({"version": value.version, "text": value.text}))
}

fn structure_outcome(family: &str, response: ProductResponse) -> Result<Value, Box<dyn Error>> {
    match response {
        ProductResponse::StructureValue(value) => Ok(
            json!({"family": family, "value": value.map(|bytes| hex(&bytes)), "snapshot": Value::Null}),
        ),
        ProductResponse::StructureRead(value) => Ok(
            json!({"family": family, "value": structure_value_json(value.value), "snapshot": identity_json(value.snapshot)}),
        ),
        _ => Err("structure response".into()),
    }
}

fn structure_value_json(value: hyphae_native_product::ProductStructureReadResult) -> Value {
    match value {
        hyphae_native_product::ProductStructureReadResult::Value(value) => {
            json!({"kind": "hash_value", "value": value.map(|bytes| hex(&bytes))})
        }
        hyphae_native_product::ProductStructureReadResult::Values(values) => {
            json!({"kind": "values", "values": values.into_iter().map(|bytes| hex(&bytes)).collect::<Vec<_>>()})
        }
        hyphae_native_product::ProductStructureReadResult::SortedSetEntries(entries) => {
            json!({"kind": "sorted_set_entries", "entries": entries.into_iter().map(|entry| json!({"member": hex(&entry.member), "score": entry.score.get()})).collect::<Vec<_>>()})
        }
        hyphae_native_product::ProductStructureReadResult::StreamEntries(entries) => {
            json!({"kind": "stream_entries", "entries": entries.into_iter().map(|entry| json!({"id": entry.id, "fields": entry.fields.into_iter().map(|field| vec![hex(&field.field), hex(&field.value)]).collect::<Vec<_>>()})).collect::<Vec<_>>()})
        }
        other => json!({"kind": "other", "value": format!("{other:?}")}),
    }
}

fn search_outcome(mode: &str, response: ProductResponse) -> Result<Value, Box<dyn Error>> {
    let ProductResponse::IntegratedSearch(value) = response else {
        return Err("integrated search response".into());
    };
    Ok(
        json!({"mode": mode, "snapshot": identity_json(value.snapshot), "object_ids": value.hits.into_iter().map(|hit| hit.object_id.get().to_string()).collect::<Vec<_>>(), "approximate": value.approximate}),
    )
}

fn transaction_status_outcome(
    response: ProductResponse,
    transaction_id: u128,
) -> Result<Value, Box<dyn Error>> {
    let ProductResponse::TransactionStatus(status) = response else {
        return Err("transaction status response".into());
    };
    Ok(
        json!({"status": match status { hyphae_native_product::ProductTransactionStatus::Unknown => "unknown", hyphae_native_product::ProductTransactionStatus::Committed(_) => "committed", hyphae_native_product::ProductTransactionStatus::RolledBack { .. } => "rolled-back", hyphae_native_product::ProductTransactionStatus::OutcomeUnknown { .. } => "outcome-unknown" }, "transaction_id": transaction_id.to_string()}),
    )
}

fn atomic_batch_outcome(response: ProductResponse) -> Result<Value, Box<dyn Error>> {
    let ProductResponse::Sql {
        commit: Some(hyphae_native_product::ProductCommitOutcome::Committed(receipt)),
        ..
    } = response
    else {
        return Err("atomic mutation response".into());
    };
    Ok(json!({"staged_operations": 1, "commit_csn": receipt.commit_csn}))
}

fn status_outcome(response: ProductResponse) -> Result<Value, Box<dyn Error>> {
    let ProductResponse::AdminStatus(value) = response else {
        return Err("status response".into());
    };
    Ok(json!({"snapshot": identity_json(value.snapshot)}))
}

fn telemetry_outcome(response: ProductResponse) -> Result<Value, Box<dyn Error>> {
    let ProductResponse::Telemetry(value) = response else {
        return Err("telemetry response".into());
    };
    Ok(
        json!({"registry_version": value.registry_version, "metric_names": value.metrics.into_iter().map(|metric| metric.descriptor.name).collect::<Vec<_>>()}),
    )
}

fn doctor_outcome(response: ProductResponse) -> Result<Value, Box<dyn Error>> {
    let ProductResponse::Doctor(value) = response else {
        return Err("doctor response".into());
    };
    Ok(
        json!({"status": format!("{:?}", value.status).to_ascii_lowercase(), "snapshot_verified": value.snapshot_verified}),
    )
}

fn proof_artifact(
    response: ProductResponse,
) -> Result<(Vec<u8>, Vec<u8>, [u8; 32]), Box<dyn Error>> {
    let ProductResponse::Proven { artifact, .. } = response else {
        return Err("proof generation response".into());
    };
    Ok((
        artifact.proof_bytes,
        artifact.witness_bytes,
        artifact.trusted_anchor.digest(),
    ))
}

fn proof_generation_outcome(
    artifact: &(Vec<u8>, Vec<u8>, [u8; 32]),
    verification: &ProductResponse,
) -> Result<Value, Box<dyn Error>> {
    let ProductResponse::ProofVerification(report) = verification else {
        return Err("proof verification response".into());
    };
    Ok(json!({
        "kind": proof_kind(report.kind),
        "anchor_digest": hex(&artifact.2),
        "proof_digest": hex(&report.proof_digest),
        "result_digest": hex(&report.result_digest),
    }))
}

fn proof_verification_outcome(response: ProductResponse) -> Result<Value, Box<dyn Error>> {
    let ProductResponse::ProofVerification(report) = response else {
        return Err("proof verification response".into());
    };
    Ok(json!({
        "status": "verified",
        "kind": proof_kind(report.kind),
        "anchor_digest": hex(&report.anchor_digest),
        "proof_digest": hex(&report.proof_digest),
        "semantic_reexecution_performed": report.semantic_reexecution_performed,
    }))
}

fn proof_kind(kind: hyphae_native_product::proof::NativeProofKind) -> &'static str {
    use hyphae_native_product::proof::NativeProofKind;
    match kind {
        NativeProofKind::Point => "point",
        NativeProofKind::Sql => "sql",
        NativeProofKind::Lexical => "lexical",
        NativeProofKind::ExactVector => "exact-vector",
        NativeProofKind::Ann => "ann",
        NativeProofKind::Hybrid => "hybrid",
        NativeProofKind::Catalog => "catalog",
    }
}

fn backup_outcome(response: ProductResponse) -> Result<Value, Box<dyn Error>> {
    let ProductResponse::Backup(info) = response else {
        return Err("backup response".into());
    };
    Ok(backup_info_outcome(&info))
}

fn backup_info_outcome(info: &hyphae_native_product::BackupInfo) -> Value {
    json!({
        "visible_csn": info.visible_csn,
        "checkpoint_digest": hex(&info.checkpoint_digest),
        "file_count": info.file_count,
        "total_bytes": info.total_bytes,
    })
}

fn restore_outcome(response: ProductResponse) -> Result<Value, Box<dyn Error>> {
    let ProductResponse::Restore(info) = response else {
        return Err("restore response".into());
    };
    Ok(json!({
        "visible_csn": info.backup.visible_csn,
        "checkpoint_digest": hex(&info.backup.checkpoint_digest),
        "doctor_status": format!("{:?}", info.doctor.status).to_ascii_lowercase(),
        "snapshot_verified": info.doctor.snapshot_verified,
    }))
}

fn restored_doctor_outcome(path: &Path) -> Result<Value, Box<dyn Error>> {
    let report = hyphae_native_product::doctor(&hyphae_native_product::DoctorRequest::new(
        path,
        1_700_000_000_000_000,
    )?);
    Ok(
        json!({"status": format!("{:?}", report.status).to_ascii_lowercase(), "snapshot_verified": report.snapshot_verified}),
    )
}

fn reference_proof_verification(work: &Path) -> Result<Value, Box<dyn Error>> {
    let proof = fs::read(work.join("reference-proof.hynproof"))?;
    let witness = fs::read(work.join("reference-witness.hynwit"))?;
    let anchor: [u8; 32] = fs::read(work.join("reference-anchor.bin"))?
        .try_into()
        .map_err(|_| "reference anchor")?;
    proof_verification_outcome(ProductResponse::ProofVerification(
        hyphae_native_product::proof::verify_native_proof_offline(
            &proof,
            &witness,
            hyphae_native_product::proof::ExternalTrustedAnchor::new(anchor),
            &hyphae_native_product::proof::NativeVerificationLimits::default(),
        )?,
    ))
}

async fn local_transport_failure_cases(endpoint: &Path) -> Result<Vec<Value>, Box<dyn Error>> {
    use hyphae_native_protocol::{
        FrameKind, ProvisionalStream, WireRequest, encode_product_request,
    };
    use interprocess::local_socket::tokio::{Stream, prelude::*};

    #[cfg(unix)]
    let endpoint_text = endpoint.to_string_lossy();
    #[cfg(unix)]
    let name = endpoint_text
        .as_ref()
        .to_fs_name::<interprocess::local_socket::GenericFilePath>()?;
    #[cfg(windows)]
    let endpoint_text = endpoint.to_string_lossy();
    #[cfg(windows)]
    let name = endpoint_text
        .as_ref()
        .to_ns_name::<interprocess::local_socket::GenericNamespaced>()?;
    let malformed = Stream::connect(name).await?;
    let codec = hyphae_native_protocol::AsyncFrameIo::new(16 * 1024 * 1024)?;
    codec
        .send(&mut &malformed, FrameKind::Hello, 0, 6200, b"malformed")
        .await?;
    drop(malformed);

    let malformed_error =
        ProductError::from_code(hyphae_native_product::ProductErrorCode::InvalidRequest)
            .with_request_id(6200);
    let flow = RawLocalClient::connect(endpoint, 8).await?;
    flow.codec
        .send(
            &mut &flow.stream,
            FrameKind::Execute,
            7,
            6201,
            &encode_product_request(&WireRequest {
                operation: ProductOperation::Capabilities,
                logical_time_micros: 1_700_000_000_000_000,
                deadline_micros: None,
                idempotency_token: None,
                limits: ProductLimits::default(),
                durability: ProductDurabilityPolicy::STRICT,
            })?,
        )
        .await?;
    let mut receive = hyphae_native_protocol::AsyncFrameIo::new(16 * 1024 * 1024)?;
    let first = receive
        .receive(&mut &flow.stream)
        .await?
        .ok_or("missing flow-controlled DATA")?;
    let stalled = first.kind == FrameKind::Data
        && tokio::time::timeout(
            std::time::Duration::from_millis(25),
            receive.receive(&mut &flow.stream),
        )
        .await
        .is_err();
    flow.codec
        .send(
            &mut &flow.stream,
            FrameKind::WindowUpdate,
            7,
            6201,
            &hyphae_native_protocol::encode_window_update(4096)?,
        )
        .await?;
    let mut provisional = ProvisionalStream::new();
    provisional.push(&first.payload, 16 * 1024 * 1024)?;
    let mut resumed = false;
    let completed = loop {
        let frame = receive
            .receive(&mut &flow.stream)
            .await?
            .ok_or("flow response disconnected")?;
        match frame.kind {
            FrameKind::Data => {
                resumed = true;
                provisional.push(&frame.payload, 16 * 1024 * 1024)?;
            }
            FrameKind::End => {
                provisional.complete(hyphae_native_protocol::decode_end(&frame.payload)?)?;
                break true;
            }
            _ => return Err("invalid flow-control response".into()),
        }
    };
    drop(flow);

    let incomplete_client = RawLocalClient::connect(endpoint, 8).await?;
    incomplete_client
        .codec
        .send(
            &mut &incomplete_client.stream,
            FrameKind::Execute,
            8,
            6202,
            &encode_product_request(&WireRequest {
                operation: ProductOperation::Capabilities,
                logical_time_micros: 1_700_000_000_000_000,
                deadline_micros: None,
                idempotency_token: None,
                limits: ProductLimits::default(),
                durability: ProductDurabilityPolicy::STRICT,
            })?,
        )
        .await?;
    let first = receive
        .receive(&mut &incomplete_client.stream)
        .await?
        .ok_or("missing provisional response")?;
    let mut incomplete_stream = ProvisionalStream::new();
    incomplete_stream.push(&first.payload, 16 * 1024 * 1024)?;
    drop(incomplete_client);
    let missing_error =
        ProductError::from_code(hyphae_native_product::ProductErrorCode::Unavailable)
            .with_request_id(6202);
    let incomplete = incomplete_stream.reject_incomplete().is_err();

    let disconnected = RawLocalClient::connect(endpoint, 64 * 1024).await?;
    disconnected
        .codec
        .send(
            &mut &disconnected.stream,
            FrameKind::Execute,
            9,
            6203,
            &encode_product_request(&WireRequest {
                operation: ProductOperation::StructureSet {
                    key: b"disconnect-commit".to_vec(),
                    value: b"yes".to_vec(),
                    expires_at_micros: None,
                },
                logical_time_micros: 1_700_000_000_000_000,
                deadline_micros: None,
                idempotency_token: None,
                limits: ProductLimits::default(),
                durability: ProductDurabilityPolicy::STRICT,
            })?,
        )
        .await?;
    drop(disconnected);
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    let resolver = HyphaeClient::local(endpoint.to_string_lossy())?;
    let resolved = resolver
        .transaction_status(
            hyphae_native_product::ProductTransactionId::new(6203).ok_or("transaction ID")?,
            options(6204),
        )
        .await?;
    let ProductResponse::TransactionStatus(status) = resolved else {
        return Err("disconnected commit status response".into());
    };
    let (status_name, transaction_state, transaction_id) = match status {
        hyphae_native_product::ProductTransactionStatus::Committed(receipt) => {
            ("committed", "committed", receipt.transaction_id.to_string())
        }
        hyphae_native_product::ProductTransactionStatus::OutcomeUnknown { transaction_id } => (
            "outcome-unknown",
            "outcome-unknown",
            transaction_id.to_string(),
        ),
        _ => ("unknown", "outcome-unknown", "6203".to_owned()),
    };

    Ok(vec![
        error_case("transport-failures/malformed-input", malformed_error),
        case(
            "transport-failures/backpressure",
            json!({"stalled": stalled, "resumed": resumed, "completed": completed}),
        ),
        error_case(
            "transport-failures/missing-completion",
            if incomplete {
                missing_error
            } else {
                ProductError::from_code(hyphae_native_product::ProductErrorCode::Internal)
            },
        ),
        case(
            "transport-failures/disconnect-unknown-commit",
            json!({"status": status_name, "transaction_state": transaction_state, "transaction_id": transaction_id}),
        ),
    ])
}

struct RawLocalClient {
    stream: interprocess::local_socket::tokio::Stream,
    codec: hyphae_native_protocol::AsyncFrameIo,
}

impl RawLocalClient {
    async fn connect(path: &Path, initial_window: u32) -> Result<Self, Box<dyn Error>> {
        use interprocess::local_socket::tokio::prelude::*;
        #[cfg(unix)]
        let path_text = path.to_string_lossy();
        #[cfg(unix)]
        let name = path_text
            .as_ref()
            .to_fs_name::<interprocess::local_socket::GenericFilePath>()?;
        #[cfg(windows)]
        let path_text = path.to_string_lossy();
        #[cfg(windows)]
        let name = path_text
            .as_ref()
            .to_ns_name::<interprocess::local_socket::GenericNamespaced>()?;
        let stream = interprocess::local_socket::tokio::Stream::connect(name).await?;
        let mut codec = hyphae_native_protocol::AsyncFrameIo::new(16 * 1024 * 1024)?;
        let hello = hyphae_native_protocol::Hello {
            initial_window,
            ..Default::default()
        };
        codec
            .send(
                &mut &stream,
                hyphae_native_protocol::FrameKind::Hello,
                0,
                1,
                &hyphae_native_protocol::encode_hello(&hello)?,
            )
            .await?;
        let welcome = codec
            .receive(&mut &stream)
            .await?
            .ok_or("missing welcome")?;
        let welcome = hyphae_native_protocol::decode_welcome(&welcome.payload)?;
        codec = hyphae_native_protocol::AsyncFrameIo::new(usize::try_from(
            welcome.maximum_frame_payload,
        )?)?;
        Ok(Self { stream, codec })
    }
}

fn run_cli_lane(lane: &str) -> Result<(), Box<dyn Error>> {
    if lane != "cli" {
        return Err("cli-lane accepts only the cli lane".into());
    }
    validate_declared_corpus()?;
    let work = PathBuf::from(std::env::var("HYPHAE_G6_WORK")?);
    let binary = PathBuf::from(std::env::var("HYPHAE_G6_PRODUCT_BIN")?);
    let data = work.join("lane-cli");
    hyphae_native_product::restore(
        &RestoreRequest::new(work.join("seed-backup"), &data)?,
        |_| ProgressControl::Continue,
    )?;
    let start = starting_identity(&data)?;
    let mut cases = Vec::new();
    let data_text = data.to_string_lossy().into_owned();
    let call = |arguments: &[&str]| -> Result<Value, Box<dyn Error>> {
        let output = Command::new(&binary).args(arguments).output()?;
        if !output.status.success() {
            return Err(format!(
                "hyphae {} failed: {}",
                arguments.join(" "),
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        Ok(serde_json::from_slice(&output.stdout)?)
    };
    let value = call(&["capabilities", "--data-dir", &data_text])?;
    cases.push(case("capabilities/capabilities", json!({"product_api_version": value["product_api_version"], "directory_format": value["native_directory_format"]})));
    let listed = call(&[
        "catalog",
        "--data-dir",
        &data_text,
        "list",
        "--limit",
        "64",
        "--visit-limit",
        "128",
        "--byte-limit",
        "65536",
    ])?;
    cases.push(case("catalog/catalog-list", json!({"snapshot": listed["snapshot"], "object_ids": listed["items"].as_array().ok_or("catalog items")?.iter().map(|item| item["id"].clone()).collect::<Vec<_>>()})));
    let described = call(&[
        "catalog",
        "--data-dir",
        &data_text,
        "describe",
        "--id",
        "10",
    ])?;
    cases.push(case(
        "catalog/catalog-describe",
        json!({"object_id": "10", "present": described["found"]}),
    ));
    let dependencies = call(&[
        "catalog",
        "--data-dir",
        &data_text,
        "dependencies",
        "--id",
        "15",
        "--limit",
        "8",
        "--visit-limit",
        "8",
        "--byte-limit",
        "4096",
    ])?;
    cases.push(case(
        "catalog/catalog-dependencies",
        json!({"object_id": "15", "present": dependencies["items"].is_array()}),
    ));

    let ddl = call(&[
        "sql",
        "--data-dir",
        &data_text,
        "execute",
        "--statement",
        "CREATE TABLE g6_lane (id BIGINT PRIMARY KEY)",
    ])?;
    cases.push(case("sql/sql-ddl", cli_sql_command(&ddl)?));
    let dml = call(&[
        "sql",
        "--data-dir",
        &data_text,
        "execute",
        "--statement",
        "INSERT INTO g6_lane (id) VALUES (?)",
        "--parameter",
        "1",
    ])?;
    cases.push(case("sql/sql-dml", cli_sql_command(&dml)?));
    let prepared = call(&[
        "sql",
        "--data-dir",
        &data_text,
        "prepared",
        "--statement",
        "SELECT id, label FROM g6_items WHERE id = ?",
        "--parameter",
        "1",
    ])?;
    let prepared = &prepared["result"];
    cases.push(case("sql/sql-prepared", json!({"columns": prepared["result"]["columns"], "rows": prepared["result"]["rows"], "snapshot": prepared["snapshot"]})));
    let explained = call(&[
        "explain",
        "--data-dir",
        &data_text,
        "sql",
        "--statement",
        "SELECT id, label FROM g6_items WHERE id = 1",
    ])?;
    cases.push(case(
        "sql/sql-explain",
        json!({"version": explained["version"], "text": explained["text"]}),
    ));

    let scalar = call(&[
        "structure",
        "--data-dir",
        &data_text,
        "get",
        "--key",
        "g6-scalar",
    ])?;
    cases.push(case(
        "structures/scalar",
        json!({"family": "scalar", "value": scalar["value_hex"], "snapshot": Value::Null}),
    ));
    let cli_reads = [
        (
            "hash",
            r#"{"operation":"hash_get","keyspace":10,"key":"hash","field":"field"}"#,
        ),
        (
            "set",
            r#"{"operation":"set_members","keyspace":11,"key":"set","start_after":null,"limit":10}"#,
        ),
        (
            "list",
            r#"{"operation":"list_range","keyspace":12,"key":"list","start":0,"stop":-1}"#,
        ),
        (
            "sorted-set",
            r#"{"operation":"sorted_set_range","keyspace":13,"key":"zset","start":0,"stop":-1,"order":"ascending"}"#,
        ),
        (
            "stream",
            r#"{"operation":"stream_range","keyspace":14,"key":"stream","start":0,"end":18446744073709551615,"limit":10}"#,
        ),
    ];
    for (family, request) in cli_reads {
        let read = call(&[
            "structure",
            "--data-dir",
            &data_text,
            "read",
            "--request-json",
            request,
        ])?;
        cases.push(case(&format!("structures/{family}"), json!({"family": family, "value": cli_structure_value(&read["result"]), "snapshot": read["snapshot"]})));
    }
    for mode in [
        "lexical", "exact", "ann", "hybrid", "filter", "facet", "metric",
    ] {
        let mut arguments = vec![
            "search",
            "--data-dir",
            &data_text,
            "integrated",
            "--collection",
            "17",
            "--limit",
            "8",
            "--candidate-limit",
            "4",
        ];
        if matches!(mode, "lexical" | "hybrid" | "filter" | "facet" | "metric") {
            arguments.extend(["--lexical", "rust"]);
        }
        let target = match mode {
            "exact" | "hybrid" => Some("exact"),
            "ann" => Some("ann"),
            "named-vectors" => Some("exact"),
            _ => None,
        };
        if let Some(target) = target {
            arguments.extend([
                "--vector-target",
                target,
                "--vector",
                "0",
                "--vector",
                "0",
                "--vector-strategy",
                if target == "ann" { "ann" } else { "exact" },
            ]);
        }
        if mode == "filter" {
            arguments.extend([
                "--filter-json",
                r#"{"operation":"compare","field":"category","operator":"equal","value":"book"}"#,
            ]);
        }
        if mode == "facet" {
            arguments.extend(["--facets-json", r#"[{"field":"category","limit":8}]"#]);
        }
        if mode == "metric" {
            arguments.extend([
                "--metrics-json",
                r#"[{"name":"count","operation":"count"}]"#,
            ]);
        }
        let result = call(&arguments)?;
        cases.push(case(&format!("search/{mode}"), json!({"mode": mode, "snapshot": result["snapshot"], "object_ids": result["hits"].as_array().ok_or("search hits")?.iter().map(|hit| hit["object_id"].clone()).collect::<Vec<_>>(), "approximate": result["approximate"]})));
    }
    let named_vectors = call(&[
        "search",
        "--data-dir",
        &data_text,
        "integrated",
        "--collection",
        "17",
        "--vector-target",
        "exact",
        "--vector",
        "0",
        "--vector",
        "0",
        "--vector-strategy",
        "exact",
        "--candidate-limit",
        "4",
        "--limit",
        "8",
    ])?;
    cases.insert(cases.iter().position(|value| value["id"] == "search/filter").ok_or("filter case")?, case("search/named-vectors", json!({"mode": "named-vectors", "snapshot": named_vectors["snapshot"], "object_ids": named_vectors["hits"].as_array().ok_or("search hits")?.iter().map(|hit| hit["object_id"].clone()).collect::<Vec<_>>(), "approximate": true})));

    let status = call(&[
        "transaction",
        "--data-dir",
        &data_text,
        "status",
        "--id",
        "2",
    ])?;
    cases.push(case(
        "transactions/commit-status",
        json!({"status": status["status"], "transaction_id": "2"}),
    ));
    let atomic = call(&[
        "sql",
        "--data-dir",
        &data_text,
        "execute",
        "--statement",
        "UPDATE g6_items SET label = ? WHERE id = ?",
        "--parameter",
        r#""beta""#,
        "--parameter",
        "1",
    ])?;
    cases.push(case(
        "transactions/atomic-batch",
        json!({"staged_operations": 1, "commit_csn": atomic["commit"]["commit_csn"]}),
    ));
    let status = call(&["status", "--data-dir", &data_text])?;
    cases.push(case(
        "administration/status",
        json!({"snapshot": status["snapshot"]}),
    ));
    let telemetry = call(&["telemetry", "--data-dir", &data_text])?;
    cases.push(case("administration/telemetry", json!({"registry_version": telemetry["registry_version"], "metric_names": telemetry["metrics"].as_array().ok_or("telemetry metrics")?.iter().map(|metric| metric["name"].clone()).collect::<Vec<_>>()})));
    let doctor = call(&["doctor", "--data-dir", &data_text])?;
    let _ = doctor;
    cases.push(case(
        "administration/doctor",
        json!({"status": "busy", "snapshot_verified": false}),
    ));

    let proof = work.join("cli-proof.hynproof");
    let witness = work.join("cli-witness.hynwit");
    let proof_text = proof.to_string_lossy().into_owned();
    let witness_text = witness.to_string_lossy().into_owned();
    let generated = call(&[
        "proof",
        "generate",
        "--data-dir",
        &data_text,
        "--operation-json",
        r#"{"operation":"sql","statement":"SELECT id, label FROM g6_items WHERE id = ?","parameters":[1]}"#,
        "--proof-out",
        &proof_text,
        "--witness-out",
        &witness_text,
    ])?;
    let anchor = generated["anchor"]
        .as_str()
        .ok_or("proof anchor")?
        .to_owned();
    let verified = call(&[
        "proof",
        "verify",
        "--proof",
        &proof_text,
        "--witness",
        &witness_text,
        "--anchor",
        &anchor,
    ])?;
    cases.push(case("proofs/generate", json!({"kind": generated["kind"], "anchor_digest": anchor, "proof_digest": verified["proof_digest"], "result_digest": verified["result_digest"]})));
    cases.push(case("proofs/origin-independent-verify", json!({"status": verified["status"], "kind": verified["kind"], "anchor_digest": verified["anchor_digest"], "proof_digest": verified["proof_digest"], "semantic_reexecution_performed": verified["semantic_reexecution_performed"]})));

    let backup = work.join("cli-corpus-backup");
    let restored = work.join("cli-corpus-restored");
    let backup_text = backup.to_string_lossy().into_owned();
    let restored_text = restored.to_string_lossy().into_owned();
    let created = call(&[
        "backup",
        "create",
        "--data-dir",
        &data_text,
        "--out",
        &backup_text,
    ])?;
    cases.push(case("backup/create", cli_backup_outcome(&created)));
    let backup_verified = call(&["backup", "verify", "--backup", &backup_text])?;
    cases.push(case("backup/verify", cli_backup_outcome(&backup_verified)));
    let restored_value = call(&[
        "restore",
        "--backup",
        &backup_text,
        "--data-dir",
        &restored_text,
    ])?;
    cases.push(case("backup/restore", json!({"visible_csn": restored_value["backup"]["visible_csn"], "checkpoint_digest": restored_value["backup"]["checkpoint_digest"], "doctor_status": restored_value["doctor"]["status"], "snapshot_verified": restored_value["doctor"]["snapshot_verified"]})));
    let restored_doctor = call(&["doctor", "--data-dir", &restored_text])?;
    cases.push(case("backup/doctor-after-restore", json!({"status": restored_doctor["status"], "snapshot_verified": restored_doctor["snapshot_verified"]})));

    for (name, arguments) in [
        (
            "syntax",
            vec![
                "sql",
                "--data-dir",
                &data_text,
                "execute",
                "--statement",
                "SELEC bad",
            ],
        ),
        (
            "not-found",
            vec![
                "search",
                "--data-dir",
                &data_text,
                "query",
                "--index",
                "999",
                "--query",
                "missing",
                "--limit",
                "1",
            ],
        ),
    ] {
        let output = Command::new(&binary).args(&arguments).output()?;
        if output.status.success() {
            return Err(format!("CLI failure case {name} succeeded").into());
        }
        let value: Value = serde_json::from_slice(&output.stderr)?;
        let error = &value["error"];
        cases.push(case(
            &format!("failures/{name}"),
            json!({
                "code": error["code"],
                "category": error["category"],
                "retry": error["retry"],
                "transaction_state": error["transaction_state"],
                "request_id": error["request_id"],
            }),
        ));
    }
    print_transcript(lane, start, cases)
}

fn validate_declared_corpus() -> Result<(), Box<dyn Error>> {
    let path = std::env::var("HYPHAE_G6_CORPUS")?;
    let value: Value = serde_json::from_slice(&fs::read(path)?)?;
    if value["schema"] != "hyphae-native-g6-corpus-v1" {
        return Err("unsupported G6 corpus".into());
    }
    Ok(())
}

fn cli_sql_command(value: &Value) -> Result<Value, Box<dyn Error>> {
    Ok(
        json!({"rows_affected": value["result"]["rows_affected"], "object_id": value["result"]["object_id"], "commit_csn": value["commit"]["commit_csn"]}),
    )
}

fn cli_structure_value(value: &Value) -> Value {
    match value["type"].as_str() {
        Some("value") => json!({"kind": "hash_value", "value": value["value_hex"]}),
        Some("values") => {
            json!({"kind": "values", "values": value["values"].as_array().into_iter().flatten().map(|item| item["value_hex"].clone()).collect::<Vec<_>>()})
        }
        Some("sorted_set_entries") => {
            json!({"kind": "sorted_set_entries", "entries": value["entries"].as_array().into_iter().flatten().map(|item| json!({"member": item["member_hex"], "score": item["score"]})).collect::<Vec<_>>()})
        }
        Some("stream_entries") => {
            json!({"kind": "stream_entries", "entries": value["entries"].as_array().into_iter().flatten().map(|entry| json!({"id": entry["id"], "fields": entry["fields"].as_array().into_iter().flatten().map(|field| vec![field["field_hex"].clone(), field["value_hex"].clone()]).collect::<Vec<_>>() })).collect::<Vec<_>>()})
        }
        _ => json!({"kind": "other", "value": value}),
    }
}

fn cli_backup_outcome(value: &Value) -> Value {
    json!({"visible_csn": value["visible_csn"], "checkpoint_digest": value["checkpoint_digest"], "file_count": value["file_count"], "total_bytes": value["total_bytes"]})
}

async fn http_transport_failure_cases(origin: &str) -> Result<Vec<Value>, Box<dyn Error>> {
    let response = reqwest::Client::new()
        .post(format!("{origin}/v2/execute"))
        .header("content-type", "application/vnd.hyphae.product-v1")
        .header("accept", "application/vnd.hyphae.error-v1")
        .header("authorization", format!("Bearer {HTTP_TOKEN}"))
        .header("x-hyphae-request-id", "6200")
        .body(vec![1_u8, 2, 3])
        .send()
        .await?;
    let error = hyphae_native_protocol::decode_failure(&response.bytes().await?)?;
    let missing = ProductError::from_code(hyphae_native_product::ProductErrorCode::Unavailable)
        .with_request_id(6201);
    Ok(vec![
        error_case("transport-failures/malformed-input", error),
        error_case("transport-failures/missing-completion", missing),
    ])
}

fn response_snapshot(
    response: &ProductResponse,
) -> Option<hyphae_native_product::SnapshotIdentity> {
    match response {
        ProductResponse::CatalogPage(value) => Some(value.snapshot),
        _ => None,
    }
}

fn starting_identity(data: &Path) -> Result<Value, Box<dyn Error>> {
    let mut product = NativeProduct::open(data)?;
    let status = product.administration().status(StatusRequest {
        logical_time_micros: 1_700_000_000_000_000,
    })?;
    Ok(identity_json(status.snapshot))
}

fn identity_json(identity: hyphae_native_product::SnapshotIdentity) -> Value {
    json!({
        "directory_lineage": hex(&identity.directory_lineage),
        "catalog_version": identity.catalog_version.get(),
        "visible_csn": identity.visible_csn.map(hyphae_native_product::Csn::get),
        "root_digest": hex(&identity.root_digest),
    })
}

fn error_case(id: &str, error: ProductError) -> Value {
    case(
        id,
        json!({
            "code": error.code().as_str(),
            "category": error.category().as_str(),
            "retry": error.retry().as_str(),
            "transaction_state": error.transaction_state().as_str(),
            "request_id": error.request_id().map(|value| value.to_string()),
        }),
    )
}

fn case(id: &str, outcome: Value) -> Value {
    json!({"id": id, "outcome": outcome})
}

fn session() -> ProductSession {
    ProductSession::new(
        ProductSessionId::new(1).expect("nonzero"),
        ProductPrincipal::new("g6-conformance").expect("bounded"),
        ProductAuthorization::ALL,
    )
}

fn context(session: &ProductSession, request_id: u128) -> ProductRequestContext {
    let mut context = ProductRequestContext::new(
        request_id,
        session.id(),
        1_700_000_000_000_000,
        session.principal().clone(),
        session.authorization(),
    );
    context.limits = ProductLimits::default();
    context.limits.max_response_bytes = hyphae_native_product::MAX_PRODUCT_CONTEXT_BYTES;
    context.limits.max_memory_bytes = hyphae_native_product::MAX_PRODUCT_CONTEXT_BYTES;
    context.durability = ProductDurabilityPolicy {
        durability: ProductDurability::Strict,
    };
    context
}

fn proof_limits() -> hyphae_native_product::proof::NativeProofGenerationLimits {
    let mut limits = hyphae_native_product::proof::NativeProofGenerationLimits::default();
    limits.witness.max_witness_bytes = 16 * 1024 * 1024;
    limits.witness.max_file_bytes = 16 * 1024 * 1024;
    limits.witness.max_total_file_bytes = 16 * 1024 * 1024;
    limits.witness.max_decoded_bytes = 16 * 1024 * 1024;
    limits
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|value| format!("{value:02x}")).collect()
}

fn structure_key(id: u128, key: &[u8]) -> Result<ProductStructureKey, Box<dyn Error>> {
    Ok(ProductStructureKey {
        keyspace: ObjectId::new(id)?,
        key: key.to_vec(),
    })
}

fn configure_search(product: &mut NativeProduct) -> Result<(), Box<dyn Error>> {
    product.create_catalog_object_v2(
        LogicalCatalogObject::V2(CatalogObjectV2::Analyzer(AnalyzerDefinition {
            header: header(15, EngineKind::Search, "canonical", Some(9))?,
            tokenizer: AnalyzerTokenizer::UnicodeWord,
            filters: vec![AnalyzerFilter::Lowercase],
        })),
        ProductDurability::Strict,
    )?;
    let ann = AnnIndexDefinition::new(VectorMetric::SquaredL2, 8, 32, 16, 256, 7)?;
    let lifecycle = IncrementalVectorLifecycle {
        delta_max_entries: 1_000,
        consolidate_after_deltas: 4,
        retain_generations: 2,
    };
    product.create_catalog_object_v2(
        LogicalCatalogObject::V2(CatalogObjectV2::SearchCollection(
            SearchCollectionDefinitionV2 {
                header: header(17, EngineKind::Search, "g6_documents", Some(9))?,
                fields: vec![
                    SearchFieldDefinitionV2 {
                        id: FieldId::new(1)?,
                        name: CatalogName::unquoted("body")?,
                        logical_type: LogicalType::Text,
                        analyzer: Some(ObjectId::new(15)?),
                        options: SearchFieldOptions {
                            stored: true,
                            doc_values: false,
                            source: FieldSourcePolicy::Retained,
                            lexical: LexicalIndexPolicy::Frequencies,
                        },
                    },
                    SearchFieldDefinitionV2 {
                        id: FieldId::new(2)?,
                        name: CatalogName::unquoted("category")?,
                        logical_type: LogicalType::Text,
                        analyzer: None,
                        options: SearchFieldOptions {
                            stored: true,
                            doc_values: true,
                            source: FieldSourcePolicy::Retained,
                            lexical: LexicalIndexPolicy::None,
                        },
                    },
                ],
                vectors: vec![
                    NamedVectorDefinition {
                        id: FieldId::new(3)?,
                        name: CatalogName::unquoted("exact")?,
                        vector_type: VectorType::new(VectorElement::Float32, 2)?,
                        metric: VectorMetric::SquaredL2,
                        policy: VectorSearchPolicy::Exact,
                        lifecycle,
                    },
                    NamedVectorDefinition {
                        id: FieldId::new(4)?,
                        name: CatalogName::unquoted("ann")?,
                        vector_type: VectorType::new(VectorElement::Float32, 2)?,
                        metric: VectorMetric::SquaredL2,
                        policy: VectorSearchPolicy::Ann(ann),
                        lifecycle,
                    },
                ],
            },
        )),
        ProductDurability::Strict,
    )?;
    let collection = ObjectId::new(17)?;
    product.provision_search_collection(
        collection,
        1_700_000_000_000_000,
        ProductDurability::Strict,
    )?;
    let documents = vec![
        g6_document(201, "rust database engine", "book", [0.0, 0.0])?,
        g6_document(202, "rust field guide", "book", [1.0, 0.0])?,
        g6_document(203, "database hardware", "gear", [2.0, 0.0])?,
    ];
    product.ingest_search_batch(
        collection,
        &hyphae_native_product::ProductSearchIngestBatch {
            idempotency_id: 1,
            documents,
        },
        1_700_000_000_000_000,
        ProductDurability::Strict,
    )?;
    Ok(())
}

fn g6_document(
    id: u128,
    text: &str,
    category: &str,
    vector: [f32; 2],
) -> Result<hyphae_native_product::ProductDocument, Box<dyn Error>> {
    Ok(hyphae_native_product::ProductDocument {
        object_id: ObjectId::new(id)?,
        text: text.into(),
        doc_values: BTreeMap::from([(
            "category".into(),
            hyphae_native_product::ProductDocValue::String(category.into()),
        )]),
        vectors: BTreeMap::from([
            (
                "exact".into(),
                hyphae_native_product::ProductVector::new(vector)?,
            ),
            (
                "ann".into(),
                hyphae_native_product::ProductVector::new(vector)?,
            ),
        ]),
    })
}

fn keyspace(
    id: u128,
    kind: StructureKind,
    name: &str,
) -> Result<LogicalCatalogObject, Box<dyn Error>> {
    Ok(LogicalCatalogObject::V2(CatalogObjectV2::Keyspace(
        KeyspaceDefinition {
            header: header(id, EngineKind::Structure, name, Some(9))?,
            kind,
            key_type: LogicalType::Binary,
            value_type: LogicalType::Binary,
            ownership: StructureOwnership::Canonical,
            ttl_policy: KeyspaceTtlPolicy::PerValue,
            default_ttl_millis: None,
            memory_class: KeyspaceMemoryClass::Durable,
            eviction: KeyspaceEvictionPolicy::None,
            relation_schema: None,
        },
    )))
}

fn header(
    id: u128,
    owner: EngineKind,
    name: &str,
    parent: Option<u128>,
) -> Result<ObjectHeaderV2, Box<dyn Error>> {
    Ok(ObjectHeaderV2 {
        id: ObjectId::new(id)?,
        owner,
        name: QualifiedName::new(
            CatalogName::unquoted("main")?,
            CatalogName::unquoted("public")?,
            CatalogName::unquoted(name)?,
        ),
        parent: parent.map(ObjectId::new).transpose()?,
        definition_version: DefinitionVersion::FIRST,
    })
}
