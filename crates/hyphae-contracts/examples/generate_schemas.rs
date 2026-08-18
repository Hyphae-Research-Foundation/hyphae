// SPDX-License-Identifier: Apache-2.0

//! Regenerates checked-in JSON Schema 2020-12 public contract files.

use std::{env, error::Error, fs, path::PathBuf};

use hyphae_contracts::generate_json_schema;
use hyphae_contracts::v1::{
    CapabilitiesV1, CommitReceiptV1, DefineLexicalIndexRequestV1, DefineVectorSpaceRequestV1,
    DeleteRequestV1, DeleteVectorsRequestV1, ErrorV1, ExactRetrievalRequestV1,
    ExactRetrievalResponseV1, GetRequestV1, GetResponseV1, HealthV1, HybridRetrievalRequestV1,
    HybridRetrievalResponseV1, LexicalRetrievalRequestV1, LexicalRetrievalResponseV1, ProofV1,
    PutRequestV1, PutVectorsRequestV1, QueryRequestV1, QueryResponseV1, RetrievalProofV1,
};
use schemars::JsonSchema;

fn main() -> Result<(), Box<dyn Error>> {
    let directory = env::args_os().nth(1).map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/json-schema"),
        PathBuf::from,
    );
    fs::create_dir_all(&directory)?;
    write::<CapabilitiesV1>(&directory, "capabilities-v1.schema.json")?;
    write::<ErrorV1>(&directory, "error-v1.schema.json")?;
    write::<HealthV1>(&directory, "health-v1.schema.json")?;
    write::<PutRequestV1>(&directory, "put-request-v1.schema.json")?;
    write::<DeleteRequestV1>(&directory, "delete-request-v1.schema.json")?;
    write::<GetRequestV1>(&directory, "get-request-v1.schema.json")?;
    write::<GetResponseV1>(&directory, "get-response-v1.schema.json")?;
    write::<CommitReceiptV1>(&directory, "commit-receipt-v1.schema.json")?;
    write::<QueryRequestV1>(&directory, "query-request-v1.schema.json")?;
    write::<QueryResponseV1>(&directory, "query-response-v1.schema.json")?;
    write::<ProofV1>(&directory, "proof-v1.schema.json")?;
    write::<DefineVectorSpaceRequestV1>(&directory, "define-vector-space-request-v1.schema.json")?;
    write::<PutVectorsRequestV1>(&directory, "put-vectors-request-v1.schema.json")?;
    write::<DeleteVectorsRequestV1>(&directory, "delete-vectors-request-v1.schema.json")?;
    write::<ExactRetrievalRequestV1>(&directory, "exact-retrieval-request-v1.schema.json")?;
    write::<ExactRetrievalResponseV1>(&directory, "exact-retrieval-response-v1.schema.json")?;
    write::<RetrievalProofV1>(&directory, "retrieval-proof-v1.schema.json")?;
    write::<DefineLexicalIndexRequestV1>(
        &directory,
        "define-lexical-index-request-v1.schema.json",
    )?;
    write::<LexicalRetrievalRequestV1>(&directory, "lexical-retrieval-request-v1.schema.json")?;
    write::<LexicalRetrievalResponseV1>(&directory, "lexical-retrieval-response-v1.schema.json")?;
    write::<HybridRetrievalRequestV1>(&directory, "hybrid-retrieval-request-v1.schema.json")?;
    write::<HybridRetrievalResponseV1>(&directory, "hybrid-retrieval-response-v1.schema.json")?;
    Ok(())
}

fn write<T: JsonSchema>(directory: &std::path::Path, name: &str) -> Result<(), Box<dyn Error>> {
    let schema = generate_json_schema::<T>();
    let mut encoded = serde_json::to_string_pretty(&schema)?;
    encoded.push('\n');
    fs::write(directory.join(name), encoded)?;
    Ok(())
}
