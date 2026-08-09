// SPDX-License-Identifier: Apache-2.0

//! Independent, bounded verifier for the public native backup envelope.
//!
//! This binary intentionally does not depend on `hyphae-native-runtime` or
//! `hyphae-native-product`. It provides a second implementation for G8 restore
//! evidence instead of asking the producer to verify its own output.

use std::{
    collections::BTreeSet,
    env, fs,
    io::{self, Read},
    path::{Component, Path},
};

use serde::{Deserialize, Serialize};

const MANIFEST_NAME: &str = "NATIVE_BACKUP.json";
const DATA_NAME: &str = "data";
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_FILES: usize = 16_384;
const MAX_DIRECTORIES: usize = 16_384;
const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4_096;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    kind: String,
    version: u16,
    visible_csn: u64,
    checkpoint_digest: String,
    file_count: usize,
    directory_count: usize,
    total_bytes: u64,
    directories: Vec<String>,
    files: Vec<FileEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FileEntry {
    path: String,
    size: u64,
    blake3: String,
}

#[derive(Debug, Serialize)]
struct Receipt {
    schema: &'static str,
    status: &'static str,
    verifier: &'static str,
    visible_csn: u64,
    checkpoint_digest: String,
    file_count: usize,
    directory_count: usize,
    total_bytes: u64,
}

fn main() {
    let arguments = env::args_os().skip(1).collect::<Vec<_>>();
    if let Err(error) = run(&arguments) {
        eprintln!("independent native backup verification failed: {error}");
        std::process::exit(1);
    }
}

fn run(arguments: &[std::ffi::OsString]) -> Result<(), String> {
    if arguments.len() != 1 {
        return Err("usage: verify_native_backup <backup-directory>".to_owned());
    }
    let receipt = verify(Path::new(&arguments[0]))?;
    println!(
        "{}",
        serde_json::to_string(&receipt).map_err(|error| error.to_string())?
    );
    Ok(())
}

fn verify(root: &Path) -> Result<Receipt, String> {
    require_directory(root, "backup root")?;
    let root_entries = entry_names(root)?;
    if root_entries != BTreeSet::from([MANIFEST_NAME.to_owned(), DATA_NAME.to_owned()]) {
        return Err("backup root must contain only NATIVE_BACKUP.json and data".to_owned());
    }

    let manifest_path = root.join(MANIFEST_NAME);
    let metadata = fs::symlink_metadata(&manifest_path).map_err(display_io)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("backup manifest must be a regular file".to_owned());
    }
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err("backup manifest exceeds the verifier bound".to_owned());
    }
    let bytes = fs::read(&manifest_path).map_err(display_io)?;
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    validate_manifest(&manifest)?;

    let data = root.join(DATA_NAME);
    require_directory(&data, "backup data root")?;
    let mut directories = Vec::new();
    let mut files = Vec::new();
    let mut total_bytes = 0_u64;
    collect(&data, &data, &mut directories, &mut files, &mut total_bytes)?;
    directories.sort();
    files.sort_by(|left, right| left.0.cmp(&right.0));

    if directories != manifest.directories
        || files.len() != manifest.file_count
        || directories.len() != manifest.directory_count
        || total_bytes != manifest.total_bytes
    {
        return Err("backup inventory totals do not match the manifest".to_owned());
    }
    for ((path, size, digest), expected) in files.iter().zip(&manifest.files) {
        if path != &expected.path || size != &expected.size || digest != &expected.blake3 {
            return Err(format!("backup file differs from manifest: {path}"));
        }
    }

    Ok(Receipt {
        schema: "hyphae-independent-backup-verification-v1",
        status: "passed",
        verifier: "independent-envelope-v1",
        visible_csn: manifest.visible_csn,
        checkpoint_digest: manifest.checkpoint_digest,
        file_count: manifest.file_count,
        directory_count: manifest.directory_count,
        total_bytes: manifest.total_bytes,
    })
}

fn validate_manifest(manifest: &Manifest) -> Result<(), String> {
    if manifest.kind != "hyphae-native-directory-backup"
        || manifest.version != 1
        || manifest.visible_csn == 0
        || !is_digest(&manifest.checkpoint_digest)
    {
        return Err("backup identity or checkpoint is invalid".to_owned());
    }
    if manifest.file_count != manifest.files.len()
        || manifest.directory_count != manifest.directories.len()
        || manifest.file_count > MAX_FILES
        || manifest.directory_count > MAX_DIRECTORIES
        || manifest.total_bytes > MAX_TOTAL_BYTES
    {
        return Err("backup manifest exceeds bounds or has inconsistent totals".to_owned());
    }
    require_sorted_unique_paths(&manifest.directories)?;
    let file_paths: Vec<String> = manifest
        .files
        .iter()
        .map(|entry| entry.path.clone())
        .collect();
    require_sorted_unique_paths(&file_paths)?;
    let mut total = 0_u64;
    for file in &manifest.files {
        if !is_digest(&file.blake3) {
            return Err(format!("noncanonical BLAKE3 digest for {}", file.path));
        }
        total = total
            .checked_add(file.size)
            .ok_or_else(|| "backup manifest byte total overflowed".to_owned())?;
    }
    if total != manifest.total_bytes {
        return Err("backup manifest byte total is inconsistent".to_owned());
    }
    Ok(())
}

fn require_sorted_unique_paths(paths: &[String]) -> Result<(), String> {
    let mut previous: Option<&str> = None;
    for path in paths {
        validate_relative(Path::new(path))?;
        if path.contains('\\') || previous.is_some_and(|value| value >= path.as_str()) {
            return Err(format!("noncanonical or unsorted backup path: {path}"));
        }
        previous = Some(path);
    }
    Ok(())
}

fn collect(
    root: &Path,
    directory: &Path,
    directories: &mut Vec<String>,
    files: &mut Vec<(String, u64, String)>,
    total_bytes: &mut u64,
) -> Result<(), String> {
    for entry in fs::read_dir(directory).map_err(display_io)? {
        let entry = entry.map_err(display_io)?;
        let path = entry.path();
        let kind = entry.file_type().map_err(display_io)?;
        if kind.is_symlink() {
            return Err(format!("symlink is forbidden: {}", path.display()));
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| "backup path escaped the data root".to_owned())?;
        validate_relative(relative)?;
        let canonical = slash_path(relative)?;
        if kind.is_dir() {
            if directories.len() >= MAX_DIRECTORIES {
                return Err("backup directory count exceeds the verifier bound".to_owned());
            }
            directories.push(canonical);
            collect(root, &path, directories, files, total_bytes)?;
        } else if kind.is_file() {
            if files.len() >= MAX_FILES {
                return Err("backup file count exceeds the verifier bound".to_owned());
            }
            let (size, digest) = hash_file(&path)?;
            *total_bytes = total_bytes
                .checked_add(size)
                .ok_or_else(|| "backup byte total overflowed".to_owned())?;
            if *total_bytes > MAX_TOTAL_BYTES {
                return Err("backup bytes exceed the verifier bound".to_owned());
            }
            files.push((canonical, size, digest));
        } else {
            return Err(format!("special entry is forbidden: {}", path.display()));
        }
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<(u64, String), String> {
    let mut input = fs::File::open(path).map_err(display_io)?;
    let mut hasher = blake3::Hasher::new();
    let mut total = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let count = input.read(&mut buffer).map_err(display_io)?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .ok_or_else(|| "backup file length overflowed".to_owned())?;
        if total > MAX_TOTAL_BYTES {
            return Err("backup file exceeds the verifier byte bound".to_owned());
        }
        hasher.update(&buffer[..count]);
    }
    Ok((total, hasher.finalize().to_hex().to_string()))
}

fn require_directory(path: &Path, label: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(display_io)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!("{label} must be a real directory"));
    }
    Ok(())
}

fn entry_names(path: &Path) -> Result<BTreeSet<String>, String> {
    fs::read_dir(path)
        .map_err(display_io)?
        .map(|entry| {
            entry
                .map_err(display_io)?
                .file_name()
                .into_string()
                .map_err(|_| "backup root entry is not UTF-8".to_owned())
        })
        .collect()
}

fn validate_relative(path: &Path) -> Result<(), String> {
    let encoded = path
        .to_str()
        .ok_or_else(|| "backup path is not UTF-8".to_owned())?;
    if encoded.is_empty() || encoded.len() > MAX_PATH_BYTES {
        return Err("backup path is empty or exceeds the verifier bound".to_owned());
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "backup path is not relative and canonical: {encoded}"
        ));
    }
    Ok(())
}

fn slash_path(path: &Path) -> Result<String, String> {
    let parts: Result<Vec<_>, _> = path
        .components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| "backup path component is not UTF-8".to_owned()),
            _ => Err("backup path is not canonical".to_owned()),
        })
        .collect();
    Ok(parts?.join("/"))
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn display_io(error: io::Error) -> String {
    Box::new(error).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temporary(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "hyphae-independent-backup-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        ))
    }

    #[test]
    fn digest_validation_is_lowercase_and_exact() {
        assert!(is_digest(&"a".repeat(64)));
        assert!(!is_digest(&"A".repeat(64)));
        assert!(!is_digest(&"a".repeat(63)));
        assert!(!is_digest(&"g".repeat(64)));
    }

    #[test]
    fn manifest_rejects_unsorted_paths() {
        let manifest = Manifest {
            kind: "hyphae-native-directory-backup".to_owned(),
            version: 1,
            visible_csn: 1,
            checkpoint_digest: "a".repeat(64),
            file_count: 2,
            directory_count: 0,
            total_bytes: 0,
            directories: Vec::new(),
            files: vec![
                FileEntry {
                    path: "z".to_owned(),
                    size: 0,
                    blake3: "b".repeat(64),
                },
                FileEntry {
                    path: "a".to_owned(),
                    size: 0,
                    blake3: "c".repeat(64),
                },
            ],
        };
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn complete_backup_is_verified_and_tampering_is_rejected() -> Result<(), String> {
        let root = temporary("round-trip");
        let data = root.join(DATA_NAME);
        fs::create_dir_all(data.join("nested")).map_err(display_io)?;
        fs::write(data.join("FORMAT"), b"format bytes").map_err(display_io)?;
        fs::write(data.join("nested/page"), b"page bytes").map_err(display_io)?;
        let mut files = Vec::new();
        for path in ["FORMAT", "nested/page"] {
            let (size, digest) = hash_file(&data.join(path))?;
            files.push(FileEntry {
                path: path.to_owned(),
                size,
                blake3: digest,
            });
        }
        let manifest = Manifest {
            kind: "hyphae-native-directory-backup".to_owned(),
            version: 1,
            visible_csn: 7,
            checkpoint_digest: "a".repeat(64),
            file_count: files.len(),
            directory_count: 1,
            total_bytes: files.iter().map(|file| file.size).sum(),
            directories: vec!["nested".to_owned()],
            files,
        };
        fs::write(
            root.join(MANIFEST_NAME),
            serde_json::to_vec(&manifest).map_err(|error| error.to_string())?,
        )
        .map_err(display_io)?;

        let receipt = verify(&root)?;
        assert_eq!(receipt.file_count, 2);
        fs::write(data.join("nested/page"), b"tampered").map_err(display_io)?;
        assert!(verify(&root).is_err());
        fs::remove_dir_all(root).map_err(display_io)?;
        Ok(())
    }
}
