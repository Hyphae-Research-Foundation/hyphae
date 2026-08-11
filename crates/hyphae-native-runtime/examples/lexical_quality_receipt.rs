// SPDX-License-Identifier: AGPL-3.0-only

//! Deterministic bounded BM25 equivalence receipt for the native lexical index.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use hyphae_native_runtime::{MatchHit, NativeDatabase};
use hyphae_native_types::{DurabilityClass, ObjectId};

const DOCUMENT_COUNT: u32 = 512;
const QUERIES: [&str; 4] = ["rust", "sql engine", "common", "missing"];
const TOP_K: usize = 25;
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create() -> Self {
        Self(std::env::temp_dir().join(format!(
            "hyphae-native-lexical-quality-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.0);
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let source_commit = std::env::args()
        .nth(1)
        .ok_or("lexical_quality_receipt requires the exact source commit")?;
    let temporary = TemporaryDirectory::create();
    let index = ObjectId::new(10)?;
    let documents = documents();
    let dataset_digest = digest_dataset(&documents);
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut seed = database.begin(10, DurabilityClass::Strict)?;
    seed.create_search_index(index, "native_search")?;
    for (document_id, text) in &documents {
        seed.index_document(index, document_id.clone(), text)?;
    }
    seed.commit()?;

    let expected = QUERIES
        .iter()
        .map(|query| reference_bm25(&documents, query, TOP_K))
        .collect::<Result<Vec<_>, _>>()?;
    let before = QUERIES
        .iter()
        .map(|query| database.match_latest_text(index, query, TOP_K))
        .collect::<Result<Vec<_>, _>>()?;
    if before != expected {
        return Err("native lexical result differs from independent BM25 oracle".into());
    }
    drop(database);

    let reopened = NativeDatabase::open(temporary.path())?;
    let after = QUERIES
        .iter()
        .map(|query| reopened.match_latest_text(index, query, TOP_K))
        .collect::<Result<Vec<_>, _>>()?;
    if after != before {
        return Err("native lexical result differs after reopen".into());
    }

    let result_digests = after
        .iter()
        .map(|hits| digest_hits(hits))
        .collect::<Vec<_>>();
    println!("{{");
    println!("  \"schema\": \"hyphae-native-lexical-quality-v1\",");
    println!("  \"source_commit\": \"{source_commit}\",");
    println!("  \"dataset_digest\": \"{}\",", hex(&dataset_digest)?);
    println!("  \"document_count\": {DOCUMENT_COUNT},");
    println!("  \"query_count\": {},", QUERIES.len());
    println!("  \"top_k\": {TOP_K},");
    println!("  \"exact_score_order_equivalence\": true,");
    println!("  \"reopen_equivalence\": true,");
    println!("  \"query_result_digests\": [");
    for (index, digest) in result_digests.iter().enumerate() {
        let comma = if index + 1 == result_digests.len() {
            ""
        } else {
            ","
        };
        println!("    \"{}\"{comma}", hex(digest)?);
    }
    println!("  ]");
    println!("}}");
    Ok(())
}

fn documents() -> Vec<(Vec<u8>, String)> {
    (0..DOCUMENT_COUNT)
        .map(|document| {
            let text = if document % 2 == 0 {
                format!("rust engine common document{document}")
            } else {
                format!("sql engine common document{document}")
            };
            (document.to_be_bytes().to_vec(), text)
        })
        .collect()
}

fn analyze(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn count_f64(value: usize) -> Result<f64, std::num::TryFromIntError> {
    u32::try_from(value).map(f64::from)
}

fn reference_bm25(
    documents: &[(Vec<u8>, String)],
    query: &str,
    limit: usize,
) -> Result<Vec<MatchHit>, std::num::TryFromIntError> {
    let analyzed = documents
        .iter()
        .map(|(_, text)| analyze(text))
        .collect::<Vec<_>>();
    let document_count = count_f64(documents.len())?;
    let average_length = count_f64(analyzed.iter().map(Vec::len).sum::<usize>())? / document_count;
    let terms = analyze(query).into_iter().collect::<BTreeSet<_>>();
    let mut scores = BTreeMap::<Vec<u8>, f64>::new();
    for term in terms {
        let document_frequency = count_f64(
            analyzed
                .iter()
                .filter(|tokens| tokens.iter().any(|token| token == &term))
                .count(),
        )?;
        if document_frequency == 0.0 {
            continue;
        }
        let idf =
            (1.0 + (document_count - document_frequency + 0.5) / (document_frequency + 0.5)).ln();
        for ((document_id, _), tokens) in documents.iter().zip(&analyzed) {
            let frequency = count_f64(tokens.iter().filter(|token| *token == &term).count())?;
            if frequency == 0.0 {
                continue;
            }
            let normalization = 1.2 * (0.25 + 0.75 * (count_f64(tokens.len())? / average_length));
            *scores.entry(document_id.clone()).or_default() +=
                idf * (frequency * 2.2) / (frequency + normalization);
        }
    }
    let mut hits = scores
        .into_iter()
        .map(|(document_id, score)| MatchHit { document_id, score })
        .collect::<Vec<_>>();
    hits.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.document_id.cmp(&right.document_id))
    });
    hits.truncate(limit);
    Ok(hits)
}

fn digest_dataset(documents: &[(Vec<u8>, String)]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-native-lexical-quality-corpus-v1");
    for (id, text) in documents {
        hasher.update(&(id.len() as u64).to_le_bytes());
        hasher.update(id);
        hasher.update(&(text.len() as u64).to_le_bytes());
        hasher.update(text.as_bytes());
    }
    *hasher.finalize().as_bytes()
}

fn digest_hits(hits: &[MatchHit]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-native-lexical-query-result-v1");
    for hit in hits {
        hasher.update(&(hit.document_id.len() as u64).to_le_bytes());
        hasher.update(&hit.document_id);
        hasher.update(&hit.score.to_bits().to_le_bytes());
    }
    *hasher.finalize().as_bytes()
}

fn hex(bytes: &[u8]) -> Result<String, std::fmt::Error> {
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        write!(encoded, "{byte:02x}")?;
    }
    Ok(encoded)
}
