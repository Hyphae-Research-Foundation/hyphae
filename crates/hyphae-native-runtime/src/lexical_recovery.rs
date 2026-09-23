// SPDX-License-Identifier: Apache-2.0

//! Complete lexical validation with a bounded borrowed traversal frontier.
//! Each document and posting is checked against its counterpart in the same
//! immutable root, so no corpus-sized projection is retained during recovery.

use std::{
    collections::BTreeMap,
    ops::{Bound, ControlFlow},
};

use hyphae_native_btree::{BTree, BorrowedVisitError, BorrowedVisitLimits};
use hyphae_native_pages::PageStore;
use hyphae_native_types::ObjectId;

use crate::{
    NativeRuntimeError, PhysicalSearchFormat, RECOVERY_MEMORY_BYTES, SEARCH_DOCUMENT_PREFIX,
    SEARCH_INDEX_META_PREFIX, SEARCH_POSTING_PREFIX, SEARCH_TERM_META_PREFIX, analyze,
    decode_live_search_document, decode_live_search_posting, decode_live_search_term_metadata,
    decode_search_index_metadata, decode_search_object_key, decode_search_posting_key,
    is_canonical_search_term, model::SearchState, physical_search_format, search_document_key,
    search_document_logical_bytes, search_index_meta_key, search_posting_key,
    search_posting_prefix, search_term_meta_key, validate_search_document_identity,
};
use hyphae_native_blobs::BlobStore;

const RECOVERY_VISIT_ENTRIES: usize = 1_024;
// A compact vocabulary projection avoids repeated document hydration on the
// common path. Very large vocabularies fall back to per-posting verification.
const EXPECTED_TERMS_BYTES: u64 = RECOVERY_MEMORY_BYTES / 4;
type ExpectedTermFrequencies = BTreeMap<(ObjectId, Vec<u8>), u64>;

/// Visit an arbitrarily large immutable range while retaining at most one
/// bounded borrowed visitor's page set and one resume key at a time.
pub(super) fn visit_chunked(
    tree: BTree,
    pages: &PageStore,
    lower: Bound<&[u8]>,
    upper: Bound<&[u8]>,
    mut accept: impl FnMut(&[u8], &[u8]) -> Result<(), NativeRuntimeError>,
) -> Result<(), NativeRuntimeError> {
    let mut resume = None::<Vec<u8>>;
    loop {
        let current_lower = resume.as_deref().map_or(lower, Bound::Excluded);
        let mut last_key = Vec::new();
        let mut failure = None;
        let visited = tree.visit_range_borrowed_with_control(
            pages,
            current_lower,
            upper,
            BorrowedVisitLimits {
                maximum_entries: RECOVERY_VISIT_ENTRIES,
                maximum_bytes: RECOVERY_MEMORY_BYTES,
            },
            || ControlFlow::Continue(()),
            |key, value| {
                last_key.clear();
                last_key.extend_from_slice(key);
                match accept(key, value) {
                    Ok(()) => ControlFlow::Continue(()),
                    Err(error) => {
                        failure = Some(error);
                        ControlFlow::Break(())
                    }
                }
            },
        );
        if let Some(error) = failure {
            return Err(error);
        }
        match visited {
            Ok(stats) if stats.complete => return Ok(()),
            Err(BorrowedVisitError::LimitExceeded) if !last_key.is_empty() => {
                resume = Some(last_key);
            }
            Err(BorrowedVisitError::LimitExceeded) => {
                return Err(NativeRuntimeError::SearchRecoveryVisitLimitExceeded);
            }
            Err(BorrowedVisitError::Tree(error)) => return Err(error.into()),
            Err(BorrowedVisitError::Cancelled) | Ok(_) => {
                return Err(NativeRuntimeError::InvalidSearchTree);
            }
        }
    }
}

fn prefix_end(prefix: &[u8]) -> Result<Vec<u8>, NativeRuntimeError> {
    let mut end = prefix.to_vec();
    for index in (0..end.len()).rev() {
        if end[index] != u8::MAX {
            end[index] += 1;
            end.truncate(index + 1);
            return Ok(end);
        }
    }
    Err(NativeRuntimeError::InvalidSearchTree)
}

fn invalid_identity(error: NativeRuntimeError) -> NativeRuntimeError {
    if matches!(error, NativeRuntimeError::SearchIdentityTooLarge) {
        NativeRuntimeError::InvalidSearchTree
    } else {
        error
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn validate_root(
    pages: &PageStore,
    blobs: &BlobStore,
    tree: BTree,
) -> Result<(), NativeRuntimeError> {
    let format = physical_search_format(pages, tree)?;
    let expected_terms = validate_documents_by_index(pages, blobs, tree, format)?;
    validate_document_index_owners(pages, tree)?;
    validate_term_frequencies(pages, tree, format, expected_terms.as_ref())?;
    validate_postings(pages, blobs, tree, format, expected_terms.is_none())?;
    validate_unknown_search_prefix(pages, tree)
}

fn validate_unknown_search_prefix(
    pages: &PageStore,
    tree: BTree,
) -> Result<(), NativeRuntimeError> {
    // Match the complete-state loader's zero-entry check: ANN owns prefixes
    // 0x05 through 0x0b, and any key at or above 0x0c is corruption.
    tree.visit_range_borrowed_with_control(
        pages,
        Bound::Included(&[12]),
        Bound::Unbounded,
        BorrowedVisitLimits {
            maximum_entries: 0,
            maximum_bytes: 0,
        },
        || ControlFlow::Continue(()),
        |_, _| ControlFlow::Continue(()),
    )
    .map_err(crate::map_borrowed_search_visit_error)?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn validate_documents_by_index(
    pages: &PageStore,
    blobs: &BlobStore,
    tree: BTree,
    format: PhysicalSearchFormat,
) -> Result<Option<ExpectedTermFrequencies>, NativeRuntimeError> {
    let mut expected_terms = Some(ExpectedTermFrequencies::new());
    let mut expected_term_bytes = 0_u64;
    visit_chunked(
        tree,
        pages,
        Bound::Included(&[SEARCH_INDEX_META_PREFIX]),
        Bound::Excluded(&[SEARCH_DOCUMENT_PREFIX]),
        |key, value| {
            if key.len() != 17 {
                return Err(NativeRuntimeError::InvalidSearchTree);
            }
            let (index, suffix) = decode_search_object_key(key, SEARCH_INDEX_META_PREFIX)?;
            if !suffix.is_empty() {
                return Err(NativeRuntimeError::InvalidSearchTree);
            }
            let (expected_documents, expected_tokens) = decode_search_index_metadata(value)?;
            let prefix = search_document_key(index, &[]).map_err(invalid_identity)?;
            let end = prefix_end(&prefix)?;
            let mut document_count = 0_u64;
            let mut token_count_total = 0_u64;
            visit_chunked(
                tree,
                pages,
                Bound::Included(&prefix),
                Bound::Excluded(&end),
                |document_key, document_value| {
                    let (_, document_id) =
                        decode_search_object_key(document_key, SEARCH_DOCUMENT_PREFIX)?;
                    validate_search_document_identity(document_id, "").map_err(invalid_identity)?;
                    let Some((text, token_count)) =
                        decode_live_search_document(document_value, blobs, format)?
                    else {
                        return Ok(());
                    };
                    validate_search_document_identity(document_id, &text)
                        .map_err(invalid_identity)?;
                    document_count = document_count
                        .checked_add(1)
                        .ok_or(NativeRuntimeError::InvalidSearchTree)?;
                    token_count_total = token_count_total
                        .checked_add(token_count)
                        .ok_or(NativeRuntimeError::InvalidSearchTree)?;
                    let mut frequencies = BTreeMap::<Vec<u8>, u32>::new();
                    for token in analyze(&text) {
                        let frequency = frequencies.entry(token.into_bytes()).or_default();
                        *frequency = frequency
                            .checked_add(1)
                            .ok_or(NativeRuntimeError::InvalidSearchTree)?;
                    }
                    for (term, frequency) in frequencies {
                        let mut discard_projection = false;
                        if let Some(projected) = expected_terms.as_mut() {
                            let identity = (index, term.clone());
                            if let Some(count) = projected.get_mut(&identity) {
                                *count = count
                                    .checked_add(1)
                                    .ok_or(NativeRuntimeError::InvalidSearchTree)?;
                            } else {
                                let charge = 512_u64
                                    .checked_add(4_u64.saturating_mul(term.len() as u64))
                                    .ok_or(NativeRuntimeError::InvalidSearchTree)?;
                                let next = expected_term_bytes
                                    .checked_add(charge)
                                    .ok_or(NativeRuntimeError::InvalidSearchTree)?;
                                if next > EXPECTED_TERMS_BYTES {
                                    discard_projection = true;
                                } else {
                                    projected.insert(identity, 1);
                                    expected_term_bytes = next;
                                }
                            }
                        }
                        if discard_projection {
                            expected_terms = None;
                        }
                        let posting_key = search_posting_key(index, &term, document_id)
                            .map_err(invalid_identity)?;
                        let posting_value = tree
                            .get(pages, &posting_key)?
                            .ok_or(NativeRuntimeError::InvalidSearchTree)?;
                        let posting = decode_live_search_posting(&posting_value, format)?
                            .ok_or(NativeRuntimeError::InvalidSearchTree)?;
                        if posting.term_frequency != frequency
                            || posting
                                .document_length
                                .is_some_and(|length| u64::from(length) != token_count)
                        {
                            return Err(NativeRuntimeError::InvalidSearchTree);
                        }
                    }
                    Ok(())
                },
            )?;
            if document_count != expected_documents || token_count_total != expected_tokens {
                return Err(NativeRuntimeError::InvalidSearchTree);
            }
            Ok(())
        },
    )?;
    Ok(expected_terms)
}

fn validate_document_index_owners(
    pages: &PageStore,
    tree: BTree,
) -> Result<(), NativeRuntimeError> {
    let mut last_index = None::<ObjectId>;
    visit_chunked(
        tree,
        pages,
        Bound::Included(&[SEARCH_DOCUMENT_PREFIX]),
        Bound::Excluded(&[SEARCH_TERM_META_PREFIX]),
        |key, _| {
            let (index, _) = decode_search_object_key(key, SEARCH_DOCUMENT_PREFIX)?;
            if last_index != Some(index) {
                let metadata = tree
                    .get(pages, &search_index_meta_key(index))?
                    .ok_or(NativeRuntimeError::InvalidSearchTree)?;
                decode_search_index_metadata(&metadata)?;
                last_index = Some(index);
            }
            Ok(())
        },
    )
}

#[allow(clippy::too_many_lines)]
fn validate_term_frequencies(
    pages: &PageStore,
    tree: BTree,
    format: PhysicalSearchFormat,
    expected_terms: Option<&ExpectedTermFrequencies>,
) -> Result<(), NativeRuntimeError> {
    visit_chunked(
        tree,
        pages,
        Bound::Included(&[SEARCH_TERM_META_PREFIX]),
        Bound::Excluded(&[SEARCH_POSTING_PREFIX]),
        |key, value| {
            let (index, term) = decode_search_object_key(key, SEARCH_TERM_META_PREFIX)?;
            if !is_canonical_search_term(term) {
                return Err(NativeRuntimeError::InvalidSearchTree);
            }
            let metadata = tree
                .get(pages, &search_index_meta_key(index))?
                .ok_or(NativeRuntimeError::InvalidSearchTree)?;
            decode_search_index_metadata(&metadata)?;
            let live_frequency = decode_live_search_term_metadata(value, format)?;
            if live_frequency == Some(0) {
                return Err(NativeRuntimeError::InvalidSearchTree);
            }
            let expected = live_frequency.unwrap_or(0);
            if let Some(projected) = expected_terms {
                let from_documents = projected.get(&(index, term.to_vec())).copied().unwrap_or(0);
                if from_documents != expected {
                    return Err(NativeRuntimeError::InvalidSearchTree);
                }
            }
            let prefix = search_posting_prefix(index, term).map_err(invalid_identity)?;
            let end = prefix_end(&prefix)?;
            let mut actual = 0_u64;
            visit_chunked(
                tree,
                pages,
                Bound::Included(&prefix),
                Bound::Excluded(&end),
                |_, posting_value| {
                    if decode_live_search_posting(posting_value, format)?.is_some() {
                        actual = actual
                            .checked_add(1)
                            .ok_or(NativeRuntimeError::InvalidSearchTree)?;
                    }
                    Ok(())
                },
            )?;
            if actual != expected {
                return Err(NativeRuntimeError::InvalidSearchTree);
            }
            Ok(())
        },
    )
}

#[allow(clippy::too_many_lines)]
fn validate_postings(
    pages: &PageStore,
    blobs: &BlobStore,
    tree: BTree,
    format: PhysicalSearchFormat,
    verify_source_text: bool,
) -> Result<(), NativeRuntimeError> {
    let mut last_term = None::<(ObjectId, Vec<u8>)>;
    visit_chunked(
        tree,
        pages,
        Bound::Included(&[SEARCH_POSTING_PREFIX]),
        Bound::Excluded(&[SEARCH_POSTING_PREFIX + 1]),
        |key, value| {
            let (index, term, document_id) = decode_search_posting_key(key)?;
            if !is_canonical_search_term(term) {
                return Err(NativeRuntimeError::InvalidSearchTree);
            }
            validate_search_document_identity(document_id, "").map_err(invalid_identity)?;
            let Some(posting) = decode_live_search_posting(value, format)? else {
                return Ok(());
            };
            if last_term
                .as_ref()
                .is_none_or(|(prior_index, prior_term)| *prior_index != index || prior_term != term)
            {
                let term_key = search_term_meta_key(index, term).map_err(invalid_identity)?;
                let term_value = tree
                    .get(pages, &term_key)?
                    .ok_or(NativeRuntimeError::InvalidSearchTree)?;
                decode_live_search_term_metadata(&term_value, format)?
                    .ok_or(NativeRuntimeError::InvalidSearchTree)?;
                last_term = Some((index, term.to_vec()));
            }
            if !verify_source_text {
                // Forward validation and exact physical frequency counts already
                // prove this entry when the bounded vocabulary fits.
                return Ok(());
            }
            let document_key = search_document_key(index, document_id).map_err(invalid_identity)?;
            let document_value = tree
                .get(pages, &document_key)?
                .ok_or(NativeRuntimeError::InvalidSearchTree)?;
            let (text, document_length) =
                decode_live_search_document(&document_value, blobs, format)?
                    .ok_or(NativeRuntimeError::InvalidSearchTree)?;
            if posting
                .document_length
                .is_some_and(|length| u64::from(length) != document_length)
            {
                return Err(NativeRuntimeError::InvalidSearchTree);
            }
            let frequency = analyze(&text)
                .into_iter()
                .filter(|token| token.as_bytes() == term)
                .count();
            if usize::try_from(posting.term_frequency).ok() != Some(frequency) {
                return Err(NativeRuntimeError::InvalidSearchTree);
            }
            Ok(())
        },
    )
}

const DOCUMENT_STATE_INDEX_BYTES: u64 = 4_096;
const DOCUMENT_STATE_ENTRY_BYTES: u64 = 192;

fn admit_document_state(total: &mut u64, bytes: u64) -> Result<(), NativeRuntimeError> {
    let observed = total.checked_add(bytes).unwrap_or(u64::MAX);
    let configured = crate::document_state_budget();
    if observed > configured {
        return Err(NativeRuntimeError::SearchRecoveryRetainedLimitExceeded {
            configured,
            observed,
        });
    }
    *total = observed;
    Ok(())
}

fn document_state_entry_bytes(key: &[u8], text_length: u64) -> Result<u64, NativeRuntimeError> {
    let document_id_length = key
        .len()
        .checked_sub(17)
        .ok_or(NativeRuntimeError::InvalidSearchTree)?;
    DOCUMENT_STATE_ENTRY_BYTES
        .checked_add(
            u64::try_from(document_id_length).map_err(|_| NativeRuntimeError::InvalidSearchTree)?,
        )
        .and_then(|bytes| bytes.checked_add(text_length))
        .ok_or(NativeRuntimeError::InvalidSearchTree)
}

/// Admission proof used before WAL publication. This charges only state that
/// an all-engine snapshot actually retains, not the ephemeral posting checks.
pub(super) fn measure_document_state(
    pages: &PageStore,
    tree: BTree,
) -> Result<u64, NativeRuntimeError> {
    let format = physical_search_format(pages, tree)?;
    let mut total = 0_u64;
    visit_chunked(
        tree,
        pages,
        Bound::Included(&[SEARCH_INDEX_META_PREFIX]),
        Bound::Excluded(&[SEARCH_DOCUMENT_PREFIX]),
        |key, value| {
            if key.len() != 17 {
                return Err(NativeRuntimeError::InvalidSearchTree);
            }
            let (_, suffix) = decode_search_object_key(key, SEARCH_INDEX_META_PREFIX)?;
            if !suffix.is_empty() {
                return Err(NativeRuntimeError::InvalidSearchTree);
            }
            decode_search_index_metadata(value)?;
            admit_document_state(&mut total, DOCUMENT_STATE_INDEX_BYTES)
        },
    )?;
    visit_chunked(
        tree,
        pages,
        Bound::Included(&[SEARCH_DOCUMENT_PREFIX]),
        Bound::Excluded(&[SEARCH_TERM_META_PREFIX]),
        |key, value| {
            decode_search_object_key(key, SEARCH_DOCUMENT_PREFIX)?;
            if let Some(logical_length) = search_document_logical_bytes(value, format)? {
                admit_document_state(&mut total, document_state_entry_bytes(key, logical_length)?)?;
            }
            Ok(())
        },
    )?;
    Ok(total)
}

/// Rehydrates only the durable document texts used by snapshot readers. The
/// complete projection has already been checked with bounded traversals.
pub(super) fn load_document_state(
    pages: &PageStore,
    blobs: &BlobStore,
    tree: BTree,
) -> Result<SearchState, NativeRuntimeError> {
    let format = physical_search_format(pages, tree)?;
    let mut total = 0_u64;
    let mut indexes = BTreeMap::new();
    visit_chunked(
        tree,
        pages,
        Bound::Included(&[SEARCH_INDEX_META_PREFIX]),
        Bound::Excluded(&[SEARCH_DOCUMENT_PREFIX]),
        |key, value| {
            if key.len() != 17 {
                return Err(NativeRuntimeError::InvalidSearchTree);
            }
            let (index, suffix) = decode_search_object_key(key, SEARCH_INDEX_META_PREFIX)?;
            if !suffix.is_empty() || indexes.contains_key(&index) {
                return Err(NativeRuntimeError::InvalidSearchTree);
            }
            decode_search_index_metadata(value)?;
            admit_document_state(&mut total, DOCUMENT_STATE_INDEX_BYTES)?;
            indexes.insert(index, BTreeMap::<Vec<u8>, String>::new());
            Ok(())
        },
    )?;
    visit_chunked(
        tree,
        pages,
        Bound::Included(&[SEARCH_DOCUMENT_PREFIX]),
        Bound::Excluded(&[SEARCH_TERM_META_PREFIX]),
        |key, value| {
            let (index, document_id) = decode_search_object_key(key, SEARCH_DOCUMENT_PREFIX)?;
            let Some(logical_length) = search_document_logical_bytes(value, format)? else {
                return Ok(());
            };
            admit_document_state(&mut total, document_state_entry_bytes(key, logical_length)?)?;
            let (text, _) = decode_live_search_document(value, blobs, format)?
                .ok_or(NativeRuntimeError::InvalidSearchTree)?;
            let documents = indexes
                .get_mut(&index)
                .ok_or(NativeRuntimeError::InvalidSearchTree)?;
            if documents.insert(document_id.to_vec(), text).is_some() {
                return Err(NativeRuntimeError::InvalidSearchTree);
            }
            Ok(())
        },
    )?;
    Ok(SearchState { indexes })
}
