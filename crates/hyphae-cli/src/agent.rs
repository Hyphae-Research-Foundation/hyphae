// SPDX-License-Identifier: Apache-2.0

//! Agent Memory lifecycle: one command turns the engine into the product.
//!
//! `hyphae agent setup` creates everything the five-tool memory surface
//! needs — the data directory, the memory-schema collection, the operator
//! and agent credentials in restricted files — runs a store/recall/forget
//! smoke test through the real daemon, and prints exact operating,
//! backup, and removal instructions. Every resource lands under the
//! user's XDG paths, removal never deletes data, and only the explicit
//! purge command destroys the directory after interactive confirmation.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use hyphae_client::v2::{HyphaeClient, RequestOptions};
use hyphae_native_product::{
    BackupRequest, DoctorRequest, NativeProduct, ObjectId, ProductDocValue, ProductDocument,
    ProductDurability, ProductErrorCode, ProductResponse, ProductSearchDocumentDelete,
    ProductSearchIngestBatch, ProductSnapshot, ProductTtl,
};
use serde::{Deserialize, Serialize};

use crate::exit::CliFailure;
use crate::native_client::{
    OfflineOwnerClient, ensure_key_output_outside_data_dir, reserve_restricted_api_key_file,
};

/// Legacy mixed Agent Memory collection retained for migration compatibility.
pub(crate) const MEMORY_COLLECTION: u128 = 13;
pub(crate) const PERSONAL_MEMORY_COLLECTION: u128 = 21;
pub(crate) const WORK_MEMORY_COLLECTION: u128 = 22;
pub(crate) const JOURNAL_MEMORY_COLLECTION: u128 = 23;
const MEMORY_DATABASE: u128 = 10;
const MEMORY_SCHEMA: u128 = 11;
const MEMORY_ANALYZER: u128 = 12;
const SERVICE_NAME: &str = "hyphae-agent-memory";
const DOMAIN_MIGRATION_PLAN_KEY: &[u8] = b"hyphae-agent-memory/migration/13-to-domains/v1/plan";
const DOMAIN_MIGRATION_COPY_KEY: &[u8] =
    b"hyphae-agent-memory/migration/13-to-domains/v1/copy-complete";
const DOMAIN_MIGRATION_DONE_KEY: &[u8] = b"hyphae-agent-memory/migration/13-to-domains/v1/done";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct DomainMigrationPlan {
    schema: String,
    directory_lineage: String,
    records: Vec<DomainMigrationRecord>,
    records_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct DomainMigrationRecord {
    object_id: String,
    destination: String,
    document_digest: String,
    payload_digest: String,
}

struct DomainMigrationSource {
    document: ProductDocument,
    envelope: Vec<u8>,
    expires_at_micros: Option<i64>,
    destination: u128,
}

/// Resolved user paths for every Agent Memory resource.
pub(crate) struct AgentPaths {
    pub data: PathBuf,
    pub config: PathBuf,
    pub credentials: PathBuf,
    pub backups: PathBuf,
}

impl AgentPaths {
    pub(crate) fn resolve() -> Result<Self, CliFailure> {
        let home = std::env::var_os("HOME").ok_or_else(CliFailure::invalid)?;
        let home = PathBuf::from(home);
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map_or_else(|| home.join(".local/share"), PathBuf::from);
        let config_home =
            std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from);
        Ok(Self {
            data: data_home.join("hyphae/agent-memory"),
            config: config_home.join("hyphae"),
            credentials: config_home.join("hyphae/credentials"),
            backups: data_home.join("hyphae/backups"),
        })
    }

    fn operator_key(&self) -> PathBuf {
        self.credentials.join("operator.key")
    }

    pub(crate) fn reader_key(&self) -> PathBuf {
        self.credentials.join("memory-reader.key")
    }

    pub(crate) fn writer_key(&self) -> PathBuf {
        self.credentials.join("memory-writer.key")
    }
}

fn run_self(arguments: &[&str]) -> Result<(), CliFailure> {
    run_self_json(arguments).map(|_| ())
}

fn run_self_json(arguments: &[&str]) -> Result<serde_json::Value, CliFailure> {
    let output = std::process::Command::new(std::env::current_exe().map_err(|_| CliFailure::io())?)
        .args(arguments)
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|_| CliFailure::io())?;
    if !output.status.success() {
        // A failing step prints its typed error on stdout; surface it.
        let _ignored = std::io::stderr().write_all(&output.stdout);
        return Err(CliFailure::internal());
    }
    serde_json::from_slice(&output.stdout).map_err(|_| CliFailure::internal())
}

fn restrict(path: &Path) -> Result<(), CliFailure> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| CliFailure::io())?;
    }
    Ok(())
}

/// Creates every Agent Memory resource, explains each one first, smoke
/// tests the five operations, and prints operating instructions.
const READER_PERMISSIONS: &[&str] = &[
    "catalog.read",
    "data.read",
    "discover",
    "proof.generate",
    "proof.verify",
    "search.execute",
];
const WRITER_PERMISSIONS: &[&str] = &[
    "catalog.read",
    "data.read",
    "data.write",
    "discover",
    "proof.generate",
    "proof.verify",
    "search.execute",
];

#[allow(clippy::too_many_lines)]
pub(crate) fn setup(enable_service: bool, no_service: bool) -> Result<(), CliFailure> {
    let paths = AgentPaths::resolve()?;
    println!("Hyphae Agent Memory setup will create:");
    println!("  data directory     {}", paths.data.display());
    println!("  configuration      {}", paths.config.display());
    println!(
        "  credentials        {} (mode 0600)",
        paths.credentials.display()
    );
    println!("  backups            {}", paths.backups.display());
    println!();

    for directory in [&paths.config, &paths.credentials, &paths.backups] {
        std::fs::create_dir_all(directory).map_err(|_| CliFailure::io())?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&paths.credentials, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| CliFailure::io())?;
    }
    let data_text = paths.data.display().to_string();

    let initialized = paths.data.join("FORMAT").exists();
    if initialized {
        println!("data directory already initialized; reconciling Agent Memory domains");
    } else {
        std::fs::create_dir_all(paths.data.parent().ok_or_else(CliFailure::invalid)?)
            .map_err(|_| CliFailure::io())?;
        run_self(&["init", "--data-dir", &data_text])?;
        run_self(&[
            "catalog",
            "--data-dir",
            &data_text,
            "create-search-collection",
            "--database",
            &MEMORY_DATABASE.to_string(),
            "--schema",
            &MEMORY_SCHEMA.to_string(),
            "--collection",
            &MEMORY_COLLECTION.to_string(),
            "--analyzer",
            &MEMORY_ANALYZER.to_string(),
            "--name",
            "main.public.agent_memory",
            "--memory-schema",
        ])?;
        println!("created the legacy agent-memory collection for migration compatibility");
    }
    for (collection, name) in [
        (
            PERSONAL_MEMORY_COLLECTION,
            "main.public.agent_memory_personal",
        ),
        (WORK_MEMORY_COLLECTION, "main.public.agent_memory_work"),
        (
            JOURNAL_MEMORY_COLLECTION,
            "main.public.agent_memory_journal",
        ),
    ] {
        ensure_memory_collection_definition(&paths.data, &data_text, collection, name)?;
    }
    for collection in [
        MEMORY_COLLECTION,
        PERSONAL_MEMORY_COLLECTION,
        WORK_MEMORY_COLLECTION,
        JOURNAL_MEMORY_COLLECTION,
    ] {
        ensure_memory_collection_provisioned(&paths.data, &data_text, collection)?;
    }
    println!("personal, work, and model-journal collections are physically separated");

    // Credentials: the operator key from bootstrap, plus one reader and
    // one writer key for agent hosts. The directory holds nothing but the
    // Agent Memory collection, so the built-in roles are collection-bound
    // in effect.
    let operator_key = paths.operator_key();
    if operator_key.exists() {
        println!("operator credential already present");
    } else if security_bootstrapped(&paths.data)? {
        recover_operator_key(&paths.data, &operator_key)?;
        println!("recovered the operator credential for the preserved data directory");
    } else {
        run_self(&[
            "security",
            "--data-dir",
            &data_text,
            "bootstrap",
            "--name",
            "Agent Memory Operator",
            "--key-out",
            &operator_key.display().to_string(),
        ])?;
        restrict(&operator_key)?;
        println!("created the operator credential (never hand this to an agent)");
    }
    let operator_text = operator_key.display().to_string();
    for (key_path, role, principal_name, label, token_base, permissions) in [
        (
            paths.reader_key(),
            "reader",
            "Agent Memory Reader",
            "memory-reader",
            0x4147_u64,
            READER_PERMISSIONS,
        ),
        (
            paths.writer_key(),
            "writer",
            "Agent Memory Writer",
            "memory-writer",
            0x4157_u64,
            WRITER_PERMISSIONS,
        ),
    ] {
        if key_path.exists() {
            println!("{label} credential already present");
            continue;
        }
        let existing = agent_principal(&paths.data, &operator_key, principal_name)?;
        let token_base = existing
            .as_ref()
            .map_or(token_base, |_| new_idempotency_base());
        let principal_id = if let Some(principal_id) = existing {
            principal_id
        } else {
            let created = run_self_json(&[
                "security",
                "--data-dir",
                &data_text,
                "--native-api-key-file",
                &operator_text,
                "principal",
                "create",
                "--name",
                principal_name,
                "--idempotency-token",
                &token_base.to_string(),
            ])?;
            let principal_id = created
                .get("result_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(CliFailure::internal)?
                .to_owned();
            run_self(&[
                "security",
                "--data-dir",
                &data_text,
                "--native-api-key-file",
                &operator_text,
                "principal",
                "set-enabled",
                "--principal-id",
                &principal_id,
                "--enabled",
                "true",
                "--idempotency-token",
                &(token_base + 3).to_string(),
            ])?;
            run_self(&[
                "security",
                "--data-dir",
                &data_text,
                "--native-api-key-file",
                &operator_text,
                "assignment",
                "create-built-in",
                "--principal-id",
                &principal_id,
                "--role",
                role,
                "--scope",
                "instance",
                "--idempotency-token",
                &(token_base + 1).to_string(),
            ])?;
            principal_id
        };
        let mut issue = vec![
            "security".to_owned(),
            "--data-dir".to_owned(),
            data_text.clone(),
            "--native-api-key-file".to_owned(),
            operator_text.clone(),
            "key".to_owned(),
            "issue".to_owned(),
            "--principal-id".to_owned(),
            principal_id.clone(),
            "--label".to_owned(),
            label.to_owned(),
            "--role".to_owned(),
            role.to_owned(),
        ];
        for permission in permissions {
            issue.push("--permission".to_owned());
            issue.push((*permission).to_owned());
        }
        issue.extend([
            "--scope".to_owned(),
            "instance".to_owned(),
            "--key-out".to_owned(),
            key_path.display().to_string(),
            "--idempotency-token".to_owned(),
            (token_base + 2).to_string(),
        ]);
        let issue: Vec<&str> = issue.iter().map(String::as_str).collect();
        run_self(&issue)?;
        restrict(&key_path)?;
        println!("created the {label} credential");
    }

    if no_service {
        println!("service installation skipped (--no-service)");
    } else {
        let unit_path = install_service_unit(&paths)?;
        println!("installed service {}", unit_path.display());
        let start = enable_service || {
            print!("enable and start the service now? [y/N]: ");
            std::io::stdout().flush().map_err(|_| CliFailure::io())?;
            let mut answer = String::new();
            std::io::stdin()
                .read_line(&mut answer)
                .map_err(|_| CliFailure::io())?;
            matches!(answer.trim(), "y" | "Y" | "yes")
        };
        if start {
            systemctl(&["enable", "--now", SERVICE_NAME])?;
            println!("service enabled and started");
        } else {
            println!("service installed but not enabled; start it with:");
            println!("  systemctl --user enable --now {SERVICE_NAME}");
        }
    }

    println!();
    println!("Agent Memory is ready. Operate it with:");
    println!("  hyphae serve --data-dir {data_text} --native-api-key-auth");
    println!("  hyphae mcp --profile memory \\");
    println!(
        "      --endpoint {}",
        crate::native::default_endpoint(&paths.data)
    );
    println!(
        "      (HYPHAE_NATIVE_API_KEY_FILE={})",
        paths.reader_key().display()
    );
    println!("  hyphae agent status");
    println!("  hyphae agent backup");
    println!();
    println!("Removal never deletes data: `hyphae agent remove` keeps");
    println!("{data_text} and {} intact;", paths.backups.display());
    println!("only `hyphae agent purge-data` deletes, after confirmation.");
    Ok(())
}

fn ensure_memory_collection_definition(
    data: &Path,
    data_text: &str,
    collection: u128,
    name: &str,
) -> Result<(), CliFailure> {
    let product = hyphae_native_product::NativeProduct::open(data)?;
    let snapshot = product.catalog_snapshot()?;
    let existing = product.catalog_describe(
        &snapshot,
        hyphae_native_product::ObjectId::new(collection).map_err(|_| CliFailure::invalid())?,
    )?;
    drop(product);
    if existing.is_none() {
        run_self(&[
            "catalog",
            "--data-dir",
            data_text,
            "create-search-collection",
            "--database",
            &MEMORY_DATABASE.to_string(),
            "--schema",
            &MEMORY_SCHEMA.to_string(),
            "--collection",
            &collection.to_string(),
            "--analyzer",
            &MEMORY_ANALYZER.to_string(),
            "--name",
            name,
            "--memory-schema",
            "--reuse-schema",
        ])?;
    }
    Ok(())
}

fn ensure_memory_collection_provisioned(
    data: &Path,
    data_text: &str,
    collection: u128,
) -> Result<(), CliFailure> {
    let product = hyphae_native_product::NativeProduct::open(data)?;
    let bound = product
        .resolve_search_collection_binding(
            hyphae_native_product::ObjectId::new(collection).map_err(|_| CliFailure::invalid())?,
            crate::native::logical_time_micros(),
        )
        .is_ok();
    drop(product);
    if !bound {
        provision_collection(data_text, collection)?;
    }
    Ok(())
}

fn provision_collection(data_text: &str, collection: u128) -> Result<(), CliFailure> {
    run_self(&[
        "search",
        "--data-dir",
        data_text,
        "provision",
        "--collection",
        &collection.to_string(),
    ])
}

fn security_bootstrapped(data: &Path) -> Result<bool, CliFailure> {
    let product = hyphae_native_product::NativeProduct::open(data)?;
    product.access_control_bootstrapped().map_err(Into::into)
}

/// Offline, retry-safe migration from the legacy mixed collection into the
/// personal, work, and journal collections.
#[allow(clippy::too_many_lines)]
pub(crate) fn migrate_domains() -> Result<(), CliFailure> {
    let paths = AgentPaths::resolve()?;
    if service_active() || local_endpoint_present(&paths) {
        eprintln!("stop the service first: systemctl --user stop {SERVICE_NAME}");
        return Err(CliFailure::invalid());
    }
    if !paths.data.join("FORMAT").exists() {
        return Err(CliFailure::invalid());
    }

    let logical_time = crate::native::logical_time_micros();
    let mut product = NativeProduct::open(&paths.data)?;
    for collection in [
        MEMORY_COLLECTION,
        PERSONAL_MEMORY_COLLECTION,
        WORK_MEMORY_COLLECTION,
        JOURNAL_MEMORY_COLLECTION,
    ] {
        product.resolve_search_collection_binding(object_id(collection)?, logical_time)?;
    }
    let snapshot = product.snapshot_bounded(logical_time)?;
    if snapshot.structure_get(DOMAIN_MIGRATION_DONE_KEY).is_some() {
        println!("legacy Agent Memory domain migration is already complete");
        return Ok(());
    }

    let existing_plan = snapshot.structure_get(DOMAIN_MIGRATION_PLAN_KEY);
    let (plan, plan_bytes) = if let Some(existing) = existing_plan {
        let plan: DomainMigrationPlan =
            serde_json::from_slice(existing).map_err(|_| CliFailure::invalid())?;
        if plan.schema != "hyphae-agent-memory-domain-migration-v1"
            || plan.directory_lineage != crate::encode_hex(&snapshot.identity().directory_lineage)
        {
            return Err(CliFailure::invalid());
        }
        (plan, existing.to_vec())
    } else {
        let sources = migration_sources(&snapshot)?;
        let plan = migration_plan(&snapshot, &sources);
        let plan_bytes = serde_json::to_vec(&plan).map_err(|_| CliFailure::internal())?;
        product.migration_store_public_entry(
            DOMAIN_MIGRATION_PLAN_KEY.to_vec(),
            plan_bytes.clone(),
            None,
        )?;
        (plan, plan_bytes)
    };
    let plan_digest = blake3::hash(&plan_bytes).as_bytes().to_vec();
    if plan.records_digest != migration_records_digest(&plan.records) {
        return Err(CliFailure::invalid());
    }
    let copy_complete = match snapshot.structure_get(DOMAIN_MIGRATION_COPY_KEY) {
        Some(existing) if existing == plan_digest => true,
        Some(_) => return Err(CliFailure::invalid()),
        None => false,
    };

    if !copy_complete {
        let sources = migration_sources(&snapshot)?;
        if migration_records(&sources) != plan.records {
            eprintln!("legacy Agent Memory changed after its migration plan was sealed");
            return Err(CliFailure::invalid());
        }
        for source in &sources {
            let destination = object_id(source.destination)?;
            let batch = ProductSearchIngestBatch {
                idempotency_id: source.document.object_id.get(),
                documents: vec![source.document.clone()],
            };
            match product.ingest_search_batch(
                destination,
                &batch,
                logical_time,
                ProductDurability::Strict,
            ) {
                Ok(_) => {}
                Err(error) if error.code() == ProductErrorCode::CatalogConflict => {
                    verify_destination_document(
                        &product.snapshot_bounded(logical_time)?,
                        destination,
                        &source.document,
                    )?;
                }
                Err(error) => return Err(error.into()),
            }
            let destination_key =
                crate::mcp::memory_key(source.destination, source.document.object_id.get());
            let current = product.snapshot_bounded(logical_time)?;
            match current.structure_get(&destination_key) {
                Some(existing) if existing == source.envelope => {}
                Some(_) => {
                    eprintln!(
                        "destination lifecycle conflict for memory {}",
                        source.document.object_id.get()
                    );
                    return Err(CliFailure::invalid());
                }
                None => {
                    product.migration_store_public_entry(
                        destination_key,
                        source.envelope.clone(),
                        source.expires_at_micros,
                    )?;
                }
            }
        }

        let copied = product.snapshot_bounded(logical_time)?;
        for source in &sources {
            verify_destination(&copied, source)?;
        }
        if copied.structure_get(DOMAIN_MIGRATION_COPY_KEY).is_none() {
            product.migration_store_public_entry(
                DOMAIN_MIGRATION_COPY_KEY.to_vec(),
                plan_digest.clone(),
                None,
            )?;
        }
    }

    let source_documents =
        migration_documents(&product.snapshot_bounded(logical_time)?, MEMORY_COLLECTION)?
            .into_iter()
            .map(|document| (document.object_id.get(), document))
            .collect::<BTreeMap<_, _>>();
    for record in &plan.records {
        let identity = record_identity(record)?;
        let destination = record_destination(record)?;
        verify_planned_destination(
            &product.snapshot_bounded(logical_time)?,
            record,
            destination,
            identity,
        )?;
        if let Some(document) = source_documents.get(&identity)
            && crate::encode_hex(&migration_document_digest(document)) != record.document_digest
        {
            eprintln!("legacy Agent Memory changed after its migration copy barrier");
            return Err(CliFailure::invalid());
        }
        let source_key = crate::mcp::memory_key(MEMORY_COLLECTION, identity);
        if product
            .snapshot_bounded(logical_time)?
            .structure_get(&source_key)
            .is_some()
        {
            product.migration_delete_public_entry(source_key)?;
        }
        if source_documents.contains_key(&identity) {
            product.delete_search_document(
                object_id(MEMORY_COLLECTION)?,
                ProductSearchDocumentDelete {
                    idempotency_id: migration_delete_identity(identity),
                    object_id: object_id(identity)?,
                },
                logical_time,
                ProductDurability::Strict,
            )?;
        }
    }

    let final_snapshot = product.snapshot_bounded(logical_time)?;
    let remaining = NativeProduct::search_documents_at_snapshot(
        &final_snapshot,
        object_id(MEMORY_COLLECTION)?,
        None,
        1,
    )?;
    if !remaining.documents.is_empty() {
        return Err(CliFailure::internal());
    }
    product.migration_store_public_entry(DOMAIN_MIGRATION_DONE_KEY.to_vec(), plan_digest, None)?;
    println!(
        "migrated {} legacy memories into physically separated domains",
        plan.records.len()
    );
    Ok(())
}

fn migration_sources(snapshot: &ProductSnapshot) -> Result<Vec<DomainMigrationSource>, CliFailure> {
    let documents = migration_documents(snapshot, MEMORY_COLLECTION)?;
    let document_ids = documents
        .iter()
        .map(|document| document.object_id.get())
        .collect::<BTreeSet<_>>();
    let mut lifecycle_ids = BTreeSet::new();
    let prefix = memory_prefix(MEMORY_COLLECTION);
    let end = prefix_end(&prefix).ok_or_else(CliFailure::invalid)?;
    let lifecycle_keys = snapshot
        .structure_keys_in_range(&prefix, &end, 100_001)
        .ok_or_else(CliFailure::invalid)?;
    for key in lifecycle_keys {
        let identity = memory_key_identity(&key, MEMORY_COLLECTION)?;
        lifecycle_ids.insert(identity);
    }
    if document_ids != lifecycle_ids {
        eprintln!("legacy Agent Memory has unpaired search documents or lifecycle records");
        return Err(CliFailure::invalid());
    }

    documents
        .into_iter()
        .map(|document| {
            let key = crate::mcp::memory_key(MEMORY_COLLECTION, document.object_id.get());
            let envelope = snapshot
                .structure_get(&key)
                .ok_or_else(CliFailure::invalid)?
                .to_vec();
            let parsed: serde_json::Value =
                serde_json::from_slice(&envelope).map_err(|_| CliFailure::invalid())?;
            validate_migration_envelope(&document, &parsed)?;
            let layer = parsed
                .get("layer")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(CliFailure::invalid)?;
            let destination = match layer {
                "personal" => PERSONAL_MEMORY_COLLECTION,
                "work" => WORK_MEMORY_COLLECTION,
                "journal" => JOURNAL_MEMORY_COLLECTION,
                _ => return Err(CliFailure::invalid()),
            };
            let expires_at_micros = match snapshot.structure_ttl(&key) {
                ProductTtl::Missing => return Err(CliFailure::invalid()),
                ProductTtl::Persistent => None,
                ProductTtl::RemainingMicros(remaining) => Some(
                    snapshot
                        .identity()
                        .logical_time_micros
                        .saturating_add(remaining),
                ),
            };
            if parsed
                .get("expires_at_micros")
                .and_then(serde_json::Value::as_i64)
                != expires_at_micros
            {
                return Err(CliFailure::invalid());
            }
            Ok(DomainMigrationSource {
                document,
                envelope,
                expires_at_micros,
                destination,
            })
        })
        .collect()
}

fn migration_documents(
    snapshot: &ProductSnapshot,
    collection: u128,
) -> Result<Vec<ProductDocument>, CliFailure> {
    let mut documents = Vec::new();
    let mut continuation = None;
    loop {
        let page = NativeProduct::search_documents_at_snapshot(
            snapshot,
            object_id(collection)?,
            continuation,
            1_024,
        )?;
        documents.extend(page.documents);
        let Some(next) = page.continuation else {
            break;
        };
        continuation = Some(next);
    }
    Ok(documents)
}

fn validate_migration_envelope(
    document: &ProductDocument,
    envelope: &serde_json::Value,
) -> Result<(), CliFailure> {
    let project = envelope
        .get("project")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(CliFailure::invalid)?;
    let scope = envelope
        .get("scope")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(CliFailure::invalid)?;
    let kind = envelope
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(CliFailure::invalid)?;
    let layer = envelope
        .get("layer")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(CliFailure::invalid)?;
    let harness = envelope
        .get("harness")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(CliFailure::invalid)?;
    let model = envelope
        .get("model")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(CliFailure::invalid)?;
    let text = envelope
        .get("text")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(CliFailure::invalid)?;
    let envelope_expiry = envelope
        .get("expires_at_micros")
        .ok_or_else(CliFailure::invalid)?;
    if text != document.text
        || !matches!(scope, "project" | "global")
        || !matches!(layer, "personal" | "work" | "journal")
        || project.is_empty()
        || project == "_global"
        || harness.is_empty()
        || model.is_empty()
        || (!envelope_expiry.is_null() && envelope_expiry.as_i64().is_none())
        || !document.vectors.is_empty()
    {
        return Err(CliFailure::invalid());
    }
    let effective_project = if scope == "global" {
        "_global"
    } else {
        project
    };
    let expected = BTreeMap::from([
        (
            "project".to_owned(),
            ProductDocValue::String(effective_project.to_owned()),
        ),
        ("kind".to_owned(), ProductDocValue::String(kind.to_owned())),
        (
            "layer".to_owned(),
            ProductDocValue::String(layer.to_owned()),
        ),
        (
            "harness".to_owned(),
            ProductDocValue::String(harness.to_owned()),
        ),
        (
            "model".to_owned(),
            ProductDocValue::String(model.to_owned()),
        ),
    ]);
    if document.doc_values != expected
        || document.object_id.get() != crate::mcp::envelope_identity(effective_project, layer, text)
    {
        return Err(CliFailure::invalid());
    }
    Ok(())
}

fn migration_plan(
    snapshot: &ProductSnapshot,
    sources: &[DomainMigrationSource],
) -> DomainMigrationPlan {
    let records = migration_records(sources);
    DomainMigrationPlan {
        schema: "hyphae-agent-memory-domain-migration-v1".to_owned(),
        directory_lineage: crate::encode_hex(&snapshot.identity().directory_lineage),
        records_digest: migration_records_digest(&records),
        records,
    }
}

fn migration_records_digest(records: &[DomainMigrationRecord]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-agent-memory-domain-plan-v1\0");
    for record in records {
        hasher.update(record.object_id.as_bytes());
        hasher.update(&[0]);
        hasher.update(record.destination.as_bytes());
        hasher.update(&[0]);
        hasher.update(record.document_digest.as_bytes());
        hasher.update(&[0]);
        hasher.update(record.payload_digest.as_bytes());
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

fn migration_records(sources: &[DomainMigrationSource]) -> Vec<DomainMigrationRecord> {
    sources
        .iter()
        .map(|source| DomainMigrationRecord {
            object_id: source.document.object_id.get().to_string(),
            destination: source.destination.to_string(),
            document_digest: crate::encode_hex(&migration_document_digest(&source.document)),
            payload_digest: crate::encode_hex(blake3::hash(&source.envelope).as_bytes()),
        })
        .collect()
}

fn verify_destination(
    snapshot: &ProductSnapshot,
    source: &DomainMigrationSource,
) -> Result<(), CliFailure> {
    verify_destination_document(snapshot, object_id(source.destination)?, &source.document)?;
    let key = crate::mcp::memory_key(source.destination, source.document.object_id.get());
    if snapshot.structure_get(&key) != Some(source.envelope.as_slice()) {
        return Err(CliFailure::invalid());
    }
    Ok(())
}

fn verify_planned_destination(
    snapshot: &ProductSnapshot,
    record: &DomainMigrationRecord,
    destination: u128,
    identity: u128,
) -> Result<(), CliFailure> {
    let document = migration_documents(snapshot, destination)?
        .into_iter()
        .find(|document| document.object_id.get() == identity)
        .ok_or_else(CliFailure::invalid)?;
    let key = crate::mcp::memory_key(destination, identity);
    let envelope = snapshot
        .structure_get(&key)
        .ok_or_else(CliFailure::invalid)?;
    if crate::encode_hex(&migration_document_digest(&document)) != record.document_digest
        || crate::encode_hex(blake3::hash(envelope).as_bytes()) != record.payload_digest
    {
        return Err(CliFailure::invalid());
    }
    Ok(())
}

fn record_identity(record: &DomainMigrationRecord) -> Result<u128, CliFailure> {
    record
        .object_id
        .parse::<u128>()
        .ok()
        .filter(|identity| *identity != 0)
        .ok_or_else(CliFailure::invalid)
}

fn record_destination(record: &DomainMigrationRecord) -> Result<u128, CliFailure> {
    match record.destination.as_str() {
        "21" => Ok(PERSONAL_MEMORY_COLLECTION),
        "22" => Ok(WORK_MEMORY_COLLECTION),
        "23" => Ok(JOURNAL_MEMORY_COLLECTION),
        _ => Err(CliFailure::invalid()),
    }
}

fn verify_destination_document(
    snapshot: &ProductSnapshot,
    collection: ObjectId,
    expected: &ProductDocument,
) -> Result<(), CliFailure> {
    let start_after = expected
        .object_id
        .get()
        .checked_sub(1)
        .and_then(|value| (value != 0).then(|| ObjectId::new(value).ok()).flatten());
    let page = NativeProduct::search_documents_at_snapshot(snapshot, collection, start_after, 1)?;
    if page.documents.first() != Some(expected) {
        return Err(CliFailure::invalid());
    }
    Ok(())
}

fn migration_document_digest(document: &ProductDocument) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-agent-memory-domain-document-v1\0");
    hasher.update(&document.object_id.get().to_le_bytes());
    hasher.update(&(document.text.len() as u64).to_le_bytes());
    hasher.update(document.text.as_bytes());
    for (name, value) in &document.doc_values {
        hasher.update(&(name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
        hasher.update(format!("{value:?}").as_bytes());
    }
    *hasher.finalize().as_bytes()
}

fn migration_delete_identity(identity: u128) -> u128 {
    let digest = blake3::Hasher::new()
        .update(b"hyphae-agent-memory-domain-delete-v1\0")
        .update(&identity.to_le_bytes())
        .finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    u128::from_le_bytes(bytes).max(1)
}

fn memory_prefix(collection: u128) -> Vec<u8> {
    let mut prefix = b"hyphae-memory/".to_vec();
    prefix.extend_from_slice(&collection.to_le_bytes());
    prefix
}

fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    for byte in end.iter_mut().rev() {
        if *byte != u8::MAX {
            *byte = byte.saturating_add(1);
            return Some(end);
        }
        *byte = 0;
    }
    None
}

fn memory_key_identity(key: &[u8], collection: u128) -> Result<u128, CliFailure> {
    let prefix = memory_prefix(collection);
    let identity = key
        .strip_prefix(prefix.as_slice())
        .filter(|suffix| suffix.len() == 16)
        .ok_or_else(CliFailure::invalid)?;
    Ok(u128::from_le_bytes(
        identity.try_into().map_err(|_| CliFailure::invalid())?,
    ))
}

fn object_id(value: u128) -> Result<ObjectId, CliFailure> {
    ObjectId::new(value).map_err(|_| CliFailure::invalid())
}

fn recover_operator_key(data: &Path, key: &Path) -> Result<(), CliFailure> {
    ensure_key_output_outside_data_dir(data, key)?;
    let mut output = reserve_restricted_api_key_file(key)?;
    let mut client = OfflineOwnerClient::open(data)?;
    if client.inspect()?.pending.is_some() {
        eprintln!(
            "pending owner recovery requires explicit `hyphae security owner resume` or `abort-pending`"
        );
        return Err(CliFailure::invalid());
    }
    let receipt = client.start("agent-memory-operator-recovery")?;
    output.write_secret(receipt.secret.expose_secret_bytes())?;
    client.resume(receipt.key_id, key, receipt.authorization_epoch)?;
    Ok(())
}

fn agent_principal(
    data: &Path,
    operator_key: &Path,
    name: &str,
) -> Result<Option<String>, CliFailure> {
    let value = run_self_json(&[
        "security",
        "--data-dir",
        &data.display().to_string(),
        "--native-api-key-file",
        &operator_key.display().to_string(),
        "principal",
        "list",
        "--limit",
        "1000",
    ])?;
    Ok(value
        .get("items")
        .and_then(serde_json::Value::as_array)
        .and_then(|items| {
            items.iter().find_map(|item| {
                (item.get("display_name").and_then(serde_json::Value::as_str) == Some(name))
                    .then(|| {
                        item.get("id")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })
                    .flatten()
            })
        }))
}

fn new_idempotency_base() -> u64 {
    let value = crate::native::logical_time_micros().unsigned_abs();
    value.max(1)
}

/// Redacted local status: paths, initialization, and credential presence.
pub(crate) fn status() -> Result<(), CliFailure> {
    let paths = AgentPaths::resolve()?;
    let initialized = paths.data.join("FORMAT").exists();
    let value = serde_json::json!({
        "schema": "hyphae-agent-status-v1",
        "data_directory": paths.data.display().to_string(),
        "initialized": initialized,
        "credentials": {
            "operator": paths.operator_key().exists(),
            "memory_reader": paths.reader_key().exists(),
            "memory_writer": paths.writer_key().exists(),
        },
        "backups_directory": paths.backups.display().to_string(),
        "service": SERVICE_NAME,
        "service_installed": service_unit_path().is_ok_and(|path| path.exists()),
        "service_active": service_active(),
    });
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

/// Engine doctor over the Agent Memory directory with the operator credential.
pub(crate) async fn doctor() -> Result<(), CliFailure> {
    let paths = AgentPaths::resolve()?;
    if service_active() || local_endpoint_present(&paths) {
        let client = agent_client(&paths)?;
        let request = DoctorRequest::new(&paths.data, crate::native::logical_time_micros())
            .map_err(|_| CliFailure::invalid())?;
        let response = client
            .doctor(request, RequestOptions::default())
            .await
            .map_err(client_failure)?;
        let ProductResponse::Doctor(report) = response else {
            return Err(CliFailure::internal());
        };
        let value = serde_json::json!({
            "status": doctor_status(report.status),
            "verified_open": report.verified_open,
            "snapshot_verified": report.snapshot_verified,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    let output = run_self_json(&[
        "doctor",
        "--data-dir",
        &paths.data.display().to_string(),
        "--native-api-key-file",
        &paths.operator_key().display().to_string(),
    ])?;
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

/// One verified backup under the backups directory.
pub(crate) async fn backup() -> Result<(), CliFailure> {
    let paths = AgentPaths::resolve()?;
    std::fs::create_dir_all(&paths.backups).map_err(|_| CliFailure::io())?;
    let destination = paths.backups.join(format!(
        "agent-memory-{}",
        crate::native::logical_time_micros()
    ));
    if service_active() || local_endpoint_present(&paths) {
        let client = agent_client(&paths)?;
        let request = BackupRequest::new(&destination).map_err(|_| CliFailure::invalid())?;
        let response = client
            .backup(request, RequestOptions::default())
            .await
            .map_err(client_failure)?;
        let ProductResponse::Backup(info) = response else {
            return Err(CliFailure::internal());
        };
        println!("backup written: {}", info.path.display());
        println!(
            "checkpoint digest: {}",
            crate::encode_hex(&info.checkpoint_digest)
        );
        return Ok(());
    }
    let output = run_self_json(&[
        "backup",
        "create",
        "--data-dir",
        &paths.data.display().to_string(),
        "--native-api-key-file",
        &paths.operator_key().display().to_string(),
        "--out",
        &destination.display().to_string(),
    ])?;
    let digest = output
        .get("checkpoint_digest")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(CliFailure::internal)?;
    println!("backup written: {}", destination.display());
    println!("checkpoint digest: {digest}");
    Ok(())
}

fn agent_client(paths: &AgentPaths) -> Result<HyphaeClient, CliFailure> {
    let key = crate::native_client::read_api_key_file(&paths.operator_key())?;
    HyphaeClient::local_authenticated(
        crate::native::default_endpoint(&paths.data),
        key.credential()?,
    )
    .map_err(client_failure)
}

fn local_endpoint_present(paths: &AgentPaths) -> bool {
    #[cfg(unix)]
    {
        Path::new(&crate::native::default_endpoint(&paths.data)).exists()
    }
    #[cfg(windows)]
    {
        false
    }
}

fn client_failure(error: hyphae_client::v2::ClientError) -> CliFailure {
    match error {
        hyphae_client::v2::ClientError::Product(error) => error.into(),
        hyphae_client::v2::ClientError::Http(_)
        | hyphae_client::v2::ClientError::Local(_)
        | hyphae_client::v2::ClientError::Protocol(_)
        | hyphae_client::v2::ClientError::UnexpectedResponse
        | hyphae_client::v2::ClientError::Cancelled => CliFailure::internal(),
    }
}

fn doctor_status(status: hyphae_native_product::DoctorStatus) -> &'static str {
    match status {
        hyphae_native_product::DoctorStatus::Healthy => "healthy",
        hyphae_native_product::DoctorStatus::Busy => "busy",
        hyphae_native_product::DoctorStatus::Corrupt => "corrupt",
        hyphae_native_product::DoctorStatus::Io => "io",
    }
}

/// Restores one verified backup: the service must be stopped, the current
/// directory is preserved aside, and the backup is verified before it
/// replaces anything.
pub(crate) fn restore(backup: &Path) -> Result<(), CliFailure> {
    let paths = AgentPaths::resolve()?;
    if service_active() {
        eprintln!("stop the service first: systemctl --user stop {SERVICE_NAME}");
        return Err(CliFailure::invalid());
    }
    run_self(&[
        "backup",
        "verify",
        "--backup",
        &backup.display().to_string(),
    ])?;
    let stamp = crate::native::logical_time_micros();
    let preserved = paths
        .data
        .with_file_name(format!("agent-memory.pre-restore-{stamp}"));
    if paths.data.exists() {
        std::fs::rename(&paths.data, &preserved).map_err(|_| CliFailure::io())?;
        println!("previous data preserved at {}", preserved.display());
    }
    run_self(&[
        "restore",
        "--backup",
        &backup.display().to_string(),
        "--data-dir",
        &paths.data.display().to_string(),
    ])?;
    println!(
        "restored {} from {}",
        paths.data.display(),
        backup.display()
    );
    Ok(())
}

/// The upgrade flow from the product contract: stop, backup, doctor,
/// start, and verify a recall answers.
pub(crate) async fn upgrade() -> Result<(), CliFailure> {
    println!("stopping the service");
    let _ignored = systemctl(&["stop", SERVICE_NAME]);
    backup().await?;
    doctor().await?;
    println!("starting the service");
    systemctl(&["start", SERVICE_NAME])?;
    println!("service restarted; verify a known memory with your agent host");
    Ok(())
}

fn systemctl(arguments: &[&str]) -> Result<(), CliFailure> {
    let status = std::process::Command::new("systemctl")
        .arg("--user")
        .args(arguments)
        .status()
        .map_err(|_| CliFailure::io())?;
    if status.success() {
        Ok(())
    } else {
        Err(CliFailure::internal())
    }
}

fn service_active() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", SERVICE_NAME])
        .status()
        .is_ok_and(|status| status.success())
}

fn service_unit_path() -> Result<PathBuf, CliFailure> {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or_else(CliFailure::invalid)?;
    Ok(config_home.join(format!("systemd/user/{SERVICE_NAME}.service")))
}

/// Writes the user service: loopback-only, explicit paths, bounded
/// resources, clean shutdown, and no secrets in arguments or logs.
fn install_service_unit(paths: &AgentPaths) -> Result<PathBuf, CliFailure> {
    let unit_path = service_unit_path()?;
    std::fs::create_dir_all(unit_path.parent().ok_or_else(CliFailure::invalid)?)
        .map_err(|_| CliFailure::io())?;
    let binary = std::env::current_exe().map_err(|_| CliFailure::io())?;
    let unit = format!(
        "[Unit]\n\
         Description=Hyphae Agent Memory (local, shared, verifiable memory for coding agents)\n\
         Documentation=https://github.com/Hyphae-Research-Foundation/hyphae\n\n\
         [Service]\n\
         Type=simple\n\
         ExecStart={binary} serve --data-dir {data} --endpoint %t/{service}.sock --native-api-key-auth --http-bind 127.0.0.1:8787\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         TimeoutStopSec=30\n\
         NoNewPrivileges=yes\n\
         PrivateTmp=yes\n\
         MemoryHigh=384M\n\
         MemoryMax=512M\n\
         TasksMax=64\n\
         LimitNOFILE=4096\n\n\
         [Install]\n\
         WantedBy=default.target\n",
        binary = binary.display(),
        data = paths.data.display(),
        service = SERVICE_NAME,
    );
    std::fs::write(&unit_path, unit).map_err(|_| CliFailure::io())?;
    let _ignored = systemctl(&["daemon-reload"]);
    Ok(unit_path)
}

/// Removes the service and generated credentials while preserving data
/// and backups.
pub(crate) fn remove() -> Result<(), CliFailure> {
    let paths = AgentPaths::resolve()?;
    let unit_path = service_unit_path()?;
    if unit_path.exists() {
        let _ignored = systemctl(&["disable", "--now", SERVICE_NAME]);
        std::fs::remove_file(&unit_path).map_err(|_| CliFailure::io())?;
        let _ignored = systemctl(&["daemon-reload"]);
        println!("removed service {}", unit_path.display());
    }
    for key in [paths.operator_key(), paths.reader_key(), paths.writer_key()] {
        if key.exists() {
            std::fs::remove_file(&key).map_err(|_| CliFailure::io())?;
            println!("removed credential {}", key.display());
        }
    }
    deconfigure_hosts();
    println!("preserved data      {}", paths.data.display());
    println!("preserved backups   {}", paths.backups.display());
    println!("reinstall any time with `hyphae agent setup`");
    Ok(())
}

/// Supported agent hosts for configuration generation.
#[derive(Clone, Copy)]
pub(crate) enum Host {
    Claude,
    Codex,
    Opencode,
    Pi,
}

/// Agent Memory authority granted to one configured host.
#[derive(Clone, Copy)]
pub(crate) enum Access {
    Read,
    Write,
}

/// Generates one host's MCP configuration for the memory profile. The
/// configuration carries only the credential file path — never a secret —
/// and the same binary, profile, and endpoint on every host.
#[allow(clippy::too_many_lines)]
pub(crate) fn configure(host: Host, access: Access, apply: bool) -> Result<(), CliFailure> {
    let paths = AgentPaths::resolve()?;
    let key = match access {
        Access::Read => paths.reader_key(),
        Access::Write => paths.writer_key(),
    };
    if !key.exists() {
        eprintln!("Agent Memory credential is missing: {}", key.display());
        eprintln!("run `hyphae agent setup` before configuring a host");
        return Err(CliFailure::invalid());
    }
    let binary = std::env::current_exe().map_err(|_| CliFailure::io())?;
    let endpoint = crate::native::default_endpoint(&paths.data);
    let mut arguments = vec![
        "mcp".to_owned(),
        "--profile".to_owned(),
        "memory".to_owned(),
        "--endpoint".to_owned(),
        endpoint,
    ];
    if matches!(access, Access::Write) {
        arguments.push("--allow-write".to_owned());
    }
    let key_text = key.display().to_string();
    let binary_text = binary.display().to_string();
    match host {
        Host::Claude => {
            let server = serde_json::json!({
                "command": binary_text,
                "args": arguments,
                "env": {"HYPHAE_NATIVE_API_KEY_FILE": key_text},
            });
            let encoded = serde_json::to_string(&server)?;
            if apply {
                let status = std::process::Command::new("claude")
                    .args([
                        "mcp",
                        "add-json",
                        "hyphae-memory",
                        &encoded,
                        "--scope",
                        "user",
                    ])
                    .status()
                    .map_err(|_| CliFailure::io())?;
                if !status.success() {
                    eprintln!("Claude Code already has a hyphae-memory entry or rejected it");
                    eprintln!(
                        "remove it with `claude mcp remove hyphae-memory --scope user` before replacing it"
                    );
                    return Err(CliFailure::invalid());
                }
                println!("Claude Code configured through `claude mcp add-json`");
                install_proactive_host(host, &binary)?;
            } else {
                println!("Claude Code registration (user scope):");
                println!("  claude mcp add-json hyphae-memory <JSON> --scope user");
                println!();
                println!("{encoded}");
            }
        }
        Host::Codex => {
            if apply {
                let mut command = std::process::Command::new("codex");
                command.args([
                    "mcp",
                    "add",
                    "hyphae-memory",
                    "--env",
                    &format!("HYPHAE_NATIVE_API_KEY_FILE={key_text}"),
                    "--",
                    &binary_text,
                ]);
                command.args(&arguments);
                if !command.status().map_err(|_| CliFailure::io())?.success() {
                    return Err(CliFailure::invalid());
                }
                println!("Codex configured through `codex mcp add`");
                install_proactive_host(host, &binary)?;
            } else {
                print_host_command("codex mcp add hyphae-memory", &binary, &arguments, &key);
            }
        }
        Host::Opencode => {
            if apply {
                let mut command = std::process::Command::new("opencode");
                command.args([
                    "mcp",
                    "add",
                    "hyphae-memory",
                    "--env",
                    &format!("HYPHAE_NATIVE_API_KEY_FILE={key_text}"),
                    "--",
                    &binary_text,
                ]);
                command.args(&arguments);
                if !command.status().map_err(|_| CliFailure::io())?.success() {
                    return Err(CliFailure::invalid());
                }
                println!("OpenCode configured through `opencode mcp add`");
                install_proactive_host(host, &binary)?;
            } else {
                print_host_command("opencode mcp add hyphae-memory", &binary, &arguments, &key);
            }
        }
        Host::Pi => return configure_pi(&paths, access, apply),
    }
    Ok(())
}

fn print_host_command(prefix: &str, binary: &Path, arguments: &[String], key: &Path) {
    println!(
        "{prefix} --env HYPHAE_NATIVE_API_KEY_FILE={} -- {} {}",
        key.display(),
        binary.display(),
        arguments.join(" ")
    );
}

fn configure_pi(paths: &AgentPaths, access: Access, apply: bool) -> Result<(), CliFailure> {
    let pi_home = std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".pi/agent")))
        .ok_or_else(CliFailure::invalid)?;
    let extension = pi_home.join("extensions/hyphae-memory.ts");
    let manifest = paths.config.join("pi-agent-memory.json");
    let key = match access {
        Access::Read => paths.reader_key(),
        Access::Write => paths.writer_key(),
    };
    let value = serde_json::json!({
        "schema": "hyphae-pi-agent-memory-v1",
        "binary": std::env::current_exe().map_err(|_| CliFailure::io())?.display().to_string(),
        "endpoint": crate::native::default_endpoint(&paths.data),
        "credential_file": key.display().to_string(),
        "allow_write": matches!(access, Access::Write),
    });
    if !apply {
        println!("Pi requires the Hyphae Agent Memory extension:");
        println!("  extension  {}", extension.display());
        println!("  manifest   {}", manifest.display());
        println!("run again with --apply to install both files");
        return Ok(());
    }
    std::fs::create_dir_all(extension.parent().ok_or_else(CliFailure::invalid)?)
        .map_err(|_| CliFailure::io())?;
    std::fs::write(&manifest, serde_json::to_string_pretty(&value)? + "\n")
        .map_err(|_| CliFailure::io())?;
    std::fs::write(
        &extension,
        include_str!("../assets/agent-hosts/pi-hyphae-memory.ts"),
    )
    .map_err(|_| CliFailure::io())?;
    println!(
        "Pi Agent Memory extension installed at {}",
        extension.display()
    );
    Ok(())
}

fn install_proactive_host(host: Host, binary: &Path) -> Result<(), CliFailure> {
    match host {
        Host::Claude => install_command_hooks(
            &user_home()?.join(".claude/settings.json"),
            "hooks",
            binary,
            "claude",
        ),
        Host::Codex => install_command_hooks(
            std::env::var_os("CODEX_HOME")
                .map_or_else(
                    || user_home().unwrap_or_default().join(".codex"),
                    PathBuf::from,
                )
                .join("hooks.json")
                .as_path(),
            "hooks",
            binary,
            "codex",
        ),
        Host::Opencode => {
            let config = std::env::var_os("XDG_CONFIG_HOME").map_or_else(
                || user_home().unwrap_or_default().join(".config"),
                PathBuf::from,
            );
            let plugin = config.join("opencode/plugins/hyphae-memory.ts");
            std::fs::create_dir_all(plugin.parent().ok_or_else(CliFailure::invalid)?)
                .map_err(|_| CliFailure::io())?;
            let source = include_str!("../assets/agent-hosts/opencode-hyphae-memory.ts")
                .replace("__HYPHAE_BINARY__", &binary.display().to_string());
            std::fs::write(&plugin, source).map_err(|_| CliFailure::io())?;
            println!(
                "OpenCode proactive memory plugin installed at {}",
                plugin.display()
            );
            Ok(())
        }
        Host::Pi => Ok(()),
    }
}

fn user_home() -> Result<PathBuf, CliFailure> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(CliFailure::invalid)
}

fn install_command_hooks(
    path: &Path,
    root_key: &str,
    binary: &Path,
    host: &str,
) -> Result<(), CliFailure> {
    let mut root = if path.exists() {
        serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(path).map_err(|_| CliFailure::io())?,
        )?
    } else {
        serde_json::json!({})
    };
    let command = binary.display().to_string();
    let handler = |timeout: u64, asynchronous: bool| {
        let mut value = serde_json::json!({
            "type": "command",
            "command": command,
            "args": ["agent", "hook", "--host", host],
            "timeout": timeout,
        });
        if asynchronous {
            value["async"] = serde_json::json!(true);
        }
        value
    };
    let hooks = serde_json::json!({
        "SessionStart": [{"hooks": [handler(5, false)]}],
        "UserPromptSubmit": [{"hooks": [handler(5, false)]}],
        "PostToolUse": [{"matcher": "Bash", "hooks": [handler(5, true)]}],
        "Stop": [{"hooks": [handler(5, true)]}],
        "SessionEnd": [{"hooks": [handler(3, false)]}],
    });
    let root_object = root.as_object_mut().ok_or_else(CliFailure::invalid)?;
    let configured = root_object
        .entry(root_key.to_owned())
        .or_insert_with(|| serde_json::json!({}));
    let configured = configured.as_object_mut().ok_or_else(CliFailure::invalid)?;
    for (event, groups) in hooks.as_object().ok_or_else(CliFailure::internal)? {
        let target = configured
            .entry(event.clone())
            .or_insert_with(|| serde_json::json!([]));
        let target = target.as_array_mut().ok_or_else(CliFailure::invalid)?;
        target.retain(|group| !is_hyphae_hook_group(group, binary, host));
        target.extend(
            groups
                .as_array()
                .ok_or_else(CliFailure::internal)?
                .iter()
                .cloned(),
        );
    }
    std::fs::create_dir_all(path.parent().ok_or_else(CliFailure::invalid)?)
        .map_err(|_| CliFailure::io())?;
    write_atomic(
        path,
        (serde_json::to_string_pretty(&root)? + "\n").as_bytes(),
    )?;
    println!(
        "{host} proactive memory hooks installed at {}",
        path.display()
    );
    Ok(())
}

fn is_hyphae_hook_group(group: &serde_json::Value, binary: &Path, host: &str) -> bool {
    group
        .get("hooks")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command").and_then(serde_json::Value::as_str)
                    == Some(binary.to_string_lossy().as_ref())
                    && hook
                        .get("args")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|args| {
                            args.iter()
                                .filter_map(serde_json::Value::as_str)
                                .eq(["agent", "hook", "--host", host])
                        })
            })
        })
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), CliFailure> {
    let temporary = path.with_extension(format!(
        "hyphae-tmp-{}",
        crate::native::logical_time_micros().unsigned_abs()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(|_| CliFailure::io())?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ignored = std::fs::remove_file(&temporary);
        return Err(error.into());
    }
    drop(file);
    std::fs::rename(&temporary, path).map_err(|_| CliFailure::io())?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| CliFailure::io())?;
    }
    Ok(())
}

fn deconfigure_hosts() {
    for (program, arguments) in [
        (
            "claude",
            &["mcp", "remove", "hyphae-memory", "--scope", "user"][..],
        ),
        ("codex", &["mcp", "remove", "hyphae-memory"][..]),
    ] {
        let _ignored = std::process::Command::new(program).args(arguments).status();
    }
    if let Ok(home) = user_home() {
        let _ignored = std::fs::remove_file(home.join(".pi/agent/extensions/hyphae-memory.ts"));
    }
    if let Ok(paths) = AgentPaths::resolve() {
        let _ignored = std::fs::remove_file(paths.config.join("pi-agent-memory.json"));
    }
    let config = std::env::var_os("XDG_CONFIG_HOME").map_or_else(
        || user_home().unwrap_or_default().join(".config"),
        PathBuf::from,
    );
    let _ignored = std::fs::remove_file(config.join("opencode/plugins/hyphae-memory.ts"));
}

/// Deletes the Agent Memory data directory after explicit confirmation.
pub(crate) fn purge_data(confirmed: bool) -> Result<(), CliFailure> {
    let paths = AgentPaths::resolve()?;
    if service_active() {
        eprintln!("the service owns the directory; stop it first:");
        eprintln!("  systemctl --user stop {SERVICE_NAME}");
        return Err(CliFailure::invalid());
    }
    if !confirmed {
        print!(
            "This permanently deletes {} — type 'purge' to confirm: ",
            paths.data.display()
        );
        std::io::stdout().flush().map_err(|_| CliFailure::io())?;
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|_| CliFailure::io())?;
        if answer.trim() != "purge" {
            println!("purge aborted; nothing was deleted");
            return Ok(());
        }
    }
    if paths.data.exists() {
        std::fs::remove_dir_all(&paths.data).map_err(|_| CliFailure::io())?;
        println!("deleted {}", paths.data.display());
    } else {
        println!("nothing to delete at {}", paths.data.display());
    }
    println!("backups remain at {}", paths.backups.display());
    Ok(())
}
