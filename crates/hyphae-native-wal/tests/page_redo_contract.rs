// SPDX-License-Identifier: Apache-2.0

//! Test-only model for inventory and page-tail rules in the proposed redo
//! transition.
//!
//! Nothing in this integration test is available to a library build or can
//! emit a released native WAL record. Physical WAL, manifest, checkpoint, and
//! terminal authority verification are explicit preconditions represented by
//! fixtures here; this model does not implement or prove those codecs.

use hyphae_native_types::{
    DirectoryUuid, EngineKind, HistoryEpoch, LineageIdentity, TransactionId,
};
use hyphae_native_wal::{PendingRecord, RecordKind, WAL_BLOCK_HEADER_SIZE, WalBlock, WalError};

const REQUIRED_WAL_FORMAT: u16 = 2;
const REDO_FEATURE_VERSION: u16 = 1;
const MAX_REDO_PAGES: usize = 64;
const PAGE_SIZE: usize = 256;
const PAGE_PAYLOAD_START: usize = 24;
const PAGE_DIGEST_START: usize = PAGE_SIZE - 32;
const PAGE_MAGIC: &[u8; 8] = b"HYTSTPG1";
const WAL_BLOCK_CHECKSUM_START: usize = 44;
const WAL_BLOCK_CHECKSUM_END: usize = 48;
const WAL_BLOCK_DIGEST_START: usize = 80;
const WAL_BLOCK_DIGEST_END: usize = 112;
const WAL_RECORD_CHECKSUM_START: usize = 36;
const WAL_RECORD_CHECKSUM_END: usize = 40;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PageGeneration(u64);

impl PageGeneration {
    const FIRST: Self = Self(1);

    const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PageId(u64);

impl PageId {
    fn new(value: u64) -> Result<Self, ContractError> {
        if value == 0 {
            Err(ContractError::InvalidPage)
        } else {
            Ok(Self(value))
        }
    }

    const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug)]
enum PageKind {
    StructureNode,
}

#[derive(Clone, Copy, Debug)]
struct Csn;

impl Csn {
    const FIRST: Self = Self;
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Page {
    id: PageId,
    payload: Vec<u8>,
}

impl Page {
    fn new(
        id: PageId,
        _kind: PageKind,
        _creating_csn: Option<Csn>,
        _next: Option<PageId>,
        payload: Vec<u8>,
    ) -> Result<Self, ContractError> {
        if payload.len() > PAGE_DIGEST_START - PAGE_PAYLOAD_START {
            return Err(ContractError::InvalidPage);
        }
        Ok(Self { id, payload })
    }

    const fn id(&self) -> PageId {
        self.id
    }

    fn encode(&self) -> Vec<u8> {
        let mut encoded = vec![0; PAGE_SIZE];
        encoded[..8].copy_from_slice(PAGE_MAGIC);
        encoded[8..16].copy_from_slice(&self.id.get().to_le_bytes());
        encoded[16..18].copy_from_slice(
            &u16::try_from(self.payload.len())
                .unwrap_or(u16::MAX)
                .to_le_bytes(),
        );
        encoded[PAGE_PAYLOAD_START..PAGE_PAYLOAD_START + self.payload.len()]
            .copy_from_slice(&self.payload);
        let digest = blake3::hash(&encoded);
        encoded[PAGE_DIGEST_START..].copy_from_slice(digest.as_bytes());
        encoded
    }

    fn decode(expected_id: PageId, encoded: &[u8]) -> Result<Self, ContractError> {
        if encoded.len() != PAGE_SIZE
            || encoded.get(..8) != Some(PAGE_MAGIC.as_slice())
            || encoded[18..PAGE_PAYLOAD_START]
                .iter()
                .any(|byte| *byte != 0)
        {
            return Err(ContractError::InvalidPage);
        }
        let found_id = u64::from_le_bytes(
            encoded[8..16]
                .try_into()
                .map_err(|_| ContractError::InvalidPage)?,
        );
        let payload_length = usize::from(u16::from_le_bytes(
            encoded[16..18]
                .try_into()
                .map_err(|_| ContractError::InvalidPage)?,
        ));
        let payload_end = PAGE_PAYLOAD_START
            .checked_add(payload_length)
            .ok_or(ContractError::InvalidPage)?;
        if found_id != expected_id.get()
            || payload_end > PAGE_DIGEST_START
            || encoded[payload_end..PAGE_DIGEST_START]
                .iter()
                .any(|byte| *byte != 0)
        {
            return Err(ContractError::InvalidPage);
        }
        let mut canonical = encoded.to_vec();
        canonical[PAGE_DIGEST_START..].fill(0);
        if blake3::hash(&canonical).as_bytes() != &encoded[PAGE_DIGEST_START..] {
            return Err(ContractError::InvalidPage);
        }
        Ok(Self {
            id: expected_id,
            payload: encoded[PAGE_PAYLOAD_START..payload_end].to_vec(),
        })
    }
}

#[derive(Clone, Debug)]
struct RootManifestFixture {
    lineage: LineageIdentity,
    generation: u64,
    visible_csn: u64,
    page_generation: PageGeneration,
    page_count: u64,
    page_prefix_digest: [u8; 32],
    base_descriptor_digest: [u8; 32],
    manifest_digest: [u8; 32],
}

#[derive(Clone, Debug)]
struct CheckpointRecordFixture {
    lineage: LineageIdentity,
    manifest_generation: u64,
    manifest_digest: [u8; 32],
    base_descriptor_digest: [u8; 32],
    checkpoint_lsn: u64,
    checkpoint_block_digest: [u8; 32],
}

#[derive(Clone, Debug)]
struct WalFeatureFixture {
    format_version: u16,
    feature_version: u16,
    lineage: LineageIdentity,
    checkpoint_lsn: u64,
    checkpoint_block_digest: [u8; 32],
}

#[derive(Clone, Debug)]
struct CheckpointFixture {
    manifest: RootManifestFixture,
    checkpoint: CheckpointRecordFixture,
    wal: WalFeatureFixture,
}

#[derive(Clone, Copy, Debug)]
struct VerifiedCheckpointToken {
    page_generation: PageGeneration,
    page_count: u64,
    digest: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExpectedTerminal {
    transaction_id: TransactionId,
    checkpoint_token_digest: [u8; 32],
    first_page_id: u64,
    page_count: u32,
    final_page_count: u64,
    inventory_digest: [u8; 32],
    target_roots_digest: [u8; 32],
    blob_generation: u64,
    page_generation: PageGeneration,
    commit_csn: u64,
    commit_lsn: u64,
    commit_block_digest: [u8; 32],
    digest: [u8; 32],
}

#[derive(Clone, Debug)]
enum FrameBody {
    Page {
        index: u32,
        declared_count: u32,
        page: Page,
        image_digest: [u8; 32],
    },
    Terminal {
        first_page_id: u64,
        page_count: u32,
        final_page_count: u64,
        inventory_digest: [u8; 32],
        target_roots_digest: [u8; 32],
        authority_digest: [u8; 32],
    },
}

#[derive(Clone, Debug)]
struct ContractFrame {
    wal_format: u16,
    feature_version: u16,
    transaction_id: TransactionId,
    checkpoint_token_digest: [u8; 32],
    body: FrameBody,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct VerifiedBatch {
    first_page_id: u64,
    pages: Vec<Page>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContractError {
    Authority,
    UnsupportedBoundary,
    MissingTerminal,
    TerminalNotLast,
    CountMismatch,
    OrderMismatch,
    InventoryMismatch,
    TerminalAuthorityMismatch,
    InvalidPage,
    BaseMismatch,
    ExistingPageMismatch,
    AppendFailure,
    SyncFailure,
    TruncateFailure,
    WriterPoisoned,
    UnalignedFile,
}

#[derive(Clone, Copy, Debug)]
enum InjectedFault {
    AppendAfterBytes(usize),
    SyncWithStableLength(usize),
    Truncate,
}

#[derive(Debug)]
struct FaultyPageFile {
    generation: PageGeneration,
    bytes: Vec<u8>,
    stable_bytes: Vec<u8>,
    physical_reads: u64,
    next_fault: Option<InjectedFault>,
    poisoned: bool,
}

impl FaultyPageFile {
    fn from_pages(generation: PageGeneration, pages: &[Page]) -> Result<Self, ContractError> {
        for (index, page) in pages.iter().enumerate() {
            if page.id().get()
                != u64::try_from(index + 1).map_err(|_| ContractError::InvalidPage)?
            {
                return Err(ContractError::InvalidPage);
            }
        }
        let bytes = pages.iter().flat_map(Page::encode).collect::<Vec<_>>();
        Ok(Self {
            generation,
            stable_bytes: bytes.clone(),
            bytes,
            physical_reads: 0,
            next_fault: None,
            poisoned: false,
        })
    }

    fn inject(&mut self, fault: InjectedFault) {
        self.next_fault = Some(fault);
    }

    fn clear_fault(&mut self) {
        self.next_fault = None;
    }

    fn complete_page_count(&self) -> Result<u64, ContractError> {
        if !self.bytes.len().is_multiple_of(PAGE_SIZE) {
            return Err(ContractError::UnalignedFile);
        }
        u64::try_from(self.bytes.len() / PAGE_SIZE).map_err(|_| ContractError::InvalidPage)
    }

    fn read_page(&mut self, page_id: PageId) -> Result<Page, ContractError> {
        let index = usize::try_from(
            page_id
                .get()
                .checked_sub(1)
                .ok_or(ContractError::InvalidPage)?,
        )
        .map_err(|_| ContractError::InvalidPage)?;
        let start = index
            .checked_mul(PAGE_SIZE)
            .ok_or(ContractError::InvalidPage)?;
        let end = start
            .checked_add(PAGE_SIZE)
            .ok_or(ContractError::InvalidPage)?;
        let encoded = self
            .bytes
            .get(start..end)
            .ok_or(ContractError::InvalidPage)?;
        self.physical_reads = self
            .physical_reads
            .checked_add(1)
            .ok_or(ContractError::InvalidPage)?;
        Page::decode(page_id, encoded).map_err(|_| ContractError::InvalidPage)
    }

    fn append_page(&mut self, page: &Page) -> Result<(), ContractError> {
        if self.poisoned {
            return Err(ContractError::WriterPoisoned);
        }
        let expected = self
            .complete_page_count()?
            .checked_add(1)
            .ok_or(ContractError::InvalidPage)?;
        if page.id().get() != expected {
            return Err(ContractError::OrderMismatch);
        }
        let encoded = page.encode();
        if let Some(InjectedFault::AppendAfterBytes(after)) = self.next_fault {
            self.next_fault = None;
            let written = after.min(encoded.len());
            self.bytes.extend_from_slice(&encoded[..written]);
            // Model the worst legal outcome of an uncertain partial append.
            self.stable_bytes = self.bytes.clone();
            self.poisoned = true;
            return Err(ContractError::AppendFailure);
        }
        self.bytes.extend_from_slice(&encoded);
        Ok(())
    }

    fn sync_data(&mut self) -> Result<(), ContractError> {
        if self.poisoned {
            return Err(ContractError::WriterPoisoned);
        }
        if let Some(InjectedFault::SyncWithStableLength(length)) = self.next_fault {
            self.next_fault = None;
            let stable_length = length.min(self.bytes.len());
            self.stable_bytes = self.bytes[..stable_length].to_vec();
            self.poisoned = true;
            return Err(ContractError::SyncFailure);
        }
        self.stable_bytes = self.bytes.clone();
        Ok(())
    }

    fn crash(&mut self) {
        self.bytes = self.stable_bytes.clone();
    }

    fn reopen_repair_tail(&mut self) -> Result<u64, ContractError> {
        self.bytes = self.stable_bytes.clone();
        let tail = self.bytes.len() % PAGE_SIZE;
        if tail == 0 {
            return Ok(0);
        }
        if matches!(self.next_fault, Some(InjectedFault::Truncate)) {
            self.next_fault = None;
            self.poisoned = true;
            return Err(ContractError::TruncateFailure);
        }
        self.bytes.truncate(self.bytes.len() - tail);
        self.stable_bytes = self.bytes.clone();
        u64::try_from(tail).map_err(|_| ContractError::InvalidPage)
    }
}

struct RedoTarget<'file> {
    file: &'file mut FaultyPageFile,
    checkpoint: VerifiedCheckpointToken,
}

impl<'file> RedoTarget<'file> {
    fn open(
        file: &'file mut FaultyPageFile,
        authority: &CheckpointFixture,
    ) -> Result<(Self, u64), ContractError> {
        let descriptor = validate_checkpoint_fixture(authority)?;
        if file.generation != authority.manifest.page_generation {
            return Err(ContractError::Authority);
        }
        validate_checkpoint_prefix(file, authority)?;
        let repaired = file.reopen_repair_tail()?;
        let checkpoint = checkpoint_token(authority, descriptor);
        file.poisoned = false;
        Ok((Self { file, checkpoint }, repaired))
    }

    fn stage(
        &mut self,
        frames: &[ContractFrame],
        terminal: &ExpectedTerminal,
    ) -> Result<(), ContractError> {
        if self.file.poisoned {
            return Err(ContractError::WriterPoisoned);
        }
        let batch = verify_batch(frames, self.checkpoint, terminal)?;
        let current_count = self.file.complete_page_count()?;
        let prior_count = batch
            .first_page_id
            .checked_sub(1)
            .ok_or(ContractError::OrderMismatch)?;
        let final_count = prior_count
            .checked_add(
                u64::try_from(batch.pages.len()).map_err(|_| ContractError::CountMismatch)?,
            )
            .ok_or(ContractError::CountMismatch)?;
        if prior_count < self.checkpoint.page_count
            || current_count < prior_count
            || current_count > final_count
            || self.file.generation != self.checkpoint.page_generation
        {
            return Err(ContractError::BaseMismatch);
        }

        let overlap = usize::try_from(current_count - prior_count)
            .map_err(|_| ContractError::CountMismatch)?;
        for expected in batch.pages.iter().take(overlap) {
            if self.file.read_page(expected.id())? != *expected {
                return Err(ContractError::ExistingPageMismatch);
            }
        }
        for page in &batch.pages[overlap..] {
            self.file.append_page(page)?;
        }
        self.file.sync_data()
    }
}

fn validate_checkpoint_fixture(authority: &CheckpointFixture) -> Result<[u8; 32], ContractError> {
    let manifest = &authority.manifest;
    let checkpoint = &authority.checkpoint;
    let wal = &authority.wal;
    if wal.format_version != REQUIRED_WAL_FORMAT || wal.feature_version != REDO_FEATURE_VERSION {
        return Err(ContractError::UnsupportedBoundary);
    }
    if manifest.lineage != checkpoint.lineage
        || checkpoint.lineage != wal.lineage
        || manifest.generation != checkpoint.manifest_generation
        || manifest.manifest_digest != checkpoint.manifest_digest
        || manifest.base_descriptor_digest != checkpoint.base_descriptor_digest
        || checkpoint.checkpoint_lsn != wal.checkpoint_lsn
        || checkpoint.checkpoint_block_digest != wal.checkpoint_block_digest
    {
        return Err(ContractError::Authority);
    }

    let descriptor = base_descriptor_digest(
        manifest.lineage,
        manifest.generation,
        manifest.visible_csn,
        manifest.page_generation,
        manifest.page_count,
        manifest.page_prefix_digest,
    );
    if descriptor != manifest.base_descriptor_digest {
        return Err(ContractError::Authority);
    }
    let manifest_digest = root_manifest_digest(manifest, descriptor);
    if manifest_digest != manifest.manifest_digest {
        return Err(ContractError::Authority);
    }
    let checkpoint_block_digest = checkpoint_digest(checkpoint);
    if checkpoint_block_digest != checkpoint.checkpoint_block_digest {
        return Err(ContractError::Authority);
    }
    Ok(descriptor)
}

fn validate_checkpoint_prefix(
    file: &mut FaultyPageFile,
    authority: &CheckpointFixture,
) -> Result<(), ContractError> {
    let manifest = &authority.manifest;
    let page_count = usize::try_from(manifest.page_count).map_err(|_| ContractError::Authority)?;
    let required_bytes = page_count
        .checked_mul(PAGE_SIZE)
        .ok_or(ContractError::Authority)?;
    if page_count == 0 || file.stable_bytes.len() < required_bytes {
        return Err(ContractError::Authority);
    }

    let mut prefix = blake3::Hasher::new();
    prefix.update(b"hyphae-page-redo-checkpoint-prefix-v1");
    for index in 0..page_count {
        let start = index
            .checked_mul(PAGE_SIZE)
            .ok_or(ContractError::Authority)?;
        let end = start
            .checked_add(PAGE_SIZE)
            .ok_or(ContractError::Authority)?;
        file.physical_reads = file
            .physical_reads
            .checked_add(1)
            .ok_or(ContractError::Authority)?;
        let encoded = &file.stable_bytes[start..end];
        let page_id = PageId::new(
            u64::try_from(index)
                .ok()
                .and_then(|value| value.checked_add(1))
                .ok_or(ContractError::Authority)?,
        )?;
        Page::decode(page_id, encoded).map_err(|_| ContractError::Authority)?;
        prefix.update(encoded);
    }
    if *prefix.finalize().as_bytes() != manifest.page_prefix_digest {
        return Err(ContractError::Authority);
    }
    Ok(())
}

fn checkpoint_token(
    authority: &CheckpointFixture,
    descriptor: [u8; 32],
) -> VerifiedCheckpointToken {
    let manifest = &authority.manifest;
    let checkpoint = &authority.checkpoint;
    let mut token = blake3::Hasher::new();
    token.update(b"hyphae-page-redo-verified-checkpoint-token-v1");
    token.update(&manifest.manifest_digest);
    token.update(&checkpoint.checkpoint_lsn.to_le_bytes());
    token.update(&checkpoint.checkpoint_block_digest);
    token.update(&descriptor);
    VerifiedCheckpointToken {
        page_generation: manifest.page_generation,
        page_count: manifest.page_count,
        digest: *token.finalize().as_bytes(),
    }
}

fn verify_batch(
    frames: &[ContractFrame],
    checkpoint: VerifiedCheckpointToken,
    expected_terminal: &ExpectedTerminal,
) -> Result<VerifiedBatch, ContractError> {
    if frames.iter().any(|frame| {
        frame.wal_format != REQUIRED_WAL_FORMAT
            || frame.feature_version != REDO_FEATURE_VERSION
            || frame.checkpoint_token_digest != checkpoint.digest
    }) {
        return Err(ContractError::UnsupportedBoundary);
    }
    let terminal_index = frames
        .iter()
        .position(|frame| matches!(frame.body, FrameBody::Terminal { .. }))
        .ok_or(ContractError::MissingTerminal)?;
    if terminal_index + 1 != frames.len()
        || frames[..terminal_index]
            .iter()
            .any(|frame| matches!(frame.body, FrameBody::Terminal { .. }))
    {
        return Err(ContractError::TerminalNotLast);
    }
    if frames
        .iter()
        .any(|frame| frame.transaction_id != expected_terminal.transaction_id)
    {
        return Err(ContractError::TerminalAuthorityMismatch);
    }
    let FrameBody::Terminal {
        first_page_id,
        page_count,
        final_page_count,
        inventory_digest: terminal_inventory,
        target_roots_digest,
        authority_digest,
    } = &frames[terminal_index].body
    else {
        return Err(ContractError::MissingTerminal);
    };
    if *first_page_id != expected_terminal.first_page_id
        || expected_terminal.checkpoint_token_digest != checkpoint.digest
        || *page_count != expected_terminal.page_count
        || *final_page_count != expected_terminal.final_page_count
        || *terminal_inventory != expected_terminal.inventory_digest
        || *target_roots_digest != expected_terminal.target_roots_digest
        || *authority_digest != expected_terminal.digest
        || terminal_authority_digest(expected_terminal) != expected_terminal.digest
    {
        return Err(ContractError::TerminalAuthorityMismatch);
    }
    let count = usize::try_from(*page_count).map_err(|_| ContractError::CountMismatch)?;
    if count == 0 || count > MAX_REDO_PAGES || terminal_index != count {
        return Err(ContractError::CountMismatch);
    }

    let mut pages = Vec::with_capacity(count);
    for (index, frame) in frames[..terminal_index].iter().enumerate() {
        let FrameBody::Page {
            index: encoded_index,
            declared_count,
            page,
            image_digest,
        } = &frame.body
        else {
            return Err(ContractError::TerminalNotLast);
        };
        let expected_index = u32::try_from(index).map_err(|_| ContractError::CountMismatch)?;
        let expected_page_id = first_page_id
            .checked_add(u64::try_from(index).map_err(|_| ContractError::CountMismatch)?)
            .ok_or(ContractError::CountMismatch)?;
        if *encoded_index != expected_index
            || *declared_count != *page_count
            || page.id().get() != expected_page_id
        {
            return Err(ContractError::OrderMismatch);
        }
        let image = page.encode();
        if *blake3::hash(&image).as_bytes() != *image_digest
            || Page::decode(page.id(), &image).map_err(|_| ContractError::InvalidPage)? != *page
        {
            return Err(ContractError::InvalidPage);
        }
        pages.push(page.clone());
    }
    if inventory_digest(&pages)? != *terminal_inventory {
        return Err(ContractError::InventoryMismatch);
    }
    Ok(VerifiedBatch {
        first_page_id: *first_page_id,
        pages,
    })
}

fn checkpoint_fixture(
    lineage: LineageIdentity,
    pages: &[Page],
) -> Result<CheckpointFixture, ContractError> {
    let page_count = u64::try_from(pages.len()).map_err(|_| ContractError::InvalidPage)?;
    let mut prefix = blake3::Hasher::new();
    prefix.update(b"hyphae-page-redo-checkpoint-prefix-v1");
    for page in pages {
        prefix.update(&page.encode());
    }
    let page_prefix_digest = *prefix.finalize().as_bytes();
    let descriptor = base_descriptor_digest(
        lineage,
        3,
        11,
        PageGeneration::FIRST,
        page_count,
        page_prefix_digest,
    );
    let mut manifest = RootManifestFixture {
        lineage,
        generation: 3,
        visible_csn: 11,
        page_generation: PageGeneration::FIRST,
        page_count,
        page_prefix_digest,
        base_descriptor_digest: descriptor,
        manifest_digest: [0; 32],
    };
    manifest.manifest_digest = root_manifest_digest(&manifest, descriptor);
    let mut checkpoint = CheckpointRecordFixture {
        lineage,
        manifest_generation: manifest.generation,
        manifest_digest: manifest.manifest_digest,
        base_descriptor_digest: descriptor,
        checkpoint_lsn: 196_720,
        checkpoint_block_digest: [0; 32],
    };
    checkpoint.checkpoint_block_digest = checkpoint_digest(&checkpoint);
    Ok(CheckpointFixture {
        wal: WalFeatureFixture {
            format_version: REQUIRED_WAL_FORMAT,
            feature_version: REDO_FEATURE_VERSION,
            lineage,
            checkpoint_lsn: checkpoint.checkpoint_lsn,
            checkpoint_block_digest: checkpoint.checkpoint_block_digest,
        },
        manifest,
        checkpoint,
    })
}

fn expected_terminal(
    checkpoint: VerifiedCheckpointToken,
    transaction_id: TransactionId,
    pages: &[Page],
    target_roots_digest: [u8; 32],
) -> Result<ExpectedTerminal, ContractError> {
    let first_page_id = pages
        .first()
        .ok_or(ContractError::CountMismatch)?
        .id()
        .get();
    let page_count = u32::try_from(pages.len()).map_err(|_| ContractError::CountMismatch)?;
    let final_page_count = first_page_id
        .checked_sub(1)
        .and_then(|prior| prior.checked_add(u64::from(page_count)))
        .ok_or(ContractError::CountMismatch)?;
    let mut authority = ExpectedTerminal {
        transaction_id,
        checkpoint_token_digest: checkpoint.digest,
        first_page_id,
        page_count,
        final_page_count,
        inventory_digest: inventory_digest(pages)?,
        target_roots_digest,
        blob_generation: 2,
        page_generation: checkpoint.page_generation,
        commit_csn: 12,
        commit_lsn: 262_256,
        commit_block_digest: [4; 32],
        digest: [0; 32],
    };
    authority.digest = terminal_authority_digest(&authority);
    Ok(authority)
}

fn contract_frames(
    checkpoint: VerifiedCheckpointToken,
    terminal: &ExpectedTerminal,
    pages: &[Page],
) -> Vec<ContractFrame> {
    let mut frames = pages
        .iter()
        .enumerate()
        .map(|(index, page)| ContractFrame {
            wal_format: REQUIRED_WAL_FORMAT,
            feature_version: REDO_FEATURE_VERSION,
            transaction_id: terminal.transaction_id,
            checkpoint_token_digest: checkpoint.digest,
            body: FrameBody::Page {
                index: u32::try_from(index).unwrap_or(u32::MAX),
                declared_count: terminal.page_count,
                image_digest: *blake3::hash(&page.encode()).as_bytes(),
                page: page.clone(),
            },
        })
        .collect::<Vec<_>>();
    frames.push(ContractFrame {
        wal_format: REQUIRED_WAL_FORMAT,
        feature_version: REDO_FEATURE_VERSION,
        transaction_id: terminal.transaction_id,
        checkpoint_token_digest: checkpoint.digest,
        body: FrameBody::Terminal {
            first_page_id: terminal.first_page_id,
            page_count: terminal.page_count,
            final_page_count: terminal.final_page_count,
            inventory_digest: terminal.inventory_digest,
            target_roots_digest: terminal.target_roots_digest,
            authority_digest: terminal.digest,
        },
    });
    frames
}

fn base_descriptor_digest(
    lineage: LineageIdentity,
    manifest_generation: u64,
    visible_csn: u64,
    page_generation: PageGeneration,
    page_count: u64,
    page_prefix_digest: [u8; 32],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-page-redo-checkpoint-base-v1");
    hasher.update(&lineage.encode());
    hasher.update(&manifest_generation.to_le_bytes());
    hasher.update(&visible_csn.to_le_bytes());
    hasher.update(&page_generation.get().to_le_bytes());
    hasher.update(&page_count.to_le_bytes());
    hasher.update(&page_prefix_digest);
    *hasher.finalize().as_bytes()
}

fn root_manifest_digest(manifest: &RootManifestFixture, descriptor: [u8; 32]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-root-manifest-redo-contract-v1");
    hasher.update(&manifest.lineage.encode());
    hasher.update(&manifest.generation.to_le_bytes());
    hasher.update(&manifest.visible_csn.to_le_bytes());
    hasher.update(&descriptor);
    *hasher.finalize().as_bytes()
}

fn checkpoint_digest(checkpoint: &CheckpointRecordFixture) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-checkpoint-redo-contract-v1");
    hasher.update(&checkpoint.lineage.encode());
    hasher.update(&checkpoint.manifest_generation.to_le_bytes());
    hasher.update(&checkpoint.manifest_digest);
    hasher.update(&checkpoint.base_descriptor_digest);
    hasher.update(&checkpoint.checkpoint_lsn.to_le_bytes());
    *hasher.finalize().as_bytes()
}

fn inventory_digest(pages: &[Page]) -> Result<[u8; 32], ContractError> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-page-redo-inventory-v1");
    hasher.update(
        &u32::try_from(pages.len())
            .map_err(|_| ContractError::CountMismatch)?
            .to_le_bytes(),
    );
    for (index, page) in pages.iter().enumerate() {
        hasher.update(
            &u32::try_from(index)
                .map_err(|_| ContractError::CountMismatch)?
                .to_le_bytes(),
        );
        hasher.update(&page.id().get().to_le_bytes());
        hasher.update(blake3::hash(&page.encode()).as_bytes());
    }
    Ok(*hasher.finalize().as_bytes())
}

fn terminal_authority_digest(authority: &ExpectedTerminal) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"hyphae-page-redo-terminal-authority-v1");
    hasher.update(&authority.transaction_id.get().to_le_bytes());
    hasher.update(&authority.checkpoint_token_digest);
    hasher.update(&authority.first_page_id.to_le_bytes());
    hasher.update(&authority.page_count.to_le_bytes());
    hasher.update(&authority.final_page_count.to_le_bytes());
    hasher.update(&authority.inventory_digest);
    hasher.update(&authority.target_roots_digest);
    hasher.update(&authority.blob_generation.to_le_bytes());
    hasher.update(&authority.page_generation.get().to_le_bytes());
    hasher.update(&authority.commit_csn.to_le_bytes());
    hasher.update(&authority.commit_lsn.to_le_bytes());
    hasher.update(&authority.commit_block_digest);
    *hasher.finalize().as_bytes()
}

fn pages(first: u64, count: usize, marker: u8) -> Result<Vec<Page>, ContractError> {
    (0..count)
        .map(|index| {
            let raw_id = first
                .checked_add(u64::try_from(index).map_err(|_| ContractError::InvalidPage)?)
                .ok_or(ContractError::InvalidPage)?;
            Page::new(
                PageId::new(raw_id).map_err(|_| ContractError::InvalidPage)?,
                PageKind::StructureNode,
                Some(Csn::FIRST),
                None,
                vec![marker, u8::try_from(index).unwrap_or(u8::MAX)],
            )
            .map_err(|_| ContractError::InvalidPage)
        })
        .collect()
}

fn lineage(marker: u8) -> Result<LineageIdentity, ContractError> {
    let mut uuid = [marker; 16];
    uuid[6] = (uuid[6] & 0x0f) | 0x70;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    Ok(LineageIdentity::new(
        DirectoryUuid::new(uuid).map_err(|_| ContractError::Authority)?,
        HistoryEpoch::FIRST,
    ))
}

fn flatten_frame_groups(groups: &[Vec<ContractFrame>]) -> Vec<ContractFrame> {
    groups.iter().flatten().cloned().collect()
}

fn refresh_page_digest(encoded: &mut [u8]) -> Result<(), ContractError> {
    if encoded.len() != PAGE_SIZE {
        return Err(ContractError::InvalidPage);
    }
    encoded[PAGE_DIGEST_START..].fill(0);
    let digest = *blake3::hash(encoded).as_bytes();
    encoded[PAGE_DIGEST_START..].copy_from_slice(&digest);
    Ok(())
}

fn assert_checkpoint_rejection_preserves_bytes(
    file: &mut FaultyPageFile,
    authority: &CheckpointFixture,
) {
    let before_bytes = file.bytes.clone();
    let before_stable = file.stable_bytes.clone();
    assert_eq!(before_stable.len() % PAGE_SIZE, 17);
    assert_eq!(
        RedoTarget::open(file, authority).err(),
        Some(ContractError::Authority)
    );
    assert_eq!(file.bytes, before_bytes);
    assert_eq!(file.stable_bytes, before_stable);
}

fn wal_record_checksum(encoded: &[u8]) -> u32 {
    let mut canonical = encoded.to_vec();
    canonical[WAL_RECORD_CHECKSUM_START..WAL_RECORD_CHECKSUM_END].fill(0);
    crc32c::crc32c(&canonical)
}

fn wal_block_checksum(encoded: &[u8]) -> u32 {
    let mut canonical = encoded.to_vec();
    canonical[WAL_BLOCK_CHECKSUM_START..WAL_BLOCK_CHECKSUM_END].fill(0);
    canonical[WAL_BLOCK_DIGEST_START..WAL_BLOCK_DIGEST_END].fill(0);
    crc32c::crc32c(&canonical)
}

fn wal_block_digest(encoded: &[u8]) -> [u8; 32] {
    let mut canonical = encoded.to_vec();
    canonical[WAL_BLOCK_DIGEST_START..WAL_BLOCK_DIGEST_END].fill(0);
    *blake3::hash(&canonical).as_bytes()
}

#[test]
fn valid_integrity_wal_v1_block_rejects_unallocated_kind_seven()
-> Result<(), Box<dyn std::error::Error>> {
    let pending = PendingRecord::new(
        RecordKind::Mutation,
        EngineKind::Relational,
        0,
        TransactionId::new(8)?,
        b"valid unknown kind".to_vec(),
    )?;
    let mut encoded = WalBlock::build(1, [0; 32], vec![pending])?.encode()?;
    let record_start = WAL_BLOCK_HEADER_SIZE;
    let record_length = usize::try_from(u32::from_le_bytes(
        encoded[record_start..record_start + 4].try_into()?,
    ))?;
    let record_end = record_start
        .checked_add(record_length)
        .ok_or("record length overflow")?;
    encoded[record_start + 8] = 7;
    let checksum = wal_record_checksum(&encoded[record_start..record_end]);
    encoded[record_start + WAL_RECORD_CHECKSUM_START..record_start + WAL_RECORD_CHECKSUM_END]
        .copy_from_slice(&checksum.to_le_bytes());
    let checksum = wal_block_checksum(&encoded);
    encoded[WAL_BLOCK_CHECKSUM_START..WAL_BLOCK_CHECKSUM_END]
        .copy_from_slice(&checksum.to_le_bytes());
    let digest = wal_block_digest(&encoded);
    encoded[WAL_BLOCK_DIGEST_START..WAL_BLOCK_DIGEST_END].copy_from_slice(&digest);

    assert!(matches!(
        WalBlock::decode(1, [0; 32], &encoded),
        Err(WalError::UnknownRecordKind(7))
    ));
    Ok(())
}

#[test]
fn checkpoint_token_is_fixture_bound_and_cached_across_batches() -> Result<(), ContractError> {
    let base = pages(1, 4, 1)?;
    let authority = checkpoint_fixture(lineage(1)?, &base)?;
    let mut file = FaultyPageFile::from_pages(PageGeneration::FIRST, &base)?;
    let (mut target, repaired) = RedoTarget::open(&mut file, &authority)?;
    assert_eq!(repaired, 0);
    assert_eq!(target.file.physical_reads, 4);
    assert!(std::mem::size_of::<VerifiedCheckpointToken>() <= 64);

    let first = pages(5, 2, 2)?;
    let first_terminal = expected_terminal(
        target.checkpoint,
        TransactionId::new(21).map_err(|_| ContractError::Authority)?,
        &first,
        [2; 32],
    )?;
    let first_frames = contract_frames(target.checkpoint, &first_terminal, &first);
    target.stage(&first_frames, &first_terminal)?;
    assert_eq!(target.file.physical_reads, 4);

    let second = pages(7, 2, 3)?;
    let second_terminal = expected_terminal(
        target.checkpoint,
        TransactionId::new(22).map_err(|_| ContractError::Authority)?,
        &second,
        [3; 32],
    )?;
    let second_frames = contract_frames(target.checkpoint, &second_terminal, &second);
    target.stage(&second_frames, &second_terminal)?;
    assert_eq!(target.file.physical_reads, 4);
    target.stage(&second_frames, &second_terminal)?;
    assert_eq!(target.file.physical_reads, 6);
    Ok(())
}

#[test]
fn future_format_authority_is_validated_before_tail_repair() -> Result<(), ContractError> {
    let base = pages(1, 3, 1)?;
    let authority = checkpoint_fixture(lineage(1)?, &base)?;

    for (format_version, feature_version) in [(1, 1), (2, 2)] {
        let mut unsupported = authority.clone();
        unsupported.wal.format_version = format_version;
        unsupported.wal.feature_version = feature_version;
        let mut file = FaultyPageFile::from_pages(PageGeneration::FIRST, &base)?;
        file.bytes.extend_from_slice(&[0xa5; 17]);
        file.stable_bytes = file.bytes.clone();
        let before = file.bytes.clone();
        assert_eq!(
            RedoTarget::open(&mut file, &unsupported).err(),
            Some(ContractError::UnsupportedBoundary)
        );
        assert_eq!(file.bytes, before);
        assert_eq!(file.stable_bytes, before);
    }

    let mut wrong_manifest = authority.clone();
    wrong_manifest.manifest.page_prefix_digest[0] ^= 1;
    let mut file = FaultyPageFile::from_pages(PageGeneration::FIRST, &base)?;
    assert_eq!(
        RedoTarget::open(&mut file, &wrong_manifest).err(),
        Some(ContractError::Authority)
    );

    let mut wrong_checkpoint = authority.clone();
    wrong_checkpoint.checkpoint.manifest_digest[0] ^= 1;
    let mut file = FaultyPageFile::from_pages(PageGeneration::FIRST, &base)?;
    assert_eq!(
        RedoTarget::open(&mut file, &wrong_checkpoint).err(),
        Some(ContractError::Authority)
    );

    let mut wrong_wal = authority;
    wrong_wal.wal.lineage = lineage(2)?;
    let mut file = FaultyPageFile::from_pages(PageGeneration::FIRST, &base)?;
    assert_eq!(
        RedoTarget::open(&mut file, &wrong_wal).err(),
        Some(ContractError::Authority)
    );
    Ok(())
}

#[test]
fn invalid_checkpoint_prefix_never_repairs_incomplete_tail() -> Result<(), ContractError> {
    let base = pages(1, 3, 1)?;
    let authority = checkpoint_fixture(lineage(1)?, &base)?;

    let mut short = FaultyPageFile::from_pages(PageGeneration::FIRST, &base)?;
    short.stable_bytes.truncate(2 * PAGE_SIZE);
    short.stable_bytes.extend_from_slice(&[0xa1; 17]);
    short.bytes = short.stable_bytes.clone();
    assert_checkpoint_rejection_preserves_bytes(&mut short, &authority);

    let mut noncanonical = FaultyPageFile::from_pages(PageGeneration::FIRST, &base)?;
    noncanonical.stable_bytes[18] = 1;
    refresh_page_digest(&mut noncanonical.stable_bytes[..PAGE_SIZE])?;
    noncanonical.stable_bytes.extend_from_slice(&[0xa2; 17]);
    noncanonical.bytes = noncanonical.stable_bytes.clone();
    assert_checkpoint_rejection_preserves_bytes(&mut noncanonical, &authority);

    let mut divergent = FaultyPageFile::from_pages(PageGeneration::FIRST, &base)?;
    let replacement = Page::new(
        PageId::new(2)?,
        PageKind::StructureNode,
        Some(Csn::FIRST),
        None,
        b"different canonical page".to_vec(),
    )?
    .encode();
    divergent.stable_bytes[PAGE_SIZE..2 * PAGE_SIZE].copy_from_slice(&replacement);
    divergent.stable_bytes.extend_from_slice(&[0xa3; 17]);
    divergent.bytes = divergent.stable_bytes.clone();
    assert_checkpoint_rejection_preserves_bytes(&mut divergent, &authority);
    Ok(())
}

#[test]
fn terminal_fixture_binds_complete_count_order_and_inventory() -> Result<(), ContractError> {
    let base = pages(1, 2, 1)?;
    let authority = checkpoint_fixture(lineage(1)?, &base)?;
    let mut file = FaultyPageFile::from_pages(PageGeneration::FIRST, &base)?;
    let (target, _) = RedoTarget::open(&mut file, &authority)?;
    let redo = pages(3, 4, 2)?;
    let terminal = expected_terminal(
        target.checkpoint,
        TransactionId::new(31).map_err(|_| ContractError::Authority)?,
        &redo,
        [9; 32],
    )?;
    let frames = contract_frames(target.checkpoint, &terminal, &redo);
    assert!(verify_batch(&frames, target.checkpoint, &terminal).is_ok());
    assert_eq!(
        verify_batch(&frames[..frames.len() - 1], target.checkpoint, &terminal),
        Err(ContractError::MissingTerminal)
    );

    let mut missing_page = frames.clone();
    missing_page.remove(1);
    assert_eq!(
        verify_batch(&missing_page, target.checkpoint, &terminal),
        Err(ContractError::CountMismatch)
    );

    let mut wrong_order = frames.clone();
    wrong_order.swap(0, 1);
    assert_eq!(
        verify_batch(&wrong_order, target.checkpoint, &terminal),
        Err(ContractError::OrderMismatch)
    );

    let mut wrong_inventory = frames.clone();
    if let FrameBody::Terminal {
        inventory_digest, ..
    } = &mut wrong_inventory[redo.len()].body
    {
        inventory_digest[0] ^= 1;
    }
    assert_eq!(
        verify_batch(&wrong_inventory, target.checkpoint, &terminal),
        Err(ContractError::TerminalAuthorityMismatch)
    );

    let mut substituted_page = frames.clone();
    if let FrameBody::Page {
        page, image_digest, ..
    } = &mut substituted_page[0].body
    {
        *page = Page::new(
            page.id(),
            PageKind::StructureNode,
            Some(Csn::FIRST),
            None,
            b"substituted".to_vec(),
        )
        .map_err(|_| ContractError::InvalidPage)?;
        *image_digest = *blake3::hash(&page.encode()).as_bytes();
    }
    assert_eq!(
        verify_batch(&substituted_page, target.checkpoint, &terminal),
        Err(ContractError::InventoryMismatch)
    );

    let other_terminal =
        expected_terminal(target.checkpoint, terminal.transaction_id, &redo, [8; 32])?;
    assert_eq!(
        verify_batch(&frames, target.checkpoint, &other_terminal),
        Err(ContractError::TerminalAuthorityMismatch)
    );

    let mut other_checkpoint = target.checkpoint;
    other_checkpoint.digest[0] ^= 1;
    let cross_checkpoint_terminal = expected_terminal(
        other_checkpoint,
        terminal.transaction_id,
        &redo,
        terminal.target_roots_digest,
    )?;
    assert_eq!(
        verify_batch(&frames, target.checkpoint, &cross_checkpoint_terminal),
        Err(ContractError::TerminalAuthorityMismatch)
    );

    let mut v1_frames = frames;
    v1_frames[0].wal_format = 1;
    assert_eq!(
        verify_batch(&v1_frames, target.checkpoint, &terminal),
        Err(ContractError::UnsupportedBoundary)
    );
    Ok(())
}

#[test]
fn every_complete_frame_group_prefix_without_terminal_is_rejected_before_staging()
-> Result<(), ContractError> {
    let base = pages(1, 2, 1)?;
    let authority = checkpoint_fixture(lineage(1)?, &base)?;
    let mut file = FaultyPageFile::from_pages(PageGeneration::FIRST, &base)?;
    let (mut target, _) = RedoTarget::open(&mut file, &authority)?;
    let redo = pages(3, 7, 2)?;
    let terminal = expected_terminal(
        target.checkpoint,
        TransactionId::new(41).map_err(|_| ContractError::Authority)?,
        &redo,
        [7; 32],
    )?;
    let frames = contract_frames(target.checkpoint, &terminal, &redo);
    let frame_groups = frames
        .chunks(2)
        .map(<[ContractFrame]>::to_vec)
        .collect::<Vec<_>>();
    assert!(frame_groups.len() > 1);

    for complete_groups in 0..frame_groups.len() {
        let prefix = flatten_frame_groups(&frame_groups[..complete_groups]);
        let before = target.file.bytes.clone();
        assert_eq!(
            target.stage(&prefix, &terminal),
            Err(ContractError::MissingTerminal)
        );
        assert_eq!(target.file.bytes, before);
    }
    target.stage(&flatten_frame_groups(&frame_groups), &terminal)?;
    assert_eq!(target.file.complete_page_count()?, 9);
    Ok(())
}

#[test]
fn injected_partial_append_is_repaired_on_reopen_before_complete_replay()
-> Result<(), ContractError> {
    let base = pages(1, 2, 1)?;
    let authority = checkpoint_fixture(lineage(1)?, &base)?;
    let mut file = FaultyPageFile::from_pages(PageGeneration::FIRST, &base)?;
    let redo = pages(3, 3, 2)?;
    let terminal;
    {
        let (mut target, _) = RedoTarget::open(&mut file, &authority)?;
        terminal = expected_terminal(
            target.checkpoint,
            TransactionId::new(51).map_err(|_| ContractError::Authority)?,
            &redo,
            [5; 32],
        )?;
        let frames = contract_frames(target.checkpoint, &terminal, &redo);
        target.file.inject(InjectedFault::AppendAfterBytes(73));
        assert_eq!(
            target.stage(&frames, &terminal),
            Err(ContractError::AppendFailure)
        );
        assert_eq!(
            target.stage(&frames, &terminal),
            Err(ContractError::WriterPoisoned)
        );
    }
    file.crash();
    let (mut reopened, repaired) = RedoTarget::open(&mut file, &authority)?;
    assert_eq!(repaired, 73);
    let frames = contract_frames(reopened.checkpoint, &terminal, &redo);
    reopened.stage(&frames, &terminal)?;
    assert_eq!(reopened.file.complete_page_count()?, 5);
    Ok(())
}

#[test]
fn injected_sync_failure_reopens_repairs_and_verifies_complete_overlap() -> Result<(), ContractError>
{
    let base = pages(1, 2, 1)?;
    let authority = checkpoint_fixture(lineage(1)?, &base)?;
    let mut file = FaultyPageFile::from_pages(PageGeneration::FIRST, &base)?;
    let redo = pages(3, 3, 2)?;
    let terminal;
    {
        let (mut target, _) = RedoTarget::open(&mut file, &authority)?;
        terminal = expected_terminal(
            target.checkpoint,
            TransactionId::new(61).map_err(|_| ContractError::Authority)?,
            &redo,
            [6; 32],
        )?;
        let frames = contract_frames(target.checkpoint, &terminal, &redo);
        target
            .file
            .inject(InjectedFault::SyncWithStableLength(3 * PAGE_SIZE + 91));
        assert_eq!(
            target.stage(&frames, &terminal),
            Err(ContractError::SyncFailure)
        );
        assert_eq!(
            target.stage(&frames, &terminal),
            Err(ContractError::WriterPoisoned)
        );
    }
    file.crash();
    let (mut reopened, repaired) = RedoTarget::open(&mut file, &authority)?;
    assert_eq!(repaired, 91);
    let reads_after_checkpoint = reopened.file.physical_reads;
    let frames = contract_frames(reopened.checkpoint, &terminal, &redo);
    reopened.stage(&frames, &terminal)?;
    assert_eq!(reopened.file.complete_page_count()?, 5);
    assert_eq!(reopened.file.physical_reads, reads_after_checkpoint + 1);
    Ok(())
}

#[test]
fn injected_reopen_truncation_failure_fails_closed_and_retry_repairs() -> Result<(), ContractError>
{
    let base = pages(1, 2, 1)?;
    let authority = checkpoint_fixture(lineage(1)?, &base)?;
    let mut file = FaultyPageFile::from_pages(PageGeneration::FIRST, &base)?;
    let redo = pages(3, 2, 2)?;
    let terminal;
    {
        let (mut target, _) = RedoTarget::open(&mut file, &authority)?;
        terminal = expected_terminal(
            target.checkpoint,
            TransactionId::new(71).map_err(|_| ContractError::Authority)?,
            &redo,
            [7; 32],
        )?;
        let frames = contract_frames(target.checkpoint, &terminal, &redo);
        target.file.inject(InjectedFault::AppendAfterBytes(127));
        assert_eq!(
            target.stage(&frames, &terminal),
            Err(ContractError::AppendFailure)
        );
    }
    file.crash();
    file.inject(InjectedFault::Truncate);
    assert_eq!(
        RedoTarget::open(&mut file, &authority).err(),
        Some(ContractError::TruncateFailure)
    );
    assert!(file.poisoned);
    assert_eq!(file.bytes.len() % PAGE_SIZE, 127);

    file.clear_fault();
    let (mut reopened, repaired) = RedoTarget::open(&mut file, &authority)?;
    assert_eq!(repaired, 127);
    let frames = contract_frames(reopened.checkpoint, &terminal, &redo);
    reopened.stage(&frames, &terminal)?;
    assert_eq!(reopened.file.complete_page_count()?, 4);
    Ok(())
}
