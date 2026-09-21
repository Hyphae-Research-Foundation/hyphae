// SPDX-License-Identifier: Apache-2.0

//! Bounded client for Hyphae's optional local Candle worker.

use crate::{
    agent_policy::{Policy, embedding_endpoint},
    exit::CliFailure,
};
use serde_json::{Value, json};
use std::time::Duration;

pub(crate) async fn request(
    operation: &str,
    texts: &[String],
    policy: &Policy,
) -> Result<Value, CliFailure> {
    #[cfg(unix)]
    {
        use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};
        let payload = json!({"schema":"hyphae-embed-request-v1","id":1,"operation":operation,
            "texts":texts,"expected_model":if operation == "status" { None } else { policy.model_fingerprint() }});
        let mut encoded = serde_json::to_vec(&payload)?;
        if encoded.len() > 2 * 1024 * 1024 {
            return Err(CliFailure::invalid());
        }
        encoded.push(b'\n');
        let endpoint = embedding_endpoint()?;
        let response =
            tokio::time::timeout(Duration::from_millis(policy.recall_timeout_ms), async {
                let mut stream = tokio::net::UnixStream::connect(endpoint).await?;
                stream.write_all(&encoded).await?;
                stream.shutdown().await?;
                let mut reply = Vec::new();
                tokio::io::BufReader::new(stream)
                    .take(16 * 1024 * 1024 + 1)
                    .read_until(b'\n', &mut reply)
                    .await?;
                Ok::<_, std::io::Error>(reply)
            })
            .await
            .map_err(|_| CliFailure::invalid())??;
        if response.len() > 16 * 1024 * 1024 || response.last() != Some(&b'\n') {
            return Err(CliFailure::invalid());
        }
        let response: Value = serde_json::from_slice(&response)?;
        if response["schema"] != "hyphae-embed-response-v1"
            || response["id"] != 1
            || response["ok"] != true
        {
            return Err(CliFailure::invalid());
        }
        Ok(response["result"].clone())
    }
    #[cfg(not(unix))]
    {
        let _ = (operation, texts, policy);
        Err(CliFailure::invalid())
    }
}

pub(crate) async fn embed(
    text: &str,
    policy: &Policy,
) -> Result<(hyphae_native_product::ProductVector, Value), CliFailure> {
    let output = request("embed", &[text.to_owned()], policy).await?;
    if output["model"]["fingerprint"].as_str() != policy.model_fingerprint() {
        return Err(CliFailure::invalid());
    }
    let vectors = output["vectors"]
        .as_array()
        .ok_or_else(CliFailure::invalid)?;
    if vectors.len() != 1 {
        return Err(CliFailure::invalid());
    }
    let values: Vec<f32> = serde_json::from_value(vectors[0].clone())?;
    if Some(values.len()) != policy.embedding_dimensions().map(usize::from) {
        return Err(CliFailure::invalid());
    }
    let vector =
        hyphae_native_product::ProductVector::new(values).map_err(|_| CliFailure::invalid())?;
    Ok((
        vector,
        json!({"model":output["model"],"attestation_hex":output["attestation_hex"]}),
    ))
}

pub(crate) fn plain_search(limit: usize) -> hyphae_native_product::ProductSearchRequest {
    hyphae_native_product::ProductSearchRequest {
        lexical: None,
        vectors: Vec::new(),
        filter: hyphae_native_product::ProductSearchFilter::MatchAll,
        sort: Vec::new(),
        facets: Vec::new(),
        range_facets: Vec::new(),
        aggregations: Vec::new(),
        limit,
        fusion: None,
        parent_dedupe: None,
        rerank: None,
        highlight: None,
        autocut: None,
        offset: 0,
    }
}

fn pending_filter(policy: &Policy) -> hyphae_native_product::ProductSearchFilter {
    use hyphae_native_product::{
        ProductDocValue as Value, ProductSearchFilter as Filter, ProductSearchOperator,
    };
    Filter::Any(vec![
        Filter::Not(Box::new(Filter::Exists("embedding_model".into()))),
        Filter::Compare {
            field: "embedding_model".into(),
            operator: ProductSearchOperator::NotEqual,
            value: Value::String(policy.model_fingerprint().unwrap_or_default().into()),
        },
    ])
}

pub(crate) async fn pending_count(policy: &Policy) -> Result<u64, CliFailure> {
    use hyphae_native_product::{
        ObjectId, ProductMemoryRecallRequest, ProductOperation, ProductResponse,
    };
    let client = crate::agent_control::operator_client()?;
    let mut search = plain_search(1);
    search.filter = pending_filter(policy);
    let mut collections = policy
        .collections
        .into_iter()
        .map(ObjectId::new)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| CliFailure::invalid())?;
    collections.sort_unstable();
    let response = client
        .execute(
            ProductOperation::MemoryRecall(ProductMemoryRecallRequest {
                collections,
                search,
                limit: 1,
                provenance: Vec::new(),
            }),
            hyphae_client::v2::RequestOptions {
                logical_time_micros: crate::native::logical_time_micros(),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| CliFailure::io())?;
    let ProductResponse::MemoryRecall(result) = response else {
        return Err(CliFailure::invalid());
    };
    Ok(result.searches.iter().fold(0_u64, |total, search| {
        total.saturating_add(search.result.eligible_documents as u64)
    }))
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn maintain() -> Result<(), CliFailure> {
    let _ = crate::agent_hooks::drain_pending().await;
    let Ok(client) = crate::agent_control::operator_client() else {
        return Ok(());
    };
    let policy = Policy::active(&client).await?;
    if !policy.semantic.enabled {
        return Ok(());
    }
    let mut search = plain_search(1_000);
    search.filter = pending_filter(&policy);
    let mut collections = policy
        .collections
        .into_iter()
        .map(hyphae_native_product::ObjectId::new)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| CliFailure::invalid())?;
    collections.sort_unstable();
    let response = client
        .execute(
            hyphae_native_product::ProductOperation::MemoryRecall(
                hyphae_native_product::ProductMemoryRecallRequest {
                    collections,
                    search,
                    limit: 16,
                    provenance: Vec::new(),
                },
            ),
            hyphae_client::v2::RequestOptions {
                logical_time_micros: crate::native::logical_time_micros(),
                ..Default::default()
            },
        )
        .await
        .map_err(|_| CliFailure::io())?;
    let hyphae_native_product::ProductResponse::MemoryRecall(result) = response else {
        return Err(CliFailure::invalid());
    };
    for memory in result.memories {
        let envelope: Value = serde_json::from_slice(&memory.envelope)?;
        let text = envelope["text"].as_str().ok_or_else(CliFailure::invalid)?;
        let Ok((vector, provenance)) = embed(text, &policy).await else {
            break;
        };
        let mut values = memory.hit.doc_values;
        values.insert(
            "embedding_model".into(),
            hyphae_native_product::ProductDocValue::String(
                policy
                    .model_fingerprint()
                    .ok_or_else(CliFailure::invalid)?
                    .into(),
            ),
        );
        values.insert(
            "embedding_attestation".into(),
            hyphae_native_product::ProductDocValue::String(
                provenance["attestation_hex"]
                    .as_str()
                    .ok_or_else(CliFailure::invalid)?
                    .into(),
            ),
        );
        values.insert(
            "embedding_manifest".into(),
            hyphae_native_product::ProductDocValue::String(serde_json::to_string(
                &provenance["model"],
            )?),
        );
        let mut identity = blake3::Hasher::new();
        identity.update(b"hyphae-agent-enrichment-v1\0");
        identity.update(&memory.collection.get().to_le_bytes());
        identity.update(&memory.hit.object_id.get().to_le_bytes());
        identity.update(
            &result
                .snapshot
                .visible_csn
                .map_or(0, hyphae_native_product::Csn::get)
                .to_le_bytes(),
        );
        identity.update(
            policy
                .model_fingerprint()
                .ok_or_else(CliFailure::invalid)?
                .as_bytes(),
        );
        let mut bytes = [0_u8; 16];
        bytes.copy_from_slice(&identity.finalize().as_bytes()[..16]);
        let operation = hyphae_native_product::ProductOperation::MemoryEnrich(
            hyphae_native_product::ProductMemoryEnrichRequest {
                collection: memory.collection,
                expected_envelope_digest: *blake3::hash(&memory.envelope).as_bytes(),
                update: hyphae_native_product::ProductSearchDocumentUpdate {
                    idempotency_id: u128::from_le_bytes(bytes).max(1),
                    document: hyphae_native_product::ProductDocument {
                        object_id: memory.hit.object_id,
                        text: text.into(),
                        doc_values: values,
                        vectors: std::collections::BTreeMap::from([("memory".into(), vector)]),
                    },
                },
            },
        );
        // Stale/deleted sources are rejected by the owning native operation.
        let _ = client
            .execute(
                operation,
                hyphae_client::v2::RequestOptions {
                    logical_time_micros: crate::native::logical_time_micros(),
                    ..Default::default()
                },
            )
            .await;
    }
    Ok(())
}

fn embed_binary() -> Result<std::path::PathBuf, CliFailure> {
    if let Some(path) = std::env::var_os("HYPHAE_EMBED_BINARY").map(std::path::PathBuf::from) {
        if path.is_file() {
            return Ok(path);
        }
        return Err(CliFailure::invalid());
    }
    let binary = std::env::current_exe()?;
    let sibling = binary
        .parent()
        .ok_or_else(CliFailure::invalid)?
        .join("hyphae-embed");
    if sibling.is_file() {
        return Ok(sibling);
    }
    for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let path = directory.join("hyphae-embed");
        if path.is_file() {
            return Ok(path);
        }
    }
    Err(CliFailure::invalid())
}

pub(crate) async fn configure(
    enabled: bool,
    model_dir: Option<std::path::PathBuf>,
    mode: Option<crate::agent_policy::SemanticSearchMode>,
) -> Result<Value, CliFailure> {
    let mut policy = Policy::current().await?;
    let search_mode = mode.unwrap_or(policy.semantic.search_mode);
    if !enabled {
        policy.semantic.enabled = false;
        policy.semantic.search_mode = search_mode;
        crate::agent_control::commit_policy(&policy).await?;
        if policy.manage_services {
            let _ = crate::agent::systemctl(&["stop", "hyphae-agent-embed"]);
        }
        return Ok(
            json!({"message":"Semantic search disabled. Lexical memory remains available."}),
        );
    }
    let paths = crate::agent::AgentPaths::resolve()?;
    let directory = model_dir
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| policy.semantic.model_dir.clone())
        .unwrap_or_else(|| {
            paths
                .data
                .parent()
                .unwrap_or(&paths.data)
                .join("models/bge-small-en-v1.5")
        })
        .canonicalize()?;
    let binary = embed_binary()?;
    let output = tokio::process::Command::new(&binary)
        .args(["model-info", "--model-dir"])
        .arg(&directory)
        .output()
        .await?;
    if !output.status.success() || output.stdout.len() > 64 * 1024 {
        return Err(CliFailure::invalid());
    }
    let model: Value = serde_json::from_slice(&output.stdout)?;
    if model["schema"] != "hyphae-attested-model-v1" {
        return Err(CliFailure::invalid());
    }
    let same_model = policy.model_fingerprint() == model["fingerprint"].as_str();
    let mut next = policy.clone();
    next.semantic = crate::agent_policy::Semantic {
        enabled: true,
        model_dir: Some(directory.clone()),
        model: Some(model),
        search_mode,
    };
    next.validate()?;
    // Capture remains durable in the spool while the daemon is stopped. Do
    // not persist a temporary pause which could survive a process crash.
    let active = policy.manage_services && crate::agent::service_active();
    if active {
        crate::agent::systemctl(&["stop", "hyphae-agent-memory"])?;
    }
    let migration: Result<(), CliFailure> = async {
        if same_model {
            recover()?;
            let mut product = hyphae_native_product::NativeProduct::open(&paths.data)?;
            product.migration_store_public_entry(
                crate::agent_control::POLICY_KEY.to_vec(),
                serde_json::to_vec(&next)?,
                None,
            )?;
            next.save()
        } else {
            // A native backup retains the previous active profile and full data.
            {
                let mut product = hyphae_native_product::NativeProduct::open(&paths.data)?;
                product.migration_store_public_entry(
                    crate::agent_control::POLICY_KEY.to_vec(),
                    serde_json::to_vec(&policy)?,
                    None,
                )?;
            }
            crate::agent_control::run_self(&["agent", "backup"]).await?;
            migrate_semantic(&mut next)
        }
    }
    .await;
    let restart = if active {
        crate::agent::start_memory_service()
    } else {
        Ok(())
    };
    migration?;
    restart?;
    crate::agent_policy::runtime_directory()?;
    if next.manage_services {
        install_worker_unit(&binary, &directory)?;
        crate::agent::systemctl(&["enable", "hyphae-agent-embed"])?;
        crate::agent::systemctl(&["restart", "hyphae-agent-embed"])?;
    }
    Ok(
        json!({"message":"Semantic memory enabled. Existing records will be enriched in the background.","model":next.semantic.model}),
    )
}

pub(crate) async fn activate_worker() -> Result<(), CliFailure> {
    crate::agent_control::wait_for_service().await?;
    let policy = Policy::current().await?;
    if policy.manage_services && policy.semantic.enabled {
        let model = policy
            .semantic
            .model_dir
            .as_ref()
            .ok_or_else(CliFailure::invalid)?;
        install_worker_unit(&embed_binary()?, model)?;
        crate::agent::systemctl(&["enable", "hyphae-agent-embed"])?;
        crate::agent::systemctl(&["restart", "hyphae-agent-embed"])?;
    }
    Ok(())
}

fn install_worker_unit(
    binary: &std::path::Path,
    model: &std::path::Path,
) -> Result<(), CliFailure> {
    let paths = crate::agent::AgentPaths::resolve()?;
    let directory = paths
        .config
        .parent()
        .ok_or_else(CliFailure::invalid)?
        .join("systemd/user");
    std::fs::create_dir_all(&directory)?;
    let unit = format!(
        "[Unit]\nDescription=Hyphae local attested embeddings\nPartOf=hyphae-agent-memory.service\n\n[Service]\nType=simple\nExecStart={} serve --model-dir {} --endpoint {}\nRestart=on-failure\nRestartSec=2\nUMask=0077\nNoNewPrivileges=yes\nMemoryMax=1536M\nNice=10\n\n[Install]\nWantedBy=default.target\n",
        crate::agent::systemd_argument(&binary.to_string_lossy()),
        crate::agent::systemd_argument(&model.to_string_lossy()),
        crate::agent::systemd_argument(&embedding_endpoint()?.to_string_lossy())
    );
    crate::agent::write_atomic(
        &directory.join("hyphae-agent-embed.service"),
        unit.as_bytes(),
    )?;
    crate::agent::systemctl(&["daemon-reload"])?;
    crate::agent::reset_service_failure("hyphae-agent-embed");
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SemanticMigration {
    schema: String,
    sources: [u128; 3],
    destinations: [u128; 3],
    fingerprint: String,
    source_digest: String,
    stamp: i64,
}

const MIGRATION_KEY: &[u8] = b"hyphae-agent-semantic-migration/v1";

/// Reconcile a committed cutover before starting the managed daemon. The
/// native record survives a crash between profile publication and retirement.
pub(crate) fn recover() -> Result<(), CliFailure> {
    let paths = crate::agent::AgentPaths::resolve()?;
    let mut product = hyphae_native_product::NativeProduct::open(&paths.data)?;
    let mut policy = Policy::load()?;
    if let Some(active) = recover_at(&mut product)? {
        policy.collections = active.collections;
        policy.semantic = active.semantic;
    }
    policy.save()
}

fn recover_at(
    product: &mut hyphae_native_product::NativeProduct,
) -> Result<Option<Policy>, CliFailure> {
    let active = Policy::from_product(product)?;
    let snapshot = product.snapshot_bounded(crate::native::logical_time_micros())?;
    let plan = snapshot
        .structure_get(MIGRATION_KEY)
        .map(serde_json::from_slice::<SemanticMigration>)
        .transpose()?;
    if let (Some(active), Some(plan)) = (&active, plan) {
        if plan.schema != "hyphae-agent-semantic-migration-v1"
            || plan.sources.iter().any(|id| plan.destinations.contains(id))
        {
            return Err(CliFailure::invalid());
        }
        if active.collections == plan.destinations
            && active.model_fingerprint() == Some(plan.fingerprint.as_str())
        {
            retire_sources(product, &plan)?;
        }
    }
    Ok(active)
}

fn retire_sources(
    product: &mut hyphae_native_product::NativeProduct,
    plan: &SemanticMigration,
) -> Result<(), CliFailure> {
    let now = crate::native::logical_time_micros();
    let snapshot = product.snapshot_bounded(now)?;
    visit_documents(&snapshot, &plan.sources, |index, document, _| {
        product.migration_delete_public_entry(crate::mcp::memory_key(
            plan.sources[index],
            document.object_id.get(),
        ))?;
        product.delete_search_document(
            hyphae_native_product::ObjectId::new(plan.sources[index])
                .map_err(|_| CliFailure::invalid())?,
            hyphae_native_product::ProductSearchDocumentDelete {
                idempotency_id: migration_identity(plan, index, document.object_id, b"retire"),
                object_id: document.object_id,
            },
            now,
            hyphae_native_product::ProductDurability::Strict,
        )?;
        Ok(())
    })?;
    product.migration_delete_public_entry(MIGRATION_KEY.to_vec())?;
    Ok(())
}

fn migration_identity(
    plan: &SemanticMigration,
    index: usize,
    identity: hyphae_native_product::ObjectId,
    purpose: &[u8],
) -> u128 {
    let mut digest = blake3::Hasher::new();
    digest.update(b"hyphae-agent-semantic-operation-v1\0");
    digest.update(purpose);
    digest.update(&plan.sources[index].to_le_bytes());
    digest.update(&plan.destinations[index].to_le_bytes());
    digest.update(&identity.get().to_le_bytes());
    digest.update(&plan.stamp.to_le_bytes());
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest.finalize().as_bytes()[..16]);
    u128::from_le_bytes(bytes).max(1)
}

fn document_bytes(
    collection: hyphae_native_product::ObjectId,
    document: &hyphae_native_product::ProductDocument,
) -> Result<Vec<u8>, CliFailure> {
    hyphae_native_protocol::encode_product_request(&hyphae_native_protocol::WireRequest {
        operation: hyphae_native_product::ProductOperation::SearchIngest {
            collection,
            batch: hyphae_native_product::ProductSearchIngestBatch {
                idempotency_id: document.object_id.get(),
                documents: vec![document.clone()],
            },
        },
        logical_time_micros: 0,
        deadline_micros: None,
        idempotency_token: None,
        limits: hyphae_native_product::ProductLimits::default(),
        durability: hyphae_native_product::ProductDurabilityPolicy::STRICT,
    })
    .map_err(|_| CliFailure::invalid())
}

fn visit_documents(
    snapshot: &hyphae_native_product::ProductSnapshot,
    collections: &[u128; 3],
    mut visit: impl FnMut(
        usize,
        hyphae_native_product::ProductDocument,
        Option<Vec<u8>>,
    ) -> Result<(), CliFailure>,
) -> Result<(), CliFailure> {
    use hyphae_native_product::{NativeProduct, ObjectId, memory_lifecycle_key};
    for (index, collection) in collections.iter().enumerate() {
        let collection = ObjectId::new(*collection).map_err(|_| CliFailure::invalid())?;
        let mut cursor = None;
        let mut count = 0_usize;
        loop {
            let page =
                NativeProduct::search_documents_at_snapshot(snapshot, collection, cursor, 1_000)?;
            for document in page.documents {
                count += 1;
                if count > 250_000 {
                    return Err(CliFailure::invalid());
                }
                let envelope = snapshot
                    .structure_get(&memory_lifecycle_key(collection, document.object_id))
                    .filter(|value| !value.is_empty())
                    .map(<[u8]>::to_vec);
                visit(index, document, envelope)?;
            }
            if page.continuation.is_none() {
                break;
            }
            cursor = page.continuation;
        }
    }
    Ok(())
}

fn migrate_semantic(policy: &mut Policy) -> Result<(), CliFailure> {
    let paths = crate::agent::AgentPaths::resolve()?;
    migrate_semantic_at(&paths.data, policy, |_| Ok(()))?;
    policy.save()
}

#[allow(clippy::too_many_lines)]
fn migrate_semantic_at(
    data: &std::path::Path,
    policy: &mut Policy,
    mut checkpoint: impl FnMut(&str) -> Result<(), CliFailure>,
) -> Result<(), CliFailure> {
    use hyphae_native_catalog::{
        AnnIndexDefinition, CatalogName, CatalogObjectV2, FieldSourcePolicy,
        IncrementalVectorLifecycle, LexicalIndexPolicy, LogicalCatalogObject,
        NamedVectorDefinition, QualifiedName, SearchFieldDefinitionV2, SearchFieldOptions,
        VectorMetric, VectorSearchPolicy,
    };
    use hyphae_native_product::{
        NativeProduct, ObjectId, ProductDurability, ProductSearchIngestBatch,
    };
    use hyphae_native_types::{FieldId, LogicalType, VectorElement, VectorType};
    let mut product = NativeProduct::open(data)?;
    let now = crate::native::logical_time_micros();
    let snapshot = product.snapshot_bounded(now)?;
    let sources = policy.collections;
    let mut digest = blake3::Hasher::new();
    digest.update(b"hyphae-semantic-migration-source-v1\0");
    visit_documents(&snapshot, &sources, |index, document, envelope| {
        if let Some(envelope) = envelope {
            digest.update(&document_bytes(
                ObjectId::new(sources[index]).map_err(|_| CliFailure::invalid())?,
                &document,
            )?);
            digest.update(&(envelope.len() as u64).to_le_bytes());
            digest.update(&envelope);
        }
        Ok(())
    })?;
    let source_digest = digest.finalize().to_hex().to_string();
    let previous = snapshot
        .structure_get(MIGRATION_KEY)
        .map(serde_json::from_slice::<SemanticMigration>)
        .transpose()?;
    let plan = if let Some(plan) = previous.filter(|plan| {
        plan.sources == sources
            && plan.fingerprint == policy.model_fingerprint().unwrap_or_default()
            && plan.source_digest == source_digest
    }) {
        plan
    } else {
        let mut destinations = [0; 3];
        for (index, source) in sources.iter().enumerate() {
            let mut candidate = source
                .checked_add(100_000)
                .ok_or_else(CliFailure::invalid)?;
            while product
                .catalog_describe(
                    &product.catalog_snapshot()?,
                    ObjectId::new(candidate).map_err(|_| CliFailure::invalid())?,
                )?
                .is_some()
                || destinations.contains(&candidate)
            {
                candidate = candidate.checked_add(1).ok_or_else(CliFailure::invalid)?;
            }
            destinations[index] = candidate;
        }
        let plan = SemanticMigration {
            schema: "hyphae-agent-semantic-migration-v1".into(),
            sources,
            destinations,
            fingerprint: policy
                .model_fingerprint()
                .ok_or_else(CliFailure::invalid)?
                .into(),
            source_digest,
            stamp: now,
        };
        product.migration_store_public_entry(
            MIGRATION_KEY.to_vec(),
            serde_json::to_vec(&plan)?,
            None,
        )?;
        plan
    };
    for (index, source) in sources.iter().enumerate() {
        let source = ObjectId::new(*source).map_err(|_| CliFailure::invalid())?;
        let destination =
            ObjectId::new(plan.destinations[index]).map_err(|_| CliFailure::invalid())?;
        let LogicalCatalogObject::V2(CatalogObjectV2::SearchCollection(mut definition)) = product
            .catalog_describe(&product.catalog_snapshot()?, source)?
            .ok_or_else(CliFailure::invalid)?
        else {
            return Err(CliFailure::invalid());
        };
        definition.header.id = destination;
        definition.header.name = QualifiedName::new(
            definition.header.name.database.clone(),
            definition.header.name.schema.clone(),
            CatalogName::unquoted(format!(
                "agent_memory_{}_s{}",
                ["personal", "work", "journal"][index],
                plan.stamp
            ))
            .map_err(|_| CliFailure::invalid())?,
        );
        let mut field = 100;
        for name in [
            "embedding_model",
            "embedding_attestation",
            "embedding_manifest",
        ] {
            if definition
                .fields
                .iter()
                .any(|item| item.name.lookup() == name)
            {
                continue;
            }
            while definition.fields.iter().any(|item| item.id.get() == field)
                || definition.vectors.iter().any(|item| item.id.get() == field)
            {
                field += 1;
            }
            definition.fields.push(SearchFieldDefinitionV2 {
                id: FieldId::new(field).map_err(|_| CliFailure::invalid())?,
                name: CatalogName::unquoted(name).map_err(|_| CliFailure::invalid())?,
                logical_type: LogicalType::Text,
                analyzer: None,
                options: SearchFieldOptions {
                    stored: true,
                    doc_values: true,
                    source: FieldSourcePolicy::Retained,
                    lexical: LexicalIndexPolicy::None,
                },
            });
            field += 1;
        }
        definition
            .vectors
            .retain(|vector| vector.name.lookup() != "memory");
        while definition.fields.iter().any(|item| item.id.get() == field)
            || definition.vectors.iter().any(|item| item.id.get() == field)
        {
            field += 1;
        }
        definition.vectors.push(NamedVectorDefinition {
            id: FieldId::new(field).map_err(|_| CliFailure::invalid())?,
            name: CatalogName::unquoted("memory").map_err(|_| CliFailure::invalid())?,
            vector_type: VectorType::new(
                VectorElement::Float32,
                policy
                    .embedding_dimensions()
                    .ok_or_else(CliFailure::invalid)?,
            )
            .map_err(|_| CliFailure::invalid())?,
            metric: VectorMetric::Cosine,
            policy: VectorSearchPolicy::Adaptive {
                exact_candidate_threshold: 1000,
                ann: AnnIndexDefinition::new(VectorMetric::Cosine, 16, 100, 64, 256, 7)
                    .map_err(|_| CliFailure::invalid())?,
            },
            lifecycle: IncrementalVectorLifecycle {
                delta_max_entries: 4096,
                consolidate_after_deltas: 256,
                retain_generations: 2,
            },
        });
        definition.fields.sort_by_key(|field| field.id);
        definition.vectors.sort_by_key(|vector| vector.id);
        let object = LogicalCatalogObject::V2(CatalogObjectV2::SearchCollection(definition));
        match product.catalog_describe(&product.catalog_snapshot()?, destination)? {
            Some(existing) if existing != object => return Err(CliFailure::invalid()),
            Some(_) => {}
            None => {
                product.create_catalog_object_v2(object, ProductDurability::Strict)?;
            }
        }
    }
    // Reserve every logical destination before physical indexes allocate IDs.
    // A retry reuses an already provisioned binding instead of recreating it.
    for destination in plan.destinations {
        let destination = ObjectId::new(destination).map_err(|_| CliFailure::invalid())?;
        if product
            .resolve_search_collection_binding(destination, now)
            .is_err()
        {
            product.provision_search_collection(destination, now, ProductDurability::Strict)?;
        }
    }
    visit_documents(&snapshot, &sources, |index, mut document, envelope| {
        let Some(envelope) = envelope else {
            return Ok(());
        };
        let value: Value = serde_json::from_slice(&envelope)?;
        if value["text"].as_str() != Some(document.text.as_str()) {
            return Err(CliFailure::invalid());
        }
        let expiry = value["expires_at_micros"].as_i64();
        let destination =
            ObjectId::new(plan.destinations[index]).map_err(|_| CliFailure::invalid())?;
        document.vectors.remove("memory");
        document.doc_values.retain(|name, _| {
            !matches!(
                name.as_str(),
                "embedding_model" | "embedding_attestation" | "embedding_manifest"
            )
        });
        let batch = ProductSearchIngestBatch {
            idempotency_id: migration_identity(&plan, index, document.object_id, b"copy"),
            documents: vec![document.clone()],
        };
        match product.ingest_search_batch(destination, &batch, now, ProductDurability::Strict) {
            Ok(_) => {}
            Err(error)
                if error.code() == hyphae_native_product::ProductErrorCode::CatalogConflict => {}
            Err(error) => return Err(error.into()),
        }
        product.migration_store_public_entry(
            hyphae_native_product::memory_lifecycle_key(destination, document.object_id),
            envelope.clone(),
            expiry,
        )?;
        let current = product.snapshot_bounded(now)?;
        let previous = document
            .object_id
            .get()
            .checked_sub(1)
            .and_then(|id| ObjectId::new(id).ok());
        let page = NativeProduct::search_documents_at_snapshot(&current, destination, previous, 1)?;
        if page.documents.first() != Some(&document)
            || current.structure_get(&hyphae_native_product::memory_lifecycle_key(
                destination,
                document.object_id,
            )) != Some(envelope.as_slice())
        {
            return Err(CliFailure::invalid());
        }
        checkpoint("copy")?;
        Ok(())
    })?;
    policy.collections = plan.destinations;
    product.migration_store_public_entry(
        crate::agent_control::POLICY_KEY.to_vec(),
        serde_json::to_vec(policy)?,
        None,
    )?;
    checkpoint("cutover")?;
    // The verified copy barrier precedes retirement. Old explicit collection
    // bindings must not retain live versions of memories later forgotten.
    retire_sources(&mut product, &plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyphae_native_catalog::{
        AnalyzerDefinition, AnalyzerFilter, AnalyzerTokenizer, CatalogObjectV2, FieldSourcePolicy,
        LexicalIndexPolicy, LogicalCatalogObject, SearchCollectionDefinitionV2,
        SearchFieldDefinitionV2, SearchFieldOptions,
    };
    use hyphae_native_product::{
        NativeProduct, ProductDocument, ProductDurability, ProductSearchIngestBatch,
        memory_lifecycle_key,
    };
    use hyphae_native_types::{EngineKind, LogicalType};

    #[allow(clippy::too_many_lines)]
    fn fixture(label: &str) -> Result<(std::path::PathBuf, Policy), Box<dyn std::error::Error>> {
        let path = std::env::temp_dir().join(format!(
            "hyphae-semantic-{label}-{}-{}",
            std::process::id(),
            crate::native::logical_time_micros()
        ));
        let mut product = NativeProduct::create(&path)?;
        let objects = [
            CatalogObjectV2::Database(crate::catalog_header(
                10,
                EngineKind::Kernel,
                "main.public.database",
                None,
            )?),
            CatalogObjectV2::Schema(crate::catalog_header(
                11,
                EngineKind::Kernel,
                "main.public.schema",
                Some(10),
            )?),
            CatalogObjectV2::Analyzer(AnalyzerDefinition {
                header: crate::catalog_header(
                    12,
                    EngineKind::Search,
                    "main.public.analyzer",
                    Some(11),
                )?,
                tokenizer: AnalyzerTokenizer::UnicodeWord,
                filters: vec![AnalyzerFilter::Lowercase],
            }),
        ];
        for object in objects {
            product.create_catalog_object_v2(
                LogicalCatalogObject::V2(object),
                ProductDurability::Strict,
            )?;
        }
        let mut policy = Policy::default();
        for id in policy.collections {
            let collection = SearchCollectionDefinitionV2 {
                header: crate::catalog_header(
                    id,
                    EngineKind::Search,
                    &format!("main.public.memory_{id}"),
                    Some(11),
                )?,
                bm25: None,
                vectors: Vec::new(),
                fields: vec![SearchFieldDefinitionV2 {
                    id: crate::field_id(1)?,
                    name: crate::catalog_name("body")?,
                    logical_type: LogicalType::Text,
                    analyzer: Some(crate::object_id(12)?),
                    options: SearchFieldOptions {
                        stored: true,
                        doc_values: false,
                        source: FieldSourcePolicy::Retained,
                        lexical: LexicalIndexPolicy::Frequencies,
                    },
                }],
            };
            product.create_catalog_object_v2(
                LogicalCatalogObject::V2(CatalogObjectV2::SearchCollection(collection)),
                ProductDurability::Strict,
            )?;
        }
        for id in policy.collections {
            let collection = crate::object_id(id)?;
            product.provision_search_collection(collection, 1, ProductDurability::Strict)?;
            let identity = crate::object_id(501)?;
            let document = ProductDocument {
                object_id: identity,
                text: "durable source".into(),
                doc_values: std::collections::BTreeMap::new(),
                vectors: std::collections::BTreeMap::new(),
            };
            product.ingest_search_batch(
                collection,
                &ProductSearchIngestBatch {
                    idempotency_id: 501,
                    documents: vec![document],
                },
                1,
                ProductDurability::Strict,
            )?;
            product.migration_store_public_entry(
                memory_lifecycle_key(collection, identity),
                serde_json::to_vec(&json!({"text":"durable source"}))?,
                None,
            )?;
        }
        product.migration_store_public_entry(
            crate::agent_control::POLICY_KEY.to_vec(),
            serde_json::to_vec(&policy)?,
            None,
        )?;
        policy.semantic = crate::agent_policy::Semantic {
            enabled: true,
            model_dir: Some(path.join("model")),
            model: Some(json!({"fingerprint":"a".repeat(64),"dimensions":2})),
            search_mode: crate::agent_policy::SemanticSearchMode::Hybrid,
        };
        Ok((path, policy))
    }

    fn count(product: &NativeProduct, id: u128) -> Result<usize, CliFailure> {
        let snapshot = product.snapshot_bounded(crate::native::logical_time_micros())?;
        Ok(
            NativeProduct::search_documents_at_snapshot(
                &snapshot,
                crate::object_id(id)?,
                None,
                10,
            )?
            .documents
            .len(),
        )
    }

    #[test]
    fn interrupted_copy_retains_sources_and_retries_without_duplicates()
    -> Result<(), Box<dyn std::error::Error>> {
        let (path, mut next) = fixture("copy")?;
        assert!(
            migrate_semantic_at(&path, &mut next, |stage| {
                if stage == "copy" {
                    Err(CliFailure::io())
                } else {
                    Ok(())
                }
            })
            .is_err()
        );
        {
            let mut product = NativeProduct::open(&path)?;
            assert_eq!(
                recover_at(&mut product)?
                    .ok_or("policy missing")?
                    .collections,
                [21, 22, 23]
            );
            for id in [21, 22, 23] {
                assert_eq!(count(&product, id)?, 1);
            }
        }
        migrate_semantic_at(&path, &mut next, |_| Ok(()))?;
        {
            let product = NativeProduct::open(&path)?;
            for id in [21, 22, 23] {
                assert_eq!(count(&product, id)?, 0);
            }
            for id in next.collections {
                assert_eq!(count(&product, id)?, 1);
            }
        }
        std::fs::remove_dir_all(path)?;
        Ok(())
    }

    #[test]
    fn enrichment_rejects_changed_expired_and_forgotten_sources()
    -> Result<(), Box<dyn std::error::Error>> {
        use hyphae_native_product::{
            ProductMemoryEnrichRequest, ProductSearchDocumentDelete, ProductSearchDocumentUpdate,
            ProductVector,
        };
        let (path, mut policy) = fixture("enrichment")?;
        migrate_semantic_at(&path, &mut policy, |_| Ok(()))?;
        {
            let mut product = NativeProduct::open(&path)?;
            let collection = crate::object_id(policy.collections[1])?;
            let identity = crate::object_id(501)?;
            let key = memory_lifecycle_key(collection, identity);
            let envelope = serde_json::to_vec(&json!({"text":"durable source"}))?;
            let now = crate::native::logical_time_micros();
            let mut request = ProductMemoryEnrichRequest {
                collection,
                expected_envelope_digest: *blake3::hash(&envelope).as_bytes(),
                update: ProductSearchDocumentUpdate {
                    idempotency_id: 701,
                    document: ProductDocument {
                        object_id: identity,
                        text: "durable source".into(),
                        doc_values: std::collections::BTreeMap::new(),
                        vectors: std::collections::BTreeMap::from([(
                            "memory".into(),
                            ProductVector::new([1.0, 0.0])?,
                        )]),
                    },
                },
            };
            request.update.document.text = "changed by inference".into();
            assert!(product.memory_enrich(&request, now).is_err());
            request.update.document.text = "durable source".into();
            product.memory_enrich(&request, now)?;
            product.migration_store_public_entry(
                key.clone(),
                b"changed envelope".to_vec(),
                None,
            )?;
            assert!(product.memory_enrich(&request, now).is_err());
            product.migration_store_public_entry(key.clone(), envelope.clone(), Some(now + 1))?;
            assert!(product.memory_enrich(&request, now + 1).is_err());
            product.migration_delete_public_entry(key.clone())?;
            assert!(product.memory_enrich(&request, now).is_err());
            assert!(product.snapshot_bounded(now)?.structure_get(&key).is_none());
            product.migration_store_public_entry(key, envelope, None)?;
            product.delete_search_document(
                collection,
                ProductSearchDocumentDelete {
                    idempotency_id: 702,
                    object_id: identity,
                },
                now,
                ProductDurability::Strict,
            )?;
            assert!(product.memory_enrich(&request, now).is_err());
            assert_eq!(count(&product, collection.get())?, 0);
        }
        {
            let product = NativeProduct::open(&path)?;
            assert_eq!(count(&product, policy.collections[1])?, 0);
        }
        let doctor = hyphae_native_product::doctor(&hyphae_native_product::DoctorRequest::new(
            &path,
            crate::native::logical_time_micros(),
        )?);
        assert!(doctor.is_healthy(), "post-delete doctor report: {doctor:?}");
        std::fs::remove_dir_all(path)?;
        Ok(())
    }

    #[test]
    fn interrupted_cutover_recovers_native_profile_and_finishes_retirement()
    -> Result<(), Box<dyn std::error::Error>> {
        let (path, mut next) = fixture("cutover")?;
        assert!(
            migrate_semantic_at(&path, &mut next, |stage| {
                if stage == "cutover" {
                    Err(CliFailure::io())
                } else {
                    Ok(())
                }
            })
            .is_err()
        );
        {
            let mut product = NativeProduct::open(&path)?;
            assert_ne!(next.collections, [21, 22, 23]);
            assert_eq!(
                Policy::from_product(&product)?
                    .ok_or("policy missing")?
                    .collections,
                next.collections
            );
            for id in [21, 22, 23] {
                assert_eq!(count(&product, id)?, 1);
            }
            for _ in 0..2 {
                assert_eq!(
                    recover_at(&mut product)?
                        .ok_or("policy missing")?
                        .collections,
                    next.collections
                );
                for id in [21, 22, 23] {
                    assert_eq!(count(&product, id)?, 0);
                }
                for id in next.collections {
                    assert_eq!(count(&product, id)?, 1);
                }
            }
        }
        std::fs::remove_dir_all(path)?;
        Ok(())
    }
}
