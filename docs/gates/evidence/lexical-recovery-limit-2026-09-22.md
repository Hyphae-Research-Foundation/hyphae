<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->
# Lexical recovery limit: synthetic source-bound reproduction

Status: source-bound repair and the tested 250,000-document short-text
rung verified before publication. The repair is included in the later 4.0.0
tagged source, but the measurements below remain bound to their stated source
commits. This receipt uses only synthetic text; it does not identify the sole
cause of any external incident.

## Candidate before the repair

- Source: `6ed65d2b82388dd8eac26e3d1933c6c5f8d77168`.
- Platform: H100 Ubuntu lane, CPU execution for this test; Rust 1.90.0.
- Data: one vectorless product collection, lowercase analyzer, strict commit
  durability, documents of 2,957 UTF-8 bytes made from a fixed sentence and
  145 synthetic `kinase000` through `kinase144` terms repeated twice. No
  customer text, embedding service, or external database was used.
- Load: 128-document ingest batches, then progressively smaller batches to
  find the first failing document. Every accepted batch carried a strict
  commit receipt. The final accepted state had 612 live documents, CSN 13,
  commit LSN 2,635,433, 93,790 physical lexical entries, 7,610,521 encoded
  lexical key/value bytes, and a conservative retained-memory charge of
  67,010,852 bytes.
- First single-document failure: document 613 crossed the 67,108,864-byte
  recovery budget at entry 93,927, after 7,621,938 encoded bytes, with a
  retained charge of 67,108,993 bytes (129 above budget). These physical
  counts describe the traversal prefix at rejection, not every entry in the
  rejected candidate root. The product returned `corruption`, category
  `corruption`, despite an intact prior root. No commit receipt was returned;
  the snapshot identity was unchanged. Reopening recovered CSN 13 and
  `hyphae doctor` reported `snapshot_verified=true`, `verified_open=true`,
  `status=healthy`.

The retained-memory charge fired before the 131,072 physical-entry or 64 MiB
encoded-byte visitor limits. The published `release-v3.0.0-crates` tag lacks
this borrowed-visitor recovery path. The later `6655296a` checkout contains
it while still identifying its workspace as 3.0.0. Those source states must
not be reported as the same release.

## Repaired source and verification

The lexical validator and typed admission repair were committed as
`76ceb660a5e5cbc69f2dd1ebd08fedfb608dbf01`. A subsequent review found
that explicit lexical compaction and initial ANN bulk publication still used
the complete posting projection. Their large-root load now uses the same
bounded validator and document-only state in
`8b9b214e6db3c076a9c07a53ad3257b01ec1dcd1`. These are candidate code
commits, not a published release identity.

- In a repaired working-tree reproduction, a strict commit of synthetic
  document 613 into the previously rejected directory returned CSN 14. The
  first open was killed; the second independently recovered CSN 14 and the
  same root digest. This is a crash/restart observation on a precursor of the
  committed repair, not a claim about any external deployment.
- On the clean `76ceb660` CLI, the 613-document directory passed `doctor`.
  A native backup at CSN 14 contained 15 files and 65,345,690 bytes, with
  checkpoint digest
  `161f44174b1cf400c86bcc012913b58d461d6027a122901cde02a70626f68914`.
  `backup verify` accepted it, `restore` reported a healthy directory, and a
  separate `doctor` on that restore passed. Backup create, verify, restore,
  and restored doctor took 11.13, 0.02, 16.46, and 10.83 seconds on the
  shared H100; these are diagnostic times, not latency claims.
- The strict scientific-corpus regression committed 640 documents of about
  3 KB each in five batches. On the clean `8b9b214e` code it reopened,
  returned 640 documents for a synthetic kinase term (16 page hits), then
  executed a no-op lexical compaction with the same snapshot identity. The
  same test added to the preceding `76ceb660` code failed at compaction with
  `limit_exceeded`, configured 67,108,864 and observed 67,109,280 bytes.
  The later code passed it. A separate short-title control committed and
  reopened 1,024 documents and returned 1,024 documents for its term on
  `8b9b214e`.
- A runtime test lowers the document-state budget only for its own test
  thread, then attempts a second strict document commit. It checks the exact
  typed limit and observed excess, proves WAL length and the committed root
  did not change, reopens the directory, and confirms the first document is
  searchable while the rejected second document is absent. Production keeps
  the 67,108,864-byte budget; the injection exercises its admission path
  without requiring a 64 MiB test fixture on every CI run. The clean
  `8b9b214e` runtime library passed 758 tests under the Rust 1.96.0 workspace
  all-features run. The full run passed 2,011 tests across 144 suite summaries,
  with one G6 hosted-toolchain test intentionally ignored. Its log has SHA-256
  `e51f9a05be6ea2a74a82ddbc92ab0350e98324eaff20ed71bce48c90048f5ba9`.
- A diagnostic 250,000-document load from the repaired working-tree producer
  used deterministic short text, one vectorless collection with doc values,
  and group durability. With no maintenance it reached CSN 983 and left 983
  unretired WAL commits. Its four in-process query scenarios each completed
  16 rounds and returned ten hits, but it was not reopened.
- A second load from that producer reached exactly 250,000 documents with
  four intermediate vacuum/checkpoint/retention rounds and one final vacuum
  plus checkpoint/retention. The final directory was about 232 MB with no
  unretired WAL bytes. Its BM25, filtered/faceted, phrase, and fuzzy ladders
  each completed 16 rounds with ten hits. An independent `hyphae doctor`
  process built from clean `76ceb660` validated this directory as
  `snapshot_verified=true`, `verified_open=true`, and `status=healthy` in
  312.26 seconds. The first cold BM25 query in the producer took much longer
  than later queries. These shared-host times are diagnostic, not a public
  performance comparison. A separate clean `8b9b214e` CLI process reopened
  that maintained directory and returned one MatchAll hit with
  `total_documents=250000` at CSN 988. Its root digest was
  `eaa2104b540abb2b01ed55a60355553ad7056ec03d49b9c0ae0846ba23bfc874`.
  The cold open and count took 339.79 seconds on the shared H100; this is a
  diagnostic time, not a public latency claim.
- An independently seeded run using the clean `8b9b214e` example binary
  (SHA-256 `5068c8a30c0fc8de7b6c8d64097a15166438662d0c495c7c81aea425e56bd224`)
  ingested exactly 250,000 documents without maintenance. It ended at CSN 983
  with 983 unretired WAL commits. Each of its four 16-round query scenarios
  returned ten hits. The BM25 ladder included one cold 185.5-second query;
  these shared-host timings are diagnostic. This transient directory was not
  reopened. Its retained run log has SHA-256
  `b08f0096f5b980b2f947fccc27b3610fa67e73fb535edc731cd15c1071c57288`.
- A separate clean `8b9b214e` producer ingested 250,000 short-text
  documents with group durability and four intermediate
  vacuum/checkpoint/retention rounds (`HYPHAE_SCALE_MAINTENANCE_EVERY=200`).
  Its final vacuum applied, followed by checkpoint and WAL retention. The
  resulting directory was about 232 MB with a zero-byte WAL. Its BM25,
  filtered/faceted, phrase, and fuzzy ladders each ran 16 rounds and returned
  ten hits. One cold BM25 query took 161.2 seconds on the shared H100; this
  is diagnostic, not a product latency claim. The retained log has SHA-256
  `4a1c0c2e31421bd1c5038b38cff38c217f036364b4651cb0d122314ea1b876d1`.
  A separate clean `8b9b214e` CLI (SHA-256
  `879fcf825aaf0faeed20b09b939b4eecf4e77581fe78a2b7a868f4f08426736c`)
  reopened this exact directory and returned `doctor` with
  `snapshot_verified=true`, `verified_open=true`, and `status=healthy` in
  309.68 seconds. Its JSON has SHA-256
  `faa3395667a0836a00ee1a07d0984e2c8b93adb3582ec151743f3b3c502daa58`.
  Another independent clean `8b9b214e` CLI process reopened the directory
  and returned one MatchAll hit with `total_documents=250000` at CSN 988. Its
  root digest was
  `8278c9f3e29e34d66afd5e243993ae15ffe73d856a4b0de3342c8ea81d4bba0c`.
  This cold open and count took 309.59 seconds on the shared H100, a
  diagnostic time rather than a product latency claim. Its JSON has SHA-256
  `0ff82e6aade5d0459d7849f9a8cef1177baed2d3caf43aded129b96d5d1297c3`.

The source change preserves the product's 250,000-document count ceiling,
subject to a separate 64 MiB document-state admission budget, request limits,
and the M05 shared lexical/ANN budget. It does not claim that every possible
250,000-document text or vector shape fits those additional limits.

## Candidate and release boundary

The source-bound reproduction, strict scientific corpus, verified
backup/restore, and clean `8b9b214e` short-text scale with independent reopen
and count pass. At the time of this reproduction, the draft PR still needed
package and hosted receipts bound to its final head. The later annotated 4.0.0 tag targets
`7cbcf97d165beeb08aba27ff21203fa468f45bec`, which contains both repair
commits. Its separate signed Release run `35912595431` and exact-SHA G8 closure
run `35912994589` passed. Those release gates do not turn the measurements here
into tag-bound performance results or prove a unique cause for an external
incident.
