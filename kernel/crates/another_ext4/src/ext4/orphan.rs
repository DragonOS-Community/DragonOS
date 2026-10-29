//! Legacy ext4 orphan-list validation and mount-time cleanup.
//!
//! Validation of the complete list precedes the first cleanup transaction so
//! a corrupt tail cannot turn a partially-cleaned list into an unrecoverable
//! leak.  Cleanup then advances only from the current durable head and checks
//! every link against that immutable snapshot.

use super::Ext4;
use crate::constants::EXT4_ROOT_INO;
use crate::ext4_defs::{Bitmap, FileType, InodeRef, SuperBlock};
use crate::prelude::*;

#[derive(Debug)]
pub(super) struct LegacyOrphanChain {
    pub(super) inodes: Vec<InodeId>,
}

/// The durable role of an inode in the legacy orphan chain.
///
/// A linked tail is not interchangeable with a final-unlink orphan.  The
/// former asks mount recovery to trim mappings beyond its durable EOF, while
/// the latter asks it to reclaim the complete inode lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LegacyOrphanMembership {
    Absent,
    LinkedTail,
    ZeroLink,
}

/// The only valid transition when a linked-tail inode loses its final name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FinalUnlinkOrphanAction {
    /// The inode was not already recoverable, so insert a zero-link orphan.
    AddZeroLink,
    /// Preserve the existing chain node and change only `i_nlink` to zero.
    PreserveLinkedTail,
}

trait LegacyOrphanReader {
    fn inode_count(&self) -> u32;
    fn first_inode(&self) -> u32;
    fn journal_inode(&self) -> u32;
    fn inode_allocated(&self, inode: InodeId) -> Result<bool>;
    fn read_orphan_inode(&self, inode: InodeId) -> Result<OrphanNode>;
}

#[derive(Clone, Copy, Debug)]
struct OrphanNode {
    next: InodeId,
    links: u16,
    valid_mode: bool,
    valid_checksum: bool,
}

fn corruption() -> Ext4Error {
    Ext4Error::new(ErrCode::EIO)
}

pub(super) fn inode_checksum_valid(sb: &SuperBlock, inode: &InodeRef) -> bool {
    !sb.has_read_only_compatible_feature(SuperBlock::FEATURE_RO_COMPAT_METADATA_CSUM)
        || inode.verify_checksum(sb.metadata_checksum_seed())
}

fn unsupported_orphan_format(sb: &SuperBlock) -> bool {
    unsupported_orphan_features(sb.compatible_features(), sb.read_only_compatible_features())
}

fn unsupported_orphan_features(_compatible: u32, read_only_compatible: u32) -> bool {
    // FEATURE_COMPAT_ORPHAN_FILE is a *compat* feature: kernels without orphan-file
    // support can still mount read-write and simply ignore the dedicated orphan inode.
    // Reject only when ORPHAN_PRESENT is set, which means there are actual unrecovered
    // orphans in the orphan file that this implementation cannot process.
    read_only_compatible & SuperBlock::FEATURE_RO_COMPAT_ORPHAN_PRESENT != 0
}

fn valid_orphan_number<R: LegacyOrphanReader>(reader: &R, inode: InodeId) -> bool {
    inode >= reader.first_inode()
        && inode <= reader.inode_count()
        && inode != EXT4_ROOT_INO
        && inode != reader.journal_inode()
}

fn validate_chain<R: LegacyOrphanReader>(reader: &R, head: InodeId) -> Result<LegacyOrphanChain> {
    let mut current = head;
    let mut visited = BTreeSet::new();
    let mut inodes = Vec::new();

    while current != 0 {
        // Validate the inode number before calculating a block group or doing
        // any inode-table / bitmap I/O.
        if !valid_orphan_number(reader, current)
            || inodes.len() >= reader.inode_count() as usize
            || !visited.insert(current)
        {
            return Err(corruption());
        }
        if !reader.inode_allocated(current)? {
            return Err(corruption());
        }

        let node = reader.read_orphan_inode(current)?;
        if !node.valid_checksum || !node.valid_mode {
            return Err(corruption());
        }
        if node.next != 0 && !valid_orphan_number(reader, node.next) {
            return Err(corruption());
        }

        inodes.push(current);
        current = node.next;
    }

    Ok(LegacyOrphanChain { inodes })
}

impl Ext4 {
    /// Classify the inode's durable orphan role while the caller excludes all
    /// namespace and transactional metadata writers which could alter the
    /// chain.  `next_orphan == 0` cannot identify membership: both a list tail
    /// and a non-member use that value.
    pub(super) fn legacy_orphan_membership(
        &self,
        inode: &InodeRef,
    ) -> Result<LegacyOrphanMembership> {
        // The overwhelmingly common steady state after mount recovery has no
        // legacy orphan members.  Avoid a complete chain validation on every
        // ordinary final unlink when the durable head proves that directly.
        if self.read_super_block_cached().last_orphan() == 0 {
            return Ok(LegacyOrphanMembership::Absent);
        }
        if !self.legacy_orphan_contains(inode.id)? {
            return Ok(LegacyOrphanMembership::Absent);
        }
        if inode.inode.link_count() == 0 {
            return Ok(LegacyOrphanMembership::ZeroLink);
        }
        if inode.inode.is_file() && inode.inode.uses_extents() {
            return Ok(LegacyOrphanMembership::LinkedTail);
        }
        Err(corruption())
    }

    /// Verify that `inode_id` is a member of the complete, valid legacy list.
    ///
    /// Reclaim calls this while holding the target inode's mutation shard.  A
    /// complete bounded walk is intentional: accepting a locally plausible
    /// node from a corrupt list could permanently lose the remainder at final
    /// deletion.
    pub(super) fn legacy_orphan_contains(&self, inode_id: InodeId) -> Result<bool> {
        Ok(self
            .validate_legacy_orphan_chain()?
            .inodes
            .contains(&inode_id))
    }

    /// Insert a zero-link inode at the head of the legacy orphan list in the
    /// caller's transaction.  This helper only stages images; atomicity and
    /// commit error handling remain the namespace operation's responsibility.
    pub(super) fn transaction_orphan_add_zero_link(
        &self,
        transaction: &mut super::journal_transaction::Transaction<'_>,
        inode: &mut InodeRef,
        sb: &mut SuperBlock,
    ) -> Result<()> {
        if inode.inode.link_count() != 0
            || inode.inode.file_type() == FileType::Unknown
            || !valid_orphan_number(self, inode.id)
        {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        let old_head = sb.last_orphan();
        if old_head == inode.id || (old_head != 0 && !valid_orphan_number(self, old_head)) {
            return Err(corruption());
        }
        inode.inode.set_next_orphan(old_head);
        self.transaction_stage_inode_with_csum(transaction, inode)?;
        sb.set_last_orphan(inode.id);
        self.transaction_stage_super_block(transaction, sb)
    }

    /// Insert a still-linked regular extent inode at the orphan-list head so
    /// mount recovery can discard a crash-interrupted mapped tail beyond its
    /// durable EOF.  The caller must have classified the inode as `Absent`
    /// under the same metadata exclusion domain before starting this
    /// transaction; this helper deliberately does not perform a second walk
    /// over a transaction-private chain image.
    #[allow(dead_code)] // Consumed by the stage-3b mapper; host tests reach it through the gated constructor.
    fn transaction_orphan_add_linked_tail(
        &self,
        transaction: &mut super::journal_transaction::Transaction<'_>,
        inode: &mut InodeRef,
        sb: &mut SuperBlock,
    ) -> Result<()> {
        if inode.inode.link_count() == 0
            || !inode.inode.is_file()
            || !inode.inode.uses_extents()
            || !valid_orphan_number(self, inode.id)
        {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        let old_head = sb.last_orphan();
        if old_head == inode.id || (old_head != 0 && !valid_orphan_number(self, old_head)) {
            return Err(corruption());
        }
        inode.inode.set_next_orphan(old_head);
        self.transaction_stage_inode_with_csum(transaction, inode)?;
        sb.set_last_orphan(inode.id);
        self.transaction_stage_super_block(transaction, sb)
    }

    /// Ensure that a linked regular extent inode is recoverable as a truncate
    /// tail.  The mapper will use this in the same transaction as first tail
    /// publication.  It owns the membership classification so a future caller
    /// cannot forge an `already linked` result and silently skip recovery
    /// enrolment for an absent inode.
    #[allow(dead_code)] // The production mapper has not been connected yet.
    pub(super) fn transaction_ensure_linked_tail_orphan(
        &self,
        transaction: &mut super::journal_transaction::Transaction<'_>,
        inode: &mut InodeRef,
        sb: &mut SuperBlock,
    ) -> Result<bool> {
        let membership = self.legacy_orphan_membership(inode)?;
        match membership {
            LegacyOrphanMembership::Absent => {
                // A mapper can split one logical tail into several segments
                // within one transaction.  The first call has already staged
                // this inode as the transaction-private list head, while the
                // mounted cache still correctly reports it absent until
                // checkpoint publication.  Treat precisely that state as an
                // idempotent ensure instead of trying to add a self-head.
                if sb.last_orphan() == inode.id {
                    return Ok(false);
                }
                self.transaction_orphan_add_linked_tail(transaction, inode, sb)?;
                Ok(true)
            }
            LegacyOrphanMembership::LinkedTail => Ok(false),
            // A live namespace entry with an already zero-link orphan is
            // impossible.  Do not silently convert or detach it.
            LegacyOrphanMembership::ZeroLink => Err(corruption()),
        }
    }

    /// Test-only construction of the durable state which a future mapper will
    /// create atomically together with its first tail mapping.  It is compiled
    /// only for the host integration harness; no production write path calls
    /// it or gains a new crash window from it.
    #[cfg(any(test, feature = "test-api"))]
    pub fn test_enroll_linked_tail_orphan(&self, inode_id: InodeId) -> Result<bool> {
        self.ensure_mutable()?;
        if !self.uses_journal() {
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }
        let _metadata_guard = self.lock_transactional_metadata_mutation()?;
        let _mutation_guard =
            self.inode_mutation_locks[self.inode_mutation_lock_index(inode_id)].lock();
        let mut inode = self.read_inode(inode_id)?;
        let mut transaction = self.transaction_start(2)?;
        let mut sb = self.transaction_read_super_block(&transaction)?;
        let enrolled =
            self.transaction_ensure_linked_tail_orphan(&mut transaction, &mut inode, &mut sb)?;
        if self.transaction_ensure_linked_tail_orphan(&mut transaction, &mut inode, &mut sb)? {
            // The same transaction must not turn its already staged head
            // into a duplicate/self-referential chain node.
            transaction.abort();
            return Err(corruption());
        }
        if !enrolled {
            transaction.abort();
            return Ok(false);
        }
        if let Err(error) = transaction.commit(self.block_device.as_ref(), self) {
            self.poison(ErrCode::EIO);
            return Err(error.error);
        }
        Ok(true)
    }

    /// Remove an inode from the legacy orphan chain in the caller's final
    /// reclaim transaction. Both head and non-head deletion are supported.
    /// The walk is bounded by `s_inodes_count` and validates every visited
    /// inode before changing either the predecessor or superblock image.
    pub(super) fn transaction_orphan_del(
        &self,
        transaction: &mut super::journal_transaction::Transaction<'_>,
        inode: &InodeRef,
        sb: &mut SuperBlock,
    ) -> Result<()> {
        let target = inode.id;
        let target_next = inode.inode.next_orphan();
        if !valid_orphan_number(self, target) {
            return Err(corruption());
        }

        let mut current = sb.last_orphan();
        let mut predecessor: Option<InodeRef> = None;
        let mut visited = BTreeSet::new();
        while current != 0 {
            if !valid_orphan_number(self, current)
                || visited.len() >= sb.inode_count() as usize
                || !visited.insert(current)
                || !self.inode_is_allocated(current)?
            {
                return Err(corruption());
            }
            let current_inode = self.read_inode_uncached(current)?;
            if !inode_checksum_valid(sb, &current_inode)
                || current_inode.inode.file_type() == FileType::Unknown
            {
                return Err(corruption());
            }
            let next = current_inode.inode.next_orphan();
            if next != 0 && !valid_orphan_number(self, next) {
                return Err(corruption());
            }
            if current == target {
                if current_inode.inode.generation() != inode.inode.generation()
                    || next != target_next
                {
                    return Err(corruption());
                }
                if let Some(mut pred) = predecessor {
                    pred.inode.set_next_orphan(target_next);
                    self.transaction_stage_inode_with_csum(transaction, &mut pred)?;
                } else {
                    sb.set_last_orphan(target_next);
                    self.transaction_stage_super_block(transaction, sb)?;
                }
                return Ok(());
            }
            predecessor = Some(current_inode);
            current = next;
        }
        Err(corruption())
    }

    /// Reject formats whose orphan state this implementation cannot update.
    /// This is called before journal replay or setting `RECOVER`.
    pub(super) fn writable_orphan_preflight(&self) -> Result<()> {
        let sb = self.read_super_block_cached();
        if unsupported_orphan_format(&sb) {
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }
        Ok(())
    }

    /// First (read-only) phase of legacy orphan recovery.
    pub(super) fn validate_legacy_orphan_chain(&self) -> Result<LegacyOrphanChain> {
        let sb = self.read_super_block_cached();
        validate_chain(self, sb.last_orphan())
    }

    /// Reclaim a previously validated legacy orphan chain, one durable
    /// transaction at a time.
    ///
    /// The complete chain is validated under the metadata barrier before the
    /// first transaction.  Before each transaction we re-read both links and
    /// require them to still match that snapshot.  Thus a stale snapshot can
    /// never detach or reclaim a different inode, and any error stops recovery
    /// at the current durable head for the next mount to retry.
    pub(super) fn cleanup_legacy_orphan_chain(&self) -> Result<()> {
        // Keep every legacy direct metadata writer outside the complete
        // validated-snapshot -> per-inode-commit recovery window.  The mount
        // path is normally unpublished here, but the invariant belongs to
        // this operation rather than to that calling convention.
        let _metadata_guard = self.lock_transactional_metadata_mutation()?;
        let snapshot = self.validate_legacy_orphan_chain()?;
        for (index, inode_id) in snapshot.inodes.iter().copied().enumerate() {
            let expected_next = snapshot.inodes.get(index + 1).copied().unwrap_or(0);
            if self.read_super_block_cached().last_orphan() != inode_id {
                return Err(corruption());
            }

            let node = self.read_orphan_inode(inode_id)?;
            if node.next != expected_next
                || !node.valid_mode
                || !node.valid_checksum
                || !self.inode_allocated(inode_id)?
            {
                return Err(corruption());
            }

            if node.links == 0 {
                self.reclaim_orphan_inode_by_id(inode_id)?;
            } else {
                self.recover_linked_orphan_inode_by_id(inode_id)?;
            }

            if self.read_super_block_cached().last_orphan() != expected_next {
                return Err(corruption());
            }
        }
        Ok(())
    }
}

/// Decide the final-unlink action from a membership snapshot taken while the
/// caller owns namespace and inode-mutation exclusion.  A zero-link member
/// cannot still be reachable through the directory entry being removed.
pub(super) fn final_unlink_orphan_action(
    membership: LegacyOrphanMembership,
) -> Result<FinalUnlinkOrphanAction> {
    match membership {
        LegacyOrphanMembership::Absent => Ok(FinalUnlinkOrphanAction::AddZeroLink),
        LegacyOrphanMembership::LinkedTail => Ok(FinalUnlinkOrphanAction::PreserveLinkedTail),
        LegacyOrphanMembership::ZeroLink => Err(corruption()),
    }
}

impl LegacyOrphanReader for Ext4 {
    fn inode_count(&self) -> u32 {
        self.read_super_block_cached().inode_count()
    }

    fn first_inode(&self) -> u32 {
        self.read_super_block_cached().first_inode()
    }

    fn journal_inode(&self) -> u32 {
        self.read_super_block_cached().journal_inode_number()
    }

    fn inode_allocated(&self, inode: InodeId) -> Result<bool> {
        let sb = self.read_super_block_cached();
        let group = (inode - 1) / sb.inodes_per_group();
        let index = (inode - 1) % sb.inodes_per_group();
        let bg = self.read_block_group(group)?;
        let bitmap_block = self.read_block(bg.desc.inode_bitmap_block())?;
        let count = sb.inode_count_in_group(group) as usize;
        if index as usize >= count {
            return Err(corruption());
        }
        let mut bytes = bitmap_block.data.clone();
        let bitmap = Bitmap::new(&mut *bytes, count);
        Ok(!bitmap.is_bit_clear(index as usize))
    }

    fn read_orphan_inode(&self, inode: InodeId) -> Result<OrphanNode> {
        let inode_ref: InodeRef = self.read_inode_uncached(inode)?;
        let sb = self.read_super_block_cached();
        Ok(OrphanNode {
            next: inode_ref.inode.next_orphan(),
            links: inode_ref.inode.link_count(),
            valid_mode: inode_ref.inode.file_type() != FileType::Unknown,
            valid_checksum: inode_checksum_valid(&sb, &inode_ref),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockReader {
        nodes: BTreeMap<InodeId, (bool, OrphanNode)>,
    }

    impl MockReader {
        fn valid(next: InodeId) -> (bool, OrphanNode) {
            (
                true,
                OrphanNode {
                    next,
                    links: 0,
                    valid_mode: true,
                    valid_checksum: true,
                },
            )
        }
    }

    impl LegacyOrphanReader for MockReader {
        fn inode_count(&self) -> u32 {
            64
        }
        fn first_inode(&self) -> u32 {
            11
        }
        fn journal_inode(&self) -> u32 {
            8
        }
        fn inode_allocated(&self, inode: InodeId) -> Result<bool> {
            Ok(self.nodes.get(&inode).map(|entry| entry.0).unwrap_or(false))
        }
        fn read_orphan_inode(&self, inode: InodeId) -> Result<OrphanNode> {
            self.nodes
                .get(&inode)
                .map(|entry| entry.1)
                .ok_or_else(corruption)
        }
    }

    fn reader(entries: &[(InodeId, (bool, OrphanNode))]) -> MockReader {
        MockReader {
            nodes: entries.iter().copied().collect(),
        }
    }

    #[test]
    fn validates_complete_zero_link_chain() {
        let r = reader(&[(11, MockReader::valid(12)), (12, MockReader::valid(0))]);
        assert_eq!(validate_chain(&r, 11).unwrap().inodes, vec![11, 12]);
    }

    #[test]
    fn rejects_self_and_two_node_cycles() {
        let self_loop = reader(&[(11, MockReader::valid(11))]);
        assert!(validate_chain(&self_loop, 11).is_err());
        let two = reader(&[(11, MockReader::valid(12)), (12, MockReader::valid(11))]);
        assert!(validate_chain(&two, 11).is_err());
    }

    #[test]
    fn rejects_out_of_range_head_and_next() {
        assert!(validate_chain(&reader(&[]), 65).is_err());
        let r = reader(&[(11, MockReader::valid(65))]);
        assert!(validate_chain(&r, 11).is_err());
    }

    #[test]
    fn rejects_free_inode_and_accepts_linked_truncate_orphan() {
        let free = reader(&[(11, (false, MockReader::valid(0).1))]);
        assert!(validate_chain(&free, 11).is_err());
        let mut linked = MockReader::valid(0).1;
        linked.links = 1;
        assert_eq!(
            validate_chain(&reader(&[(11, (true, linked))]), 11)
                .unwrap()
                .inodes,
            vec![11]
        );
    }

    #[test]
    fn accepts_mixed_link_counts_in_complete_chain() {
        let mut linked = MockReader::valid(12).1;
        linked.links = 2;
        let r = reader(&[
            (11, MockReader::valid(13)),
            (13, (true, linked)),
            (12, MockReader::valid(0)),
        ]);
        assert_eq!(validate_chain(&r, 11).unwrap().inodes, vec![11, 13, 12]);
    }

    #[test]
    fn rejects_bad_checksum_and_mode() {
        let mut checksum = MockReader::valid(0).1;
        checksum.valid_checksum = false;
        assert!(validate_chain(&reader(&[(11, (true, checksum))]), 11).is_err());
        let mut mode = MockReader::valid(0).1;
        mode.valid_mode = false;
        assert!(validate_chain(&reader(&[(11, (true, mode))]), 11).is_err());
    }

    #[test]
    fn preflight_rejects_each_orphan_file_feature() {
        assert!(!unsupported_orphan_features(0, 0));
        assert!(!unsupported_orphan_features(
            SuperBlock::FEATURE_COMPAT_ORPHAN_FILE,
            0
        ));
        assert!(unsupported_orphan_features(
            0,
            SuperBlock::FEATURE_RO_COMPAT_ORPHAN_PRESENT
        ));
    }

    #[test]
    fn inode_checksum_is_optional_without_metadata_csum() {
        let sb: SuperBlock = unsafe { core::mem::zeroed() };
        let mut inode = InodeRef::new(11, Box::default());
        inode.inode.set_generation(7);
        inode.set_checksum(sb.metadata_checksum_seed());
        inode.inode.set_generation(8);

        assert!(!inode.verify_checksum(sb.metadata_checksum_seed()));
        assert!(inode_checksum_valid(&sb, &inode));

        let checked_sb = SuperBlock::validation_fixture();
        inode.inode.set_generation(9);
        inode.set_checksum(checked_sb.metadata_checksum_seed());
        inode.inode.set_generation(10);
        assert!(!inode_checksum_valid(&checked_sb, &inode));
    }

    #[test]
    fn final_unlink_preserves_a_linked_tail_but_never_a_zero_link_member() {
        assert_eq!(
            final_unlink_orphan_action(LegacyOrphanMembership::Absent).unwrap(),
            FinalUnlinkOrphanAction::AddZeroLink
        );
        assert_eq!(
            final_unlink_orphan_action(LegacyOrphanMembership::LinkedTail).unwrap(),
            FinalUnlinkOrphanAction::PreserveLinkedTail
        );
        assert!(final_unlink_orphan_action(LegacyOrphanMembership::ZeroLink).is_err());
    }
}
