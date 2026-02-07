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
    /// `EINVAL` if the inode is invalid (link count == 0).
    pub fn getattr(&self, id: InodeId) -> Result<FileAttr> {
        let inode = self.read_inode(id);
        if inode.inode.link_count() == 0 {
            return_error!(ErrCode::EINVAL, "Invalid inode {}", id);
        }
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
        let mut inode = self.read_inode(id);
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
            // If size increases, allocate new blocks if needed.
            let required_blocks = (size as usize).div_ceil(INODE_BLOCK_SIZE);
            for _ in inode.inode.block_count()..required_blocks as u64 {
                self.inode_append_block(&mut inode)?;
            }
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
        self.write_inode_with_csum(&mut inode);
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
        let mut parent = self.read_inode(parent);
        // Can only create a file in a directory
        if !parent.inode.is_dir() {
            return_error!(ErrCode::ENOTDIR, "Inode {} is not a directory", parent.id);
        }
        // Create child inode and link it to parent directory
        let mut child = self.create_inode(mode)?;
        self.link_inode(&mut parent, &mut child, name)?;
        // Create file handler
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
        let file = self.read_inode(file);
        if !file.inode.is_file() {
            return_error!(ErrCode::EISDIR, "Inode {} is not a file", file.id);
        }

        // Read no bytes
        if buf.is_empty() {
            return Ok(0);
        }
        // Calc the actual size to read
        let read_size = min(buf.len(), file.inode.size() as usize - offset);
        // Calc the start block of reading
        let start_iblock = (offset / BLOCK_SIZE) as LBlockId;
        // Calc the length that is not aligned to the block size
        let misaligned = offset % BLOCK_SIZE;

        let mut cursor = 0;
        let mut iblock = start_iblock;
        // Read first block
        if misaligned > 0 {
            let read_len = min(BLOCK_SIZE - misaligned, read_size);
            let fblock = self.extent_query(&file, start_iblock).unwrap();
            let block = self.read_block(fblock);
            // Copy data from block to the user buffer
            buf[cursor..cursor + read_len].copy_from_slice(block.read_offset(misaligned, read_len));
            cursor += read_len;
            iblock += 1;
        }
        // Continue with full block reads
        while cursor < read_size {
            let read_len = min(BLOCK_SIZE, read_size - cursor);
            match self.extent_query(&file, iblock) {
                Ok(fblock) => {
                    // normal
                    let block = self.read_block(fblock);
                    buf[cursor..cursor + read_len].copy_from_slice(block.read_offset(0, read_len));
                }
                Err(_) => {
                    // hole
                    buf[cursor..cursor + read_len].fill(0);
                }
            }
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
        let inode_ref = self.read_inode(inode_id);
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
            let fblock = self.extent_query(&inode_ref, start_iblock).unwrap();
            let block = self.read_block(fblock);
            buf[cursor..cursor + read_len].copy_from_slice(block.read_offset(misaligned, read_len));
            cursor += read_len;
            iblock += 1;
        }
        while cursor < read_size {
            let read_len = min(BLOCK_SIZE, read_size - cursor);
            match self.extent_query(&inode_ref, iblock) {
                Ok(fblock) => {
                    let block = self.read_block(fblock);
                    buf[cursor..cursor + read_len].copy_from_slice(block.read_offset(0, read_len));
                }
                Err(_) => {
                    // hole
                    buf[cursor..cursor + read_len].fill(0);
                }
            }
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
        // Get the inode of the file
        let mut file = self.read_inode(file);
        if !file.inode.is_file() {
            return_error!(ErrCode::EISDIR, "Inode {} is not a file", file.id);
        }

        let write_size = data.len();
        // Calc the start and end block of writing
        let start_iblock = (offset / BLOCK_SIZE) as LBlockId;
        let end_iblock = ((offset + write_size) / BLOCK_SIZE) as LBlockId;
        // Append enough block for writing
        let append_block_count = end_iblock as i64 + 1 - file.inode.fs_block_count() as i64;
        for _ in 0..append_block_count {
            self.inode_append_block(&mut file)?;
        }

        // Write data
        let mut cursor = 0;
        let mut iblock = start_iblock;
        while cursor < write_size {
            let block_offset = (offset + cursor) % BLOCK_SIZE;
            let write_len = min(BLOCK_SIZE - block_offset, write_size - cursor);
            let fblock = self.extent_query(&file, iblock)?;
            let mut block = self.read_block(fblock);
            block.write_offset(
                (offset + cursor) % BLOCK_SIZE,
                &data[cursor..cursor + write_len],
            );
            self.write_block(&block);
            cursor += write_len;
            iblock += 1;
        }
        if offset + cursor > file.inode.size() as usize {
            file.inode.set_size((offset + cursor) as u64);
        }
        self.write_inode_with_csum(&mut file);

        Ok(cursor)
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
        let mut parent = self.read_inode(parent);
        // Can only link to a directory
        if !parent.inode.is_dir() {
            return_error!(ErrCode::ENOTDIR, "Inode {} is not a directory", parent.id);
        }
        let mut child = self.read_inode(child);
        // Cannot link a directory
        if child.inode.is_dir() {
            return_error!(ErrCode::EISDIR, "Cannot link a directory");
        }
        self.link_inode(&mut parent, &mut child, name)?;
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
    pub fn unlink(&self, parent: InodeId, name: &str) -> Result<()> {
        let mut parent = self.read_inode(parent);
        // Can only unlink from a directory
        if !parent.inode.is_dir() {
            return_error!(ErrCode::ENOTDIR, "Inode {} is not a directory", parent.id);
        }
        // Cannot unlink directory
        let child_id = self.dir_find_entry(&parent, name)?;
        let mut child = self.read_inode(child_id);
        if child.inode.is_dir() {
            return_error!(ErrCode::EISDIR, "Cannot unlink a directory");
        }
        self.unlink_inode(&mut parent, &mut child, name, true)
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
        let parent_ref = self.read_inode(parent);
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
            let np = self.read_inode(new_parent);
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
            let cur_inode = self.read_inode(cur);
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
    ) -> Result<()> {
        // 1. 验证父目录
        let (mut parent_ref, mut new_parent_ref) = self.read_rename_dirs(parent, new_parent)?;

        // 2. 查找源 inode
        let child_id = self.dir_find_entry(&parent_ref, name)?;
        let mut child = self.read_inode(child_id);
        let child_is_dir = child.inode.is_dir();
        let child_file_type = child.inode.file_type();

        // 3. 循环检测：防止把目录移到自己的子目录下
        if child_is_dir && parent != new_parent {
            self.check_ancestor_cycle(child_id, new_parent)?;
        }

        // 4. 检查目标是否存在
        let target_dir_ref = new_parent_ref.as_ref().unwrap_or(&parent_ref);
        let existing = self.dir_find_entry(target_dir_ref, new_name).ok();

        match existing {
            Some(existing_id) if existing_id == child_id => {
                // 情况 A：源和目标是同一个 inode（硬链接或同名）
                // POSIX 语义：无操作，返回成功
                return Ok(());
            }
            Some(existing_id) => {
                // 情况 B：目标存在且是不同 inode → 原子替换
                let mut existing_inode = self.read_inode(existing_id);
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

                // 4b-2. 原子替换：原地修改目标目录项，指向源 inode
                // 这是原子操作的核心：目标目录项从未"消失"
                let target_dir = new_parent_ref.as_mut().unwrap_or(&mut parent_ref);
                self.dir_replace_entry(target_dir, new_name, child_id, child_file_type)?;

                // 4b-3. 处理被替换 inode 的 link count
                //
                // 目录的 link count 语义：
                // - 父目录的 link count 包含每个子目录 ".." 指向它的引用
                // - 当用 dir_a 替换 dir_b 时：
                //   * dir_b 的 ".." 被移除 → 父目录失去一个 ".." 引用 (-1)
                //   * dir_a 的 ".." 仍指向其原父目录（在 4b-5 处理）
                //
                // 同目录情况 (parent == new_parent)：
                //   - dir_a 的 ".." 原本就指向父目录，无需更改
                //   - 只需为 dir_b 的 ".." 移除递减：净 -1 ✓
                //
                // 跨目录情况 (parent != new_parent)：
                //   - dir_b 的 ".." 移除：目标父目录 -1（此处）
                //   - dir_a 的 ".." 更新：源父目录 -1，目标父目录 +1（在 4b-5）
                //   - 目标父目录净变化：-1 + 1 = 0 ✓
                if existing_is_dir {
                    self.dir_remove_entry(&existing_inode, "..")?;
                    target_dir
                        .inode
                        .set_link_count(target_dir.inode.link_count() - 1);
                    self.write_inode_with_csum(target_dir);
                }

                // 递减被替换 inode 的 link count（可能触发释放）
                // 目录最小 link count 为 2（父目录条目 + 自己的 "."），所以 <=2 表示无其他硬链接
                let existing_link_cnt = existing_inode.inode.link_count();
                if existing_link_cnt <= 1 || (existing_is_dir && existing_link_cnt <= 2) {
                    self.free_inode(&mut existing_inode)?;
                } else {
                    existing_inode.inode.set_link_count(existing_link_cnt - 1);
                    self.write_inode_with_csum(&mut existing_inode);
                }

                // 4b-4. 删除源目录项
                self.dir_remove_entry(&parent_ref, name)?;

                // 4b-5. 跨目录移动时，处理源目录的 ".." 指向
                if child_is_dir && parent != new_parent {
                    // 更新被移动目录的 ".." 指向新父目录
                    self.dir_replace_entry(&child, "..", new_parent, FileType::Directory)?;

                    // 源父目录失去 ".." 引用
                    parent_ref
                        .inode
                        .set_link_count(parent_ref.inode.link_count() - 1);
                    self.write_inode_with_csum(&mut parent_ref);

                    // 目标父目录获得 ".." 引用
                    // 注意：当 parent != new_parent 时，new_parent_ref 必定是 Some(...)
                    let new_parent_dir = new_parent_ref.as_mut().unwrap();
                    new_parent_dir
                        .inode
                        .set_link_count(new_parent_dir.inode.link_count() + 1);
                    self.write_inode_with_csum(new_parent_dir);
                }
                // 文件的 link count 不变（只是换了名字/位置）
            }
            None => {
                // 情况 C：目标不存在 → 简单重命名
                // 从源 unlink
                self.unlink_inode(&mut parent_ref, &mut child, name, false)?;
                // link 到目标
                match new_parent_ref.as_mut() {
                    Some(np) => self.link_inode(np, &mut child, new_name)?,
                    None => self.link_inode(&mut parent_ref, &mut child, new_name)?,
                }
            }
        }

        Ok(())
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
        // 1. 验证父目录
        let (mut parent_ref, mut new_parent_ref) = self.read_rename_dirs(parent, new_parent)?;

        // 2. 查找两个 inode
        let old_id = self.dir_find_entry(&parent_ref, name)?;
        let old_inode = self.read_inode(old_id);
        let old_is_dir = old_inode.inode.is_dir();
        let old_type = old_inode.inode.file_type();

        let target_dir_ref = new_parent_ref.as_ref().unwrap_or(&parent_ref);
        let new_id = self.dir_find_entry(target_dir_ref, new_name)?;
        let new_inode = self.read_inode(new_id);
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
            self.dir_replace_entry(&parent_ref, name, new_id, new_type)?;
            self.dir_replace_entry(&parent_ref, new_name, old_id, old_type)?;
        } else {
            self.dir_replace_entry(&parent_ref, name, new_id, new_type)?;
            let new_parent_dir = new_parent_ref.as_ref().unwrap();
            self.dir_replace_entry(new_parent_dir, new_name, old_id, old_type)?;
        }

        // 6. 跨目录时更新目录的 ".." 指向和父目录 link_count
        if parent != new_parent {
            if old_is_dir {
                self.dir_replace_entry(&old_inode, "..", new_parent, FileType::Directory)?;
                parent_ref
                    .inode
                    .set_link_count(parent_ref.inode.link_count() - 1);
                self.write_inode_with_csum(&mut parent_ref);
                let np = new_parent_ref.as_mut().unwrap();
                np.inode.set_link_count(np.inode.link_count() + 1);
                self.write_inode_with_csum(np);
            }
            if new_is_dir {
                self.dir_replace_entry(&new_inode, "..", parent, FileType::Directory)?;
                let np = new_parent_ref.as_mut().unwrap();
                np.inode.set_link_count(np.inode.link_count() - 1);
                self.write_inode_with_csum(np);
                parent_ref
                    .inode
                    .set_link_count(parent_ref.inode.link_count() + 1);
                self.write_inode_with_csum(&mut parent_ref);
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
        let mut parent = self.read_inode(parent);
        // Can only create a directory in a directory
        if !parent.inode.is_dir() {
            return_error!(ErrCode::ENOTDIR, "Inode {} is not a directory", parent.id);
        }
        // Create file/directory
        let mode = mode & InodeMode::PERM_MASK | InodeMode::DIRECTORY;
        let mut child = self.create_inode(mode)?;
        // Add "." entry
        let child_self = child.clone();
        self.dir_add_entry(&mut child, &child_self, ".")?;
        child.inode.set_link_count(1);
        // Link the new inode
        self.link_inode(&mut parent, &mut child, name)?;
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
        let parent = self.read_inode(parent);
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
        let inode_ref = self.read_inode(inode);
        // Can only list a directory
        if inode_ref.inode.file_type() != FileType::Directory {
            return_error!(ErrCode::ENOTDIR, "Inode {} is not a directory", inode);
        }
        Ok(self.dir_list_entries(&inode_ref))
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
    pub fn rmdir(&self, parent: InodeId, name: &str) -> Result<()> {
        let mut parent = self.read_inode(parent);
        // Can only remove a directory in a directory
        if !parent.inode.is_dir() {
            return_error!(ErrCode::ENOTDIR, "Inode {} is not a directory", parent.id);
        }
        let mut child = self.read_inode(self.dir_find_entry(&parent, name)?);
        // Child must be a directory
        if !child.inode.is_dir() {
            return_error!(ErrCode::ENOTDIR, "Inode {} is not a directory", child.id);
        }
        // Child must be empty
        if self.dir_list_entries(&child).len() > 2 {
            return_error!(ErrCode::ENOTEMPTY, "Directory {} is not empty", child.id);
        }
        // Remove directory entry
        self.unlink_inode(&mut parent, &mut child, name, true)
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
        let inode_ref = self.read_inode(inode);
        let xattr_block_id = inode_ref.inode.xattr_block();
        if xattr_block_id == 0 {
            return_error!(ErrCode::ENODATA, "Xattr {} does not exist", name);
        }
        let xattr_block = XattrBlock::new(self.read_block(xattr_block_id));
        match xattr_block.get(name) {
            Some(value) => Ok(value.to_owned()),
            None => Err(format_error!(
                ErrCode::ENODATA,
                "Xattr {} does not exist",
                name
            )),
        }
    }

    /// Set extended attribute of a file. This function will not check name conflict,
    /// call `getxattr` to check beforehand.
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
        let mut inode_ref = self.read_inode(inode);
        let xattr_block_id = inode_ref.inode.xattr_block();
        if xattr_block_id == 0 {
            // lazy allocate xattr block
            let pblock = self.alloc_block(&mut inode_ref)?;
            inode_ref.inode.set_xattr_block(pblock);
            self.write_inode_with_csum(&mut inode_ref);
        }
        let mut xattr_block = XattrBlock::new(self.read_block(inode_ref.inode.xattr_block()));
        if xattr_block_id == 0 {
            xattr_block.init();
        }
        if xattr_block.insert(name, value) {
            self.write_block(&xattr_block.block());
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
        let inode_ref = self.read_inode(inode);
        let xattr_block_id = inode_ref.inode.xattr_block();
        if xattr_block_id == 0 {
            return_error!(ErrCode::ENODATA, "Xattr {} does not exist", name);
        }
        let mut xattr_block = XattrBlock::new(self.read_block(xattr_block_id));
        if xattr_block.remove(name) {
            self.write_block(&xattr_block.block());
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
        let inode_ref = self.read_inode(inode);
        let xattr_block_id = inode_ref.inode.xattr_block();
        if xattr_block_id == 0 {
            return Ok(Vec::new());
        }
        let xattr_block = XattrBlock::new(self.read_block(xattr_block_id));
        Ok(xattr_block.list())
    }
}
