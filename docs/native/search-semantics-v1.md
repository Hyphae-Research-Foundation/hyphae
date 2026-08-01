# Native search-engine semantics v1

Status: normative target contract; deterministic tokenization, small-state
BM25, and `MATCH` are implemented in the convergence slice; postings,
segments, phrases, facets, and scale evidence remain pending

The search engine owns documents, lexical indexes, doc values, aggregations,
and transactional search visibility. It is not an OpenSearch REST facade.

## Collections and documents

A search collection declares a stable `ObjectId`, source ownership, fields,
stored/source policy, analyzers, doc-values policy and optional vector indexes.
A document has a stable object ID and MVCC version.

Documents may be search-owned or linked explicitly to another engine object.
A linked document update participates in the originating transaction when the
link is synchronous.

## Analyzer pipeline

An analyzer definition pins:

1. UTF-8 validation;
2. Unicode normalization form;
3. tokenizer and version;
4. optional case folding;
5. stop-word set digest;
6. stemmer or token filters with versions; and
7. position and offset emission rules.

Changing any component creates a new index generation. Source text remains
unchanged.

## Inverted index

- Term dictionaries use a versioned finite-state or prefix-searchable format.
- Postings include document/object ID, term frequency, positions and optional
  offsets.
- Doc values provide typed columnar sort, filter, facet and aggregation input.
- Immutable segments are ordered by generation and creating CSN.
- Tombstones and field updates are versioned.
- A mutable transactional delta is searchable at commit; background merges do
  not define visibility.

## Query operators

V1 target operators are exact term, match, boolean, phrase, range, prefix,
fuzzy, wildcard, exists, stable-ID filter, lexical top-k, facet, metric
aggregation, highlight, vector search and hybrid fusion.

The first vertical slice is one analyzer, one text field, `MATCH`, top-k and a
stable-ID tie-breaker.

## Lexical scoring

V1 default ranking is versioned BM25F:

- field weights and `k1`/`b` are index-definition values;
- document lengths and corpus statistics bind to the snapshot/index generation;
- filter context contributes no score;
- score ordering is descending, followed by stable object ID ascending;
- explanations name every term, field statistic, parameter and contribution.

The existing deterministic lexical reference remains an oracle until this
specification receives its own golden corpus.

## Transactional visibility

The commit coordinator installs the search delta root with the same CSN as
other engine roots. A transaction can read its own indexed changes. The next
transaction observes them without refresh or CDC.

Segment merging and analyzer shadow builds preserve the logical snapshot and
publish a new generation atomically.

## Aggregations and memory

Aggregations operate on doc values or bounded typed field data. Every query
declares or inherits candidate, bucket, memory, CPU and deadline limits.
Partial results are returned only under an explicitly requested partial mode
and are labelled with the skipped/error state.

## Verification

Required evidence includes analyzer/token/position goldens, posting rebuild
equivalence, BM25F score/explanation fixtures, query-operator properties,
transactional visibility, merge interruption, facet/aggregation correctness,
bounded cancellation, corruption rejection and cross-engine stable-ID joins.
