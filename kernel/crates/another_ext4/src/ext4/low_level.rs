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

    /// Link a newly created inode into `parent`.
    ///
    /// If linking fails, this function frees the newly allocated inode to avoid leaks.
    fn link_new_inode_or_free(
        &self,
        parent: &mut InodeRef,
        child: &mut InodeRef,
        name: &str,
    ) -> Result<()> {
        if let Err(link_err) = self.link_inode(parent, child, name) {
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
        let mut parent = self.read_inode(parent);
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
        let mut parent_ref = self.read_inode(parent);

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

    /// Move a file.
    ///
    /// # Params
    ///
    /// * `parent` - the inode of the directory to move from
    /// * `name` - the name of the file to move
    /// * `new_parent` - the inode of the directory to move to
    /// * `new_name` - the new name of the file
    ///
    /// # Error
    ///
    /// * `ENOTDIR` - `parent` or `new_parent` is not a directory
    /// * `ENOENT` - `name` does not exist in `parent`
    /// * `EEXIST` - `new_parent/new_name` already exists
    /// * `ENOSPC` - no space left on device
    pub fn rename(
        &self,
        parent: InodeId,
        name: &str,
        new_parent: InodeId,
        new_name: &str,
    ) -> Result<()> {
        // Check parent
        let mut parent = self.read_inode(parent);
        if !parent.inode.is_dir() {
            return_error!(ErrCode::ENOTDIR, "Inode {} is not a directory", parent.id);
        }
        // Check new parent
        let mut new_parent = self.read_inode(new_parent);
        if !new_parent.inode.is_dir() {
            return_error!(
                ErrCode::ENOTDIR,
                "Inode {} is not a directory",
                new_parent.id
            );
        }
        // Check child existence
        let child_id = self.dir_find_entry(&parent, name)?;
        let mut child = self.read_inode(child_id);
        // Check name conflict
        if self.dir_find_entry(&new_parent, new_name).is_ok() {
            return_error!(ErrCode::EEXIST, "Dest name {} already exists", new_name);
        }
        // Move
        self.unlink_inode(&mut parent, &mut child, name, false)?;
        self.link_inode(&mut new_parent, &mut child, new_name)
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
