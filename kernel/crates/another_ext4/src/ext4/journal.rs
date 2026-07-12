use super::{journal_recovery, journal_transaction, Ext4};
use crate::constants::BLOCK_SIZE;
use crate::ext4_defs::{AsBytes, Block, BlockDevice, InodeRef, SuperBlock};
use crate::jbd2::Superblock as JournalSuperblock;
use crate::prelude::*;

fn validate_journal_inode(sb: &SuperBlock, inode: &InodeRef) -> Result<()> {
    if !inode.inode.is_file()
        || !inode.inode.uses_extents()
        || (sb.has_read_only_compatible_feature(SuperBlock::FEATURE_RO_COMPAT_METADATA_CSUM)
            && !inode.verify_checksum(&sb.uuid()))
    {
        return Err(Ext4Error::new(ErrCode::EIO));
    }
    Ok(())
}

fn journal_replay_identity_matches(
    before: &InodeRef,
    after: &InodeRef,
    before_mapping: &[PBlockId],
    after_mapping: &[PBlockId],
) -> bool {
    after.id == before.id
        && after.inode.generation() == before.inode.generation()
        && after.inode.mode() == before.inode.mode()
        && after.inode.size() == before.inode.size()
        && after_mapping == before_mapping
}

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
    fn map_validated_journal_inode(
        &self,
        inode: &InodeRef,
        journal_len: usize,
        filesystem_blocks: u64,
    ) -> Result<(Vec<PBlockId>, BTreeSet<PBlockId>)> {
        self.validate_complete_extent_tree(inode)?;
        if self.extent_next_data_lblock(inode)? as usize != journal_len {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        let mut mapping = Vec::new();
        mapping
            .try_reserve_exact(journal_len)
            .map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
        let mut blocks = BTreeSet::new();
        for logical in 0..journal_len {
            let physical = self.extent_query(inode, logical as u32)?;
            if physical == 0 || physical >= filesystem_blocks || !blocks.insert(physical) {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            mapping.push(physical);
        }
        Ok((mapping, blocks))
    }

    pub fn supports_reliable_flush(&self) -> bool {
        self.block_device.supports_reliable_flush()
    }

    pub fn flush_device(&self) -> Result<()> {
        self.block_device.flush()
    }

    pub fn shutdown_writable(&self) -> Result<()> {
        // Clearing RECOVER is a metadata state transition and must not race a
        // direct writer even if the VFS normally excludes writers at umount.
        let _metadata_guard = self.lock_transactional_metadata_mutation()?;
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
        // Unsupported orphan-file states must be rejected before replay can
        // write home blocks or this mount can set RECOVER.
        self.writable_orphan_preflight()?;
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
        validate_journal_inode(&ext4_sb, &journal_inode)?;
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
        let (mapping, journal_blocks) =
            self.map_validated_journal_inode(&journal_inode, journal_len, ext4_sb.block_count())?;

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
            let recovered_groups =
                Self::read_validated_block_groups(self.block_device.as_ref(), &recovered_sb)?;
            let recovered_system_ranges =
                Self::build_system_metadata_ranges(&recovered_sb, &recovered_groups)?;
            if mapping
                .iter()
                .any(|physical| *physical >= recovered_sb.block_count())
            {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            ext4_sb = recovered_sb;
            *self.cached_super_block.lock() = recovered_sb;
            self.cached_block_groups = recovered_groups;
            self.system_metadata_ranges = recovered_system_ranges;
            let recovered_journal_inode =
                self.read_inode_uncached(recovered_sb.journal_inode_number())?;
            validate_journal_inode(&recovered_sb, &recovered_journal_inode)?;
            let (recovered_mapping, recovered_journal_blocks) = self.map_validated_journal_inode(
                &recovered_journal_inode,
                journal_len,
                recovered_sb.block_count(),
            )?;
            if !journal_replay_identity_matches(
                &journal_inode,
                &recovered_journal_inode,
                &mapping,
                &recovered_mapping,
            ) || recovered_journal_blocks != journal_blocks
            {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            let refreshed = self.block_device.read_block(mapping[0])?;
            journal_sb = JournalSuperblock::parse(&refreshed.data[..], BLOCK_SIZE as u32)?;
            self.inode_cache.lock().entries.clear();
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

        // Replay may itself publish a newer ext4 superblock.  Recheck the
        // recovered feature set before interpreting or mutating orphan state;
        // the earlier preflight is still required to prevent replay writes on
        // a format that was already unsupported at mount entry.
        self.writable_orphan_preflight()?;

        // Linux keeps RECOVER set for the complete read-write mount lifetime.
        // Publish it before the first new journal transaction. A later orphan
        // validation or cleanup error deliberately leaves RECOVER set.
        if !needs_recovery {
            ext4_sb.set_incompatible_feature(SuperBlock::FEATURE_INCOMPAT_RECOVER, true);
            self.write_super_block(&ext4_sb)?;
            self.block_device.flush()?;
        }

        // Hold the transactional metadata barrier from the complete read-only
        // orphan snapshot through every cleanup commit.
        self.cleanup_legacy_orphan_chain()?;
        Ok(())
    }

    pub(super) fn journal_owns_block_range(&self, start: PBlockId, end: PBlockId) -> bool {
        self.journal
            .as_ref()
            .is_some_and(|journal| journal.owns_block_range(start, end))
    }
}

impl journal_transaction::CachePublisher for Ext4 {
    fn publish(&self, blocks: &BTreeMap<PBlockId, journal_transaction::StagedBlock>) {
        // Normal transactions change descriptor counters/checksums, never the
        // validated bitmap/table addresses. Therefore system_metadata_ranges
        // remains immutable here; journal replay is the only path which can
        // replace the complete SB/BGD snapshot and rebuilds it explicitly.
        // Home blocks are durable at this point.  Decode cacheable value
        // snapshots directly from the transaction-owned images; all updates
        // below are infallible and allocation-free.
        if let Some(block0) = blocks.get(&0) {
            let sb = SuperBlock::from_bytes(&block0.bytes()[crate::constants::BASE_OFFSET..]);
            *self.cached_super_block.lock() = sb;
        }

        let sb = self.read_super_block_cached();
        let per_block = BLOCK_SIZE as u32 / sb.desc_size() as u32;
        for (index, cached) in self.cached_block_groups.iter().enumerate() {
            let block_id = sb.first_data_block() + index as u32 / per_block + 1;
            let Some(image) = blocks.get(&(block_id as PBlockId)) else {
                continue;
            };
            let offset = (index as u32 % per_block) * sb.desc_size() as u32;
            *cached.lock() =
                crate::ext4_defs::BlockGroupDesc::from_bytes(&image.bytes()[offset as usize..]);
        }

        // Transactional inode and directory mutations must not leave value
        // snapshots from before the checkpoint visible after commit.
        self.inode_cache.lock().entries.retain(|inode_id, _| {
            self.inode_disk_pos(*inode_id)
                .map(|(block, _)| !blocks.contains_key(&block))
                .unwrap_or(false)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::BASE_OFFSET;
    use crate::ext4_defs::{Inode, InodeMode};

    fn metadata_csum_superblock() -> SuperBlock {
        let mut raw = [0u8; 2048];
        raw[BASE_OFFSET + 100..BASE_OFFSET + 104]
            .copy_from_slice(&SuperBlock::FEATURE_RO_COMPAT_METADATA_CSUM.to_le_bytes());
        raw[BASE_OFFSET + 104..BASE_OFFSET + 120].copy_from_slice(&[0x3c; 16]);
        SuperBlock::from_bytes(&raw[BASE_OFFSET..])
    }

    fn valid_journal_inode(sb: &SuperBlock) -> InodeRef {
        let mut inode = InodeRef::new(8, Box::new(Inode::default()));
        inode.inode.set_mode(InodeMode::FILE | InodeMode::ALL_RW);
        inode.inode.set_generation(9);
        inode.inode.extent_init();
        inode.set_checksum(&sb.uuid());
        inode
    }

    #[test]
    fn journal_inode_mode_and_checksum_are_authenticated() {
        let sb = metadata_csum_superblock();
        let inode = valid_journal_inode(&sb);
        validate_journal_inode(&sb, &inode).unwrap();

        let mut bad_mode = inode.clone();
        bad_mode
            .inode
            .set_mode(InodeMode::DIRECTORY | InodeMode::ALL_RWX);
        bad_mode.set_checksum(&sb.uuid());
        assert_eq!(
            validate_journal_inode(&sb, &bad_mode).unwrap_err().code(),
            ErrCode::EIO
        );

        let mut bad_checksum = inode;
        bad_checksum.inode.set_generation(10);
        assert_eq!(
            validate_journal_inode(&sb, &bad_checksum)
                .unwrap_err()
                .code(),
            ErrCode::EIO
        );
    }

    #[test]
    fn replayed_journal_mapping_or_identity_change_is_rejected() {
        let sb = metadata_csum_superblock();
        let mut before = valid_journal_inode(&sb);
        before.inode.set_size(8192);
        before.set_checksum(&sb.uuid());
        let after = before.clone();
        assert!(journal_replay_identity_matches(
            &before,
            &after,
            &[20, 21],
            &[20, 21]
        ));
        assert!(!journal_replay_identity_matches(
            &before,
            &after,
            &[20, 21],
            &[20, 22]
        ));
        let mut changed = after;
        changed.inode.set_generation(10);
        assert!(!journal_replay_identity_matches(
            &before,
            &changed,
            &[20, 21],
            &[20, 21]
        ));
    }
}
