//! Nojournal range changes use ownership -> private nodes -> inode root ->
//! retirement ordering. Only affected paths are copied; unchanged subtrees keep
//! their existing homes. This is ordering safety, not crash-atomic journalling.
use super::PreallocationProgress;
use crate::ext4::{extent::ExtentEditTrace, journal_transaction::Transaction, Ext4};
use crate::{constants::*, ext4_defs::*, prelude::*};

const DATA_BATCH: usize = 64;
const TREE_BATCH: usize = 128;
// Each bounded claim can touch bitmap, descriptor and superblock homes. Tree
// edits additionally stage their private node and the one inode-table home.
const PHASE_CREDITS: usize = (DATA_BATCH + 2 * TREE_BATCH) * 4 + 1;

#[derive(Clone, Copy)]
enum Edit {
    Allocate {
        first: LBlockId,
        count: usize,
    },
    Punch {
        first: LBlockId,
        end: LBlockId,
    },
    PublishWritten {
        logical: LBlockId,
        completed_end: u64,
    },
}

struct NodeImage {
    old: PBlockId,
    home: PBlockId,
    image: Box<[u8; BLOCK_SIZE]>,
    reachable: bool,
}

struct Prepared {
    inode: InodeRef,
    nodes: Vec<NodeImage>,
    claims: Vec<PBlockId>,
    zeros: Vec<PBlockId>,
    retire: Vec<PBlockId>,
    removed_data: Option<(PBlockId, u32)>,
    zero_image: Box<[u8; BLOCK_SIZE]>,
}

fn reserved<T>(capacity: usize) -> Result<Vec<T>> {
    let mut result = Vec::new();
    result
        .try_reserve_exact(capacity)
        .map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
    Ok(result)
}

fn unique_push(homes: &mut Vec<PBlockId>, home: PBlockId) {
    if !homes.contains(&home) {
        homes.push(home);
    }
}

/// Walk only explicitly traced nodes, never the unmodified subtree behind a
/// shared pointer. Depth decreases on every edge; aliases/cycles are rejected.
fn mark_reachable(node: &ExtentNode<'_>, images: &mut [NodeImage]) -> Result<()> {
    let mut pending = reserved(TREE_BATCH)?;
    let queue_children = |node: &ExtentNode<'_>, pending: &mut Vec<(usize, u16)>| -> Result<()> {
        if node.header().depth() == 0 {
            return Ok(());
        }
        for index in 0..usize::from(node.header().entries_count()) {
            let home = node.extent_index_at(index).leaf();
            if let Some(position) = images.iter().position(|image| image.old == home) {
                if pending.len() == TREE_BATCH {
                    return Err(Ext4Error::new(ErrCode::EIO));
                }
                pending.push((position, node.header().depth() - 1));
            }
        }
        Ok(())
    };
    queue_children(node, &mut pending)?;
    // Collect the bounded reachable traversal before updating flags, keeping
    // node images borrowed rather than copying 4K at every recursion level.
    let mut reachable = reserved(TREE_BATCH)?;
    while let Some((position, depth)) = pending.pop() {
        if reachable.contains(&position) {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        reachable.push(position);
        let child = ExtentNode::from_bytes(&*images[position].image);
        if child.header().depth() != depth {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        queue_children(&child, &mut pending)?;
    }
    for position in reachable {
        images[position].reachable = true;
    }
    Ok(())
}

fn relocate(node: &mut ExtentNodeMut<'_>, locations: &[(PBlockId, PBlockId)]) {
    if node.header().depth() == 0 {
        return;
    }
    for index in 0..usize::from(node.header().entries_count()) {
        let pointer = node.extent_index_mut_at(index);
        if let Some((_, new)) = locations.iter().find(|(old, _)| *old == pointer.leaf()) {
            *pointer = ExtentIndex::new(pointer.start_lblock(), *new);
        }
    }
}

impl Ext4 {
    /// Caller holds the metadata mutation gate and inode mutation lock.
    /// Establish the forward link before publishing the list head. The inode
    /// still carries its old size until the separate size publication phase.
    pub(in crate::ext4) fn direct_enroll_linked_tail_locked(
        &self,
        inode: &mut InodeRef,
    ) -> Result<()> {
        use crate::ext4::orphan::LegacyOrphanMembership;
        if !self.block_device.supports_reliable_flush() {
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }
        let mut fresh = self.read_inode_uncached(inode.id)?;
        if fresh.inode.generation() != inode.inode.generation()
            || fresh.inode.link_count() == 0
            || !fresh.inode.is_file()
            || !fresh.inode.uses_extents()
            || self.legacy_orphan_membership(&fresh)? != LegacyOrphanMembership::Absent
        {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        let mut sb = self.read_super_block_cached();
        fresh.inode.set_next_orphan(sb.last_orphan());
        let mut target = self.transaction_start(1)?;
        self.transaction_stage_inode_with_csum(&mut target, &mut fresh)?;
        self.finish_direct_phase(target)?;

        let result = (|| {
            let mut head = self.transaction_start(1)?;
            sb.set_last_orphan(fresh.id);
            self.transaction_stage_super_block(&mut head, &sb)?;
            self.finish_direct_phase(head)
        })();
        if result.is_err() {
            self.fail_stop_mutations();
        }
        result?;
        *inode = fresh;
        Ok(())
    }

    /// Reload after enrollment, including a possibly shared inode-table home,
    /// so publishing attributes cannot overwrite the durable orphan link.
    pub(in crate::ext4) fn direct_publish_size_attrs_locked(
        &self,
        inode: &mut InodeRef,
        attr: &crate::SetAttr,
    ) -> Result<()> {
        let mut fresh = self.read_inode_uncached(inode.id)?;
        if fresh.inode.generation() != inode.inode.generation() {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        Self::apply_setattr_fields(&mut fresh, attr);
        let mut transaction = self.transaction_start(1)?;
        self.transaction_stage_inode_with_csum(&mut transaction, &mut fresh)?;
        self.finish_direct_phase(transaction)?;
        *inode = fresh;
        Ok(())
    }

    /// Bypass the list node durably before clearing its outgoing link. For a
    /// non-head node the predecessor may share this inode-table block; each
    /// phase reads its image afresh rather than restoring a stale snapshot.
    pub(in crate::ext4) fn direct_unlink_tail_orphan_locked(
        &self,
        inode: &mut InodeRef,
    ) -> Result<()> {
        let fresh = self.read_inode_uncached(inode.id)?;
        if fresh.inode.generation() != inode.inode.generation() || fresh.inode.link_count() == 0 {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        let mut sb = self.read_super_block_cached();
        let mut incoming = self.transaction_start(1)?;
        self.transaction_orphan_del(&mut incoming, &fresh, &mut sb)?;
        self.finish_direct_phase(incoming)?;

        let result = (|| {
            let mut fresh = self.read_inode_uncached(inode.id)?;
            fresh.inode.set_next_orphan(0);
            let mut target = self.transaction_start(1)?;
            self.transaction_stage_inode_with_csum(&mut target, &mut fresh)?;
            self.finish_direct_phase(target)?;
            *inode = fresh;
            Ok(())
        })();
        // The incoming link is already durable: there is no rollback to the
        // old chain, even if preparing the final target image fails.
        if result.is_err() {
            self.fail_stop_mutations();
        }
        result
    }

    pub(in crate::ext4) fn direct_preallocate_range_batch_locked(
        &self,
        inode: &mut InodeRef,
        offset: usize,
        len: usize,
    ) -> Result<PreallocationProgress> {
        let end = offset
            .checked_add(len)
            .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
        let first = (offset / BLOCK_SIZE) as LBlockId;
        let next = self.allocated_extent_at_or_after(inode, first, None)?;
        if let Some(extent) = next {
            if extent.start_lblock() <= first {
                let stop = extent
                    .start_lblock()
                    .checked_add(extent.block_count())
                    .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
                return Ok(PreallocationProgress {
                    next_offset: (stop as usize * BLOCK_SIZE).min(end),
                });
            }
        }
        let limit = end
            .div_ceil(BLOCK_SIZE)
            .min(next.map_or(usize::MAX, |extent| extent.start_lblock() as usize));
        let mut count = (limit - first as usize).min(DATA_BATCH);
        loop {
            let mut ownership = self.transaction_start(PHASE_CREDITS)?;
            match self.prepare_direct_range(inode, Edit::Allocate { first, count }, &mut ownership)
            {
                // Shrink only unpublished private preparation, never retry an
                // operation after ownership or a new root has reached disk.
                Err(error)
                    if count > 1 && matches!(error.code(), ErrCode::ENOSPC | ErrCode::E2BIG) =>
                {
                    count /= 2;
                }
                result => {
                    let prepared = result?;
                    self.execute_direct_range(inode, ownership, prepared)?;
                    return Ok(PreallocationProgress {
                        next_offset: ((first as usize + count) * BLOCK_SIZE).min(end),
                    });
                }
            }
        }
    }

    pub(in crate::ext4) fn direct_punch_extent_slice_locked(
        &self,
        inode: &mut InodeRef,
        first: LBlockId,
        end: LBlockId,
    ) -> Result<()> {
        self.direct_range_edit_locked(inode, Edit::Punch { first, end })
    }

    /// Caller owns the exclusive metadata gate and inode mutation lock, and
    /// has durably written the complete physical block image. Publish only
    /// that block's conversion together with its actual completed byte EOF.
    pub(in crate::ext4) fn direct_publish_written_block_locked(
        &self,
        inode: &mut InodeRef,
        logical: LBlockId,
        completed_end: u64,
    ) -> Result<()> {
        let start = u64::from(logical) * BLOCK_SIZE as u64;
        if completed_end <= start || completed_end > start + BLOCK_SIZE as u64 {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        let extent = self
            .allocated_extent_at_or_after(inode, logical, None)?
            .ok_or_else(|| Ext4Error::new(ErrCode::ENOENT))?;
        if extent.start_lblock() > logical {
            return Err(Ext4Error::new(ErrCode::ENOENT));
        }
        if extent.is_unwritten() || completed_end > inode.inode.size() {
            self.direct_range_edit_locked(
                inode,
                Edit::PublishWritten {
                    logical,
                    completed_end,
                },
            )?;
        }
        Ok(())
    }

    fn prepare_direct_range(
        &self,
        original: &InodeRef,
        edit: Edit,
        transaction: &mut Transaction<'_>,
    ) -> Result<Prepared> {
        let mut inode = InodeRef::new(
            original.id,
            Box::try_new(original.inode.as_ref().clone())
                .map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?,
        );
        let mut trace = ExtentEditTrace::new()?;
        let mut claims = reserved(DATA_BATCH + 2 * TREE_BATCH)?;
        let mut zeros = reserved(DATA_BATCH)?;
        let mut retire = reserved(2 * TREE_BATCH)?;
        let mut nodes = reserved(TREE_BATCH)?;
        let mut locations = reserved(TREE_BATCH)?;
        let mut removed_data = None;
        match edit {
            Edit::Allocate { first, count } => {
                for delta in 0..count {
                    let home =
                        self.transaction_alloc_metadata_block(transaction, inode.id, None)?;
                    claims.push(home);
                    zeros.push(home);
                    let mut extent = Extent::new(first + delta as u32, home, 1);
                    extent.mark_unwritten();
                    self.transaction_insert_allocated_extent_traced(
                        transaction,
                        &mut inode,
                        &extent,
                        Some(&mut trace),
                    )?;
                }
            }
            Edit::Punch { first, end } => {
                let extent = self
                    .allocated_extent_at_or_after(&inode, first, Some(transaction))?
                    .ok_or_else(|| Ext4Error::new(ErrCode::ENOENT))?;
                let home = extent
                    .start_pblock()
                    .checked_add(u64::from(
                        first
                            .checked_sub(extent.start_lblock())
                            .ok_or_else(|| Ext4Error::new(ErrCode::EINVAL))?,
                    ))
                    .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
                removed_data = Some((home, end - first));
                for home in self.transaction_splice_allocated_extent_traced(
                    transaction,
                    &mut inode,
                    first,
                    end,
                    false,
                    Some(&mut trace),
                )? {
                    unique_push(&mut retire, home);
                }
            }
            Edit::PublishWritten {
                logical,
                completed_end,
            } => {
                let extent = self
                    .allocated_extent_at_or_after(&inode, logical, Some(transaction))?
                    .ok_or_else(|| Ext4Error::new(ErrCode::ENOENT))?;
                if extent.start_lblock() > logical {
                    return Err(Ext4Error::new(ErrCode::ENOENT));
                }
                if extent.is_unwritten() {
                    let end = logical
                        .checked_add(1)
                        .ok_or_else(|| Ext4Error::new(ErrCode::EFBIG))?;
                    for home in self.transaction_splice_allocated_extent_traced(
                        transaction,
                        &mut inode,
                        logical,
                        end,
                        true,
                        Some(&mut trace),
                    )? {
                        unique_push(&mut retire, home);
                    }
                }
                // The payload is already durable: never zero it here. The
                // same B publication makes both initialization and EOF visible.
                inode.inode.set_size(inode.inode.size().max(completed_end));
            }
        }
        for home in &trace.new_tree_homes {
            unique_push(&mut claims, *home);
        }
        for home in &trace.touched_tree_homes {
            let image = Box::try_new(*transaction.read(self, *home)?)
                .map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
            nodes.push(NodeImage {
                old: *home,
                home: *home,
                image,
                reachable: false,
            });
        }
        mark_reachable(&inode.inode.extent_root(), &mut nodes)?;
        for node in &mut nodes {
            if !node.reachable {
                unique_push(&mut retire, node.old);
            } else if !trace.new_tree_homes.contains(&node.old) {
                node.home = self.transaction_alloc_metadata_block(transaction, inode.id, None)?;
                unique_push(&mut claims, node.home);
                unique_push(&mut retire, node.old);
                locations.push((node.old, node.home));
            }
        }
        relocate(&mut inode.inode.extent_root_mut(), &locations);
        for node in &mut nodes {
            if node.reachable {
                relocate(&mut ExtentNodeMut::from_bytes(&mut *node.image), &locations);
                Self::set_extent_block_checksum(
                    self.read_super_block_cached().metadata_checksum_seed(),
                    &inode,
                    &mut node.image,
                );
            }
            transaction.take_direct_image(node.old)?;
        }
        transaction.take_direct_image(self.inode_disk_pos(inode.id)?.0)?;
        let removed_count = removed_data.map_or(0, |(_, count)| u64::from(count));
        let blocks = original
            .inode
            .fs_block_count()
            .checked_add(claims.len() as u64)
            .and_then(|blocks| blocks.checked_sub(retire.len() as u64))
            .and_then(|blocks| blocks.checked_sub(removed_count))
            .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
        inode.inode.set_fs_block_count(blocks);
        let zero_image =
            Box::try_new([0; BLOCK_SIZE]).map_err(|_| Ext4Error::new(ErrCode::ENOMEM))?;
        Ok(Prepared {
            inode,
            nodes,
            claims,
            zeros,
            retire,
            removed_data,
            zero_image,
        })
    }

    fn finish_direct_phase(&self, transaction: Transaction<'_>) -> Result<()> {
        transaction
            .commit_direct_flush_before_publish(self.block_device.as_ref(), self)
            .map_err(|error| {
                if error.poisoned {
                    self.poison(ErrCode::EIO);
                }
                error.error
            })
    }

    fn direct_release_claims(
        &self,
        homes: &[PBlockId],
        data: Option<(PBlockId, u32)>,
    ) -> Result<()> {
        if homes.is_empty() && data.is_none() {
            return Ok(());
        }
        let mut transaction = self.transaction_start(PHASE_CREDITS)?;
        for home in homes {
            self.transaction_dealloc_block_range(&mut transaction, *home, 1)?;
        }
        if let Some((home, count)) = data {
            self.transaction_dealloc_block_range(&mut transaction, home, count)?;
        }
        self.finish_direct_phase(transaction)
    }

    fn direct_range_edit_locked(&self, inode: &mut InodeRef, edit: Edit) -> Result<()> {
        if !self.block_device.supports_reliable_flush() {
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }
        let mut ownership = self.transaction_start(PHASE_CREDITS)?;
        let prepared = self.prepare_direct_range(inode, edit, &mut ownership)?;
        self.execute_direct_range(inode, ownership, prepared)
    }

    fn execute_direct_range(
        &self,
        inode: &mut InodeRef,
        ownership: Transaction<'_>,
        mut prepared: Prepared,
    ) -> Result<()> {
        // A: only allocation ownership images remain. No old bitmap bit has
        // been cleared, nor any inode or old tree block published.
        self.finish_direct_phase(ownership)?;
        let before_root: Result<Transaction<'_>> = (|| {
            let needs_image_flush =
                !prepared.zeros.is_empty() || prepared.nodes.iter().any(|node| node.reachable);
            let mut zero = Block::new(0, prepared.zero_image);
            for home in &prepared.zeros {
                zero.id = *home;
                self.block_device.write_block(&zero)?;
            }
            for node in prepared.nodes {
                if node.reachable {
                    self.block_device
                        .write_block(&Block::new(node.home, node.image))?;
                }
            }
            if needs_image_flush {
                self.block_device.flush()?;
            }
            let mut root = self.transaction_start(1)?;
            self.transaction_stage_inode_with_csum(&mut root, &mut prepared.inode)?;
            Ok(root)
        })();
        let root = match before_root {
            Ok(root) => root,
            Err(error) => {
                // B never started: none of these claims can be reachable.
                if self.direct_release_claims(&prepared.claims, None).is_err() {
                    self.poison(ErrCode::EIO);
                }
                return Err(error);
            }
        };
        // B: uncertain write/flush retains BOTH old and new ownership.
        self.finish_direct_phase(root)?;
        *inode = prepared.inode;
        // C: reload committed allocation metadata, then retire only unreachable
        // old paths/data and any newly allocated node detached during planning.
        if let Err(error) = self.direct_release_claims(&prepared.retire, prepared.removed_data) {
            self.poison(ErrCode::EIO);
            return Err(error);
        }
        Ok(())
    }
}
