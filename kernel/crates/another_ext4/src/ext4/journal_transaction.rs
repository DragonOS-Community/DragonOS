//! Synchronous, single-writer JBD2 transaction core.
//!
//! This module deliberately does not discover the journal inode.  Mount code
//! must validate the journal superblock and provide the complete logical to
//! physical block map.  Keeping mapping outside the commit path also ensures
//! that no filesystem spin lock is held while block I/O is in flight.
#![allow(dead_code)] // Activated only after every production metadata writer uses handles.

use crate::constants::BLOCK_SIZE;
use crate::ext4_defs::{Block, BlockDevice};
use crate::jbd2::{
    block_checksum, checksum_seed, commit_checksum, tag_checksum, BlockType, ChecksumMode,
    Features, Header, Superblock, BLOCK_TAIL_BYTES, CRC32C_CHKSUM, FLAG_ESCAPE, FLAG_LAST_TAG,
    FLAG_SAME_UUID, HEADER_BYTES, MAGIC,
};
use crate::prelude::*;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

const CLEAN: u8 = 0;
const POISONED: u8 = 1;

/// A fully validated journal mapping and its current allocation cursor.
///
/// `logical_blocks[n]` is the filesystem physical block containing journal
/// logical block `n`.  Entry zero is the JBD2 superblock.
pub struct JournalContext {
    pub superblock: Superblock,
    pub logical_blocks: Arc<[PBlockId]>,
    pub journal_blocks: Arc<BTreeSet<PBlockId>>,
    /// Number of addressable blocks on the filesystem device.
    pub target_blocks: u64,
    /// Next journal logical block to allocate.  It must be in the data ring.
    pub head: u32,
    /// Exact 1024-byte JBD2 superblock image read at mount.
    pub superblock_image: Box<[u8; 1024]>,
}

impl JournalContext {
    pub fn validate(&self) -> Result<()> {
        let sb = &self.superblock;
        if sb.block_size as usize != BLOCK_SIZE
            || (sb.features.checksum == ChecksumMode::V3 && sb.checksum_type != CRC32C_CHKSUM)
            || self.logical_blocks.len() != sb.max_len as usize
            || self.journal_blocks.len() != self.logical_blocks.len()
            || self.head < sb.first
            || self.head >= sb.max_len
            || self.target_blocks == 0
        {
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }
        if self
            .logical_blocks
            .iter()
            .any(|block| *block == 0 || !self.journal_blocks.contains(block))
        {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        Ok(())
    }
}

/// A transaction-private metadata image.  It is intentionally not `Clone`:
/// callers cannot retain a second publishable copy past commit/abort.
pub struct StagedBlock {
    home: PBlockId,
    image: Box<[u8; BLOCK_SIZE]>,
}

impl StagedBlock {
    pub fn home(&self) -> PBlockId {
        self.home
    }

    pub fn bytes(&self) -> &[u8; BLOCK_SIZE] {
        &self.image
    }
}

/// Cache changes are published only after checkpoint data is durable.
pub trait CachePublisher: Send + Sync {
    /// Publish already-checkpointed images to in-memory caches.
    ///
    /// This callback runs after the home-block flush, so it must not allocate,
    /// perform I/O, or otherwise fail.  Borrowing the transaction's map also
    /// lets publishers inspect the final image for a particular home block
    /// without building a temporary collection.
    fn publish(&self, blocks: &BTreeMap<PBlockId, StagedBlock>);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitFailure {
    /// No commit record was issued; recovery will ignore any log fragments.
    BeforeCommit,
    /// Commit write or its durability flush failed, so commit is uncertain.
    CommitUncertain,
    /// The transaction committed, but home blocks are not known durable.
    CheckpointFailed,
    /// Home blocks are durable, but the journal tail was not durably cleared.
    TailUpdateFailed,
}

#[derive(Debug)]
pub struct CommitError {
    pub error: Ext4Error,
    pub failure: CommitFailure,
}

/// Owns the single-writer token and poison state.  The token is held only as
/// an atomic bit; no spin guard crosses a block-device operation.
pub struct JournalTransactionCore {
    writer: AtomicBool,
    poison: AtomicU8,
    context: spin::Mutex<JournalContext>,
}

impl JournalTransactionCore {
    pub fn new(context: JournalContext) -> Result<Self> {
        context.validate()?;
        Ok(Self {
            writer: AtomicBool::new(false),
            poison: AtomicU8::new(CLEAN),
            context: spin::Mutex::new(context),
        })
    }

    pub fn is_poisoned(&self) -> bool {
        self.poison.load(Ordering::Acquire) != CLEAN
    }

    pub fn can_shutdown(&self) -> bool {
        !self.writer.load(Ordering::Acquire) && !self.is_poisoned()
    }

    pub fn owns_block_range(&self, start: PBlockId, end: PBlockId) -> bool {
        start < end
            && self
                .context
                .lock()
                .journal_blocks
                .range(start..end)
                .next()
                .is_some()
    }

    pub fn start(&self, credits: usize) -> Result<Transaction<'_>> {
        if credits == 0 || self.is_poisoned() {
            return Err(Ext4Error::new(if credits == 0 {
                ErrCode::EINVAL
            } else {
                ErrCode::EROFS
            }));
        }
        if self
            .writer
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(Ext4Error::new(ErrCode::EAGAIN));
        }

        // Reserve against a strict upper bound before any mutation is staged.
        let reservation = {
            let context = self.context.lock();
            required_log_blocks(credits, context.superblock.features).and_then(|needed| {
                ring_len(&context.superblock).map(|available| needed <= available)
            })
        };
        let fits = match reservation {
            Ok(fits) => fits,
            Err(error) => {
                self.writer.store(false, Ordering::Release);
                return Err(error);
            }
        };
        if !fits {
            self.writer.store(false, Ordering::Release);
            return Err(Ext4Error::new(ErrCode::E2BIG));
        }
        Ok(Transaction {
            core: self,
            credits,
            staged: BTreeMap::new(),
            owns_writer: true,
        })
    }

    fn poison(&self) {
        self.poison.store(POISONED, Ordering::Release);
    }
}

pub struct Transaction<'a> {
    core: &'a JournalTransactionCore,
    credits: usize,
    staged: BTreeMap<PBlockId, StagedBlock>,
    owns_writer: bool,
}

impl Transaction<'_> {
    /// Replace the final image for `home`.  Re-staging the same home block does
    /// not consume another credit and subsequent reads observe the replacement.
    pub fn stage(&mut self, home: PBlockId, image: Box<[u8; BLOCK_SIZE]>) -> Result<()> {
        if !self.staged.contains_key(&home) && self.staged.len() == self.credits {
            return Err(Ext4Error::new(ErrCode::E2BIG));
        }
        self.staged.insert(home, StagedBlock { home, image });
        Ok(())
    }

    /// Return the transaction-private final image of `home` for mutation.
    ///
    /// The first access snapshots the device block and consumes one credit;
    /// later accesses return the same image, providing read-your-writes and
    /// naturally merging updates to shared metadata blocks.
    pub fn read_for_update<'a>(
        &'a mut self,
        device: &dyn BlockDevice,
        home: PBlockId,
    ) -> Result<&'a mut [u8; BLOCK_SIZE]> {
        if !self.staged.contains_key(&home) {
            if self.staged.len() == self.credits {
                return Err(Ext4Error::new(ErrCode::E2BIG));
            }
            let block = device.read_block(home)?;
            self.staged.insert(
                home,
                StagedBlock {
                    home,
                    image: block.data,
                },
            );
        }
        Ok(self
            .staged
            .get_mut(&home)
            .expect("staged block was just inserted")
            .image
            .as_mut())
    }

    pub fn read<'a>(&'a self, device: &dyn BlockDevice, home: PBlockId) -> Result<BlockView<'a>> {
        if let Some(block) = self.staged.get(&home) {
            Ok(BlockView::Staged(block.bytes()))
        } else {
            Ok(BlockView::Device(device.read_block(home)?))
        }
    }

    pub fn abort(mut self) {
        self.release_writer();
    }

    pub fn commit(
        mut self,
        device: &dyn BlockDevice,
        publisher: &dyn CachePublisher,
    ) -> core::result::Result<(), CommitError> {
        if self.staged.is_empty() {
            self.release_writer();
            return Ok(());
        }
        if !device.supports_reliable_flush() {
            return self.fail(
                Ext4Error::new(ErrCode::ENOTSUP),
                CommitFailure::BeforeCommit,
                false,
            );
        }

        // Copy the small allocation state, then release the spin guard before I/O.
        let (sb, mapping, journal_blocks, target_blocks, head, sb_image) = {
            let context = self.core.context.lock();
            (
                context.superblock,
                Arc::clone(&context.logical_blocks),
                Arc::clone(&context.journal_blocks),
                context.target_blocks,
                context.head,
                context.superblock_image.clone(),
            )
        };
        let needed =
            required_log_blocks(self.staged.len(), sb.features).map_err(|error| CommitError {
                error,
                failure: CommitFailure::BeforeCommit,
            })?;
        if needed
            > ring_len(&sb).map_err(|error| CommitError {
                error,
                failure: CommitFailure::BeforeCommit,
            })?
        {
            return self.fail(
                Ext4Error::new(ErrCode::E2BIG),
                CommitFailure::BeforeCommit,
                false,
            );
        }

        let sequence = sb.sequence;
        let positions = ring_positions(&sb, head, needed).map_err(|error| CommitError {
            error,
            failure: CommitFailure::BeforeCommit,
        })?;
        if self
            .staged
            .values()
            .any(|block| block.home >= target_blocks || journal_blocks.contains(&block.home))
        {
            return self.fail(
                Ext4Error::new(ErrCode::EINVAL),
                CommitFailure::BeforeCommit,
                false,
            );
        }
        // Finish every fallible format operation before the first write.  Once
        // the active tail reaches disk, all remaining failures are I/O failures
        // which must poison the mount.
        let encoded = encode_log(&sb, sequence, &self.staged).map_err(|error| CommitError {
            error,
            failure: CommitFailure::BeforeCommit,
        })?;
        let commit = encode_commit(&sb, sequence).map_err(|error| CommitError {
            error,
            failure: CommitFailure::BeforeCommit,
        })?;
        let mut active_sb_image = sb_image.clone();
        update_superblock(&mut active_sb_image, sequence, head, &sb).map_err(|error| {
            CommitError {
                error,
                failure: CommitFailure::BeforeCommit,
            }
        })?;
        let next_sequence = sequence.wrapping_add(1);
        let mut clean_sb_image = sb_image;
        update_superblock(&mut clean_sb_image, next_sequence, 0, &sb).map_err(|error| {
            CommitError {
                error,
                failure: CommitFailure::BeforeCommit,
            }
        })?;

        // Publish an active tail before log payload.  Recovery may safely scan
        // an empty/uncommitted transaction after a crash at this point.
        if let Err(error) = write_journal_superblock(device, &mapping, &active_sb_image)
            .and_then(|_| device.flush())
        {
            return self.fail(error, CommitFailure::BeforeCommit, true);
        }

        debug_assert_eq!(encoded.len() + 1, positions.len());
        for (logical, bytes) in positions[..encoded.len()].iter().zip(encoded.iter()) {
            if let Err(error) = write_bytes(device, mapping[*logical as usize], bytes) {
                return self.fail(error, CommitFailure::BeforeCommit, true);
            }
        }
        if let Err(error) = device.flush() {
            return self.fail(error, CommitFailure::BeforeCommit, true);
        }

        let commit_logical = *positions.last().unwrap();
        if let Err(error) = write_bytes(device, mapping[commit_logical as usize], &commit) {
            return self.fail(error, CommitFailure::CommitUncertain, true);
        }
        if let Err(error) = device.flush() {
            return self.fail(error, CommitFailure::CommitUncertain, true);
        }

        for staged in self.staged.values() {
            if let Err(error) = write_bytes(device, staged.home, staged.bytes()) {
                return self.fail(error, CommitFailure::CheckpointFailed, true);
            }
        }
        if let Err(error) = device.flush() {
            return self.fail(error, CommitFailure::CheckpointFailed, true);
        }
        publisher.publish(&self.staged);

        if let Err(error) =
            write_journal_superblock(device, &mapping, &clean_sb_image).and_then(|_| device.flush())
        {
            return self.fail(error, CommitFailure::TailUpdateFailed, true);
        }
        {
            let mut context = self.core.context.lock();
            context.superblock.sequence = next_sequence;
            context.superblock.start = 0;
            context.head = ring_next(&sb, commit_logical);
            context.superblock_image = clean_sb_image;
        }
        self.release_writer();
        Ok(())
    }

    fn fail<T>(
        &mut self,
        error: Ext4Error,
        failure: CommitFailure,
        poison: bool,
    ) -> core::result::Result<T, CommitError> {
        if poison {
            self.core.poison();
        }
        self.release_writer();
        Err(CommitError { error, failure })
    }

    fn release_writer(&mut self) {
        if self.owns_writer {
            self.owns_writer = false;
            self.core.writer.store(false, Ordering::Release);
        }
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        self.release_writer();
    }
}

pub enum BlockView<'a> {
    Staged(&'a [u8; BLOCK_SIZE]),
    Device(Block),
}

impl core::ops::Deref for BlockView<'_> {
    type Target = [u8; BLOCK_SIZE];
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Staged(bytes) => bytes,
            Self::Device(block) => &block.data,
        }
    }
}

fn ring_len(sb: &Superblock) -> Result<usize> {
    sb.max_len
        .checked_sub(sb.first)
        .map(|n| n as usize)
        .ok_or_else(|| Ext4Error::new(ErrCode::EIO))
}

fn tags_per_descriptor(features: Features) -> Result<usize> {
    let tail = if features.checksum == ChecksumMode::V3 {
        BLOCK_TAIL_BYTES
    } else {
        0
    };
    let overhead = HEADER_BYTES + 16 + tail;
    let available = BLOCK_SIZE
        .checked_sub(overhead)
        .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
    let tags = available / features.tag_bytes();
    if tags == 0 {
        Err(Ext4Error::new(ErrCode::EIO))
    } else {
        Ok(tags)
    }
}

fn required_log_blocks(blocks: usize, features: Features) -> Result<usize> {
    let descriptors = blocks
        .checked_add(tags_per_descriptor(features)? - 1)
        .ok_or_else(|| Ext4Error::new(ErrCode::E2BIG))?
        / tags_per_descriptor(features)?;
    blocks
        .checked_add(descriptors)
        .and_then(|n| n.checked_add(1))
        .ok_or_else(|| Ext4Error::new(ErrCode::E2BIG))
}

fn ring_next(sb: &Superblock, current: u32) -> u32 {
    if current + 1 == sb.max_len {
        sb.first
    } else {
        current + 1
    }
}

fn ring_positions(sb: &Superblock, head: u32, count: usize) -> Result<Vec<u32>> {
    if count > ring_len(sb)? || head < sb.first || head >= sb.max_len {
        return Err(Ext4Error::new(ErrCode::E2BIG));
    }
    let mut result = Vec::with_capacity(count);
    let mut current = head;
    for _ in 0..count {
        result.push(current);
        current = ring_next(sb, current);
    }
    Ok(result)
}

fn encode_log(
    sb: &Superblock,
    sequence: u32,
    staged: &BTreeMap<PBlockId, StagedBlock>,
) -> Result<Vec<Box<[u8; BLOCK_SIZE]>>> {
    let seed = checksum_seed(&sb.uuid);
    let per_descriptor = tags_per_descriptor(sb.features)?;
    let all = staged.values().collect::<Vec<_>>();
    let mut output = Vec::new();
    for group in all.chunks(per_descriptor) {
        let mut descriptor = Box::new([0; BLOCK_SIZE]);
        Header {
            block_type: BlockType::Descriptor,
            sequence,
        }
        .encode_into(&mut descriptor[..])?;
        let mut journal_data = Vec::with_capacity(group.len());
        let mut offset = HEADER_BYTES;
        for (index, staged) in group.iter().enumerate() {
            let mut image = staged.image.clone();
            let mut flags = if index != 0 { FLAG_SAME_UUID } else { 0 };
            if u32::from_be_bytes(
                image[..4]
                    .try_into()
                    .map_err(|_| Ext4Error::new(ErrCode::EIO))?,
            ) == MAGIC
            {
                image[..4].fill(0);
                flags |= FLAG_ESCAPE;
            }
            if index + 1 == group.len() {
                flags |= FLAG_LAST_TAG;
            }
            descriptor[offset..offset + 4].copy_from_slice(&(staged.home as u32).to_be_bytes());
            match sb.features.checksum {
                ChecksumMode::V3 => {
                    if !sb.features.has_64bit && staged.home > u32::MAX as u64 {
                        return Err(Ext4Error::new(ErrCode::ENOTSUP));
                    }
                    descriptor[offset + 4..offset + 8].copy_from_slice(&flags.to_be_bytes());
                    descriptor[offset + 8..offset + 12]
                        .copy_from_slice(&((staged.home >> 32) as u32).to_be_bytes());
                    descriptor[offset + 12..offset + 16]
                        .copy_from_slice(&tag_checksum(seed, sequence, &image[..]).to_be_bytes());
                }
                ChecksumMode::None => {
                    if staged.home > u32::MAX as u64 {
                        return Err(Ext4Error::new(ErrCode::ENOTSUP));
                    }
                    descriptor[offset + 6..offset + 8]
                        .copy_from_slice(&(flags as u16).to_be_bytes());
                }
            }
            offset += sb.features.tag_bytes();
            if index == 0 {
                descriptor[offset..offset + 16].copy_from_slice(&sb.uuid);
                offset += 16;
            }
            journal_data.push(image);
        }
        if sb.features.checksum == ChecksumMode::V3 {
            let checksum = block_checksum(seed, &descriptor[..])?;
            descriptor[BLOCK_SIZE - 4..].copy_from_slice(&checksum.to_be_bytes());
        }
        output.push(descriptor);
        output.extend(journal_data);
    }
    Ok(output)
}

fn encode_commit(sb: &Superblock, sequence: u32) -> Result<Box<[u8; BLOCK_SIZE]>> {
    let mut block = Box::new([0; BLOCK_SIZE]);
    Header {
        block_type: BlockType::Commit,
        sequence,
    }
    .encode_into(&mut block[..])?;
    if sb.features.checksum == ChecksumMode::V3 {
        let checksum = commit_checksum(checksum_seed(&sb.uuid), &block[..])?;
        block[16..20].copy_from_slice(&checksum.to_be_bytes());
    }
    Ok(block)
}

fn update_superblock(
    image: &mut [u8; 1024],
    sequence: u32,
    start: u32,
    sb: &Superblock,
) -> Result<()> {
    image[24..28].copy_from_slice(&sequence.to_be_bytes());
    image[28..32].copy_from_slice(&start.to_be_bytes());
    if sb.features.checksum == ChecksumMode::V3 {
        image[252..256].fill(0);
        let checksum = crate::jbd2::superblock_checksum(image)?;
        image[252..256].copy_from_slice(&checksum.to_be_bytes());
    }
    Ok(())
}

fn write_journal_superblock(
    device: &dyn BlockDevice,
    mapping: &[PBlockId],
    image: &[u8; 1024],
) -> Result<()> {
    let mut block = device.read_block(mapping[0])?;
    block.data[..1024].copy_from_slice(image);
    device.write_block(&block)
}

fn write_bytes(device: &dyn BlockDevice, id: PBlockId, bytes: &[u8; BLOCK_SIZE]) -> Result<()> {
    device.write_block(&Block::new(id, Box::new(*bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicUsize, Ordering};

    struct MemoryDevice {
        volatile: spin::Mutex<BTreeMap<PBlockId, Box<[u8; BLOCK_SIZE]>>>,
        stable: spin::Mutex<BTreeMap<PBlockId, Box<[u8; BLOCK_SIZE]>>>,
        operation: AtomicUsize,
        fail_at: AtomicUsize,
    }

    impl MemoryDevice {
        fn new() -> Self {
            Self {
                volatile: spin::Mutex::new(BTreeMap::new()),
                stable: spin::Mutex::new(BTreeMap::new()),
                operation: AtomicUsize::new(0),
                fail_at: AtomicUsize::new(usize::MAX),
            }
        }

        fn step(&self) -> Result<()> {
            let operation = self.operation.fetch_add(1, Ordering::SeqCst);
            if operation == self.fail_at.load(Ordering::SeqCst) {
                Err(Ext4Error::new(ErrCode::EIO))
            } else {
                Ok(())
            }
        }

        /// Simulate power loss: only the last completed flush epoch survives.
        fn crash(&self) {
            *self.volatile.lock() = self.stable.lock().clone();
        }

        fn stable_block(&self, id: PBlockId) -> Option<Box<[u8; BLOCK_SIZE]>> {
            self.stable.lock().get(&id).cloned()
        }
    }

    impl BlockDevice for MemoryDevice {
        fn read_block(&self, block_id: PBlockId) -> Result<Block> {
            Ok(Block::new(
                block_id,
                self.volatile
                    .lock()
                    .get(&block_id)
                    .cloned()
                    .unwrap_or_else(|| Box::new([0; BLOCK_SIZE])),
            ))
        }

        fn write_block(&self, block: &Block) -> Result<()> {
            self.step()?;
            self.volatile.lock().insert(block.id, block.data.clone());
            Ok(())
        }

        fn flush(&self) -> Result<()> {
            self.step()?;
            *self.stable.lock() = self.volatile.lock().clone();
            Ok(())
        }

        fn supports_reliable_flush(&self) -> bool {
            true
        }
    }

    struct Publisher(AtomicUsize);
    impl CachePublisher for Publisher {
        fn publish(&self, blocks: &BTreeMap<PBlockId, StagedBlock>) {
            self.0.fetch_add(blocks.len(), Ordering::SeqCst);
        }
    }

    #[test]
    fn read_for_update_merges_shared_block_and_charges_one_credit() {
        let device = MemoryDevice::new();
        let core = JournalTransactionCore::new(context()).unwrap();
        let mut transaction = core.start(1).unwrap();

        transaction.read_for_update(&device, 42).unwrap()[10] = 1;
        transaction.read_for_update(&device, 42).unwrap()[11] = 2;
        assert_eq!(transaction.read(&device, 42).unwrap()[10..12], [1, 2]);
        assert_eq!(
            transaction.read_for_update(&device, 43).unwrap_err().code(),
            ErrCode::E2BIG
        );
    }

    fn context() -> JournalContext {
        let features = Features::validate(
            0,
            crate::jbd2::FEATURE_INCOMPAT_CSUM_V3 | crate::jbd2::FEATURE_INCOMPAT_64BIT,
            0,
        )
        .unwrap();
        let mut image = Box::new([0; 1024]);
        Header {
            block_type: BlockType::SuperblockV2,
            sequence: 7,
        }
        .encode_into(&mut image[..])
        .unwrap();
        image[12..16].copy_from_slice(&(BLOCK_SIZE as u32).to_be_bytes());
        image[16..20].copy_from_slice(&8u32.to_be_bytes());
        image[20..24].copy_from_slice(&1u32.to_be_bytes());
        image[24..28].copy_from_slice(&7u32.to_be_bytes());
        image[40..44].copy_from_slice(
            &(crate::jbd2::FEATURE_INCOMPAT_CSUM_V3 | crate::jbd2::FEATURE_INCOMPAT_64BIT)
                .to_be_bytes(),
        );
        image[48..64].copy_from_slice(b"0123456789abcdef");
        image[80] = CRC32C_CHKSUM;
        update_superblock(
            &mut image,
            7,
            0,
            &Superblock {
                block_size: BLOCK_SIZE as u32,
                max_len: 8,
                first: 1,
                sequence: 7,
                start: 0,
                errno: 0,
                features,
                uuid: *b"0123456789abcdef",
                checksum_type: CRC32C_CHKSUM,
            },
        )
        .unwrap();
        JournalContext {
            superblock: Superblock::parse(&image[..], BLOCK_SIZE as u32).unwrap(),
            logical_blocks: Vec::from_iter(100..108).into(),
            journal_blocks: Arc::new(BTreeSet::from_iter(100..108)),
            target_blocks: 1000,
            head: 7,
            superblock_image: image,
        }
    }

    #[test]
    fn encodes_escape_and_wraps_ring_without_losing_home_image() {
        let device = MemoryDevice::new();
        let core = JournalTransactionCore::new(context()).unwrap();
        let publisher = Publisher(AtomicUsize::new(0));
        let mut transaction = core.start(1).unwrap();
        let mut image = Box::new([0x5a; BLOCK_SIZE]);
        image[..4].copy_from_slice(&MAGIC.to_be_bytes());
        transaction.stage(42, image).unwrap();
        transaction.commit(&device, &publisher).unwrap();

        // head=7: descriptor at 7, escaped data wraps to 1, commit at 2.
        assert_eq!(&device.stable_block(101).unwrap()[..4], &[0, 0, 0, 0]);
        assert_eq!(&device.stable_block(42).unwrap()[..4], &MAGIC.to_be_bytes());
        assert_eq!(publisher.0.load(Ordering::SeqCst), 1);
        assert!(!core.is_poisoned());
    }

    #[test]
    fn commit_flush_failure_never_publishes_or_checkpoints() {
        let device = MemoryDevice::new();
        // active sb write+flush, descriptor+data writes+flush, commit write,
        // then fail the commit-point flush (zero-based operation 6).
        device.fail_at.store(6, Ordering::SeqCst);
        let core = JournalTransactionCore::new(context()).unwrap();
        let publisher = Publisher(AtomicUsize::new(0));
        let mut transaction = core.start(1).unwrap();
        transaction.stage(42, Box::new([0x33; BLOCK_SIZE])).unwrap();
        let error = transaction.commit(&device, &publisher).unwrap_err();
        assert_eq!(error.failure, CommitFailure::CommitUncertain);
        assert!(core.is_poisoned());
        assert_eq!(publisher.0.load(Ordering::SeqCst), 0);
        device.crash();
        assert!(device.stable_block(42).is_none());
    }

    #[test]
    fn staging_is_deduplicated_and_reads_its_latest_write() {
        let device = MemoryDevice::new();
        let core = JournalTransactionCore::new(context()).unwrap();
        let mut transaction = core.start(1).unwrap();
        transaction.stage(42, Box::new([1; BLOCK_SIZE])).unwrap();
        transaction.stage(42, Box::new([2; BLOCK_SIZE])).unwrap();
        assert_eq!(transaction.read(&device, 42).unwrap()[0], 2);
        transaction.abort();
        assert!(core.start(1).is_ok());
    }
}
