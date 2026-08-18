// SPDX-License-Identifier: Apache-2.0

//! Live Rust-client runner for the shared version 1 conformance fixture.

use std::{env, error::Error, io};

use hyphae_client::{ClientError, HyphaeClient};
use hyphae_contracts::v1::{
    DefineLexicalIndexRequestV1, DefineVectorSpaceRequestV1, DeleteRequestV1,
    DeleteVectorsRequestV1, ExactRetrievalOutcomeV1, ExactRetrievalRequestV1, GetRequestV1,
    HybridRetrievalOutcomeV1, HybridRetrievalRequestV1, LexicalRetrievalOutcomeV1,
    LexicalRetrievalRequestV1, PutRequestV1, PutVectorsRequestV1, QueryRequestV1,
};
use serde::Deserialize;

const FIXTURE: &str = include_str!("../../v1/cases.json");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    version: u16,
    put_request: PutRequestV1,
    conflict_put_request: PutRequestV1,
    present_get_request: GetRequestV1,
    absent_get_request: GetRequestV1,
    query_request: QueryRequestV1,
    delete_request: DeleteRequestV1,
    define_vector_space_request: DefineVectorSpaceRequestV1,
    invalid_put_vectors_request: PutVectorsRequestV1,
    put_vectors_request: PutVectorsRequestV1,
    delete_vectors_request: DeleteVectorsRequestV1,
    exact_retrieval_request: ExactRetrievalRequestV1,
    ambiguous_exact_retrieval_request: ExactRetrievalRequestV1,
    wrong_dimension_exact_retrieval_request: ExactRetrievalRequestV1,
    define_lexical_index_request: DefineLexicalIndexRequestV1,
    lexical_retrieval_request: LexicalRetrievalRequestV1,
    invalid_lexical_retrieval_request: LexicalRetrievalRequestV1,
    hybrid_retrieval_request: HybridRetrievalRequestV1,
    expected: Expected,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    first_page_keys: Vec<String>,
    second_page_keys: Vec<String>,
    matched_records: u64,
    exact_retrieval_keys: Vec<String>,
    lexical_first_key: String,
    hybrid_first_key: String,
    ambiguous_exact_reason: String,
    aggregation: serde_json::Value,
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<(), Box<dyn Error>> {
    let fixture: Fixture = serde_json::from_str(FIXTURE)?;
    require(fixture.version == 1, "unsupported conformance fixture")?;
    let base_url =
        env::var("HYPHAE_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:8787".to_owned());
    let mut builder = HyphaeClient::builder(&base_url)?;
    if let Ok(token) = env::var("HYPHAE_BEARER_TOKEN") {
        builder = builder.bearer_token(&token)?;
    }
    let client = builder.build()?;

    require(
        client.liveness().await?.value.status == "live",
        "liveness mismatch",
    )?;
    let capabilities = client.capabilities().await?.value;
    require(capabilities.api_version == "v1", "API version mismatch")?;
    require(
        capabilities
            .features
            .windows(2)
            .all(|pair| pair[0] < pair[1]),
        "capabilities are not strictly sorted",
    )?;

    let first_put = client.put(&fixture.put_request).await?.value;
    require(first_put.status == "committed", "first put did not commit")?;
    let retry = client.put(&fixture.put_request).await?.value;
    require(
        retry.status == "existing",
        "idempotent retry was not existing",
    )?;
    match client.put(&fixture.conflict_put_request).await {
        Err(ClientError::Api(failure)) => require(
            failure.code == "idempotency_conflict",
            "wrong conflict error code",
        )?,
        _ => return Err(io::Error::other("idempotency conflict was accepted").into()),
    }

    let present = client.get(&fixture.present_get_request).await?.value;
    require(present.found, "present key was reported absent")?;
    require(
        present
            .record
            .as_ref()
            .map(|record| record.key_hex.as_str())
            == Some("61"),
        "present record key mismatch",
    )?;
    let witness = client.download_witness(&present.proof).await?.value;
    require(
        witness.starts_with(b"HYSNAP01"),
        "snapshot witness magic mismatch",
    )?;

    let absent = client.get(&fixture.absent_get_request).await?.value;
    require(!absent.found && absent.record.is_none(), "absence mismatch")?;

    let first_page = client.query(&fixture.query_request).await?.value;
    require(
        keys(&first_page.rows) == fixture.expected.first_page_keys,
        "first query page mismatch",
    )?;
    require(
        first_page.matched_records == fixture.expected.matched_records,
        "matched record count mismatch",
    )?;
    require(
        serde_json::to_value(&first_page.aggregation)? == fixture.expected.aggregation,
        "aggregation mismatch",
    )?;
    let mut second_request = fixture.query_request.clone();
    second_request.cursor = first_page.next_cursor;
    let second_page = client.query(&second_request).await?.value;
    require(
        keys(&second_page.rows) == fixture.expected.second_page_keys,
        "second query page mismatch",
    )?;
    require(second_page.next_cursor.is_none(), "unexpected third page")?;

    let vector_definition = client
        .define_vector_space(&fixture.define_vector_space_request)
        .await?
        .value;
    require(
        vector_definition.status == "committed",
        "vector-space definition did not commit",
    )?;
    let vector_definition_retry = client
        .define_vector_space(&fixture.define_vector_space_request)
        .await?
        .value;
    require(
        vector_definition_retry.status == "existing",
        "vector-space definition retry was not existing",
    )?;
    match client
        .put_vectors(&fixture.invalid_put_vectors_request)
        .await
    {
        Err(ClientError::Api(failure)) => {
            require(failure.code == "invalid_request", "wrong vector error code")?;
        }
        _ => return Err(io::Error::other("mixed-validity vector batch was accepted").into()),
    }
    require(
        client
            .put_vectors(&fixture.put_vectors_request)
            .await?
            .value
            .status
            == "committed",
        "vector batch did not commit",
    )?;

    require(
        client
            .define_lexical_index(&fixture.define_lexical_index_request)
            .await?
            .value
            .status
            == "committed",
        "lexical-index definition did not commit",
    )?;

    let exact = client
        .retrieve_exact(&fixture.exact_retrieval_request)
        .await?
        .value;
    match &exact.outcome {
        ExactRetrievalOutcomeV1::Matches { matches, .. } => require(
            matches
                .iter()
                .map(|entry| entry.key_hex.clone())
                .collect::<Vec<_>>()
                == fixture.expected.exact_retrieval_keys,
            "exact retrieval order mismatch",
        )?,
        ExactRetrievalOutcomeV1::Abstained { .. } => {
            return Err(io::Error::other("exact retrieval unexpectedly abstained").into());
        }
    }
    require(
        client
            .download_retrieval_witness(&exact.proof)
            .await?
            .value
            .starts_with(b"HYSNAP01"),
        "retrieval witness magic mismatch",
    )?;

    let ambiguous = client
        .retrieve_exact(&fixture.ambiguous_exact_retrieval_request)
        .await?
        .value;
    match ambiguous.outcome {
        ExactRetrievalOutcomeV1::Abstained { abstention } => require(
            serde_json::to_value(abstention.reason)?
                == serde_json::Value::String(fixture.expected.ambiguous_exact_reason.clone()),
            "exact abstention reason mismatch",
        )?,
        ExactRetrievalOutcomeV1::Matches { .. } => {
            return Err(io::Error::other("ambiguous exact retrieval returned matches").into());
        }
    }
    match client
        .retrieve_exact(&fixture.wrong_dimension_exact_retrieval_request)
        .await
    {
        Err(ClientError::Api(failure)) => require(
            failure.code == "invalid_request",
            "wrong dimension error code",
        )?,
        _ => return Err(io::Error::other("wrong-dimension query was accepted").into()),
    }

    let lexical = client
        .retrieve_lexical(&fixture.lexical_retrieval_request)
        .await?
        .value;
    match &lexical.outcome {
        LexicalRetrievalOutcomeV1::Matches { matches, .. } => require(
            matches.first().map(|entry| entry.key_hex.as_str())
                == Some(fixture.expected.lexical_first_key.as_str()),
            "lexical retrieval order mismatch",
        )?,
        LexicalRetrievalOutcomeV1::Abstained { .. } => {
            return Err(io::Error::other("lexical retrieval unexpectedly abstained").into());
        }
    }
    match client
        .retrieve_lexical(&fixture.invalid_lexical_retrieval_request)
        .await
    {
        Err(ClientError::Api(failure)) => require(
            failure.code == "invalid_request",
            "wrong empty lexical-query error code",
        )?,
        _ => return Err(io::Error::other("empty lexical query was accepted").into()),
    }

    let hybrid = client
        .retrieve_hybrid(&fixture.hybrid_retrieval_request)
        .await?
        .value;
    match hybrid.outcome {
        HybridRetrievalOutcomeV1::Matches { matches, .. } => require(
            matches.first().map(|entry| entry.key_hex.as_str())
                == Some(fixture.expected.hybrid_first_key.as_str()),
            "hybrid retrieval order mismatch",
        )?,
        HybridRetrievalOutcomeV1::Abstained { .. } => {
            return Err(io::Error::other("hybrid retrieval unexpectedly abstained").into());
        }
    }
    require(
        client
            .delete_vectors(&fixture.delete_vectors_request)
            .await?
            .value
            .status
            == "committed",
        "vector deletion did not commit",
    )?;

    client.delete(&fixture.delete_request).await?;
    let deleted = client
        .get(&GetRequestV1 {
            key_hex: "62".to_owned(),
        })
        .await?
        .value;
    require(!deleted.found, "deleted key is still present")?;

    println!(r#"{{"client":"rust","status":"ok"}}"#);
    Ok(())
}

fn keys(records: &[hyphae_contracts::v1::RecordV1]) -> Vec<String> {
    records
        .iter()
        .map(|record| record.key_hex.clone())
        .collect()
}

fn require(condition: bool, message: &'static str) -> Result<(), io::Error> {
    if condition {
        Ok(())
    } else {
        Err(io::Error::other(message))
    }
}

#[cfg(test)]
mod tests {
    use super::{FIXTURE, Fixture};

    #[test]
    fn shared_fixture_matches_typed_public_contracts() -> Result<(), serde_json::Error> {
        let fixture: Fixture = serde_json::from_str(FIXTURE)?;
        assert_eq!(fixture.version, 1);
        assert_eq!(fixture.put_request.records.len(), 4);
        assert_eq!(fixture.put_vectors_request.vectors.len(), 3);
        Ok(())
    }
}
