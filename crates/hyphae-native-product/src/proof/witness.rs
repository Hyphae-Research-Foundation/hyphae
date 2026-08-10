// SPDX-License-Identifier: GPL-3.0-only

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path},
};

use super::{
    codec::{
        Decoder, Encoder, HEADER_BYTES, check_encoded_limit, copy_array, decode_anchor,
        encode_anchor, read_u16, read_u64, seal_envelope, verify_envelope,
    },
    crypto::blake3,
    model::{
        MAX_NATIVE_WITNESS_BYTES, NATIVE_PROOF_VERSION, NativeDirectoryWitness, NativeProofAnchor,
        NativeProofError, NativeWitnessArtifact, NativeWitnessEntry, WitnessCodecLimits, io_error,
        limit,
    },
};

const WITNESS_MAGIC: [u8; 8] = *b"HYNWIT02";
const WITNESS_DOMAIN: &[u8] = b"hyphae-native-witness-envelope-v2";
const INVENTORY_KIND: u8 = 1;
const DIRECTORY_TAG: u8 = 1;
const FILE_TAG: u8 = 2;
const COPY_BUFFER_BYTES: usize = 64 * 1024;

/// Builds one complete, portable, single-file witness from a directory inventory.
///
/// # Errors
///
/// Rejects non-directories, symlinks, special files, unsafe/non-UTF-8 paths, changing files,
/// digest inconsistencies, and configured resource-limit exhaustion.
pub fn bundle_native_witness(
    origin: impl AsRef<Path>,
    anchor: NativeProofAnchor,
    limits: &WitnessCodecLimits,
) -> Result<NativeWitnessArtifact, NativeProofError> {
    super::codec::validate_anchor(anchor)?;
    validate_witness_limits(limits)?;
    let origin = origin.as_ref();
    let root_metadata = fs::symlink_metadata(origin).map_err(|source| io_error(origin, source))?;
    if !root_metadata.file_type().is_dir() {
        return Err(NativeProofError::OriginNotDirectory(origin.to_path_buf()));
    }
    let mut entries = Vec::new();
    let mut accounting = WitnessAccounting::default();
    collect_directory(origin, Path::new(""), &mut entries, &mut accounting, limits)?;
    entries.sort_by(|left, right| left.path().as_bytes().cmp(right.path().as_bytes()));
    validate_entries(&entries, limits)?;
    let mut witness = NativeDirectoryWitness {
        anchor,
        entries,
        witness_digest: [0; 32],
    };
    let bytes = encode_native_witness(&witness, limits)?;
    witness.witness_digest = copy_array(&bytes[32..HEADER_BYTES]);
    Ok(NativeWitnessArtifact { witness, bytes })
}

/// Encodes one complete directory witness as canonical `HYNWIT02` bytes.
///
/// # Errors
///
/// Returns an error for invalid inventory content or an exceeded resource limit.
pub fn encode_native_witness(
    witness: &NativeDirectoryWitness,
    limits: &WitnessCodecLimits,
) -> Result<Vec<u8>, NativeProofError> {
    super::codec::validate_anchor(witness.anchor)?;
    validate_witness_limits(limits)?;
    let totals = validate_entries(&witness.entries, limits)?;
    let mut payload = Encoder::default();
    encode_anchor(&mut payload, witness.anchor);
    payload.count(witness.entries.len())?;
    payload.u32(u32::try_from(totals.files).map_err(|_| NativeProofError::LengthOverflow)?);
    payload.u32(u32::try_from(totals.directories).map_err(|_| NativeProofError::LengthOverflow)?);
    payload.u64(totals.file_bytes);
    for entry in &witness.entries {
        match entry {
            NativeWitnessEntry::Directory { path } => {
                payload.byte(DIRECTORY_TAG);
                encode_path(&mut payload, path)?;
            }
            NativeWitnessEntry::File {
                path,
                digest,
                bytes,
            } => {
                payload.byte(FILE_TAG);
                encode_path(&mut payload, path)?;
                payload
                    .u64(u64::try_from(bytes.len()).map_err(|_| NativeProofError::LengthOverflow)?);
                payload.extend(digest);
                payload.extend(bytes);
            }
        }
    }
    seal_envelope(
        WITNESS_MAGIC,
        INVENTORY_KIND,
        0,
        &payload.bytes,
        limits.max_witness_bytes.min(MAX_NATIVE_WITNESS_BYTES),
        "witness bytes",
        WITNESS_DOMAIN,
    )
}

/// Decodes and fully verifies one canonical `HYNWIT02` artifact from memory.
///
/// # Errors
///
/// Rejects truncation, trailing content, unsafe or noncanonical paths, unsorted/duplicate
/// inventory entries, file-digest mismatches, envelope corruption, and configured bounds.
pub fn decode_native_witness(
    encoded: &[u8],
    limits: &WitnessCodecLimits,
) -> Result<NativeDirectoryWitness, NativeProofError> {
    validate_witness_limits(limits)?;
    check_encoded_limit(
        encoded.len(),
        limits.max_witness_bytes.min(MAX_NATIVE_WITNESS_BYTES),
        "witness bytes",
    )?;
    if encoded.len() < HEADER_BYTES {
        return Err(NativeProofError::Invalid("truncated witness header"));
    }
    if encoded[..8] != WITNESS_MAGIC {
        return Err(NativeProofError::Invalid("bad witness magic"));
    }
    let version = read_u16(&encoded[8..10]);
    if version != NATIVE_PROOF_VERSION {
        return Err(NativeProofError::UnsupportedVersion {
            found: version,
            supported: NATIVE_PROOF_VERSION,
        });
    }
    if read_u16(&encoded[10..12]) != 0
        || encoded[12] != INVENTORY_KIND
        || encoded[13..16] != [0; 3]
        || encoded[28..32] != [0; 4]
    {
        return Err(NativeProofError::Invalid("invalid witness preamble"));
    }
    let payload_length = usize::try_from(read_u64(&encoded[16..24]))
        .map_err(|_| NativeProofError::LengthOverflow)?;
    let expected_length = HEADER_BYTES
        .checked_add(payload_length)
        .ok_or(NativeProofError::LengthOverflow)?;
    if encoded.len() != expected_length {
        return Err(NativeProofError::Invalid("witness file length mismatch"));
    }
    let payload = &encoded[HEADER_BYTES..];
    verify_envelope(encoded, payload, WITNESS_DOMAIN)?;

    let mut decoder = Decoder::new(payload);
    let anchor = decode_anchor(&mut decoder)?;
    let entry_count = decoder.count(limits.max_entries, "witness entries")?;
    let declared_files = decoder.count(limits.max_files, "witness files")?;
    let declared_directories = decoder.count(limits.max_directories, "witness directories")?;
    let declared_file_bytes = decoder.u64()?;
    if declared_file_bytes > limits.max_total_file_bytes {
        return Err(limit(
            "witness total file bytes",
            declared_file_bytes,
            limits.max_total_file_bytes,
        ));
    }
    if entry_count != declared_files.saturating_add(declared_directories) {
        return Err(NativeProofError::Invalid(
            "witness inventory counts disagree",
        ));
    }
    let entries = decode_entries(&mut decoder, entry_count, limits)?;
    decoder.finish()?;
    let totals = validate_entries(&entries, limits)?;
    if totals.files != declared_files
        || totals.directories != declared_directories
        || totals.file_bytes != declared_file_bytes
    {
        return Err(NativeProofError::Invalid(
            "witness inventory totals disagree",
        ));
    }
    let witness_digest = copy_array(&encoded[32..HEADER_BYTES]);
    let witness = NativeDirectoryWitness {
        anchor,
        entries,
        witness_digest,
    };
    if encode_native_witness(&witness, limits)? != encoded {
        return Err(NativeProofError::Invalid("noncanonical witness encoding"));
    }
    Ok(witness)
}

fn decode_entries(
    decoder: &mut Decoder<'_>,
    entry_count: usize,
    limits: &WitnessCodecLimits,
) -> Result<Vec<NativeWitnessEntry>, NativeProofError> {
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(entry_count)
        .map_err(|_| NativeProofError::LengthOverflow)?;
    let mut decoded_bytes = 0_u64;
    for _ in 0..entry_count {
        let tag = decoder.byte()?;
        let path = decode_path(decoder, limits, &mut decoded_bytes)?;
        if tag == DIRECTORY_TAG {
            entries.push(NativeWitnessEntry::Directory { path });
            continue;
        }
        if tag != FILE_TAG {
            return Err(NativeProofError::Invalid("invalid witness entry tag"));
        }
        let file_length = decoder.u64()?;
        if file_length > limits.max_file_bytes {
            return Err(limit(
                "witness file bytes",
                file_length,
                limits.max_file_bytes,
            ));
        }
        decoded_bytes = decoded_bytes
            .checked_add(file_length)
            .ok_or(NativeProofError::LengthOverflow)?;
        if decoded_bytes > limits.max_decoded_bytes {
            return Err(limit(
                "witness decoded bytes",
                decoded_bytes,
                limits.max_decoded_bytes,
            ));
        }
        let digest = decoder.array()?;
        let length = usize::try_from(file_length).map_err(|_| NativeProofError::LengthOverflow)?;
        let bytes = decoder.owned(length)?;
        if blake3(&bytes) != digest {
            return Err(NativeProofError::DigestMismatch("witness file"));
        }
        entries.push(NativeWitnessEntry::File {
            path,
            digest,
            bytes,
        });
    }
    Ok(entries)
}

/// Reads and verifies one witness file with a metadata preflight and bounded streaming read.
///
/// # Errors
///
/// Returns an error for I/O, concurrent file changes, invalid encoding, or limits.
pub fn read_native_witness(
    path: impl AsRef<Path>,
    limits: &WitnessCodecLimits,
) -> Result<NativeDirectoryWitness, NativeProofError> {
    let path = path.as_ref();
    let file = File::open(path).map_err(|source| io_error(path, source))?;
    let metadata = file.metadata().map_err(|source| io_error(path, source))?;
    if !metadata.file_type().is_file() {
        return Err(NativeProofError::Invalid(
            "witness path is not a regular file",
        ));
    }
    let maximum = limits.max_witness_bytes.min(MAX_NATIVE_WITNESS_BYTES);
    if metadata.len() > maximum {
        return Err(limit("witness bytes", metadata.len(), maximum));
    }
    let capacity = usize::try_from(metadata.len()).map_err(|_| NativeProofError::LengthOverflow)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| NativeProofError::LengthOverflow)?;
    file.take(maximum.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| io_error(path, source))?;
    check_encoded_limit(bytes.len(), maximum, "witness bytes")?;
    if u64::try_from(bytes.len()).map_err(|_| NativeProofError::LengthOverflow)? != metadata.len() {
        return Err(NativeProofError::Invalid(
            "witness file changed while reading",
        ));
    }
    decode_native_witness(&bytes, limits)
}

/// Bundles an origin directory and writes one new canonical witness file without replacement.
///
/// # Errors
///
/// Returns the same failures as [`bundle_native_witness`], rejects a destination inside the
/// origin, and never replaces an existing destination.
pub fn write_native_witness(
    origin: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    anchor: NativeProofAnchor,
    limits: &WitnessCodecLimits,
) -> Result<NativeWitnessArtifact, NativeProofError> {
    let origin = origin.as_ref();
    let destination = destination.as_ref();
    reject_destination_inside_origin(origin, destination)?;
    if destination.exists() {
        return Err(NativeProofError::DestinationExists(
            destination.to_path_buf(),
        ));
    }
    let artifact = bundle_native_witness(origin, anchor, limits)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|source| {
            if source.kind() == std::io::ErrorKind::AlreadyExists {
                NativeProofError::DestinationExists(destination.to_path_buf())
            } else {
                io_error(destination, source)
            }
        })?;
    let result = file
        .write_all(&artifact.bytes)
        .and_then(|()| file.sync_all())
        .map_err(|source| io_error(destination, source));
    if result.is_err() {
        let _ignored = fs::remove_file(destination);
    }
    result?;
    Ok(artifact)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct WitnessAccounting {
    entries: usize,
    files: usize,
    directories: usize,
    file_bytes: u64,
    decoded_bytes: u64,
}

fn collect_directory(
    root: &Path,
    relative: &Path,
    entries: &mut Vec<NativeWitnessEntry>,
    accounting: &mut WitnessAccounting,
    limits: &WitnessCodecLimits,
) -> Result<(), NativeProofError> {
    let directory = if relative.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(relative)
    };
    let reader = fs::read_dir(&directory).map_err(|source| io_error(&directory, source))?;
    for item in reader {
        let item = item.map_err(|source| io_error(&directory, source))?;
        let child_relative = relative.join(item.file_name());
        let path = relative_path_string(&child_relative, limits.max_path_bytes)?;
        // LOCK is ephemeral process ownership rather than durable database authority. Windows
        // denies reopening it while the product owns the directory, so verification recreates it.
        if relative.as_os_str().is_empty() && path == "LOCK" {
            continue;
        }
        account_path(accounting, path.len(), limits)?;
        let source_path = item.path();
        let metadata =
            fs::symlink_metadata(&source_path).map_err(|source| io_error(&source_path, source))?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            return Err(NativeProofError::Invalid(
                "witness origin contains a symbolic link",
            ));
        }
        if file_type.is_dir() {
            accounting.directories = accounting
                .directories
                .checked_add(1)
                .ok_or(NativeProofError::LengthOverflow)?;
            if accounting.directories > limits.max_directories {
                return Err(limit(
                    "witness directories",
                    accounting.directories,
                    limits.max_directories,
                ));
            }
            entries.push(NativeWitnessEntry::Directory { path });
            collect_directory(root, &child_relative, entries, accounting, limits)?;
        } else if file_type.is_file() {
            accounting.files = accounting
                .files
                .checked_add(1)
                .ok_or(NativeProofError::LengthOverflow)?;
            if accounting.files > limits.max_files {
                return Err(limit("witness files", accounting.files, limits.max_files));
            }
            let (bytes, digest) = read_origin_file(&source_path, &metadata, limits)?;
            let file_bytes =
                u64::try_from(bytes.len()).map_err(|_| NativeProofError::LengthOverflow)?;
            accounting.file_bytes = accounting
                .file_bytes
                .checked_add(file_bytes)
                .ok_or(NativeProofError::LengthOverflow)?;
            accounting.decoded_bytes = accounting
                .decoded_bytes
                .checked_add(file_bytes)
                .ok_or(NativeProofError::LengthOverflow)?;
            enforce_accounting(*accounting, limits)?;
            entries.push(NativeWitnessEntry::File {
                path,
                digest,
                bytes,
            });
        } else {
            return Err(NativeProofError::Invalid(
                "witness origin contains a special file",
            ));
        }
    }
    Ok(())
}

fn read_origin_file(
    path: &Path,
    before: &fs::Metadata,
    limits: &WitnessCodecLimits,
) -> Result<(Vec<u8>, [u8; 32]), NativeProofError> {
    if before.len() > limits.max_file_bytes {
        return Err(limit(
            "witness file bytes",
            before.len(),
            limits.max_file_bytes,
        ));
    }
    let mut file = File::open(path).map_err(|source| io_error(path, source))?;
    let opened = file.metadata().map_err(|source| io_error(path, source))?;
    if !opened.file_type().is_file() || opened.len() != before.len() {
        return Err(NativeProofError::Invalid(
            "witness origin changed while bundling",
        ));
    }
    let capacity = usize::try_from(opened.len()).map_err(|_| NativeProofError::LengthOverflow)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| NativeProofError::LengthOverflow)?;
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES].into_boxed_slice();
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| io_error(path, source))?;
        if read == 0 {
            break;
        }
        let next_length = bytes
            .len()
            .checked_add(read)
            .ok_or(NativeProofError::LengthOverflow)?;
        if u64::try_from(next_length).map_err(|_| NativeProofError::LengthOverflow)?
            > limits.max_file_bytes
        {
            return Err(limit(
                "witness file bytes",
                next_length,
                limits.max_file_bytes,
            ));
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    let after = file.metadata().map_err(|source| io_error(path, source))?;
    if after.len() != opened.len()
        || u64::try_from(bytes.len()).map_err(|_| NativeProofError::LengthOverflow)? != opened.len()
    {
        return Err(NativeProofError::Invalid(
            "witness origin changed while bundling",
        ));
    }
    let digest = blake3(&bytes);
    Ok((bytes, digest))
}

fn validate_entries(
    entries: &[NativeWitnessEntry],
    limits: &WitnessCodecLimits,
) -> Result<WitnessAccounting, NativeProofError> {
    if entries.len() > limits.max_entries {
        return Err(limit("witness entries", entries.len(), limits.max_entries));
    }
    let mut accounting = WitnessAccounting::default();
    let mut prior: Option<&str> = None;
    let mut directories = std::collections::BTreeSet::new();
    for entry in entries {
        let path = entry.path();
        validate_relative_path(path, limits.max_path_bytes)?;
        if prior.is_some_and(|previous| previous.as_bytes() >= path.as_bytes()) {
            return Err(NativeProofError::Invalid(
                "witness inventory is not sorted and unique",
            ));
        }
        let mut parent = path;
        while let Some((candidate, _)) = parent.rsplit_once('/') {
            if !directories.contains(candidate) {
                return Err(NativeProofError::Invalid(
                    "witness inventory omits a parent directory",
                ));
            }
            parent = candidate;
        }
        prior = Some(path);
        account_path(&mut accounting, path.len(), limits)?;
        match entry {
            NativeWitnessEntry::Directory { .. } => {
                directories.insert(path);
                accounting.directories = accounting
                    .directories
                    .checked_add(1)
                    .ok_or(NativeProofError::LengthOverflow)?;
            }
            NativeWitnessEntry::File { digest, bytes, .. } => {
                accounting.files = accounting
                    .files
                    .checked_add(1)
                    .ok_or(NativeProofError::LengthOverflow)?;
                let length =
                    u64::try_from(bytes.len()).map_err(|_| NativeProofError::LengthOverflow)?;
                if length > limits.max_file_bytes {
                    return Err(limit("witness file bytes", length, limits.max_file_bytes));
                }
                if blake3(bytes) != *digest {
                    return Err(NativeProofError::DigestMismatch("witness file"));
                }
                accounting.file_bytes = accounting
                    .file_bytes
                    .checked_add(length)
                    .ok_or(NativeProofError::LengthOverflow)?;
                accounting.decoded_bytes = accounting
                    .decoded_bytes
                    .checked_add(length)
                    .ok_or(NativeProofError::LengthOverflow)?;
            }
        }
        enforce_accounting(accounting, limits)?;
    }
    Ok(accounting)
}

fn account_path(
    accounting: &mut WitnessAccounting,
    path_bytes: usize,
    limits: &WitnessCodecLimits,
) -> Result<(), NativeProofError> {
    accounting.entries = accounting
        .entries
        .checked_add(1)
        .ok_or(NativeProofError::LengthOverflow)?;
    accounting.decoded_bytes = accounting
        .decoded_bytes
        .checked_add(u64::try_from(path_bytes).map_err(|_| NativeProofError::LengthOverflow)?)
        .ok_or(NativeProofError::LengthOverflow)?;
    enforce_accounting(*accounting, limits)
}

fn enforce_accounting(
    accounting: WitnessAccounting,
    limits: &WitnessCodecLimits,
) -> Result<(), NativeProofError> {
    for (resource, actual, maximum) in [
        ("witness entries", accounting.entries, limits.max_entries),
        ("witness files", accounting.files, limits.max_files),
        (
            "witness directories",
            accounting.directories,
            limits.max_directories,
        ),
    ] {
        if actual > maximum {
            return Err(limit(resource, actual, maximum));
        }
    }
    if accounting.file_bytes > limits.max_total_file_bytes {
        return Err(limit(
            "witness total file bytes",
            accounting.file_bytes,
            limits.max_total_file_bytes,
        ));
    }
    if accounting.decoded_bytes > limits.max_decoded_bytes {
        return Err(limit(
            "witness decoded bytes",
            accounting.decoded_bytes,
            limits.max_decoded_bytes,
        ));
    }
    Ok(())
}

fn encode_path(encoder: &mut Encoder, path: &str) -> Result<(), NativeProofError> {
    encoder.u32(u32::try_from(path.len()).map_err(|_| NativeProofError::LengthOverflow)?);
    encoder.extend(path.as_bytes());
    Ok(())
}

fn decode_path(
    decoder: &mut Decoder<'_>,
    limits: &WitnessCodecLimits,
    decoded_bytes: &mut u64,
) -> Result<String, NativeProofError> {
    let length = usize::try_from(decoder.u32()?).map_err(|_| NativeProofError::LengthOverflow)?;
    if length > limits.max_path_bytes {
        return Err(limit("witness path bytes", length, limits.max_path_bytes));
    }
    *decoded_bytes = decoded_bytes
        .checked_add(u64::try_from(length).map_err(|_| NativeProofError::LengthOverflow)?)
        .ok_or(NativeProofError::LengthOverflow)?;
    if *decoded_bytes > limits.max_decoded_bytes {
        return Err(limit(
            "witness decoded bytes",
            *decoded_bytes,
            limits.max_decoded_bytes,
        ));
    }
    let bytes = decoder.owned(length)?;
    let path = String::from_utf8(bytes)
        .map_err(|_| NativeProofError::Invalid("witness path is not UTF-8"))?;
    validate_relative_path(&path, limits.max_path_bytes)?;
    Ok(path)
}

fn relative_path_string(relative: &Path, maximum_bytes: usize) -> Result<String, NativeProofError> {
    let mut path = String::new();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(NativeProofError::Invalid("unsafe witness path"));
        };
        let component = component
            .to_str()
            .ok_or(NativeProofError::Invalid("witness path is not UTF-8"))?;
        if !path.is_empty() {
            path.push('/');
        }
        path.push_str(component);
    }
    validate_relative_path(&path, maximum_bytes)?;
    Ok(path)
}

fn validate_relative_path(path: &str, maximum_bytes: usize) -> Result<(), NativeProofError> {
    if path.is_empty() || path.len() > maximum_bytes || path.contains('\0') || path.contains('\\') {
        return Err(NativeProofError::Invalid("unsafe witness path"));
    }
    let drive_absolute = path.as_bytes().get(1) == Some(&b':')
        && path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic);
    if path.starts_with('/')
        || drive_absolute
        || path.ends_with('/')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(NativeProofError::Invalid("unsafe witness path"));
    }
    Ok(())
}

fn validate_witness_limits(limits: &WitnessCodecLimits) -> Result<(), NativeProofError> {
    if limits.max_witness_bytes < HEADER_BYTES as u64
        || limits.max_entries == 0
        || limits.max_path_bytes == 0
        || limits.max_decoded_bytes == 0
    {
        return Err(NativeProofError::Invalid("invalid witness codec limits"));
    }
    Ok(())
}

fn reject_destination_inside_origin(
    origin: &Path,
    destination: &Path,
) -> Result<(), NativeProofError> {
    let origin = fs::canonicalize(origin).map_err(|source| io_error(origin, source))?;
    let parent = destination.parent().ok_or(NativeProofError::Invalid(
        "witness destination has no parent",
    ))?;
    let parent = fs::canonicalize(parent).map_err(|source| io_error(parent, source))?;
    let file_name = destination.file_name().ok_or(NativeProofError::Invalid(
        "witness destination has no file name",
    ))?;
    let canonical_destination = parent.join(file_name);
    if canonical_destination.starts_with(&origin) {
        return Err(NativeProofError::Invalid(
            "witness destination is inside its origin",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        NativeDirectoryWitness, NativeProofAnchor, NativeProofError, NativeWitnessEntry,
        WitnessCodecLimits, encode_native_witness,
    };

    fn witness(entries: Vec<NativeWitnessEntry>) -> NativeDirectoryWitness {
        NativeDirectoryWitness {
            anchor: NativeProofAnchor {
                directory_lineage: [1; 24],
                history_epoch: 1,
                visible_csn: 1,
                catalog_version: 1,
                root_digest: [2; 32],
                checkpoint_sequence: 1,
                checkpoint_digest: [3; 32],
            },
            entries,
            witness_digest: [0; 32],
        }
    }

    #[test]
    fn unsafe_duplicate_unsorted_and_incomplete_paths_are_rejected()
    -> Result<(), Box<dyn std::error::Error>> {
        for entries in [
            vec![NativeWitnessEntry::Directory {
                path: "../escape".to_owned(),
            }],
            vec![NativeWitnessEntry::Directory {
                path: "/absolute".to_owned(),
            }],
            vec![NativeWitnessEntry::Directory {
                path: "C:/absolute".to_owned(),
            }],
            vec![
                NativeWitnessEntry::Directory {
                    path: "same".to_owned(),
                },
                NativeWitnessEntry::Directory {
                    path: "same".to_owned(),
                },
            ],
            vec![
                NativeWitnessEntry::Directory {
                    path: "z".to_owned(),
                },
                NativeWitnessEntry::Directory {
                    path: "a".to_owned(),
                },
            ],
            vec![NativeWitnessEntry::File {
                path: "missing/parent".to_owned(),
                digest: super::blake3(b""),
                bytes: Vec::new(),
            }],
        ] {
            let Err(error) =
                encode_native_witness(&witness(entries), &WitnessCodecLimits::default())
            else {
                return Err("noncanonical path was accepted".into());
            };
            assert!(matches!(error, NativeProofError::Invalid(_)));
        }
        Ok(())
    }

    #[test]
    fn incorrect_file_digest_is_rejected_before_encoding() -> Result<(), Box<dyn std::error::Error>>
    {
        let Err(error) = encode_native_witness(
            &witness(vec![NativeWitnessEntry::File {
                path: "file".to_owned(),
                digest: [9; 32],
                bytes: b"content".to_vec(),
            }]),
            &WitnessCodecLimits::default(),
        ) else {
            return Err("incorrect file digest was accepted".into());
        };
        assert!(matches!(error, NativeProofError::DigestMismatch(_)));
        Ok(())
    }
}
