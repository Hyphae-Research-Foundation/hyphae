// SPDX-License-Identifier: GPL-3.0-only

mod frame;

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{self, Seek, SeekFrom, Write},
    marker::PhantomData,
    path::Path,
};

use hyphae_core::{DISK_FORMAT_VERSION, MIN_DISK_FORMAT_VERSION};
use thiserror::Error;
use uuid::Uuid;

use self::frame::{
    Frame, FrameKind, HEADER_LENGTH, MAX_PAYLOAD_LENGTH, ReadStatus, payload_length,
    read_exact_or_tail,
};
use crate::{
    RecoveryLimits, StorageLimitError,
    limits::{OperationDeadline, limit_io_error},
};

const DESCRIPTOR_LENGTH: usize = 36;
const TRANSACTION_DOMAIN: &[u8] = b"hyphae-transaction-v1";
pub(crate) const MAX_OPERATION_BYTES: usize = MAX_PAYLOAD_LENGTH;

/// Failure while opening, validating, or appending to a durable log.
#[derive(Debug, Error)]
pub enum LogError {
    /// A filesystem operation failed.
    #[error(transparent)]
    Io(#[from] io::Error),

    /// A frame does not start with the Hyphae magic bytes.
    #[error("invalid frame magic at byte offset {offset}")]
    BadMagic {
        /// Frame offset.
        offset: u64,
    },

    /// A frame uses a disk version this binary cannot decode.
    #[error(
        "unsupported log version {found} at byte offset {offset}; supported version is {supported}"
    )]
    UnsupportedVersion {
        /// Frame offset.
        offset: u64,
        /// Version found on disk.
        found: u16,
        /// Version understood by this binary.
        supported: u16,
    },

    /// A frame kind is not part of this format version.
    #[error("unknown frame kind {kind} at byte offset {offset}")]
    UnknownFrameKind {
        /// Frame offset.
        offset: u64,
        /// Raw kind byte.
        kind: u8,
    },

    /// Reserved frame flags are nonzero.
    #[error("unsupported frame flags {flags:#04x} at byte offset {offset}")]
    UnsupportedFlags {
        /// Frame offset.
        offset: u64,
        /// Raw flag byte.
        flags: u8,
    },

    /// A payload exceeds the per-frame allocation limit.
    #[error("frame payload is {length} bytes; maximum is {maximum}")]
    PayloadTooLarge {
        /// Requested or decoded length.
        length: usize,
        /// Configured maximum.
        maximum: usize,
    },

    /// A frame sequence is not exactly the previous sequence plus one.
    #[error("invalid sequence at byte offset {offset}: expected {expected}, found {found}")]
    InvalidSequence {
        /// Frame offset.
        offset: u64,
        /// Expected sequence.
        expected: u64,
        /// Sequence found.
        found: u64,
    },

    /// The digest chain does not connect to the prior frame.
    #[error("previous-frame digest mismatch at sequence {sequence}")]
    PreviousDigestMismatch {
        /// Invalid frame sequence.
        sequence: u64,
    },

    /// The CRC32C integrity check failed.
    #[error("CRC32C mismatch at sequence {sequence}")]
    ChecksumMismatch {
        /// Invalid frame sequence.
        sequence: u64,
    },

    /// The BLAKE3 frame digest failed.
    #[error("BLAKE3 digest mismatch at sequence {sequence}")]
    DigestMismatch {
        /// Invalid frame sequence.
        sequence: u64,
    },

    /// A transaction descriptor has the wrong length or contents.
    #[error("malformed transaction descriptor at sequence {sequence}")]
    MalformedTransaction {
        /// Invalid frame sequence.
        sequence: u64,
    },

    /// An operation or commit appeared without its matching begin frame.
    #[error("{kind} frame at sequence {sequence} has no matching transaction begin")]
    TransactionBoundary {
        /// Frame kind being validated.
        kind: &'static str,
        /// Invalid frame sequence.
        sequence: u64,
    },

    /// The committed operation count or digest differs from its descriptor.
    #[error("transaction content mismatch at commit sequence {sequence}")]
    TransactionContentMismatch {
        /// Invalid commit sequence.
        sequence: u64,
    },

    /// A transaction identifier was reused for different contents.
    #[error(
        "transaction identifier {transaction_id} was already committed with different contents"
    )]
    IdempotencyConflict {
        /// Reused identifier.
        transaction_id: Uuid,
    },

    /// A transaction cannot be committed without at least one operation.
    #[error("a transaction must contain at least one operation")]
    EmptyTransaction,

    /// The operation count cannot be represented by the disk format.
    #[error("transaction has too many operations")]
    TooManyOperations,

    /// The sequence space has been exhausted.
    #[error("log sequence space is exhausted")]
    SequenceExhausted,

    /// A segment base sequence and digest do not form a canonical anchor.
    #[error("invalid log segment anchor")]
    InvalidAnchor,

    /// The writer observed an uncertain I/O result and must be reopened.
    #[error("durable log writer is poisoned; reopen it before writing again")]
    Poisoned,
}

impl From<StorageLimitError> for LogError {
    fn from(source: StorageLimitError) -> Self {
        Self::Io(limit_io_error(source))
    }
}

/// Durable identity of a committed transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommitReceipt {
    /// Caller-supplied idempotency key.
    pub transaction_id: Uuid,
    /// Sequence of the durable commit frame.
    pub commit_sequence: u64,
    /// Digest of the commit frame and its chain prefix.
    pub commit_digest: [u8; 32],
    /// Digest of the canonical operation list.
    pub transaction_digest: [u8; 32],
}

/// Result of an idempotent append request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppendOutcome {
    /// New frames were written and synchronized.
    Committed(CommitReceipt),
    /// The exact transaction was already durable; no frames were appended.
    Existing(CommitReceipt),
}

/// A committed transaction reconstructed from the verified log.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveredTransaction {
    /// Durable commit identity.
    pub receipt: CommitReceipt,
    /// Opaque operation payloads in original order.
    pub operations: Vec<Vec<u8>>,
}

/// Evidence produced while opening and validating a segment.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RecoveryReport {
    /// Sequence immediately preceding this segment, or zero for the first segment.
    pub base_sequence: u64,
    /// Digest immediately preceding this segment, or all zero for the first segment.
    pub base_digest: [u8; 32],
    /// Unique committed transactions in commit order.
    pub transactions: Vec<RecoveredTransaction>,
    /// Complete but uncommitted transaction attempts ignored during recovery.
    pub ignored_uncommitted_transactions: u64,
    /// Repeated commits with the same id and content, deduplicated during replay.
    pub duplicate_commits: u64,
    /// Incomplete bytes removed from the physical tail.
    pub truncated_tail_bytes: u64,
    /// Length after any incomplete tail was removed.
    pub valid_bytes: u64,
    /// Last complete frame sequence, including uncommitted attempts.
    pub last_sequence: u64,
    /// Digest of the last complete frame.
    pub last_digest: [u8; 32],
}

/// A newly opened writer together with its recovery evidence.
#[derive(Debug)]
pub struct OpenedLog<'directory> {
    /// Exclusive writer handle.
    pub log: DurableLog,
    /// Verified replay and tail-repair report.
    pub recovery: RecoveryReport,
    directory_lock: PhantomData<&'directory crate::DataDirectory>,
}

impl OpenedLog<'_> {
    pub(crate) fn new(log: DurableLog, recovery: RecoveryReport) -> Self {
        Self {
            log,
            recovery,
            directory_lock: PhantomData,
        }
    }
}

/// Append-only transaction log with synchronous commit durability.
#[derive(Debug)]
pub struct DurableLog {
    file: File,
    disk_format_version: u16,
    max_file_bytes: u64,
    max_frames: u64,
    max_transactions: u64,
    max_operations: u64,
    max_decoded_operation_bytes: u64,
    frame_count: u64,
    transaction_count: u64,
    operation_count: u64,
    decoded_operation_bytes: u64,
    next_sequence: u64,
    previous_digest: [u8; 32],
    committed: HashMap<Uuid, CommitReceipt>,
    poisoned: bool,
    #[cfg(test)]
    fail_next_sync: bool,
}

impl DurableLog {
    /// Opens, verifies, and repairs only an incomplete physical tail.
    ///
    /// Full frames with invalid checksums, digests, versions, sequences, or
    /// transaction boundaries are rejected as corruption and never truncated.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O failures or any complete invalid frame.
    #[cfg(test)]
    pub(crate) fn open_file(
        path: impl AsRef<Path>,
    ) -> Result<(DurableLog, RecoveryReport), LogError> {
        Self::open_file_at(path, 0, [0; 32])
    }

    #[cfg(test)]
    pub(crate) fn open_file_at(
        path: impl AsRef<Path>,
        base_sequence: u64,
        base_digest: [u8; 32],
    ) -> Result<(DurableLog, RecoveryReport), LogError> {
        Self::open_file_at_version(path, base_sequence, base_digest, DISK_FORMAT_VERSION)
    }

    pub(crate) fn open_file_at_version(
        path: impl AsRef<Path>,
        base_sequence: u64,
        base_digest: [u8; 32],
        disk_format_version: u16,
    ) -> Result<(DurableLog, RecoveryReport), LogError> {
        let limits = RecoveryLimits::default();
        let deadline = OperationDeadline::new(limits.timeout);
        Self::open_file_at_version_with_limits(
            path,
            base_sequence,
            base_digest,
            disk_format_version,
            &limits,
            &deadline,
        )
    }

    pub(crate) fn open_file_at_version_with_limits(
        path: impl AsRef<Path>,
        base_sequence: u64,
        base_digest: [u8; 32],
        disk_format_version: u16,
        limits: &RecoveryLimits,
        deadline: &OperationDeadline,
    ) -> Result<(DurableLog, RecoveryReport), LogError> {
        deadline.check()?;
        if (base_sequence == 0) != (base_digest == [0; 32]) {
            return Err(LogError::InvalidAnchor);
        }
        if !(MIN_DISK_FORMAT_VERSION..=DISK_FORMAT_VERSION).contains(&disk_format_version) {
            return Err(LogError::UnsupportedVersion {
                offset: 0,
                found: disk_format_version,
                supported: DISK_FORMAT_VERSION,
            });
        }
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let existed = path.exists();
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        if !existed {
            file.sync_all()?;
            #[cfg(unix)]
            if let Some(parent) = path.parent() {
                File::open(parent)?.sync_all()?;
            }
        }
        let physical_length = file.metadata()?.len();
        if physical_length > limits.max_log_file_bytes {
            return Err(StorageLimitError::LogFileBytesExceeded {
                actual: physical_length,
                maximum: limits.max_log_file_bytes,
            }
            .into());
        }
        let scanned = scan(
            &mut file,
            physical_length,
            base_sequence,
            base_digest,
            disk_format_version,
            limits,
            deadline,
        )?;
        deadline.check()?;
        ensure_file_length_unchanged(&file, physical_length)?;
        if physical_length != scanned.report.valid_bytes {
            file.set_len(scanned.report.valid_bytes)?;
            file.sync_data()?;
        }
        file.seek(SeekFrom::End(0))?;

        let committed = scanned
            .report
            .transactions
            .iter()
            .map(|transaction| (transaction.receipt.transaction_id, transaction.receipt))
            .collect();
        let next_sequence = scanned
            .report
            .last_sequence
            .checked_add(1)
            .ok_or(LogError::SequenceExhausted)?;
        let log = Self {
            file,
            disk_format_version,
            max_file_bytes: limits.max_log_file_bytes,
            max_frames: limits.max_log_frames,
            max_transactions: limits.max_transactions,
            max_operations: limits.max_operations,
            max_decoded_operation_bytes: limits.max_decoded_operation_bytes,
            frame_count: scanned.frame_count,
            transaction_count: scanned.transaction_count,
            operation_count: scanned.operation_count,
            decoded_operation_bytes: scanned.decoded_operation_bytes,
            next_sequence,
            previous_digest: scanned.report.last_digest,
            committed,
            poisoned: false,
            #[cfg(test)]
            fail_next_sync: false,
        };
        Ok((log, scanned.report))
    }

    /// Appends and synchronizes one atomic transaction.
    ///
    /// Retrying the same identifier and operation bytes returns the original
    /// receipt without appending. Reusing it with different bytes fails.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid bounds, conflicting idempotency keys, or
    /// filesystem failures. Any append/sync failure poisons the writer so the
    /// caller must reopen and recover before another write.
    pub fn append_transaction(
        &mut self,
        transaction_id: Uuid,
        operations: &[Vec<u8>],
    ) -> Result<AppendOutcome, LogError> {
        if self.poisoned {
            return Err(LogError::Poisoned);
        }
        if operations.is_empty() {
            return Err(LogError::EmptyTransaction);
        }
        let operation_count =
            u32::try_from(operations.len()).map_err(|_| LogError::TooManyOperations)?;
        for operation in operations {
            if operation.len() > MAX_PAYLOAD_LENGTH {
                return Err(LogError::PayloadTooLarge {
                    length: operation.len(),
                    maximum: MAX_PAYLOAD_LENGTH,
                });
            }
        }

        let transaction_digest = transaction_digest(operations, operation_count)?;
        if let Some(receipt) = self.committed.get(&transaction_id).copied() {
            return if receipt.transaction_digest == transaction_digest {
                Ok(AppendOutcome::Existing(receipt))
            } else {
                Err(LogError::IdempotencyConflict { transaction_id })
            };
        }
        let operation_delta =
            u64::try_from(operations.len()).map_err(|_| LogError::TooManyOperations)?;
        let projected_frames = self.preflight_frames(operation_delta)?;
        let projected_transactions = self.transaction_count.checked_add(1).ok_or(
            StorageLimitError::TransactionsExceeded {
                maximum: self.max_transactions,
            },
        )?;
        if projected_transactions > self.max_transactions {
            return Err(StorageLimitError::TransactionsExceeded {
                maximum: self.max_transactions,
            }
            .into());
        }
        let projected_operations = self.operation_count.checked_add(operation_delta).ok_or(
            StorageLimitError::OperationsExceeded {
                maximum: self.max_operations,
            },
        )?;
        if projected_operations > self.max_operations {
            return Err(StorageLimitError::OperationsExceeded {
                maximum: self.max_operations,
            }
            .into());
        }
        let operation_byte_delta = operations.iter().try_fold(0_u64, |total, operation| {
            let bytes = u64::try_from(operation.len()).ok()?;
            total.checked_add(bytes)
        });
        let projected_operation_bytes = operation_byte_delta
            .and_then(|delta| self.decoded_operation_bytes.checked_add(delta))
            .ok_or(StorageLimitError::DecodedOperationBytesExceeded {
                maximum: self.max_decoded_operation_bytes,
            })?;
        if projected_operation_bytes > self.max_decoded_operation_bytes {
            return Err(StorageLimitError::DecodedOperationBytesExceeded {
                maximum: self.max_decoded_operation_bytes,
            }
            .into());
        }
        let current_bytes = self.file.metadata()?.len();
        let appended_bytes = transaction_encoded_length(operations).ok_or(
            StorageLimitError::LogFileBytesExceeded {
                actual: u64::MAX,
                maximum: self.max_file_bytes,
            },
        )?;
        let projected_bytes = current_bytes.checked_add(appended_bytes).ok_or(
            StorageLimitError::LogFileBytesExceeded {
                actual: u64::MAX,
                maximum: self.max_file_bytes,
            },
        )?;
        if projected_bytes > self.max_file_bytes {
            return Err(StorageLimitError::LogFileBytesExceeded {
                actual: projected_bytes,
                maximum: self.max_file_bytes,
            }
            .into());
        }

        let descriptor = encode_descriptor(operation_count, transaction_digest);
        let append_result = self.append_new_transaction(
            transaction_id,
            operations,
            &descriptor,
            transaction_digest,
        );
        if append_result.is_err() {
            self.poisoned = true;
        } else {
            self.frame_count = projected_frames;
            self.transaction_count = projected_transactions;
            self.operation_count = projected_operations;
            self.decoded_operation_bytes = projected_operation_bytes;
        }
        append_result
    }

    fn preflight_frames(&self, operation_count: u64) -> Result<u64, LogError> {
        let frame_delta =
            operation_count
                .checked_add(2)
                .ok_or(StorageLimitError::LogFramesExceeded {
                    maximum: self.max_frames,
                })?;
        self.next_sequence
            .checked_add(frame_delta)
            .ok_or(LogError::SequenceExhausted)?;
        let projected_frames = self.frame_count.checked_add(frame_delta).ok_or(
            StorageLimitError::LogFramesExceeded {
                maximum: self.max_frames,
            },
        )?;
        if projected_frames > self.max_frames {
            return Err(StorageLimitError::LogFramesExceeded {
                maximum: self.max_frames,
            }
            .into());
        }
        Ok(projected_frames)
    }

    pub(crate) fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    pub(crate) fn set_disk_format_version(
        &mut self,
        disk_format_version: u16,
    ) -> Result<(), LogError> {
        if !(MIN_DISK_FORMAT_VERSION..=DISK_FORMAT_VERSION).contains(&disk_format_version) {
            return Err(LogError::UnsupportedVersion {
                offset: 0,
                found: disk_format_version,
                supported: DISK_FORMAT_VERSION,
            });
        }
        self.disk_format_version = disk_format_version;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn inject_sync_failure(&mut self) {
        self.fail_next_sync = true;
    }

    fn append_new_transaction(
        &mut self,
        transaction_id: Uuid,
        operations: &[Vec<u8>],
        descriptor: &[u8; DESCRIPTOR_LENGTH],
        transaction_digest: [u8; 32],
    ) -> Result<AppendOutcome, LogError> {
        self.append_frame(FrameKind::Begin, transaction_id, descriptor)?;
        for operation in operations {
            self.append_frame(FrameKind::Operation, transaction_id, operation)?;
        }
        let receipt = self.append_frame(FrameKind::Commit, transaction_id, descriptor)?;
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_sync) {
            return Err(io::Error::other("injected log sync failure").into());
        }
        self.file.sync_data()?;

        let receipt = CommitReceipt {
            transaction_id,
            commit_sequence: receipt.sequence,
            commit_digest: receipt.digest,
            transaction_digest,
        };
        self.committed.insert(transaction_id, receipt);
        Ok(AppendOutcome::Committed(receipt))
    }

    fn append_frame(
        &mut self,
        kind: FrameKind,
        transaction_id: Uuid,
        payload: &[u8],
    ) -> Result<WrittenFrame, LogError> {
        let frame = Frame {
            kind,
            sequence: self.next_sequence,
            transaction_id,
            previous_digest: self.previous_digest,
            digest: [0; 32],
            payload: payload.to_vec(),
        };
        let encoded = frame.encode(self.disk_format_version)?;
        let digest = copy_array(&encoded[80..112]);
        self.file.write_all(&encoded)?;
        let written = WrittenFrame {
            sequence: self.next_sequence,
            digest,
        };
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(LogError::SequenceExhausted)?;
        self.previous_digest = digest;
        Ok(written)
    }
}

fn transaction_encoded_length(operations: &[Vec<u8>]) -> Option<u64> {
    let header_bytes = u64::try_from(HEADER_LENGTH).ok()?;
    let descriptor_bytes = u64::try_from(DESCRIPTOR_LENGTH).ok()?;
    let mut total = header_bytes.checked_add(descriptor_bytes)?.checked_mul(2)?;
    for operation in operations {
        let operation_bytes = u64::try_from(operation.len()).ok()?;
        total = total
            .checked_add(header_bytes)?
            .checked_add(operation_bytes)?;
    }
    Some(total)
}

#[derive(Clone, Copy, Debug)]
struct WrittenFrame {
    sequence: u64,
    digest: [u8; 32],
}

#[derive(Debug)]
struct PendingTransaction {
    transaction_id: Uuid,
    operation_count: u32,
    expected_digest: [u8; 32],
    operations: Vec<Vec<u8>>,
}

struct ScanOutcome {
    report: RecoveryReport,
    frame_count: u64,
    transaction_count: u64,
    operation_count: u64,
    decoded_operation_bytes: u64,
}

#[allow(clippy::too_many_lines)]
fn scan(
    file: &mut File,
    physical_length: u64,
    base_sequence: u64,
    base_digest: [u8; 32],
    disk_format_version: u16,
    limits: &RecoveryLimits,
    deadline: &OperationDeadline,
) -> Result<ScanOutcome, LogError> {
    deadline.check()?;
    file.seek(SeekFrom::Start(0))?;
    let mut report = RecoveryReport {
        base_sequence,
        base_digest,
        last_sequence: base_sequence,
        last_digest: base_digest,
        ..RecoveryReport::default()
    };
    let mut offset = 0_u64;
    let mut expected_sequence = base_sequence
        .checked_add(1)
        .ok_or(LogError::SequenceExhausted)?;
    let mut expected_previous_digest = base_digest;
    let mut pending: Option<PendingTransaction> = None;
    let mut committed: HashMap<Uuid, CommitReceipt> = HashMap::new();
    let mut frame_count = 0_u64;
    let mut operation_count = 0_u64;
    let mut decoded_operation_bytes = 0_u64;

    loop {
        deadline.check()?;
        let remaining = physical_length
            .checked_sub(offset)
            .ok_or_else(|| io::Error::other("log scan exceeded its captured file length"))?;
        if remaining == 0 {
            break;
        }
        let header_length = u64::try_from(HEADER_LENGTH)
            .map_err(|_| io::Error::other("log header length overflow"))?;
        if remaining < header_length {
            report.truncated_tail_bytes = remaining;
            break;
        }
        let mut header = [0_u8; HEADER_LENGTH];
        match read_exact_or_tail(file, &mut header)? {
            ReadStatus::End | ReadStatus::Partial => {
                report.truncated_tail_bytes = remaining;
                break;
            }
            ReadStatus::Complete => {}
        }
        let length = payload_length(&header, offset, disk_format_version)?;
        let frame_length = header_length
            .checked_add(
                u64::try_from(length)
                    .map_err(|_| io::Error::other("log payload length overflow"))?,
            )
            .ok_or_else(|| io::Error::other("log frame length overflow"))?;
        if frame_length > remaining {
            report.truncated_tail_bytes = remaining;
            break;
        }
        let mut payload = vec![0_u8; length];
        if read_payload_or_tail(file, &mut payload, deadline)? != ReadStatus::Complete {
            report.truncated_tail_bytes = remaining;
            break;
        }
        frame_count = frame_count
            .checked_add(1)
            .ok_or(StorageLimitError::LogFramesExceeded {
                maximum: limits.max_log_frames,
            })?;
        if frame_count > limits.max_log_frames {
            return Err(StorageLimitError::LogFramesExceeded {
                maximum: limits.max_log_frames,
            }
            .into());
        }

        let frame = Frame::decode(&header, payload, offset, disk_format_version)?;
        if frame.sequence != expected_sequence {
            return Err(LogError::InvalidSequence {
                offset,
                expected: expected_sequence,
                found: frame.sequence,
            });
        }
        if frame.previous_digest != expected_previous_digest {
            return Err(LogError::PreviousDigestMismatch {
                sequence: frame.sequence,
            });
        }

        if frame.kind == FrameKind::Operation {
            operation_count =
                operation_count
                    .checked_add(1)
                    .ok_or(StorageLimitError::OperationsExceeded {
                        maximum: limits.max_operations,
                    })?;
            if operation_count > limits.max_operations {
                return Err(StorageLimitError::OperationsExceeded {
                    maximum: limits.max_operations,
                }
                .into());
            }
            let payload_bytes = u64::try_from(frame.payload.len()).map_err(|_| {
                StorageLimitError::DecodedOperationBytesExceeded {
                    maximum: limits.max_decoded_operation_bytes,
                }
            })?;
            decoded_operation_bytes = decoded_operation_bytes.checked_add(payload_bytes).ok_or(
                StorageLimitError::DecodedOperationBytesExceeded {
                    maximum: limits.max_decoded_operation_bytes,
                },
            )?;
            if decoded_operation_bytes > limits.max_decoded_operation_bytes {
                return Err(StorageLimitError::DecodedOperationBytesExceeded {
                    maximum: limits.max_decoded_operation_bytes,
                }
                .into());
            }
        }
        apply_frame(
            &frame,
            &mut pending,
            &mut committed,
            &mut report,
            limits,
            deadline,
        )?;
        offset = offset
            .checked_add(frame_length)
            .ok_or(LogError::SequenceExhausted)?;
        report.valid_bytes = offset;
        report.last_sequence = frame.sequence;
        report.last_digest = frame.digest;
        expected_sequence = expected_sequence
            .checked_add(1)
            .ok_or(LogError::SequenceExhausted)?;
        expected_previous_digest = frame.digest;
    }

    if pending.is_some() {
        report.ignored_uncommitted_transactions =
            report.ignored_uncommitted_transactions.saturating_add(1);
    }
    let transaction_count = u64::try_from(report.transactions.len()).map_err(|_| {
        StorageLimitError::TransactionsExceeded {
            maximum: limits.max_transactions,
        }
    })?;
    Ok(ScanOutcome {
        report,
        frame_count,
        transaction_count,
        operation_count,
        decoded_operation_bytes,
    })
}

fn ensure_file_length_unchanged(file: &File, expected: u64) -> io::Result<()> {
    let actual = file.metadata()?.len();
    if actual == expected {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "log changed while being scanned: expected {expected} bytes, found {actual}"
        )))
    }
}

fn read_payload_or_tail(
    file: &mut File,
    payload: &mut [u8],
    deadline: &OperationDeadline,
) -> Result<ReadStatus, LogError> {
    for chunk in payload.chunks_mut(64 * 1024) {
        deadline.check()?;
        if read_exact_or_tail(file, chunk)? != ReadStatus::Complete {
            return Ok(ReadStatus::Partial);
        }
    }
    Ok(ReadStatus::Complete)
}

fn apply_frame(
    frame: &Frame,
    pending: &mut Option<PendingTransaction>,
    committed: &mut HashMap<Uuid, CommitReceipt>,
    report: &mut RecoveryReport,
    limits: &RecoveryLimits,
    deadline: &OperationDeadline,
) -> Result<(), LogError> {
    match frame.kind {
        FrameKind::Begin => {
            if pending.is_some() {
                report.ignored_uncommitted_transactions =
                    report.ignored_uncommitted_transactions.saturating_add(1);
            }
            let (operation_count, expected_digest) =
                decode_descriptor(&frame.payload, frame.sequence)?;
            *pending = Some(PendingTransaction {
                transaction_id: frame.transaction_id,
                operation_count,
                expected_digest,
                operations: Vec::new(),
            });
            Ok(())
        }
        FrameKind::Operation => {
            let Some(current) = pending
                .as_mut()
                .filter(|current| current.transaction_id == frame.transaction_id)
            else {
                return Err(LogError::TransactionBoundary {
                    kind: "operation",
                    sequence: frame.sequence,
                });
            };
            current.operations.push(frame.payload.clone());
            Ok(())
        }
        FrameKind::Commit => {
            let Some(current) = pending
                .take()
                .filter(|current| current.transaction_id == frame.transaction_id)
            else {
                return Err(LogError::TransactionBoundary {
                    kind: "commit",
                    sequence: frame.sequence,
                });
            };
            let (operation_count, commit_digest) =
                decode_descriptor(&frame.payload, frame.sequence)?;
            let actual_count =
                u32::try_from(current.operations.len()).map_err(|_| LogError::TooManyOperations)?;
            let actual_digest = transaction_digest_with_deadline(
                &current.operations,
                actual_count,
                Some(deadline),
            )?;
            if operation_count != current.operation_count
                || commit_digest != current.expected_digest
                || actual_count != operation_count
                || actual_digest != commit_digest
            {
                return Err(LogError::TransactionContentMismatch {
                    sequence: frame.sequence,
                });
            }

            let receipt = CommitReceipt {
                transaction_id: frame.transaction_id,
                commit_sequence: frame.sequence,
                commit_digest: frame.digest,
                transaction_digest: actual_digest,
            };
            if let Some(existing) = committed.get(&frame.transaction_id) {
                if existing.transaction_digest != actual_digest {
                    return Err(LogError::IdempotencyConflict {
                        transaction_id: frame.transaction_id,
                    });
                }
                report.duplicate_commits = report.duplicate_commits.saturating_add(1);
            } else {
                let transaction_count = u64::try_from(report.transactions.len())
                    .ok()
                    .and_then(|count| count.checked_add(1))
                    .ok_or(StorageLimitError::TransactionsExceeded {
                        maximum: limits.max_transactions,
                    })?;
                if transaction_count > limits.max_transactions {
                    return Err(StorageLimitError::TransactionsExceeded {
                        maximum: limits.max_transactions,
                    }
                    .into());
                }
                committed.insert(frame.transaction_id, receipt);
                report.transactions.push(RecoveredTransaction {
                    receipt,
                    operations: current.operations,
                });
            }
            Ok(())
        }
    }
}

fn encode_descriptor(operation_count: u32, digest: [u8; 32]) -> [u8; DESCRIPTOR_LENGTH] {
    let mut descriptor = [0_u8; DESCRIPTOR_LENGTH];
    descriptor[..4].copy_from_slice(&operation_count.to_le_bytes());
    descriptor[4..].copy_from_slice(&digest);
    descriptor
}

fn decode_descriptor(payload: &[u8], sequence: u64) -> Result<(u32, [u8; 32]), LogError> {
    if payload.len() != DESCRIPTOR_LENGTH {
        return Err(LogError::MalformedTransaction { sequence });
    }
    let operation_count = u32::from_le_bytes(copy_array(&payload[..4]));
    if operation_count == 0 {
        return Err(LogError::MalformedTransaction { sequence });
    }
    let digest = copy_array(&payload[4..]);
    Ok((operation_count, digest))
}

pub(crate) fn transaction_digest(
    operations: &[Vec<u8>],
    operation_count: u32,
) -> Result<[u8; 32], LogError> {
    transaction_digest_with_deadline(operations, operation_count, None)
}

fn transaction_digest_with_deadline(
    operations: &[Vec<u8>],
    operation_count: u32,
    deadline: Option<&OperationDeadline>,
) -> Result<[u8; 32], LogError> {
    if let Some(deadline) = deadline {
        deadline.check()?;
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(TRANSACTION_DOMAIN);
    hasher.update(&u64::from(operation_count).to_le_bytes());
    for operation in operations {
        if let Some(deadline) = deadline {
            deadline.check()?;
        }
        let length = u64::try_from(operation.len()).map_err(|_| LogError::PayloadTooLarge {
            length: operation.len(),
            maximum: MAX_PAYLOAD_LENGTH,
        })?;
        hasher.update(&length.to_le_bytes());
        for chunk in operation.chunks(64 * 1024) {
            if let Some(deadline) = deadline {
                deadline.check()?;
            }
            hasher.update(chunk);
        }
    }
    Ok(*hasher.finalize().as_bytes())
}

fn copy_array<const N: usize>(source: &[u8]) -> [u8; N] {
    let mut output = [0_u8; N];
    output.copy_from_slice(source);
    output
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        fs::OpenOptions,
        io::{Read, Seek, SeekFrom, Write},
        time::Duration,
    };

    use hyphae_core::DISK_FORMAT_VERSION;
    use uuid::Uuid;

    use super::{
        AppendOutcome, DurableLog, LogError, OpenedLog, ensure_file_length_unchanged,
        frame::HEADER_LENGTH, scan, transaction_encoded_length,
    };
    use crate::{
        RecoveryLimits, StorageLimitError, limits::OperationDeadline, storage_limit_from_io,
        test_support::TestDirectory,
    };

    fn log_storage_limit(error: &LogError) -> Option<&StorageLimitError> {
        match error {
            LogError::Io(source) => storage_limit_from_io(source),
            _ => None,
        }
    }

    fn open_for_test(path: &std::path::Path) -> Result<OpenedLog<'static>, LogError> {
        let (log, recovery) = DurableLog::open_file(path)?;
        Ok(OpenedLog::new(log, recovery))
    }

    #[test]
    fn committed_transaction_recovers_in_order() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("log-recovery")?;
        let path = temporary.path().join("segment.hylog");
        let transaction_id = Uuid::now_v7();
        let mut opened = open_for_test(&path)?;
        let outcome = opened
            .log
            .append_transaction(transaction_id, &[b"put:a=1".to_vec(), b"put:b=2".to_vec()])?;
        assert!(matches!(outcome, AppendOutcome::Committed(_)));
        drop(opened);

        let reopened = open_for_test(&path)?;
        assert_eq!(reopened.recovery.transactions.len(), 1);
        assert_eq!(
            reopened.recovery.transactions[0].operations,
            [b"put:a=1".to_vec(), b"put:b=2".to_vec()]
        );
        Ok(())
    }

    #[test]
    fn recovery_limits_are_exact_and_fail_before_tail_repair() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("log-recovery-limits")?;
        let path = temporary.path().join("segment.hylog");
        let mut opened = open_for_test(&path)?;
        opened
            .log
            .append_transaction(Uuid::now_v7(), &[b"one".to_vec()])?;
        opened
            .log
            .append_transaction(Uuid::now_v7(), &[b"four".to_vec()])?;
        drop(opened);
        let file_bytes = std::fs::metadata(&path)?.len();
        let exact = RecoveryLimits {
            max_log_file_bytes: file_bytes,
            max_log_frames: 6,
            max_transactions: 2,
            max_operations: 2,
            max_decoded_operation_bytes: 7,
            ..RecoveryLimits::default()
        };
        let open = |limits: &RecoveryLimits, timeout| {
            let deadline = OperationDeadline::new(timeout);
            DurableLog::open_file_at_version_with_limits(
                &path,
                0,
                [0; 32],
                DISK_FORMAT_VERSION,
                limits,
                &deadline,
            )
        };
        assert_eq!(
            open(&exact, Duration::from_secs(5))?.1.transactions.len(),
            2
        );

        for (limits, expected) in [
            (
                RecoveryLimits {
                    max_log_file_bytes: file_bytes - 1,
                    ..exact.clone()
                },
                StorageLimitError::LogFileBytesExceeded {
                    actual: file_bytes,
                    maximum: file_bytes - 1,
                },
            ),
            (
                RecoveryLimits {
                    max_log_frames: 5,
                    ..exact.clone()
                },
                StorageLimitError::LogFramesExceeded { maximum: 5 },
            ),
            (
                RecoveryLimits {
                    max_transactions: 1,
                    ..exact.clone()
                },
                StorageLimitError::TransactionsExceeded { maximum: 1 },
            ),
            (
                RecoveryLimits {
                    max_operations: 1,
                    ..exact.clone()
                },
                StorageLimitError::OperationsExceeded { maximum: 1 },
            ),
            (
                RecoveryLimits {
                    max_decoded_operation_bytes: 6,
                    ..exact.clone()
                },
                StorageLimitError::DecodedOperationBytesExceeded { maximum: 6 },
            ),
        ] {
            assert!(matches!(
                open(&limits, Duration::from_secs(5)),
                Err(source) if log_storage_limit(&source) == Some(&expected)
            ));
            assert_eq!(std::fs::metadata(&path)?.len(), file_bytes);
        }
        assert!(matches!(
            open(&exact, Duration::ZERO),
            Err(source)
                if log_storage_limit(&source) == Some(&StorageLimitError::TimedOut)
        ));
        assert_eq!(std::fs::metadata(&path)?.len(), file_bytes);
        Ok(())
    }

    #[test]
    fn scan_stops_at_captured_length_and_detects_later_growth() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("log-captured-length")?;
        let path = temporary.path().join("segment.hylog");
        let mut opened = open_for_test(&path)?;
        opened
            .log
            .append_transaction(Uuid::now_v7(), &[b"first".to_vec()])?;
        let captured_length = opened.log.file.metadata()?.len();
        opened
            .log
            .append_transaction(Uuid::now_v7(), &[b"second".to_vec()])?;
        drop(opened);

        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        let scanned = scan(
            &mut file,
            captured_length,
            0,
            [0; 32],
            DISK_FORMAT_VERSION,
            &RecoveryLimits::default(),
            &OperationDeadline::new(Duration::from_secs(5)),
        )?;

        assert_eq!(scanned.report.transactions.len(), 1);
        assert_eq!(scanned.report.valid_bytes, captured_length);
        assert_eq!(file.stream_position()?, captured_length);
        assert!(file.metadata()?.len() > captured_length);
        assert!(ensure_file_length_unchanged(&file, captured_length).is_err());
        Ok(())
    }

    #[test]
    fn incomplete_payload_tail_does_not_consume_a_frame_limit() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("log-partial-payload-frame-limit")?;
        let path = temporary.path().join("segment.hylog");
        let operations = [b"one".to_vec()];
        let transaction_bytes =
            transaction_encoded_length(&operations).ok_or("transaction length overflow")?;
        let limits = RecoveryLimits {
            max_log_frames: 6,
            ..RecoveryLimits::default()
        };
        let open = || {
            DurableLog::open_file_at_version_with_limits(
                &path,
                0,
                [0; 32],
                DISK_FORMAT_VERSION,
                &limits,
                &OperationDeadline::new(Duration::from_secs(5)),
            )
        };

        let (mut log, _) = open()?;
        log.append_transaction(Uuid::now_v7(), &operations)?;
        log.append_transaction(Uuid::now_v7(), &operations)?;
        drop(log);

        let partial_tail_bytes = u64::try_from(HEADER_LENGTH)? + 1;
        let partial_length = transaction_bytes
            .checked_add(partial_tail_bytes)
            .ok_or("partial length overflow")?;
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        file.set_len(partial_length)?;
        file.sync_all()?;
        drop(file);

        let (mut recovered, report) = open()?;
        assert_eq!(report.truncated_tail_bytes, partial_tail_bytes);
        assert_eq!(std::fs::metadata(&path)?.len(), transaction_bytes);
        recovered.append_transaction(Uuid::now_v7(), &operations)?;
        drop(recovered);

        let (_, final_report) = open()?;
        assert_eq!(final_report.transactions.len(), 2);
        assert_eq!(std::fs::metadata(&path)?.len(), transaction_bytes * 2);
        Ok(())
    }

    #[test]
    fn append_sequence_preflight_is_exact_and_never_writes_on_exhaustion()
    -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("log-sequence-preflight")?;
        let operations = [b"one".to_vec()];
        let limits = RecoveryLimits::default();
        let base_digest = [7; 32];

        let exact_path = temporary.path().join("exact.hylog");
        let exact_base = u64::MAX - 4;
        let (mut exact, _) = DurableLog::open_file_at_version_with_limits(
            &exact_path,
            exact_base,
            base_digest,
            DISK_FORMAT_VERSION,
            &limits,
            &OperationDeadline::new(Duration::from_secs(5)),
        )?;
        let AppendOutcome::Committed(receipt) =
            exact.append_transaction(Uuid::now_v7(), &operations)?
        else {
            return Err("new transaction was not committed".into());
        };
        assert_eq!(receipt.commit_sequence, u64::MAX - 1);
        assert_eq!(exact.next_sequence, u64::MAX);
        drop(exact);
        let (_, exact_report) = DurableLog::open_file_at_version_with_limits(
            &exact_path,
            exact_base,
            base_digest,
            DISK_FORMAT_VERSION,
            &limits,
            &OperationDeadline::new(Duration::from_secs(5)),
        )?;
        assert_eq!(exact_report.transactions.len(), 1);

        let exhausted_path = temporary.path().join("exhausted.hylog");
        let exhausted_base = u64::MAX - 3;
        let (mut exhausted, _) = DurableLog::open_file_at_version_with_limits(
            &exhausted_path,
            exhausted_base,
            base_digest,
            DISK_FORMAT_VERSION,
            &limits,
            &OperationDeadline::new(Duration::from_secs(5)),
        )?;
        assert!(matches!(
            exhausted.append_transaction(Uuid::now_v7(), &operations),
            Err(LogError::SequenceExhausted)
        ));
        assert!(!exhausted.is_poisoned());
        assert_eq!(std::fs::metadata(&exhausted_path)?.len(), 0);
        drop(exhausted);

        let (_, exhausted_report) = DurableLog::open_file_at_version_with_limits(
            &exhausted_path,
            exhausted_base,
            base_digest,
            DISK_FORMAT_VERSION,
            &limits,
            &OperationDeadline::new(Duration::from_secs(5)),
        )?;
        assert!(exhausted_report.transactions.is_empty());
        assert_eq!(std::fs::metadata(&exhausted_path)?.len(), 0);
        Ok(())
    }

    #[test]
    fn append_never_creates_a_log_that_its_policy_cannot_reopen() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("log-append-limit")?;
        let path = temporary.path().join("segment.hylog");
        let operations = [b"one".to_vec()];
        let transaction_bytes =
            transaction_encoded_length(&operations).ok_or("transaction length overflow")?;
        let limits = RecoveryLimits {
            max_log_file_bytes: transaction_bytes,
            ..RecoveryLimits::default()
        };
        let deadline = OperationDeadline::new(Duration::from_secs(5));
        let (mut log, _) = DurableLog::open_file_at_version_with_limits(
            &path,
            0,
            [0; 32],
            DISK_FORMAT_VERSION,
            &limits,
            &deadline,
        )?;
        log.append_transaction(Uuid::now_v7(), &operations)?;
        assert_eq!(std::fs::metadata(&path)?.len(), transaction_bytes);

        assert!(matches!(
            log.append_transaction(Uuid::now_v7(), &operations),
            Err(source)
                if matches!(
                    log_storage_limit(&source),
                    Some(StorageLimitError::LogFileBytesExceeded { actual, maximum })
                        if *actual == transaction_bytes * 2 && *maximum == transaction_bytes
                )
        ));
        assert_eq!(std::fs::metadata(&path)?.len(), transaction_bytes);
        drop(log);

        DurableLog::open_file_at_version_with_limits(
            &path,
            0,
            [0; 32],
            DISK_FORMAT_VERSION,
            &limits,
            &OperationDeadline::new(Duration::from_secs(5)),
        )?;
        Ok(())
    }

    #[test]
    fn append_preserves_every_aggregate_recovery_ceiling() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("log-append-aggregate-limits")?;
        let operations = [b"one".to_vec()];
        for (name, limits, expected) in [
            (
                "frames",
                RecoveryLimits {
                    max_log_frames: 3,
                    ..RecoveryLimits::default()
                },
                StorageLimitError::LogFramesExceeded { maximum: 3 },
            ),
            (
                "transactions",
                RecoveryLimits {
                    max_transactions: 1,
                    ..RecoveryLimits::default()
                },
                StorageLimitError::TransactionsExceeded { maximum: 1 },
            ),
            (
                "operations",
                RecoveryLimits {
                    max_operations: 1,
                    ..RecoveryLimits::default()
                },
                StorageLimitError::OperationsExceeded { maximum: 1 },
            ),
            (
                "decoded-bytes",
                RecoveryLimits {
                    max_decoded_operation_bytes: 3,
                    ..RecoveryLimits::default()
                },
                StorageLimitError::DecodedOperationBytesExceeded { maximum: 3 },
            ),
        ] {
            let path = temporary.path().join(format!("{name}.hylog"));
            let deadline = OperationDeadline::new(Duration::from_secs(5));
            let (mut log, _) = DurableLog::open_file_at_version_with_limits(
                &path,
                0,
                [0; 32],
                DISK_FORMAT_VERSION,
                &limits,
                &deadline,
            )?;
            log.append_transaction(Uuid::now_v7(), &operations)?;
            let accepted_bytes = std::fs::metadata(&path)?.len();
            assert!(matches!(
                log.append_transaction(Uuid::now_v7(), &operations),
                Err(source) if log_storage_limit(&source) == Some(&expected)
            ));
            assert_eq!(std::fs::metadata(&path)?.len(), accepted_bytes);
            drop(log);
            DurableLog::open_file_at_version_with_limits(
                &path,
                0,
                [0; 32],
                DISK_FORMAT_VERSION,
                &limits,
                &OperationDeadline::new(Duration::from_secs(5)),
            )?;
        }
        Ok(())
    }

    #[test]
    fn idempotency_survives_reopen() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("log-idempotency")?;
        let path = temporary.path().join("segment.hylog");
        let transaction_id = Uuid::now_v7();
        let operations = [b"same".to_vec()];
        let mut opened = open_for_test(&path)?;
        let first = opened.log.append_transaction(transaction_id, &operations)?;
        drop(opened);

        let mut reopened = open_for_test(&path)?;
        let second = reopened
            .log
            .append_transaction(transaction_id, &operations)?;
        assert!(matches!(first, AppendOutcome::Committed(_)));
        assert!(matches!(second, AppendOutcome::Existing(_)));

        let conflict = reopened
            .log
            .append_transaction(transaction_id, &[b"different".to_vec()]);
        assert!(matches!(
            conflict,
            Err(LogError::IdempotencyConflict { .. })
        ));
        Ok(())
    }

    #[test]
    fn truncates_only_an_incomplete_tail() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("log-tail")?;
        let path = temporary.path().join("segment.hylog");
        let mut opened = open_for_test(&path)?;
        opened
            .log
            .append_transaction(Uuid::now_v7(), &[b"durable".to_vec()])?;
        drop(opened);
        let valid_length = std::fs::metadata(&path)?.len();

        OpenOptions::new()
            .append(true)
            .open(&path)?
            .write_all(b"partial")?;
        let reopened = open_for_test(&path)?;
        assert_eq!(reopened.recovery.truncated_tail_bytes, 7);
        assert_eq!(std::fs::metadata(&path)?.len(), valid_length);
        assert_eq!(reopened.recovery.transactions.len(), 1);
        Ok(())
    }

    #[test]
    fn rejects_complete_corruption_without_truncating() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("log-corruption")?;
        let path = temporary.path().join("segment.hylog");
        let mut opened = open_for_test(&path)?;
        opened
            .log
            .append_transaction(Uuid::now_v7(), &[b"durable".to_vec()])?;
        drop(opened);
        let original_length = std::fs::metadata(&path)?.len();

        let payload_offset = u64::try_from(HEADER_LENGTH * 2)? + 36;
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        file.seek(SeekFrom::Start(payload_offset))?;
        let mut byte = [0_u8; 1];
        file.read_exact(&mut byte)?;
        byte[0] ^= 0x01;
        file.seek(SeekFrom::Start(payload_offset))?;
        file.write_all(&byte)?;
        file.sync_all()?;
        drop(file);

        let result = open_for_test(&path);
        assert!(matches!(result, Err(LogError::ChecksumMismatch { .. })));
        assert_eq!(std::fs::metadata(&path)?.len(), original_length);
        Ok(())
    }

    #[test]
    fn retry_supersedes_an_uncommitted_attempt() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("log-retry")?;
        let path = temporary.path().join("segment.hylog");
        let transaction_id = Uuid::now_v7();
        let operations = [b"complete".to_vec()];

        let mut opened = open_for_test(&path)?;
        let digest = super::transaction_digest(&operations, 1)?;
        let descriptor = super::encode_descriptor(1, digest);
        opened
            .log
            .append_frame(super::FrameKind::Begin, transaction_id, &descriptor)?;
        opened
            .log
            .append_frame(super::FrameKind::Operation, transaction_id, b"incomplete")?;
        opened.log.file.sync_data()?;
        drop(opened);

        let mut recovered = open_for_test(&path)?;
        assert_eq!(recovered.recovery.ignored_uncommitted_transactions, 1);
        recovered
            .log
            .append_transaction(transaction_id, &operations)?;
        drop(recovered);

        let final_open = open_for_test(&path)?;
        assert_eq!(final_open.recovery.transactions.len(), 1);
        assert_eq!(final_open.recovery.transactions[0].operations, operations);
        Ok(())
    }

    #[test]
    fn every_incomplete_transaction_prefix_is_atomic() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("log-byte-cuts")?;
        let seed_path = temporary.path().join("seed.hylog");
        let target_path = temporary.path().join("cut.hylog");
        let mut seed = open_for_test(&seed_path)?;
        seed.log
            .append_transaction(Uuid::now_v7(), &[b"first".to_vec(), b"second".to_vec()])?;
        drop(seed);
        let complete = std::fs::read(&seed_path)?;

        for cut in 0..complete.len() {
            std::fs::write(&target_path, &complete[..cut])?;
            let recovered = open_for_test(&target_path)?;
            assert!(
                recovered.recovery.transactions.is_empty(),
                "cut at byte {cut} exposed an uncommitted transaction"
            );
            drop(recovered);
        }

        std::fs::write(&target_path, &complete)?;
        let recovered = open_for_test(&target_path)?;
        assert_eq!(recovered.recovery.transactions.len(), 1);
        Ok(())
    }

    #[test]
    fn future_frame_version_fails_before_payload_allocation() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("log-future-version")?;
        let path = temporary.path().join("segment.hylog");
        let mut opened = open_for_test(&path)?;
        opened
            .log
            .append_transaction(Uuid::now_v7(), &[b"durable".to_vec()])?;
        drop(opened);

        let mut bytes = std::fs::read(&path)?;
        bytes[8..10].copy_from_slice(&3_u16.to_le_bytes());
        bytes[36..44].copy_from_slice(&u64::MAX.to_le_bytes());
        std::fs::write(&path, &bytes)?;

        let result = open_for_test(&path);
        assert!(matches!(
            result,
            Err(LogError::UnsupportedVersion {
                found: 3,
                supported: 2,
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn anchored_segment_continues_the_global_digest_chain() -> Result<(), Box<dyn Error>> {
        let temporary = TestDirectory::new("log-anchored-segment")?;
        let first_path = temporary.path().join("first.hylog");
        let second_path = temporary.path().join("second.hylog");
        let mut first = open_for_test(&first_path)?;
        first
            .log
            .append_transaction(Uuid::now_v7(), &[b"before-compaction".to_vec()])?;
        drop(first);
        let (_, first_recovery) = DurableLog::open_file(&first_path)?;

        let (mut second, empty_recovery) = DurableLog::open_file_at(
            &second_path,
            first_recovery.last_sequence,
            first_recovery.last_digest,
        )?;
        assert_eq!(empty_recovery.base_sequence, first_recovery.last_sequence);
        assert_eq!(empty_recovery.last_digest, first_recovery.last_digest);
        let outcome = second.append_transaction(Uuid::now_v7(), &[b"after-compaction".to_vec()])?;
        let AppendOutcome::Committed(receipt) = outcome else {
            return Err("new anchored transaction was not committed".into());
        };
        assert_eq!(receipt.commit_sequence, first_recovery.last_sequence + 3);
        drop(second);

        let (_, reopened) = DurableLog::open_file_at(
            &second_path,
            first_recovery.last_sequence,
            first_recovery.last_digest,
        )?;
        assert_eq!(reopened.transactions.len(), 1);

        let wrong_anchor =
            DurableLog::open_file_at(&second_path, first_recovery.last_sequence, [9; 32]);
        assert!(matches!(
            wrong_anchor,
            Err(LogError::PreviousDigestMismatch { .. })
        ));
        Ok(())
    }
}
