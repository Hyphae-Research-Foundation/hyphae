# ADR-0012: Portable recovery and verifiable releases

- Status: Accepted
- Date: 2026-07-15
- Owners: Celiums Solutions LLC

## Context

Hyphae must remain recoverable with one binary and local files, while release
consumers must be able to verify what they install. Copying a live data
directory directly can mix checkpoints. Publishing an unstructured executable
without checksums, dependency inventories, signatures, or provenance makes
the release origin difficult to establish.

## Decision

A portable backup is a directory containing exactly `BACKUP.json` and one
canonical `snapshot.hysnap`. Creation holds the engine's data-directory lock,
creates a logical snapshot, copies and re-verifies it in a staging directory,
then promotes that directory. The manifest is bounded to 64 KiB and must
exactly match the verified snapshot.

Restore accepts only a new destination outside the backup directory. It
verifies the complete backup first, constructs a staging data directory,
installs the snapshot, anchors a new empty log, rebuilds the disposable Redb
index, reopens the engine, and compares the resulting checkpoint before
promoting the directory. Failed verification never activates the requested
destination.

Each supported disk format has immutable byte-for-byte fixtures. Compatibility
tests open them without a materialized index and verify both logical values and
idempotency receipts.

Release archives contain one native binary plus license and notice files.
Archive metadata is normalized to the source commit time. A tag must exactly
match the workspace version. The release workflow builds on each native
operating system and emits SHA-256 checksums, SPDX and CycloneDX SBOMs, GitHub
Actions SLSA v1 provenance predicates, and keyless Sigstore signature,
provenance, and SBOM-attestation bundles. Manual workflow runs execute the
complete build/sign/verify path but cannot publish unless they supply the
existing immutable tag and exact peeled commit for an authorized recovery. The
normal publish authority is a matching `release-vVERSION-crates` tag push. A
Python publication uses that same tag and must not create an SDK-specific tag;
its only retained manual Release authority is the exactly pinned `3.0.0`
recovery tuple.

## Consequences

- Backups are portable logical checkpoints rather than copies of mutable
  implementation files.
- Restores preserve values, sequence continuity, and durable idempotency.
- A backup destination and restore parent must be controlled by the operator;
  concurrent mutation of those paths is outside the local threat model.
- Embedded indexes are excluded from backups and rebuilt during restore.
- Release verification does not require trusting a checksum served beside an
  archive; the checksum file is signed and each archive has independently
  verified provenance and SBOM attestations.
- Repository visibility and a `0.1.0` tag remain prohibited until every release
  gate has current native CI evidence.

## Verification

Storage unit tests cover round trips, empty state, corruption, destination
safety, sequence continuity, and idempotency. The single-binary test exercises
`backup`, `backup-verify`, `restore`, and `doctor`. Compatibility fixtures are
regenerated deterministically in CI. Packaging tests compare archive bytes and
detect checksum tampering. Bounded fuzz, load, kill/restart soak, and native
release workflows provide the remaining release evidence.
