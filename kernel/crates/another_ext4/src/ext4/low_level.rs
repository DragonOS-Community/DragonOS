//! Low-level operations of Ext4 filesystem.
//!
//! These interfaces are designed and arranged coresponding to FUSE low-level ops.
//! Ref: https://libfuse.github.io/doxygen/structfuse__lowlevel__ops.html

use super::Ext4;
use crate::constants::*;
use crate::ext4_defs::*;
use crate::format_error;
use crate::prelude::*;
use crate::return_error;
use core::cmp::min;

/// Attributes that can be set on an inode via `setattr`.
#[derive(Default)]
pub struct SetAttr {
    /// File mode and permissions
    pub mode: Option<InodeMode>,
    /// 32-bit user id
    pub uid: Option<u32>,
    /// 32-bit group id
    pub gid: Option<u32>,
    /// 64-bit file size
    pub size: Option<u64>,
    /// 32-bit access time in seconds
    pub atime: Option<u32>,
    /// 32-bit modify time in seconds
    pub mtime: Option<u32>,
    /// 32-bit change time in seconds
    pub ctime: Option<u32>,
    /// 32-bit create time in seconds
    pub crtime: Option<u32>,
}

impl SetAttr {
    /// Create a new SetAttr struct with all fields set to None.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Ext4 {
    fn read_extent_or_hole(
        &self,
        file: &InodeRef,
        iblock: LBlockId,
        block_offset: usize,
        buf: &mut [u8],
    ) -> Result<()> {
        match self.extent_query(file, iblock) {
            Ok(fblock) => {
                let block = self.read_block(fblock)?;
                buf.copy_from_slice(block.read_offset(block_offset, buf.len()));
            }
            Err(err) if err.code() == ErrCode::ENOENT => {
                buf.fill(0);
            }
            Err(err) => return Err(err),
        }
        Ok(())
    }

    /// Get file attributes.
    ///
    /// # Params
    ///
    /// * `id` - inode id
    ///
    /// # Return
    ///
    /// A file attribute struct.
    ///
    /// # Error
    ///
    /// `EINVAL` if the inode-table entry is physically free.
    pub fn getattr(&self, id: InodeId) -> Result<FileAttr> {
        let inode = self.read_inode(id)?;
        if inode.inode.mode().bits() == 0 {
            return_error!(ErrCode::EINVAL, "Invalid inode {}", id);
        }

        // Get device number for device nodes
        let rdev = if inode.inode.is_device() {
            inode.inode.device()
        } else {
            (0, 0)
        };

        Ok(FileAttr {
            ino: id,
            size: inode.inode.size(),
            blocks: inode.inode.block_count(),
            atime: inode.inode.atime(),
            mtime: inode.inode.mtime(),
            ctime: inode.inode.ctime(),
            crtime: inode.inode.crtime(),
            ftype: inode.inode.file_type(),
            perm: inode.inode.perm(),
            links: inode.inode.link_count(),
            uid: inode.inode.uid(),
            gid: inode.inode.gid(),
            rdev,
        })
    }

    /// Set file attributes.
    ///
    /// # Params
    ///
    /// * `id` - inode id
    /// * `attr` - attributes to set (wrapped in SetAttr struct)
    ///
    /// # Error
    ///
    /// `EINVAL` if the inode is invalid (mode == 0).
    pub fn setattr(&self, id: InodeId, attr: SetAttr) -> Result<()> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _mutation_guard = self.inode_mutation_locks[self.inode_mutation_lock_index(id)].lock();
        let mut inode = self.read_inode(id)?;
        if inode.inode.mode().bits() == 0 {
            return_error!(ErrCode::EINVAL, "Invalid inode {}", id);
        }
        if let Some(mode) = attr.mode {
            inode.inode.set_mode(mode);
        }
        if let Some(uid) = attr.uid {
            inode.inode.set_uid(uid);
        }
        if let Some(gid) = attr.gid {
            inode.inode.set_gid(gid);
        }
        if let Some(size) = attr.size {
            inode.inode.set_size(size);
        }
        if let Some(atime) = attr.atime {
            inode.inode.set_atime(atime);
        }
        if let Some(mtime) = attr.mtime {
            inode.inode.set_mtime(mtime);
        }
        if let Some(ctime) = attr.ctime {
            inode.inode.set_ctime(ctime);
        }
        if let Some(crtime) = attr.crtime {
            inode.inode.set_crtime(crtime);
        }
        self.write_inode_with_csum(&mut inode)?;
        Ok(())
    }

    fn recompute_inode_block_count(&self, inode: &mut InodeRef) -> Result<()> {
        let data_blocks = self.extent_all_data_blocks(inode)?.len() as u64;
        let tree_blocks = self.extent_all_tree_blocks(inode)?.len() as u64;
        let sectors_per_block = (BLOCK_SIZE / INODE_BLOCK_SIZE) as u64;
        inode
            .inode
            .set_block_count((data_blocks + tree_blocks) * sectors_per_block);
        Ok(())
    }

    fn ensure_blocks_for_write_range_locked(
        &self,
        inode: &mut InodeRef,
        offset: usize,
        len: usize,
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let end = offset.checked_add(len).ok_or(format_error!(
            ErrCode::EFBIG,
            "write range overflow: offset={} len={}",
            offset,
            len
        ))?;
        let start_iblock = (offset / BLOCK_SIZE) as LBlockId;
        let end_iblock = ((end - 1) / BLOCK_SIZE) as LBlockId;
        let mut changed = false;
        for iblock in start_iblock..=end_iblock {
            match self.extent_query(inode, iblock) {
                Ok(_) => {}
                Err(err) if err.code() == ErrCode::ENOENT => {
                    self.extent_query_or_create(inode, iblock, 1)?;
                    self.extent_query(inode, iblock).map_err(|err| {
                        format_error!(
                            ErrCode::EIO,
                            "extent allocation invariant failed: inode {} iblock {} missing after create: {:?}",
                            inode.id,
                            iblock,
                            err
                        )
                    })?;
                    changed = true;
                }
                Err(err) => return Err(err),
            }
        }
        if changed {
            self.recompute_inode_block_count(inode)?;
            self.write_inode_with_csum(inode)?;
        }
        Ok(())
    }

    /// Ensure extents exist for the bytes that will actually be written.
    pub fn allocate_blocks_for_write_range(
        &self,
        id: InodeId,
        offset: usize,
        len: usize,
    ) -> Result<()> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _mutation_guard = self.inode_mutation_locks[self.inode_mutation_lock_index(id)].lock();
        let mut inode = self.read_inode(id)?;
        if inode.inode.mode().bits() == 0 {
            return_error!(ErrCode::EINVAL, "Invalid inode {}", id);
        }
        self.ensure_blocks_for_write_range_locked(&mut inode, offset, len)
    }

    /// Prepare a buffered write by allocating only the written range.
    ///
    /// The caller owns the in-memory visible size used by page-cache writeback
    /// and should call `commit_inode_size()` at fsync/truncate-style sync
    /// boundaries.
    pub fn prepare_buffered_write(
        &self,
        id: InodeId,
        offset: usize,
        len: usize,
        _size: u64,
        _mtime: Option<u32>,
    ) -> Result<()> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _mutation_guard = self.inode_mutation_locks[self.inode_mutation_lock_index(id)].lock();
        let mut inode = self.read_inode(id)?;
        if inode.inode.mode().bits() == 0 {
            return_error!(ErrCode::EINVAL, "Invalid inode {}", id);
        }
        self.ensure_blocks_for_write_range_locked(&mut inode, offset, len)?;
        Ok(())
    }

    /// Commit cached inode metadata to disk without allocating data blocks.
    pub fn commit_inode_metadata(
        &self,
        id: InodeId,
        size: Option<u64>,
        mtime: Option<u32>,
    ) -> Result<()> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _mutation_guard = self.inode_mutation_locks[self.inode_mutation_lock_index(id)].lock();
        let mut inode = self.read_inode(id)?;
        if inode.inode.mode().bits() == 0 {
            return_error!(ErrCode::EINVAL, "Invalid inode {}", id);
        }
        if let Some(size) = size {
            inode.inode.set_size(size);
        }
        if let Some(mtime) = mtime {
            inode.inode.set_mtime(mtime);
        }
        self.write_inode_with_csum(&mut inode)?;
        Ok(())
    }

    /// Commit the file size (`i_size`) and optionally `mtime` to disk,
    /// **without** allocating any blocks.
    ///
    /// Call this after successful page-cache write to finalise the new file size.
    pub fn commit_inode_size(&self, id: InodeId, size: u64, mtime: Option<u32>) -> Result<()> {
        self.commit_inode_metadata(id, Some(size), mtime)
    }

    /// Link a newly created inode into `parent`.
    ///
    /// If linking fails, this function frees the newly allocated inode to avoid leaks.
    fn link_new_inode_or_free(
        &self,
        parent: &mut InodeRef,
        child: &mut InodeRef,
        name: &str,
    ) -> Result<()> {
        if let Err(link_err) = self.link_inode(parent, child, name, false) {
            if let Err(cleanup_err) = self.free_inode(child) {
                trace!(
                    "link failed for new inode {} (name {}), cleanup failed: {:?}; original link error: {:?}",
                    child.id,
                    name,
                    cleanup_err,
                    link_err
                );
                return Err(cleanup_err);
            }
            return Err(link_err);
        }
        Ok(())
    }

    /// Create a file. This function will not check the existence of
    /// the file, call `lookup` to check beforehand.
    ///
    /// # Params
    ///
    /// * `parent` - parent directory inode id
    /// * `name` - file name
    /// * `mode` - file type and mode with which to create the new file
    /// * `flags` - open flags
    ///
    /// # Return
    ///
    /// `Ok(inode)` - Inode id of the new file
    ///
    /// # Error
    ///
    /// * `ENOTDIR` - `parent` is not a directory
    /// * `ENOSPC` - No space left on device
    pub fn create(&self, parent: InodeId, name: &str, mode: InodeMode) -> Result<InodeId> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _namespace_guard = self.namespace_lock.lock();
        let _mutation_guards = self.lock_inode_mutations(&[parent]);
        let mut parent = self.read_inode(parent)?;
        // Can only create a file in a directory
        if !parent.inode.is_dir() {
            return_error!(ErrCode::ENOTDIR, "Inode {} is not a directory", parent.id);
        }
        // Create child inode and link it to parent directory
        let mut child = self.create_inode(mode)?;
        self.link_new_inode_or_free(&mut parent, &mut child, name)?;
        // Create file handler
        Ok(child.id)
    }

    /// Create a device node (character or block device).
    ///
    /// Unlike `create()`, this function:
    /// - Does NOT initialize the extent tree
    /// - Stores the device number in i_block[0..1] (Linux ext4 standard)
    ///
    /// # Params
    ///
    /// * `parent` - parent directory inode id
    /// * `name` - device node name
    /// * `mode` - file type (must include CHARDEV or BLOCKDEV) and permissions
    /// * `major` - major device number
    /// * `minor` - minor device number
    ///
    /// # Return
    ///
    /// `Ok(inode)` - Inode id of the new device node
    ///
    /// # Error
    ///
    /// * `ENOTDIR` - `parent` is not a directory
    /// * `ENOSPC` - No space left on device
    pub fn mknod(
        &self,
        parent: InodeId,
        name: &str,
        mode: InodeMode,
        major: u32,
        minor: u32,
    ) -> Result<InodeId> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _namespace_guard = self.namespace_lock.lock();
        let _mutation_guards = self.lock_inode_mutations(&[parent]);
        let mut parent_ref = self.read_inode(parent)?;

        // Can only create in a directory
        if !parent_ref.inode.is_dir() {
            return_error!(
                ErrCode::ENOTDIR,
                "Inode {} is not a directory",
                parent_ref.id
            );
        }

        // Create device inode (uses create_device_inode which sets device number)
        let mut child = self.create_device_inode(mode, major, minor)?;

        // Link to parent directory
        self.link_new_inode_or_free(&mut parent_ref, &mut child, name)?;

        trace!("mknod {} ({}:{}) -> inode {}", name, major, minor, child.id);
        Ok(child.id)
    }

    /// Read data from a file. This function will read exactly `buf.len()`
    /// bytes unless the end of the file is reached.
    ///
    /// # Params
    ///
    /// * `file` - the file handler, acquired by `open` or `create`
    /// * `offset` - offset to read from
    /// * `buf` - the buffer to store the data
    ///
    /// # Return
    ///
    /// `Ok(usize)` - the actual number of bytes read
    ///
    /// # Error
    ///
    /// * `EISDIR` - `file` is not a regular file
    pub fn read(&self, file: InodeId, offset: usize, buf: &mut [u8]) -> Result<usize> {
        // Get the inode of the file
        let file = self.read_inode(file)?;
        if !file.inode.is_file() {
            return_error!(ErrCode::EISDIR, "Inode {} is not a file", file.id);
        }

        // Read no bytes
        if buf.is_empty() {
            return Ok(0);
        }
        let file_size = file.inode.size() as usize;
        if offset >= file_size {
            return Ok(0);
        }
        // Calc the actual size to read
        let read_size = min(buf.len(), file_size - offset);
        // Calc the start block of reading
        let start_iblock = (offset / BLOCK_SIZE) as LBlockId;
        // Calc the length that is not aligned to the block size
        let misaligned = offset % BLOCK_SIZE;

        let mut cursor = 0;
        let mut iblock = start_iblock;
        // Read first block
        if misaligned > 0 {
            let read_len = min(BLOCK_SIZE - misaligned, read_size);
            self.read_extent_or_hole(
                &file,
                start_iblock,
                misaligned,
                &mut buf[cursor..cursor + read_len],
            )?;
            cursor += read_len;
            iblock += 1;
        }
        // Continue with full block reads
        while cursor < read_size {
            let read_len = min(BLOCK_SIZE, read_size - cursor);
            self.read_extent_or_hole(&file, iblock, 0, &mut buf[cursor..cursor + read_len])?;
            cursor += read_len;
            iblock += 1;
        }

        Ok(cursor)
    }

    /// Read the target path of a symbolic link (i.e. readlink(2) semantics).
    ///
    /// - Returns the raw byte sequence of the link content (not required to end with '\0')
    /// - For fast symlink (length <= 60), content is stored in inode.i_block (here inode.block[60])
    /// - For non-fast symlink, content is stored in data blocks, reusing extent read path
    pub fn readlink(&self, inode_id: InodeId, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let inode_ref = self.read_inode(inode_id)?;
        if !inode_ref.inode.is_softlink() {
            return_error!(ErrCode::EINVAL, "Inode {} is not a symlink", inode_id);
        }
        if buf.is_empty() {
            return Ok(0);
        }

        let size = inode_ref.inode.size() as usize;
        if offset >= size {
            return Ok(0);
        }

        // fast symlink: content stored inline in inode.i_block
        let inline = inode_ref.inode.inline_block();
        if size <= inline.len() && inode_ref.inode.fs_block_count() == 0 {
            let n = core::cmp::min(buf.len(), size - offset);
            buf[..n].copy_from_slice(&inline[offset..offset + n]);
            return Ok(n);
        }

        // non-fast symlink: stored in data blocks, reuse extent-based read logic
        let read_size = min(buf.len(), size - offset);
        let start_iblock = (offset / BLOCK_SIZE) as LBlockId;
        let misaligned = offset % BLOCK_SIZE;

        let mut cursor = 0;
        let mut iblock = start_iblock;
        if misaligned > 0 {
            let read_len = min(BLOCK_SIZE - misaligned, read_size);
            self.read_extent_or_hole(
                &inode_ref,
                start_iblock,
                misaligned,
                &mut buf[cursor..cursor + read_len],
            )?;
            cursor += read_len;
            iblock += 1;
        }
        while cursor < read_size {
            let read_len = min(BLOCK_SIZE, read_size - cursor);
            self.read_extent_or_hole(&inode_ref, iblock, 0, &mut buf[cursor..cursor + read_len])?;
            cursor += read_len;
            iblock += 1;
        }

        Ok(cursor)
    }

    /// Write data to a file. This function will write exactly `data.len()` bytes.
    ///
    /// # Params
    ///
    /// * `file` - the file handler, acquired by `open` or `create`
    /// * `offset` - offset to write to
    /// * `data` - the data to write
    ///
    /// # Return
    ///
    /// `Ok(usize)` - the actual number of bytes written
    ///
    /// # Error
    ///
    /// * `EISDIR` - `file` is not a regular file
    /// * `ENOSPC` - no space left on device
    pub fn write(&self, file: InodeId, offset: usize, data: &[u8]) -> Result<usize> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let write_size = data.len();
        if write_size == 0 {
            return Ok(0);
        }
        // Get the inode of the file
        let _mutation_guard =
            self.inode_mutation_locks[self.inode_mutation_lock_index(file)].lock();
        let mut file = self.read_inode(file)?;
        if !file.inode.is_file() {
            return_error!(ErrCode::EISDIR, "Inode {} is not a file", file.id);
        }

        self.ensure_blocks_for_write_range_locked(&mut file, offset, write_size)?;

        // Write data
        let mut cursor = 0;
        let mut iblock = (offset / BLOCK_SIZE) as LBlockId;
        while cursor < write_size {
            let block_offset = (offset + cursor) % BLOCK_SIZE;
            let write_len = min(BLOCK_SIZE - block_offset, write_size - cursor);
            let fblock = self.extent_query(&file, iblock)?;
            let mut block = self.read_block(fblock)?;
            block.write_offset(block_offset, &data[cursor..cursor + write_len]);
            self.write_block(&block)?;
            cursor += write_len;
            iblock += 1;
        }
        let new_end = offset.checked_add(cursor).ok_or(format_error!(
            ErrCode::EFBIG,
            "write end overflow: offset={} len={}",
            offset,
            cursor
        ))?;
        if new_end > file.inode.size() as usize {
            file.inode.set_size(new_end as u64);
        }
        self.write_inode_with_csum(&mut file)?;

        Ok(cursor)
    }

    /// Write data to pre-allocated blocks without modifying inode metadata.
    ///
    /// This is used by page cache writeback: blocks are already allocated by
    /// `prepare_buffered_write` in the foreground `write_at` path; the writeback
    /// thread only needs to push dirty page data to the corresponding
    /// physical blocks.
    ///
    /// Unlike `write()`, this function:
    /// - Does **not** allocate blocks (`inode_append_block`)
    /// - Does **not** update inode size or write inode back to disk
    /// - Returns `ENOENT` if a required logical block has no extent mapping
    ///
    /// This eliminates the race between foreground `setattr` block-allocation
    /// and background writeback, which can corrupt the extent tree when both
    /// operate on cloned `InodeRef` snapshots from the inode cache.
    pub fn write_data_only(&self, file: InodeId, offset: usize, data: &[u8]) -> Result<usize> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let write_size = data.len();
        let mut chunks = Vec::new();
        let _mutation_guard =
            self.inode_mutation_locks[self.inode_mutation_lock_index(file)].lock();
        let file = self.read_inode(file)?;
        if !file.inode.is_file() {
            return_error!(ErrCode::EISDIR, "Inode {} is not a file", file.id);
        }

        let mut cursor = 0;
        let mut iblock = (offset / BLOCK_SIZE) as LBlockId;
        while cursor < write_size {
            let block_offset = (offset + cursor) % BLOCK_SIZE;
            let write_len = min(BLOCK_SIZE - block_offset, write_size - cursor);
            match self.extent_query(&file, iblock) {
                Ok(fblock) => {
                    chunks.push((fblock, block_offset, cursor, write_len));
                }
                Err(e) => {
                    debug!(
                            "write_data_only: extent_query FAILED ino={} iblock={} offset={} len={} fs_blkcnt={} size={} err={:?}",
                            file.id, iblock, offset, write_size,
                            file.inode.fs_block_count(), file.inode.size(), e
                        );
                    return Err(e);
                }
            }
            cursor += write_len;
            iblock += 1;
        }

        for (fblock, block_offset, cursor, write_len) in chunks {
            let mut block = self.read_block(fblock)?;
            block.write_offset(block_offset, &data[cursor..cursor + write_len]);
            self.write_block(&block)?;
        }

        Ok(write_size)
    }

    /// Create a hard link. This function will not check name conflict,
    /// call `lookup` to check beforehand.
    ///
    /// # Params
    ///
    /// * `child` - the inode of the file to link
    /// * `parent` - the inode of the directory to link to
    ///
    /// # Error
    ///
    /// * `ENOTDIR` - `parent` is not a directory
    /// * `ENOSPC` - no space left on device
    pub fn link(&self, child: InodeId, parent: InodeId, name: &str) -> Result<()> {
        self.ensure_mutable()?;
        // Relinking a zero-link inode must compose namespace publication with
        // orphan removal in one journal transaction.  Use the exclusive
        // metadata domain for both zero and nonzero link-count cases.
        let _metadata_guard = self.lock_transactional_metadata_mutation()?;
        let _namespace_guard = self.namespace_lock.lock();
        let _mutation_guards = self.lock_inode_mutations(&[parent, child]);
        let mut parent = self.read_inode(parent)?;
        // Can only link to a directory
        if !parent.inode.is_dir() {
            return_error!(ErrCode::ENOTDIR, "Inode {} is not a directory", parent.id);
        }
        let mut child = self.read_inode(child)?;
        // Cannot link a directory
        if child.inode.is_dir() {
            return_error!(ErrCode::EISDIR, "Cannot link a directory");
        }
        self.link_inode(&mut parent, &mut child, name, true)?;
        Ok(())
    }

    /// Unlink a file.
    ///
    /// # Params
    ///
    /// * `parent` - the inode of the directory to unlink from
    /// * `name` - the name of the file to unlink
    ///
    /// # Error
    ///
    /// * `ENOTDIR` - `parent` is not a directory
    /// * `ENOENT` - `name` does not exist in `parent`
    /// * `EISDIR` - `parent/name` is a directory
    pub fn unlink(&self, parent: InodeId, name: &str) -> Result<Option<InodeReclaimHandle>> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_transactional_metadata_mutation()?;
        let _namespace_guard = self.namespace_lock.lock();
        let mut parent_ref = self.read_inode(parent)?;
        // Can only unlink from a directory
        if !parent_ref.inode.is_dir() {
            return_error!(
                ErrCode::ENOTDIR,
                "Inode {} is not a directory",
                parent_ref.id
            );
        }
        // Cannot unlink directory
        let child_id = self.dir_find_entry(&parent_ref, name)?;
        let _mutation_guards = self.lock_inode_mutations(&[parent, child_id]);
        parent_ref = self.read_inode(parent)?;
        if self.dir_find_entry(&parent_ref, name)? != child_id {
            return_error!(ErrCode::ENOENT, "Namespace changed during unlink");
        }
        let mut child = self.read_inode(child_id)?;
        if child.inode.is_dir() {
            return_error!(ErrCode::EISDIR, "Cannot unlink a directory");
        }
        self.unlink_inode(&mut parent_ref, &mut child, name)
    }

    /// Helper: Read and validate parent directories for rename operations.
    ///
    /// Returns (parent_ref, Option<new_parent_ref>). If parent == new_parent,
    /// the second element is None to avoid double-locking the same inode.
    fn read_rename_dirs(
        &self,
        parent: InodeId,
        new_parent: InodeId,
    ) -> Result<(InodeRef, Option<InodeRef>)> {
        let parent_ref = self.read_inode(parent)?;
        if !parent_ref.inode.is_dir() {
            return_error!(
                ErrCode::ENOTDIR,
                "Inode {} is not a directory",
                parent_ref.id
            );
        }

        let new_parent_ref = if parent == new_parent {
            None
        } else {
            let np = self.read_inode(new_parent)?;
            if !np.inode.is_dir() {
                return_error!(ErrCode::ENOTDIR, "Inode {} is not a directory", np.id);
            }
            Some(np)
        };

        Ok((parent_ref, new_parent_ref))
    }

    /// Helper: Check if `target_dir` is a descendant of `dir_inode`.
    ///
    /// Used to prevent directory cycles in rename operations.
    /// Returns EINVAL if moving a directory into its own subdirectory.
    fn check_ancestor_cycle(&self, dir_inode: InodeId, target_dir: InodeId) -> Result<()> {
        let mut cur = target_dir;
        loop {
            if cur == dir_inode {
                return_error!(
                    ErrCode::EINVAL,
                    "Cannot move directory into its own subdirectory"
                );
            }
            if cur == EXT4_ROOT_INO {
                break;
            }
            let cur_inode = self.read_inode(cur)?;
            match self.dir_find_entry(&cur_inode, "..") {
                Ok(parent_id) if parent_id != cur => cur = parent_id,
                _ => break,
            }
        }
        Ok(())
    }

    /// Rename a directory entry, with POSIX-compliant atomic replace semantics.
    ///
    /// # POSIX Semantics
    /// - If `new_name` doesn't exist: simple rename
    /// - If `new_name` exists and is the same inode as source: no-op, return Ok
    /// - If `new_name` exists and is different inode: **atomically replace** it
    /// - Directory can only replace empty directory
    /// - Type compatibility: file<->file, dir<->dir (no cross-type replace)
    ///
    /// # Params
    ///
    /// * `parent` - the inode of the source directory
    /// * `name` - the name of the file to move
    /// * `new_parent` - the inode of the directory to move to
    /// * `new_name` - the new name of the file
    ///
    /// # Error
    ///
    /// * `ENOTDIR` - `parent` or `new_parent` is not a directory, or dir replacing non-dir
    /// * `ENOENT` - `name` does not exist in `parent`
    /// * `EISDIR` - non-dir replacing dir
    /// * `ENOTEMPTY` - target directory is not empty
    /// * `EINVAL` - would create a directory cycle (moving dir into its own subdirectory)
    /// * `ENOSPC` - no space left on device
    pub fn rename(
        &self,
        parent: InodeId,
        name: &str,
        new_parent: InodeId,
        new_name: &str,
    ) -> Result<Option<InodeReclaimHandle>> {
        self.ensure_mutable()?;
        // Rename can remove the final name of an overwritten target. Keep the
        // complete namespace transition in the exclusive domain so a follow-up
        // transactional orphan/reclaim implementation cannot inherit a stale
        // direct-writer snapshot window.
        let _metadata_guard = self.lock_transactional_metadata_mutation()?;
        let _namespace_guard = self.namespace_lock.lock();
        let mut reclaim = None;
        // 1. 验证父目录
        let (mut parent_ref, mut new_parent_ref) = self.read_rename_dirs(parent, new_parent)?;

        // 2. 查找源 inode
        let child_id = self.dir_find_entry(&parent_ref, name)?;
        let mut child = self.read_inode(child_id)?;
        let child_is_dir = child.inode.is_dir();

        // 3. 循环检测：防止把目录移到自己的子目录下
        if child_is_dir && parent != new_parent {
            self.check_ancestor_cycle(child_id, new_parent)?;
        }

        // 4. 检查目标是否存在
        let target_dir_ref = new_parent_ref.as_ref().unwrap_or(&parent_ref);
        let existing = self.dir_find_entry(target_dir_ref, new_name).ok();
        let mut mutation_ids = vec![parent, new_parent, child_id];
        if let Some(existing_id) = existing {
            mutation_ids.push(existing_id);
        }
        let _mutation_guards = self.lock_inode_mutations(&mutation_ids);
        parent_ref = self.read_inode(parent)?;
        new_parent_ref = if parent == new_parent {
            None
        } else {
            Some(self.read_inode(new_parent)?)
        };
        child = self.read_inode(child_id)?;
        let child_file_type = child.inode.file_type();

        match existing {
            Some(existing_id) if existing_id == child_id => {
                // 情况 A：源和目标是同一个 inode（硬链接或同名）
                // POSIX 语义：无操作，返回成功
                return Ok(None);
            }
            Some(existing_id) => {
                // 情况 B：目标存在且是不同 inode → 原子替换
                let mut existing_inode = self.read_inode(existing_id)?;
                let existing_is_dir = existing_inode.inode.is_dir();

                // 4b-1. 类型兼容性检查
                match (child_is_dir, existing_is_dir) {
                    (true, false) => {
                        return_error!(
                            ErrCode::ENOTDIR,
                            "Cannot replace non-directory with directory"
                        );
                    }
                    (false, true) => {
                        return_error!(
                            ErrCode::EISDIR,
                            "Cannot replace directory with non-directory"
                        );
                    }
                    (true, true) => {
                        // 目录替换目录：目标必须为空
                        if !self.dir_is_empty(&existing_inode)? {
                            return_error!(ErrCode::ENOTEMPTY, "Target directory is not empty");
                        }
                    }
                    (false, false) => {
                        // 文件替换文件：OK
                    }
                }

                let existing_link_cnt = existing_inode.inode.link_count();
                let final_target =
                    existing_link_cnt <= 1 || (existing_is_dir && existing_link_cnt <= 2);

                // Upper bound of distinct home blocks in the replace set:
                // destination dirent + source dirent + optional child "..";
                // overwritten inode + each logically changed parent inode;
                // and the superblock only for a final target.  The transaction
                // map deduplicates entries which share a directory or inode-
                // table block, so same-parent and same-block cases consume
                // fewer credits without weakening the reservation bound.
                let mut credits = 3; // two dirent blocks + overwritten inode
                if child_is_dir && parent != new_parent {
                    credits += 3; // child ".." + old parent + new parent
                }
                if existing_is_dir && !(child_is_dir && parent != new_parent) {
                    credits += 1; // target parent (new parent already counted above)
                }
                if final_target {
                    credits += 1; // superblock orphan head
                }
                let mut transaction = self.transaction_start(credits)?;

                // Match Linux ext4_rename(): ext4_setent(new), delete(old),
                // ext4_rename_dir_finish(), parent counts, target nlink, and
                // ext4_orphan_add() all belong to this single handle.
                {
                    let target_dir = new_parent_ref.as_mut().unwrap_or(&mut parent_ref);
                    self.transaction_dir_replace_entry(
                        &mut transaction,
                        target_dir,
                        new_name,
                        child_id,
                        child_file_type,
                    )?;

                    if existing_is_dir {
                        target_dir
                            .inode
                            .set_link_count(target_dir.inode.link_count() - 1);
                        self.transaction_stage_inode_with_csum(&mut transaction, target_dir)?;
                    }
                }

                self.transaction_dir_remove_entry(&mut transaction, &parent_ref, name)?;

                if child_is_dir && parent != new_parent {
                    self.transaction_dir_replace_entry(
                        &mut transaction,
                        &child,
                        "..",
                        new_parent,
                        FileType::Directory,
                    )?;

                    parent_ref
                        .inode
                        .set_link_count(parent_ref.inode.link_count() - 1);
                    self.transaction_stage_inode_with_csum(&mut transaction, &mut parent_ref)?;

                    let new_parent_dir = new_parent_ref.as_mut().ok_or(format_error!(
                        ErrCode::EINVAL,
                        "rename: missing new parent reference for directory move"
                    ))?;
                    new_parent_dir
                        .inode
                        .set_link_count(new_parent_dir.inode.link_count() + 1);
                    self.transaction_stage_inode_with_csum(&mut transaction, new_parent_dir)?;
                }

                if final_target {
                    existing_inode.inode.set_link_count(0);
                    let mut sb = self.read_super_block_cached();
                    self.transaction_orphan_add(&mut transaction, &mut existing_inode, &mut sb)?;
                } else {
                    existing_inode.inode.set_link_count(existing_link_cnt - 1);
                    self.transaction_stage_inode_with_csum(&mut transaction, &mut existing_inode)?;
                }

                if let Err(error) = transaction.commit(self.block_device.as_ref(), self) {
                    // Once commit processing starts, failures can leave an
                    // uncertain committed/checkpointed state.  Fail-stop every
                    // subsequent metadata writer on this mount.
                    self.poison(ErrCode::EIO);
                    return Err(error.error);
                }
                if final_target {
                    reclaim = Some(InodeReclaimHandle::new(
                        existing_inode.id,
                        existing_inode.inode.generation(),
                    ));
                }
                // 文件的 link count 不变（只是换了名字/位置）
            }
            None => {
                // 情况 C：目标不存在 → 简单重命名
                // Without a journal, any failure after the first namespace
                // write fail-stops this mount so a partial rename cannot be
                // followed by further allocation or metadata mutation.

                // C-1. 在目标父目录添加新条目（先 add）
                let target_dir = new_parent_ref.as_mut().unwrap_or(&mut parent_ref);
                match self.dir_add_entry_classified(target_dir, &child, new_name) {
                    Ok(()) => {}
                    Err(super::dir::DirAddFailure::Unmodified(error)) => return Err(error),
                    Err(super::dir::DirAddFailure::Indeterminate(error)) => {
                        self.poison(ErrCode::EIO);
                        return Err(error);
                    }
                }

                // C-2. 从源父目录删除旧条目（后 delete）
                self.poison_on_error(self.dir_remove_entry(&parent_ref, name))?;

                // C-3. 目录跨目录移动时，原子更新 ".." 并调整 link count
                if child_is_dir && parent != new_parent {
                    // ".." 原地替换：旧父 → 新父，单次 I/O，无中间态
                    self.poison_on_error(self.dir_replace_entry(
                        &child,
                        "..",
                        new_parent,
                        FileType::Directory,
                    ))?;

                    // 源父目录失去 ".." 引用
                    parent_ref
                        .inode
                        .set_link_count(parent_ref.inode.link_count() - 1);
                    self.poison_on_error(self.write_inode_with_csum(&mut parent_ref))?;

                    // 目标父目录获得 ".." 引用
                    let new_parent_dir = new_parent_ref.as_mut().ok_or(format_error!(
                        ErrCode::EINVAL,
                        "rename: missing new parent reference for directory move"
                    ))?;
                    new_parent_dir
                        .inode
                        .set_link_count(new_parent_dir.inode.link_count() + 1);
                    self.poison_on_error(self.write_inode_with_csum(new_parent_dir))?;
                }
                // 文件：无 ".."，nlink 不变（只换了名字/位置）
                // 目录同目录：".." 已指向正确的父，link count 不变
            }
        }

        Ok(reclaim)
    }

    /// Atomically exchange two directory entries (RENAME_EXCHANGE semantics).
    ///
    /// Both entries must exist. The operation swaps their inode references
    /// in place using `dir_replace_entry`, so directory entries never "disappear".
    ///
    /// # Params
    ///
    /// * `parent` - inode of the directory containing `name`
    /// * `name` - name of the first entry
    /// * `new_parent` - inode of the directory containing `new_name`
    /// * `new_name` - name of the second entry
    ///
    /// # Error
    ///
    /// * `ENOTDIR` - `parent` or `new_parent` is not a directory
    /// * `ENOENT` - `name` or `new_name` does not exist
    /// * `EINVAL` - would create a directory cycle
    pub fn rename_exchange(
        &self,
        parent: InodeId,
        name: &str,
        new_parent: InodeId,
        new_name: &str,
    ) -> Result<()> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _namespace_guard = self.namespace_lock.lock();
        // 1. 验证父目录
        let (mut parent_ref, mut new_parent_ref) = self.read_rename_dirs(parent, new_parent)?;

        // 2. 查找两个 inode
        let old_id = self.dir_find_entry(&parent_ref, name)?;
        let target_dir_ref = new_parent_ref.as_ref().unwrap_or(&parent_ref);
        let new_id = self.dir_find_entry(target_dir_ref, new_name)?;
        let _mutation_guards = self.lock_inode_mutations(&[parent, new_parent, old_id, new_id]);
        parent_ref = self.read_inode(parent)?;
        new_parent_ref = if parent == new_parent {
            None
        } else {
            Some(self.read_inode(new_parent)?)
        };
        let old_inode = self.read_inode(old_id)?;
        let old_is_dir = old_inode.inode.is_dir();
        let old_type = old_inode.inode.file_type();
        let new_inode = self.read_inode(new_id)?;
        let new_is_dir = new_inode.inode.is_dir();
        let new_type = new_inode.inode.file_type();

        // 3. 同一 inode → 无操作
        if old_id == new_id {
            return Ok(());
        }

        // 4. 循环检测（仅跨目录时需要，exchange 需要检查双向）
        if parent != new_parent {
            if old_is_dir {
                self.check_ancestor_cycle(old_id, new_parent)?;
            }
            if new_is_dir {
                self.check_ancestor_cycle(new_id, parent)?;
            }
        }

        // 5. 原子交换：原地替换目录项的 inode 引用
        if parent == new_parent {
            self.poison_on_error(self.dir_replace_entry(&parent_ref, name, new_id, new_type))?;
            self.poison_on_error(self.dir_replace_entry(&parent_ref, new_name, old_id, old_type))?;
        } else {
            self.poison_on_error(self.dir_replace_entry(&parent_ref, name, new_id, new_type))?;
            let new_parent_dir = new_parent_ref.as_ref().ok_or(format_error!(
                ErrCode::EINVAL,
                "rename_exchange: missing new parent reference for cross-dir exchange"
            ))?;
            self.poison_on_error(self.dir_replace_entry(
                new_parent_dir,
                new_name,
                old_id,
                old_type,
            ))?;
        }

        // 6. 跨目录时更新目录的 ".." 指向和父目录 link_count
        if parent != new_parent {
            if old_is_dir {
                self.poison_on_error(self.dir_replace_entry(
                    &old_inode,
                    "..",
                    new_parent,
                    FileType::Directory,
                ))?;
                parent_ref
                    .inode
                    .set_link_count(parent_ref.inode.link_count() - 1);
                self.poison_on_error(self.write_inode_with_csum(&mut parent_ref))?;
                let np = new_parent_ref.as_mut().ok_or(format_error!(
                    ErrCode::EINVAL,
                    "rename_exchange: missing new parent reference for old_dir update"
                ))?;
                np.inode.set_link_count(np.inode.link_count() + 1);
                self.poison_on_error(self.write_inode_with_csum(np))?;
            }
            if new_is_dir {
                self.poison_on_error(self.dir_replace_entry(
                    &new_inode,
                    "..",
                    parent,
                    FileType::Directory,
                ))?;
                let np = new_parent_ref.as_mut().ok_or(format_error!(
                    ErrCode::EINVAL,
                    "rename_exchange: missing new parent reference for new_dir update"
                ))?;
                np.inode.set_link_count(np.inode.link_count() - 1);
                self.poison_on_error(self.write_inode_with_csum(np))?;
                parent_ref
                    .inode
                    .set_link_count(parent_ref.inode.link_count() + 1);
                self.poison_on_error(self.write_inode_with_csum(&mut parent_ref))?;
            }
        }

        Ok(())
    }

    /// Create a directory. This function will not check name conflict,
    /// call `lookup` to check beforehand.
    ///
    /// # Params
    ///
    /// * `parent` - the inode of the directory to create in
    /// * `name` - the name of the directory to create
    /// * `mode` - the mode of the directory to create, type field will be ignored
    ///
    /// # Return
    ///
    /// `Ok(child)` - the inode id of the created directory
    ///
    /// # Error
    ///
    /// * `ENOTDIR` - `parent` is not a directory
    /// * `ENOSPC` - no space left on device
    pub fn mkdir(&self, parent: InodeId, name: &str, mode: InodeMode) -> Result<InodeId> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _namespace_guard = self.namespace_lock.lock();
        let _mutation_guards = self.lock_inode_mutations(&[parent]);
        let mut parent = self.read_inode(parent)?;
        // Can only create a directory in a directory
        if !parent.inode.is_dir() {
            return_error!(ErrCode::ENOTDIR, "Inode {} is not a directory", parent.id);
        }
        // Create file/directory
        let mode = mode & InodeMode::PERM_MASK | InodeMode::DIRECTORY;
        let mut child = self.create_inode(mode)?;
        // Add "." entry
        let child_self = child.clone();
        if let Err(error) = self.dir_add_entry(&mut child, &child_self, ".") {
            if self.free_inode(&mut child).is_err() {
                self.poison(ErrCode::EIO);
            }
            return Err(error);
        }
        child.inode.set_link_count(1);
        // Link the new inode
        self.link_new_inode_or_free(&mut parent, &mut child, name)?;
        Ok(child.id)
    }

    /// Look up a directory entry by name.
    ///
    /// # Params
    ///
    /// * `parent` - the inode of the directory to look in
    /// * `name` - the name of the entry to look for
    ///
    /// # Return
    ///
    /// `Ok(child)`- the inode id to which the directory entry points.
    ///
    /// # Error
    ///
    /// * `ENOTDIR` - `parent` is not a directory
    /// * `ENOENT` - `name` does not exist in `parent`
    pub fn lookup(&self, parent: InodeId, name: &str) -> Result<InodeId> {
        let parent = self.read_inode(parent)?;
        // Can only lookup in a directory
        if !parent.inode.is_dir() {
            return_error!(ErrCode::ENOTDIR, "Inode {} is not a directory", parent.id);
        }
        self.dir_find_entry(&parent, name)
    }

    /// List all directory entries in a directory.
    ///
    /// # Params
    ///
    /// * `inode` - the inode of the directory to list
    ///
    /// # Return
    ///
    /// `Ok(entries)` - a vector of directory entries in the directory.
    ///
    /// # Error
    ///
    /// `ENOTDIR` - `inode` is not a directory
    pub fn listdir(&self, inode: InodeId) -> Result<Vec<DirEntry>> {
        let inode_ref = self.read_inode(inode)?;
        // Can only list a directory
        if inode_ref.inode.file_type() != FileType::Directory {
            return_error!(ErrCode::ENOTDIR, "Inode {} is not a directory", inode);
        }
        self.dir_list_entries(&inode_ref)
    }

    /// Remove an empty directory.
    ///
    /// # Params
    ///
    /// * `parent` - the parent directory where the directory is located
    /// * `name` - the name of the directory to remove
    ///
    /// # Error
    ///
    /// * `ENOTDIR` - `parent` or `child` is not a directory
    /// * `ENOENT` - `name` does not exist in `parent`
    /// * `ENOTEMPTY` - `child` is not empty
    pub fn rmdir(&self, parent: InodeId, name: &str) -> Result<Option<InodeReclaimHandle>> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_transactional_metadata_mutation()?;
        let _namespace_guard = self.namespace_lock.lock();
        let mut parent_ref = self.read_inode(parent)?;
        // Can only remove a directory in a directory
        if !parent_ref.inode.is_dir() {
            return_error!(
                ErrCode::ENOTDIR,
                "Inode {} is not a directory",
                parent_ref.id
            );
        }
        let child_id = self.dir_find_entry(&parent_ref, name)?;
        let _mutation_guards = self.lock_inode_mutations(&[parent, child_id]);
        parent_ref = self.read_inode(parent)?;
        if self.dir_find_entry(&parent_ref, name)? != child_id {
            return_error!(ErrCode::ENOENT, "Namespace changed during rmdir");
        }
        let mut child = self.read_inode(child_id)?;
        // Child must be a directory
        if !child.inode.is_dir() {
            return_error!(ErrCode::ENOTDIR, "Inode {} is not a directory", child.id);
        }
        // Child must be empty
        if self.dir_list_entries(&child)?.len() > 2 {
            return_error!(ErrCode::ENOTEMPTY, "Directory {} is not empty", child.id);
        }
        // Remove directory entry
        self.unlink_inode(&mut parent_ref, &mut child, name)
    }

    /// Get extended attribute of a file.
    ///
    /// # Params
    ///
    /// * `inode` - the inode of the file
    /// * `name` - the name of the attribute
    ///
    /// # Return
    ///
    /// `Ok(value)` - the value of the attribute
    ///
    /// # Error
    ///
    /// `ENODATA` - the attribute does not exist
    pub fn getxattr(&self, inode: InodeId, name: &str) -> Result<Vec<u8>> {
        let inode_ref = self.read_inode(inode)?;
        let xattr_block_id = inode_ref.inode.xattr_block();
        if xattr_block_id == 0 {
            return_error!(ErrCode::ENODATA, "Xattr {} does not exist", name);
        }
        let xattr_block = XattrBlock::new(self.read_block(xattr_block_id)?);
        match xattr_block.get(name) {
            Some(value) => Ok(value.to_owned()),
            None => Err(format_error!(
                ErrCode::ENODATA,
                "Xattr {} does not exist",
                name
            )),
        }
    }

    /// Set extended attribute of a file.
    ///
    /// # Params
    ///
    /// * `inode` - the inode of the file
    /// * `name` - the name of the attribute
    /// * `value` - the value of the attribute
    ///
    /// # Error
    ///
    /// `ENOSPC` - xattr block does not have enough space
    pub fn setxattr(&self, inode: InodeId, name: &str, value: &[u8]) -> Result<()> {
        self.ensure_mutable()?;
        self.setxattr_with_flags(inode, name, value, false, false)
    }

    /// Set extended attribute of a file with Linux create/replace semantics.
    ///
    /// Existing xattr blocks are modified on a cloned candidate block first and
    /// written back only after the whole operation succeeds. This preserves the
    /// old value when replacing with a value that does not fit.
    pub fn setxattr_with_flags(
        &self,
        inode: InodeId,
        name: &str,
        value: &[u8],
        create: bool,
        replace: bool,
    ) -> Result<()> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _mutation_guard =
            self.inode_mutation_locks[self.inode_mutation_lock_index(inode)].lock();
        let mut inode_ref = self.read_inode(inode)?;
        let xattr_block_id = inode_ref.inode.xattr_block();
        if xattr_block_id == 0 {
            if replace {
                return_error!(ErrCode::ENODATA, "Xattr {} does not exist", name);
            }
            // lazy allocate xattr block
            let pblock = self.alloc_block(&mut inode_ref)?;
            let old_xattr_block = xattr_block_id;
            let result = (|| {
                let mut xattr_block = XattrBlock::new(self.read_block(pblock)?);
                xattr_block.init();
                if !xattr_block.insert(name, value) {
                    return_error!(
                        ErrCode::ENOSPC,
                        "Xattr block of Inode {} does not have enough space",
                        inode
                    );
                }
                self.write_block(&xattr_block.block())?;
                inode_ref.inode.set_xattr_block(pblock);
                self.write_inode_with_csum(&mut inode_ref)?;
                Ok(())
            })();
            if let Err(err) = result {
                inode_ref.inode.set_xattr_block(old_xattr_block);
                return match self.dealloc_block(&mut inode_ref, pblock) {
                    Ok(()) => Err(err),
                    Err(rollback_err) => Err(rollback_err),
                };
            }
            return Ok(());
        }

        let xattr_block = XattrBlock::new(self.read_block(xattr_block_id)?);
        let exists = xattr_block.get(name).is_some();
        if exists && create {
            return_error!(ErrCode::EEXIST, "Xattr {} already exists", name);
        }
        if !exists && replace {
            return_error!(ErrCode::ENODATA, "Xattr {} does not exist", name);
        }

        let mut new_xattr_block = xattr_block;
        if exists {
            let _ = new_xattr_block.remove(name);
        }
        if new_xattr_block.insert(name, value) {
            self.write_block(&new_xattr_block.block())?;
            Ok(())
        } else {
            return_error!(
                ErrCode::ENOSPC,
                "Xattr block of Inode {} does not have enough space",
                inode
            );
        }
    }

    /// Remove extended attribute of a file.
    ///
    /// # Params
    ///
    /// * `inode` - the inode of the file
    /// * `name` - the name of the attribute
    ///
    /// # Error
    ///
    /// `ENODATA` - the attribute does not exist
    pub fn removexattr(&self, inode: InodeId, name: &str) -> Result<()> {
        self.ensure_mutable()?;
        let _metadata_guard = self.lock_direct_metadata_mutation()?;
        let _mutation_guard =
            self.inode_mutation_locks[self.inode_mutation_lock_index(inode)].lock();
        let inode_ref = self.read_inode(inode)?;
        let xattr_block_id = inode_ref.inode.xattr_block();
        if xattr_block_id == 0 {
            return_error!(ErrCode::ENODATA, "Xattr {} does not exist", name);
        }
        let mut xattr_block = XattrBlock::new(self.read_block(xattr_block_id)?);
        if xattr_block.remove(name) {
            self.write_block(&xattr_block.block())?;
            Ok(())
        } else {
            return_error!(ErrCode::ENODATA, "Xattr {} does not exist", name);
        }
    }

    /// List extended attributes of a file.
    ///
    /// # Params
    ///
    /// * `inode` - the inode of the file
    ///
    /// # Returns
    ///
    /// A list of extended attributes of the file.
    pub fn listxattr(&self, inode: InodeId) -> Result<Vec<String>> {
        let inode_ref = self.read_inode(inode)?;
        let xattr_block_id = inode_ref.inode.xattr_block();
        if xattr_block_id == 0 {
            return Ok(Vec::new());
        }
        let xattr_block = XattrBlock::new(self.read_block(xattr_block_id)?);
        Ok(xattr_block.list())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FileType;

    struct StubBlockDevice {
        sb_block: Block,
    }

    impl StubBlockDevice {
        fn with_block_count(block_count: u32) -> Self {
            let mut data = [0u8; BLOCK_SIZE];
            let off = BASE_OFFSET;
            data[off..off + 4].copy_from_slice(&block_count.to_le_bytes());
            Self {
                sb_block: Block::new(0, Box::new(data)),
            }
        }
    }

    impl BlockDevice for StubBlockDevice {
        fn read_block(&self, block_id: PBlockId) -> Result<Block> {
            if block_id == 0 {
                Ok(self.sb_block.clone())
            } else {
                Ok(Block::new(block_id, Box::new([0u8; BLOCK_SIZE])))
            }
        }

        fn write_block(&self, _block: &Block) -> Result<()> {
            Ok(())
        }

        fn flush(&self) -> Result<()> {
            Ok(())
        }
        fn supports_reliable_flush(&self) -> bool {
            true
        }
    }

    fn make_test_fs(block_count: u32) -> Ext4 {
        let block_device = Arc::new(StubBlockDevice::with_block_count(block_count));
        let block = block_device.read_block(0).unwrap();
        let sb = block.read_offset_as::<SuperBlock>(BASE_OFFSET);
        Ext4 {
            block_device,
            cached_super_block: spin::Mutex::new(sb),
            cached_block_groups: Vec::new(),
            system_metadata_ranges: Vec::new(),
            inode_cache: spin::Mutex::new(crate::ext4::InodeCache::new(16)),
            alloc_lock: spin::Mutex::new(()),
            namespace_lock: spin::Mutex::new(()),
            metadata_mutation_barrier: crate::ext4::MetadataMutationGate::new(),
            poisoned: spin::Mutex::new(None),
            journal: None,
            inode_mutation_locks: (0..crate::ext4::INODE_MUTATION_LOCK_SHARDS)
                .map(|_| spin::Mutex::new(()))
                .collect(),
        }
    }

    #[test]
    fn read_extent_or_hole_zero_fills_only_missing_extent() {
        let fs = make_test_fs(16);
        let mut inode = Inode::default();
        inode.extent_init();
        let inode = InodeRef::new(2, Box::new(inode));
        let mut buf = [0x5a; 16];

        fs.read_extent_or_hole(&inode, 0, 0, &mut buf).unwrap();

        assert_eq!(buf, [0; 16]);
    }

    #[test]
    fn metadata_mutation_barrier_separates_direct_and_transactional_writers() {
        let fs = make_test_fs(16);

        let direct = fs.lock_direct_metadata_mutation().unwrap();
        let second_direct = fs.lock_direct_metadata_mutation().unwrap();
        assert_eq!(
            fs.lock_transactional_metadata_mutation()
                .expect_err("exclusive gate must not wait for direct owners")
                .code(),
            ErrCode::EAGAIN
        );
        drop(second_direct);
        drop(direct);

        let transaction = fs.lock_transactional_metadata_mutation().unwrap();
        assert_eq!(
            fs.lock_direct_metadata_mutation()
                .expect_err("direct gate must not wait for exclusive owner")
                .code(),
            ErrCode::EAGAIN
        );
        assert_eq!(
            fs.lock_transactional_metadata_mutation()
                .expect_err("second exclusive owner must be rejected")
                .code(),
            ErrCode::EAGAIN
        );
        drop(transaction);
        drop(fs.lock_transactional_metadata_mutation().unwrap());
        drop(fs.lock_direct_metadata_mutation().unwrap());
    }

    #[test]
    fn metadata_mutation_barrier_rejects_direct_count_overflow() {
        let fs = make_test_fs(16);
        fs.metadata_mutation_barrier.state.store(
            crate::ext4::METADATA_GATE_DIRECT_MAX,
            core::sync::atomic::Ordering::Relaxed,
        );
        assert_eq!(
            fs.lock_direct_metadata_mutation()
                .expect_err("direct count must not enter the exclusive bit")
                .code(),
            ErrCode::EAGAIN
        );
        fs.metadata_mutation_barrier
            .state
            .store(0, core::sync::atomic::Ordering::Relaxed);
    }

    #[test]
    fn metadata_mutation_barrier_allows_concurrent_direct_owners() {
        let fs = make_test_fs(16);
        let start = std::sync::Barrier::new(3);
        let release = std::sync::Barrier::new(3);
        let (sender, receiver) = std::sync::mpsc::channel();

        std::thread::scope(|scope| {
            for _ in 0..2 {
                let sender = sender.clone();
                let start = &start;
                let release = &release;
                let fs = &fs;
                scope.spawn(move || {
                    start.wait();
                    let guard = fs.lock_direct_metadata_mutation();
                    sender.send(guard.is_ok()).unwrap();
                    release.wait();
                    drop(guard);
                });
            }
            start.wait();
            assert!(receiver.recv().unwrap());
            assert!(receiver.recv().unwrap());
            assert_eq!(
                fs.lock_transactional_metadata_mutation()
                    .expect_err("both direct guards must remain live")
                    .code(),
                ErrCode::EAGAIN
            );
            release.wait();
        });
        drop(fs.lock_transactional_metadata_mutation().unwrap());
    }

    #[test]
    fn read_extent_or_hole_propagates_extent_corruption() {
        let fs = make_test_fs(16);
        let inode = InodeRef::new(2, Box::new(Inode::default()));
        let mut buf = [0x5a; 16];

        let err = fs
            .read_extent_or_hole(&inode, 0, 0, &mut buf)
            .expect_err("invalid extent root must not be treated as a hole");

        assert_eq!(err.code(), ErrCode::EIO);
        assert_eq!(buf, [0x5a; 16]);
    }

    const TEST_BLOCK_COUNT: usize = 16;
    const TEST_BLOCK_BITMAP: PBlockId = 2;
    const TEST_INODE_BITMAP: PBlockId = 3;
    const TEST_INODE_TABLE: PBlockId = 4;
    const TEST_XATTR_BLOCK: PBlockId = 5;
    const TEST_INITIAL_FREE_BLOCKS: u64 = (TEST_BLOCK_COUNT as u64) - 5;

    struct FailingBlockDevice {
        blocks: spin::Mutex<BTreeMap<PBlockId, Block>>,
        fail_reads: spin::Mutex<Vec<PBlockId>>,
        fail_writes: spin::Mutex<Vec<PBlockId>>,
    }

    impl FailingBlockDevice {
        fn new() -> Self {
            let mut blocks = BTreeMap::new();
            for block_id in 0..TEST_BLOCK_COUNT as PBlockId {
                blocks.insert(block_id, Block::new(block_id, Box::new([0u8; BLOCK_SIZE])));
            }

            let mut sb_block = blocks.remove(&0).unwrap();
            Self::write_u32(&mut sb_block, BASE_OFFSET, 16);
            Self::write_u32(&mut sb_block, BASE_OFFSET + 4, TEST_BLOCK_COUNT as u32);
            Self::write_u32(
                &mut sb_block,
                BASE_OFFSET + 12,
                TEST_INITIAL_FREE_BLOCKS as u32,
            );
            Self::write_u32(&mut sb_block, BASE_OFFSET + 20, 0);
            Self::write_u32(&mut sb_block, BASE_OFFSET + 24, 2);
            Self::write_u32(&mut sb_block, BASE_OFFSET + 28, 2);
            Self::write_u32(&mut sb_block, BASE_OFFSET + 32, TEST_BLOCK_COUNT as u32);
            Self::write_u32(&mut sb_block, BASE_OFFSET + 36, TEST_BLOCK_COUNT as u32);
            Self::write_u32(&mut sb_block, BASE_OFFSET + 40, 16);
            Self::write_u16(&mut sb_block, BASE_OFFSET + 56, 0xef53);
            Self::write_u32(&mut sb_block, BASE_OFFSET + 84, 1);
            Self::write_u16(&mut sb_block, BASE_OFFSET + 88, SB_GOOD_INODE_SIZE as u16);
            Self::write_u16(&mut sb_block, BASE_OFFSET + 254, SB_GOOD_DESC_SIZE as u16);
            blocks.insert(0, sb_block);

            let mut bgdt = blocks.remove(&1).unwrap();
            Self::write_u32(&mut bgdt, 0, TEST_BLOCK_BITMAP as u32);
            Self::write_u32(&mut bgdt, 4, TEST_INODE_BITMAP as u32);
            Self::write_u32(&mut bgdt, 8, TEST_INODE_TABLE as u32);
            Self::write_u16(&mut bgdt, 12, TEST_INITIAL_FREE_BLOCKS as u16);
            blocks.insert(1, bgdt);

            let mut bitmap = blocks.remove(&TEST_BLOCK_BITMAP).unwrap();
            bitmap.data[0] = 0b0001_1111;
            blocks.insert(TEST_BLOCK_BITMAP, bitmap);

            let mut inode_table = blocks.remove(&TEST_INODE_TABLE).unwrap();
            let mut inode = Inode::default();
            inode.set_mode(InodeMode::from_type_and_perm(
                FileType::RegularFile,
                InodeMode::from_bits_retain(0o644),
            ));
            inode.set_link_count(1);
            inode_table.write_offset_as(SB_GOOD_INODE_SIZE, &inode);
            blocks.insert(TEST_INODE_TABLE, inode_table);

            Self {
                blocks: spin::Mutex::new(blocks),
                fail_reads: spin::Mutex::new(Vec::new()),
                fail_writes: spin::Mutex::new(Vec::new()),
            }
        }

        fn write_u16(block: &mut Block, offset: usize, value: u16) {
            block.write_offset(offset, &value.to_le_bytes());
        }

        fn write_u32(block: &mut Block, offset: usize, value: u32) {
            block.write_offset(offset, &value.to_le_bytes());
        }

        fn fail_once_on_read(&self, block_id: PBlockId) {
            self.fail_reads.lock().push(block_id);
        }

        fn fail_once_on_write(&self, block_id: PBlockId) {
            self.fail_writes.lock().push(block_id);
        }

        fn take_failure(list: &mut Vec<PBlockId>, block_id: PBlockId) -> bool {
            if let Some(pos) = list.iter().position(|&id| id == block_id) {
                list.remove(pos);
                true
            } else {
                false
            }
        }

        fn block_bitmap_bit_is_set(&self, bit: usize) -> bool {
            let blocks = self.blocks.lock();
            let block = blocks.get(&TEST_BLOCK_BITMAP).unwrap();
            (block.data[bit / 8] & (1 << (bit % 8))) != 0
        }

        fn bg_free_blocks(&self) -> u64 {
            let blocks = self.blocks.lock();
            let block = blocks.get(&1).unwrap();
            u16::from_le_bytes(block.data[12..14].try_into().unwrap()) as u64
        }

        fn sb_free_blocks(&self) -> u64 {
            let blocks = self.blocks.lock();
            let block = blocks.get(&0).unwrap();
            u32::from_le_bytes(
                block.data[BASE_OFFSET + 12..BASE_OFFSET + 16]
                    .try_into()
                    .unwrap(),
            ) as u64
        }

        fn disk_inode_xattr_block(&self) -> PBlockId {
            let blocks = self.blocks.lock();
            let block = blocks.get(&TEST_INODE_TABLE).unwrap();
            let inode: Inode = block.read_offset_as(SB_GOOD_INODE_SIZE);
            inode.xattr_block()
        }

        fn fill_block(&self, block_id: PBlockId, byte: u8) {
            self.blocks
                .lock()
                .get_mut(&block_id)
                .unwrap()
                .data
                .fill(byte);
        }

        fn block_is_zero(&self, block_id: PBlockId) -> bool {
            self.blocks
                .lock()
                .get(&block_id)
                .unwrap()
                .data
                .iter()
                .all(|byte| *byte == 0)
        }
    }

    impl BlockDevice for FailingBlockDevice {
        fn read_block(&self, block_id: PBlockId) -> Result<Block> {
            if Self::take_failure(&mut self.fail_reads.lock(), block_id) {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            self.blocks
                .lock()
                .get(&block_id)
                .cloned()
                .ok_or_else(|| Ext4Error::new(ErrCode::EIO))
        }

        fn write_block(&self, block: &Block) -> Result<()> {
            if Self::take_failure(&mut self.fail_writes.lock(), block.id) {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            self.blocks.lock().insert(block.id, block.clone());
            Ok(())
        }

        fn flush(&self) -> Result<()> {
            Ok(())
        }
        fn supports_reliable_flush(&self) -> bool {
            true
        }
    }

    fn load_failing_test_fs() -> (Arc<FailingBlockDevice>, Ext4) {
        let block_device = Arc::new(FailingBlockDevice::new());
        let fs = Ext4::load(block_device.clone()).unwrap();
        (block_device, fs)
    }

    fn assert_xattr_alloc_rolled_back(fs: &Ext4, block_device: &FailingBlockDevice) {
        assert!(!block_device.block_bitmap_bit_is_set(TEST_XATTR_BLOCK as usize));
        assert_eq!(block_device.bg_free_blocks(), TEST_INITIAL_FREE_BLOCKS);
        assert_eq!(block_device.sb_free_blocks(), TEST_INITIAL_FREE_BLOCKS);
        assert_eq!(
            fs.read_block_group(0).unwrap().desc.get_free_blocks_count(),
            TEST_INITIAL_FREE_BLOCKS
        );
        assert_eq!(
            fs.read_super_block_cached().free_blocks_count(),
            TEST_INITIAL_FREE_BLOCKS
        );
        assert_eq!(block_device.disk_inode_xattr_block(), 0);
    }

    fn assert_allocation_state(
        fs: &Ext4,
        block_device: &FailingBlockDevice,
        allocated: bool,
        free_blocks: u64,
    ) {
        assert_eq!(
            block_device.block_bitmap_bit_is_set(TEST_XATTR_BLOCK as usize),
            allocated
        );
        assert_eq!(block_device.bg_free_blocks(), free_blocks);
        assert_eq!(block_device.sb_free_blocks(), free_blocks);
        assert_eq!(
            fs.read_block_group(0).unwrap().desc.get_free_blocks_count(),
            free_blocks
        );
        assert_eq!(
            fs.read_super_block_cached().free_blocks_count(),
            free_blocks
        );
    }

    #[test]
    fn setxattr_rolls_back_when_new_xattr_block_read_fails() {
        let (block_device, fs) = load_failing_test_fs();
        block_device.fail_once_on_read(TEST_XATTR_BLOCK);

        let err = fs
            .setxattr_with_flags(2, "user.rollback", b"value", false, false)
            .unwrap_err();

        assert_eq!(err.code(), ErrCode::EIO);
        assert_xattr_alloc_rolled_back(&fs, &block_device);
    }

    #[test]
    fn setxattr_rolls_back_when_new_xattr_block_write_fails() {
        let (block_device, fs) = load_failing_test_fs();
        block_device.fail_once_on_write(TEST_XATTR_BLOCK);

        let err = fs
            .setxattr_with_flags(2, "user.rollback", b"value", false, false)
            .unwrap_err();

        assert_eq!(err.code(), ErrCode::EIO);
        assert_xattr_alloc_rolled_back(&fs, &block_device);
    }

    #[test]
    fn setxattr_rolls_back_when_inode_write_fails() {
        let (block_device, fs) = load_failing_test_fs();
        block_device.fail_once_on_write(TEST_INODE_TABLE);

        let err = fs
            .setxattr_with_flags(2, "user.rollback", b"value", false, false)
            .unwrap_err();

        assert_eq!(err.code(), ErrCode::EIO);
        assert_xattr_alloc_rolled_back(&fs, &block_device);
    }

    #[test]
    fn setxattr_rolls_back_when_new_xattr_does_not_fit() {
        let (block_device, fs) = load_failing_test_fs();
        let value = vec![0x5au8; BLOCK_SIZE];

        let err = fs
            .setxattr_with_flags(2, "user.rollback", &value, false, false)
            .unwrap_err();

        assert_eq!(err.code(), ErrCode::ENOSPC);
        assert_xattr_alloc_rolled_back(&fs, &block_device);
    }

    #[test]
    fn block_group_cache_updates_only_after_disk_write_succeeds() {
        let (block_device, fs) = load_failing_test_fs();
        let mut bg = fs.read_block_group(0).unwrap();
        bg.desc.set_free_blocks_count(TEST_INITIAL_FREE_BLOCKS - 1);
        block_device.fail_once_on_write(1);

        let err = fs.write_block_group_with_csum(&mut bg).unwrap_err();

        assert_eq!(err.code(), ErrCode::EIO);
        assert_eq!(
            fs.read_block_group(0).unwrap().desc.get_free_blocks_count(),
            TEST_INITIAL_FREE_BLOCKS
        );
        assert_eq!(block_device.bg_free_blocks(), TEST_INITIAL_FREE_BLOCKS);
    }

    #[test]
    fn alloc_block_rolls_back_when_block_group_write_fails() {
        let (block_device, fs) = load_failing_test_fs();
        let mut inode = fs.read_inode(2).unwrap();
        block_device.fail_once_on_write(1);

        let err = fs.alloc_block(&mut inode).unwrap_err();

        assert_eq!(err.code(), ErrCode::EIO);
        assert_allocation_state(&fs, &block_device, false, TEST_INITIAL_FREE_BLOCKS);
    }

    #[test]
    fn alloc_block_rolls_back_when_superblock_write_fails() {
        let (block_device, fs) = load_failing_test_fs();
        let mut inode = fs.read_inode(2).unwrap();
        block_device.fail_once_on_write(0);

        let err = fs.alloc_block(&mut inode).unwrap_err();

        assert_eq!(err.code(), ErrCode::EIO);
        assert_allocation_state(&fs, &block_device, false, TEST_INITIAL_FREE_BLOCKS);
    }

    #[test]
    fn newly_reused_data_block_is_zeroed_before_mapping() {
        let (block_device, fs) = load_failing_test_fs();
        let mut inode = fs.read_inode(2).unwrap();
        block_device.fill_block(TEST_XATTR_BLOCK, 0xa5);

        let pblock = fs.alloc_zeroed_data_block(&mut inode).unwrap();

        assert_eq!(pblock, TEST_XATTR_BLOCK);
        assert!(block_device.block_is_zero(pblock));
        assert_allocation_state(&fs, &block_device, true, TEST_INITIAL_FREE_BLOCKS - 1);
    }

    #[test]
    fn data_block_zero_write_failure_rolls_back_allocation() {
        let (block_device, fs) = load_failing_test_fs();
        let mut inode = fs.read_inode(2).unwrap();
        block_device.fill_block(TEST_XATTR_BLOCK, 0xa5);
        block_device.fail_once_on_write(TEST_XATTR_BLOCK);

        let err = fs.alloc_zeroed_data_block(&mut inode).unwrap_err();

        assert_eq!(err.code(), ErrCode::EIO);
        assert_allocation_state(&fs, &block_device, false, TEST_INITIAL_FREE_BLOCKS);
        assert!(!block_device.block_is_zero(TEST_XATTR_BLOCK));
    }

    #[test]
    fn dealloc_block_rolls_back_when_block_group_write_fails() {
        let (block_device, fs) = load_failing_test_fs();
        let mut inode = fs.read_inode(2).unwrap();
        let pblock = fs.alloc_block(&mut inode).unwrap();
        assert_eq!(pblock, TEST_XATTR_BLOCK);
        assert_allocation_state(&fs, &block_device, true, TEST_INITIAL_FREE_BLOCKS - 1);
        block_device.fail_once_on_write(1);

        let err = fs.dealloc_block(&mut inode, pblock).unwrap_err();

        assert_eq!(err.code(), ErrCode::EIO);
        assert_allocation_state(&fs, &block_device, true, TEST_INITIAL_FREE_BLOCKS - 1);
    }

    #[test]
    fn dealloc_block_rolls_back_when_superblock_write_fails() {
        let (block_device, fs) = load_failing_test_fs();
        let mut inode = fs.read_inode(2).unwrap();
        let pblock = fs.alloc_block(&mut inode).unwrap();
        assert_eq!(pblock, TEST_XATTR_BLOCK);
        assert_allocation_state(&fs, &block_device, true, TEST_INITIAL_FREE_BLOCKS - 1);
        block_device.fail_once_on_write(0);

        let err = fs.dealloc_block(&mut inode, pblock).unwrap_err();

        assert_eq!(err.code(), ErrCode::EIO);
        assert_allocation_state(&fs, &block_device, true, TEST_INITIAL_FREE_BLOCKS - 1);
    }
}
