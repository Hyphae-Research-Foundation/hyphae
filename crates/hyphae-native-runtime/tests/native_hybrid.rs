// SPDX-License-Identifier: Apache-2.0

//! Embedded native hybrid retrieval coverage.

use hyphae_native_runtime::{
    AnnSearchOptions, HnswConfig, NativeDatabase, NativeHybridError, NativeHybridRequest,
    NativeVectorBranch, Vector, VectorMetric,
};
use hyphae_native_types::{DurabilityClass, ObjectId};
use hyphae_retrieval::{HybridOutcome, HybridRequest};

static NEXT_DIRECTORY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct TemporaryDirectory(std::path::PathBuf);

impl TemporaryDirectory {
    fn create() -> Result<Self, Box<dyn std::error::Error>> {
        let path = std::env::temp_dir().join(format!(
            "hyphae-native-hybrid-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
                + u128::from(NEXT_DIRECTORY.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
        ));
        Ok(Self(path))
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ignored = std::fs::remove_dir_all(&self.0);
    }
}

fn config() -> Result<HnswConfig, Box<dyn std::error::Error>> {
    Ok(HnswConfig::new(4, 16, 8, 32, 0x4859_5048_4145)?)
}

fn request(
    lexical: ObjectId,
    vectors: ObjectId,
    query: &Vector,
    vector_branch: NativeVectorBranch,
) -> NativeHybridRequest<'_> {
    NativeHybridRequest {
        lexical_index: lexical,
        lexical_query: "rust",
        lexical_limit: 3,
        vector_index: vectors,
        vector_query: query,
        vector_branch,
        vector_limit: 3,
        fusion: HybridRequest {
            lexical_weight: 1,
            vector_weight: 1,
            limit: 3,
        },
    }
}

#[test]
fn exact_hybrid_fuses_stable_ids_on_one_snapshot_with_explanations()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(&temporary.0)?;
    let lexical = ObjectId::new(1)?;
    let vectors = ObjectId::new(2)?;
    let first = ObjectId::new(10)?;
    let shared = ObjectId::new(20)?;
    let third = ObjectId::new(30)?;
    let mut seed = database.begin(0, DurabilityClass::Memory)?;
    seed.create_search_index(lexical, "documents")?;
    seed.create_vector_index(vectors, "vectors", 2, VectorMetric::Cosine, config()?)?;
    seed.index_document(lexical, first.get().to_be_bytes().to_vec(), "rust rust")?;
    seed.index_document(lexical, shared.get().to_be_bytes().to_vec(), "rust")?;
    seed.upsert_vectors(
        vectors,
        [
            (shared, Vector::new([1.0, 0.0])?),
            (third, Vector::new([0.8, 0.2])?),
            (first, Vector::new([0.0, 1.0])?),
        ],
    )?;
    seed.commit()?;

    let query = Vector::new([1.0, 0.0])?;
    let snapshot = database.snapshot(7)?;
    let receipt = snapshot.retrieve_hybrid(&request(
        lexical,
        vectors,
        &query,
        NativeVectorBranch::Exact,
    ))?;
    assert_eq!(receipt.snapshot_csn, snapshot.visible_csn());
    assert_eq!(receipt.lexical_candidates, 2);
    assert_eq!(receipt.vector_candidates, 3);
    assert!(receipt.ann.is_none());
    let HybridOutcome::Matches { matches, .. } = receipt.outcome else {
        return Err(std::io::Error::other("hybrid unexpectedly abstained").into());
    };
    assert_eq!(matches[0].key, shared.get().to_be_bytes());
    assert_eq!(matches[0].explanation.lexical_rank, Some(2));
    assert_eq!(matches[0].explanation.vector_rank, Some(1));
    assert_eq!(matches[0].explanation.final_rank, 1);

    let mut later = database.begin(8, DurabilityClass::Memory)?;
    later.index_document(
        lexical,
        third.get().to_be_bytes().to_vec(),
        "rust rust rust",
    )?;
    later.commit()?;
    assert_eq!(
        snapshot
            .retrieve_hybrid(&request(
                lexical,
                vectors,
                &query,
                NativeVectorBranch::Exact,
            ))?
            .snapshot_csn,
        receipt.snapshot_csn
    );
    Ok(())
}

#[test]
fn ann_hybrid_exposes_ann_receipt_and_rejects_branch_limit_mismatch()
-> Result<(), Box<dyn std::error::Error>> {
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(&temporary.0)?;
    let lexical = ObjectId::new(1)?;
    let vectors = ObjectId::new(2)?;
    let stable = ObjectId::new(10)?;
    let mut seed = database.begin(0, DurabilityClass::Memory)?;
    seed.create_search_index(lexical, "documents")?;
    seed.create_vector_index(vectors, "vectors", 2, VectorMetric::Cosine, config()?)?;
    seed.index_document(lexical, stable.get().to_be_bytes().to_vec(), "rust")?;
    seed.upsert_vector(vectors, stable, Vector::new([1.0, 0.0])?)?;
    seed.commit()?;

    let query = Vector::new([1.0, 0.0])?;
    let options = AnnSearchOptions::new(1, 8, Some(1))?;
    let mut request = request(lexical, vectors, &query, NativeVectorBranch::Ann(options));
    request.vector_limit = 1;
    let receipt = database.retrieve_hybrid_latest(0, &request)?;
    let ann = receipt
        .ann
        .ok_or_else(|| std::io::Error::other("missing ANN receipt"))?;
    assert!(ann.approximate);
    assert_eq!(ann.snapshot_csn, receipt.snapshot_csn);

    request.vector_limit = 2;
    assert!(matches!(
        database.retrieve_hybrid_latest(0, &request),
        Err(NativeHybridError::InvalidLimit)
    ));
    request.vector_limit = 1;
    request.lexical_limit = 0;
    assert!(matches!(
        database.retrieve_hybrid_latest(0, &request),
        Err(NativeHybridError::InvalidLimit)
    ));
    Ok(())
}

#[test]
fn hybrid_rejects_lexical_ids_that_cannot_join_vector_ids() -> Result<(), Box<dyn std::error::Error>>
{
    let temporary = TemporaryDirectory::create()?;
    let mut database = NativeDatabase::create(&temporary.0)?;
    let lexical = ObjectId::new(1)?;
    let vectors = ObjectId::new(2)?;
    let stable = ObjectId::new(10)?;
    let mut seed = database.begin(0, DurabilityClass::Memory)?;
    seed.create_search_index(lexical, "documents")?;
    seed.create_vector_index(vectors, "vectors", 2, VectorMetric::Cosine, config()?)?;
    seed.index_document(lexical, b"not-an-id".to_vec(), "rust")?;
    seed.upsert_vector(vectors, stable, Vector::new([1.0, 0.0])?)?;
    seed.commit()?;
    let query = Vector::new([1.0, 0.0])?;
    assert!(matches!(
        database.retrieve_hybrid_latest(
            0,
            &request(lexical, vectors, &query, NativeVectorBranch::Exact)
        ),
        Err(NativeHybridError::InvalidStableId)
    ));
    Ok(())
}
