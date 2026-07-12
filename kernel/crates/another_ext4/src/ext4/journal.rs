use super::{journal_recovery, journal_transaction, Ext4};
use crate::constants::BLOCK_SIZE;
use crate::ext4_defs::{Block, BlockDevice, SuperBlock};
use crate::jbd2::Superblock as JournalSuperblock;
use crate::prelude::*;

struct RecoveryDevice<'a> {
    device: &'a dyn BlockDevice,
    journal_blocks: &'a [PBlockId],
    journal_block_set: &'a BTreeSet<PBlockId>,
    filesystem_blocks: u64,
}

impl journal_recovery::JournalRecoveryIo for RecoveryDevice<'_> {
    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    fn filesystem_blocks(&self) -> u64 {
        self.filesystem_blocks
    }

    fn read_journal(&self, logical: u32) -> Result<Vec<u8>> {
        let physical = *self
            .journal_blocks
            .get(logical as usize)
            .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
        Ok(self.device.read_block(physical)?.data.to_vec())
    }

    fn write_journal(&self, logical: u32, data: &[u8]) -> Result<()> {
        let physical = *self
            .journal_blocks
            .get(logical as usize)
            .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
        let image: Box<[u8; BLOCK_SIZE]> = data
            .to_vec()
            .into_boxed_slice()
            .try_into()
            .map_err(|_| Ext4Error::new(ErrCode::EIO))?;
        self.device.write_block(&Block::new(physical, image))
    }

    fn write_home(&self, physical: u64, data: &[u8]) -> Result<()> {
        let image: Box<[u8; BLOCK_SIZE]> = data
            .to_vec()
            .into_boxed_slice()
            .try_into()
            .map_err(|_| Ext4Error::new(ErrCode::EIO))?;
        self.device.write_block(&Block::new(physical, image))
    }

    fn is_journal_physical(&self, physical: u64) -> bool {
        self.journal_block_set.contains(&physical)
    }

    fn flush_home(&self) -> Result<()> {
        self.device.flush()
    }

    fn flush_journal(&self) -> Result<()> {
        self.device.flush()
    }
}

impl Ext4 {
    pub fn supports_reliable_flush(&self) -> bool {
        self.block_device.supports_reliable_flush()
    }

    pub fn flush_device(&self) -> Result<()> {
        self.block_device.flush()
    }

    pub fn shutdown_writable(&self) -> Result<()> {
        let Some(journal) = self.journal.as_ref() else {
            return Ok(());
        };
        if !journal.can_shutdown() {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        // Every synchronous transaction checkpoints and clears s_start before
        // returning. With new writers excluded by VFS umount, it is now safe
        // to clear RECOVER as Linux does at clean shutdown.
        let mut sb = self.read_super_block_cached();
        sb.set_incompatible_feature(SuperBlock::FEATURE_INCOMPAT_RECOVER, false);
        self.write_super_block(&sb)?;
        self.block_device.flush()
    }

    #[allow(dead_code)]
    pub(super) fn transaction_start(
        &self,
        credits: usize,
    ) -> Result<journal_transaction::Transaction<'_>> {
        self.journal
            .as_ref()
            .ok_or_else(|| Ext4Error::new(ErrCode::ENOTSUP))?
            .start(credits)
    }

    pub(super) fn initialize_journal(&mut self) -> Result<()> {
        if !self.block_device.supports_reliable_flush() {
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }
        let mut ext4_sb = self.read_super_block_cached();
        if !ext4_sb.has_compatible_feature(SuperBlock::FEATURE_COMPAT_HAS_JOURNAL)
            || ext4_sb.has_compatible_feature(SuperBlock::FEATURE_COMPAT_FAST_COMMIT)
            || ext4_sb.has_incompatible_feature(SuperBlock::FEATURE_INCOMPAT_JOURNAL_DEV)
            || ext4_sb.journal_device() != 0
            || ext4_sb.journal_inode_number() == 0
        {
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }

        let journal_inode = self.read_inode_uncached(ext4_sb.journal_inode_number())?;
        let journal_size = journal_inode.inode.size();
        let journal_len_u64 = journal_size / BLOCK_SIZE as u64;
        if journal_size % BLOCK_SIZE as u64 != 0
            || journal_len_u64 == 0
            || journal_len_u64 > u32::MAX as u64
            || journal_len_u64 > ext4_sb.block_count()
        {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        let journal_zero = self.extent_query(&journal_inode, 0)?;
        if journal_zero == 0 || journal_zero >= ext4_sb.block_count() {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        let raw_journal_sb = self.block_device.read_block(journal_zero)?;
        let mut journal_sb = JournalSuperblock::parse(&raw_journal_sb.data[..], BLOCK_SIZE as u32)?;
        let ext4_journal_uuid = ext4_sb.journal_uuid();
        if journal_sb.max_len as u64 != journal_len_u64
            || (ext4_journal_uuid != [0; 16] && journal_sb.uuid != ext4_journal_uuid)
        {
            return Err(Ext4Error::new(ErrCode::EIO));
        }

        let journal_len = journal_sb.max_len as usize;
        let mut mapping = Vec::new();
        mapping
            .try_reserve_exact(journal_len)
            .map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
        let mut journal_blocks = BTreeSet::new();
        for logical in 0..journal_len {
            let physical = self.extent_query(&journal_inode, logical as u32)?;
            if physical == 0
                || physical >= ext4_sb.block_count()
                || !journal_blocks.insert(physical)
            {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            mapping.push(physical);
        }

        let needs_recovery = ext4_sb.has_incompatible_feature(SuperBlock::FEATURE_INCOMPAT_RECOVER);
        if journal_sb.start != 0 && !needs_recovery {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        if needs_recovery {
            let io = RecoveryDevice {
                device: self.block_device.as_ref(),
                journal_blocks: &mapping,
                journal_block_set: &journal_blocks,
                filesystem_blocks: ext4_sb.block_count(),
            };
            journal_recovery::recover(&io)?;
            let recovered_block0 = self.block_device.read_block(0)?;
            let recovered_sb: SuperBlock =
                recovered_block0.read_offset_as(crate::constants::BASE_OFFSET);
            if !recovered_sb.check_magic()
                || recovered_sb.inode_size() != crate::constants::SB_GOOD_INODE_SIZE
                || recovered_sb.desc_size() != crate::constants::SB_GOOD_DESC_SIZE
            {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            ext4_sb = recovered_sb;
            *self.cached_super_block.lock() = recovered_sb;
            let refreshed = self.block_device.read_block(mapping[0])?;
            journal_sb = JournalSuperblock::parse(&refreshed.data[..], BLOCK_SIZE as u32)?;
            self.inode_cache.lock().entries.clear();
            for (index, cached) in self.cached_block_groups.iter().enumerate() {
                let sb = self.read_super_block_cached();
                let per_block = BLOCK_SIZE as u32 / sb.desc_size() as u32;
                let block_id = sb.first_data_block() + index as u32 / per_block + 1;
                let offset = (index as u32 % per_block) * sb.desc_size() as u32;
                let block = self.block_device.read_block(block_id as PBlockId)?;
                *cached.lock() = block.read_offset_as(offset as usize);
            }
        }

        // Linux keeps RECOVER set for the complete read-write mount lifetime.
        if !needs_recovery {
            ext4_sb.set_incompatible_feature(SuperBlock::FEATURE_INCOMPAT_RECOVER, true);
            self.write_super_block(&ext4_sb)?;
            self.block_device.flush()?;
        }

        let mut image = Box::new([0u8; 1024]);
        image.copy_from_slice(&self.block_device.read_block(mapping[0])?.data[..1024]);
        let head = if journal_sb.start == 0 {
            journal_sb.first
        } else {
            return Err(Ext4Error::new(ErrCode::EIO));
        };
        self.journal = Some(journal_transaction::JournalTransactionCore::new(
            journal_transaction::JournalContext {
                superblock: journal_sb,
                logical_blocks: mapping.into(),
                journal_blocks: Arc::new(journal_blocks),
                target_blocks: ext4_sb.block_count(),
                head,
                superblock_image: image,
            },
        )?);
        Ok(())
    }
}

impl journal_transaction::CachePublisher for Ext4 {
    fn publish(&self, blocks: &[&journal_transaction::StagedBlock]) {
        // Transactional inode and directory mutations must not leave value
        // snapshots from before the checkpoint visible after commit.
        let changed = BTreeSet::from_iter(blocks.iter().map(|block| block.home()));
        self.inode_cache.lock().entries.retain(|inode_id, _| {
            self.inode_disk_pos(*inode_id)
                .map(|(block, _)| !changed.contains(&block))
                .unwrap_or(false)
        });
    }
}
