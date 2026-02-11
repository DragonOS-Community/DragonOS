//! A directory is a series of data blocks and that each block contains a
//! linear array of directory entries.

use super::crc::*;
use super::AsBytes;
use super::FileType;
use crate::constants::*;
use crate::prelude::*;
use crate::Block;

/// Directory entry.
#[repr(C)]
#[derive(Debug, Clone)]
pub struct DirEntry {
    /// Number of the inode that this directory entry points to.
    inode: InodeId,
    /// Distance to the next directory entry.
    rec_len: u16,
    /// Length of the file name.
    name_len: u8,
    /// File type code.
    file_type: FileType,
    /// File name.
    name: [u8; 255],
}

/// Fake dir entry. A normal entry without `name` field
#[repr(C)]
pub struct FakeDirEntry {
    inode: u32,
    rec_len: u16,
    name_len: u8,
    file_type: FileType,
}
unsafe impl AsBytes for FakeDirEntry {}

/// The actual size of the directory entry is determined by `name_len`.
/// So we need to implement `AsBytes` methods specifically for `DirEntry`.
unsafe impl AsBytes for DirEntry {
    fn from_bytes(bytes: &[u8]) -> Self {
        let fake_entry = FakeDirEntry::from_bytes(bytes);
        let mut entry = DirEntry {
            inode: fake_entry.inode,
            rec_len: fake_entry.rec_len,
            name_len: fake_entry.name_len,
            file_type: fake_entry.file_type,
            name: [0; 255],
        };
        let name_len = entry.name_len as usize;
        let name_offset = size_of::<FakeDirEntry>();
        entry.name[..name_len].copy_from_slice(&bytes[name_offset..name_offset + name_len]);
        entry
    }
    fn to_bytes(&self) -> &[u8] {
        let name_len = self.name_len as usize;
        unsafe {
            core::slice::from_raw_parts(
                self as *const Self as *const u8,
                size_of::<FakeDirEntry>() + name_len,
            )
        }
    }
}

impl DirEntry {
    /// Create a new directory entry
    pub fn new(inode: InodeId, rec_len: u16, name: &str, file_type: FileType) -> Self {
        let mut name_bytes = [0u8; 255];
        let name_len = name.len();
        name_bytes[..name_len].copy_from_slice(name.as_bytes());
        Self {
            inode,
            rec_len,
            name_len: name_len as u8,
            file_type,
            name: name_bytes,
        }
    }

    /// Get the inode number the directory entry points to
    pub fn inode(&self) -> InodeId {
        self.inode
    }

    /// Get the name of the directory entry
    pub fn name(&self) -> String {
        let name = &self.name[..self.name_len as usize];
        unsafe { String::from_utf8_unchecked(name.to_vec()) }
    }

    /// Compare the name of the directory entry with a given name
    pub fn compare_name(&self, name: &str) -> bool {
        if self.name_len as usize == name.len() {
            return &self.name[..name.len()] == name.as_bytes();
        }
        false
    }

    /// Check if the directory entry is unused (inode = 0)
    pub fn unused(&self) -> bool {
        self.inode == 0
    }

    /// Set a directory entry as unused
    pub fn set_unused(&mut self) {
        self.inode = 0
    }

    /// Get the dir entry's file type
    pub fn file_type(&self) -> FileType {
        self.file_type
    }

    /// Set the dir entry's file type
    pub fn set_type(&mut self, file_type: FileType) {
        self.file_type = file_type;
    }

    /// Set the inode number (for atomic rename)
    pub fn set_inode(&mut self, inode: InodeId) {
        self.inode = inode;
    }

    /// Get the required size to save a directory entry, 4-byte aligned
    pub fn required_size(name_len: usize) -> usize {
        // u32 + u16 + u8 + Ext4DirEnInner + name -> align to 4
        (core::mem::size_of::<FakeDirEntry>() + name_len).div_ceil(4) * 4
    }

    /// Get the used size of this directory entry, 4-bytes alighed
    pub fn used_size(&self) -> usize {
        Self::required_size(self.name_len as usize)
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct DirEntryTail {
    reserved_zero1: u32,
    rec_len: u16,
    reserved_zero2: u8,
    reserved_ft: u8,
    checksum: u32, // crc32c(uuid+inum+dirblock)
}
unsafe impl AsBytes for DirEntryTail {}

impl DirEntryTail {
    pub fn new() -> Self {
        Self {
            reserved_zero1: 0,
            rec_len: 12,
            reserved_zero2: 0,
            reserved_ft: 0xDE,
            checksum: 0,
        }
    }

    pub fn set_checksum(&mut self, uuid: &[u8], ino: InodeId, ino_gen: u32, block: &Block) {
        let mut csum = crc32(CRC32_INIT, uuid);
        csum = crc32(csum, &ino.to_le_bytes());
        csum = crc32(csum, &ino_gen.to_le_bytes());
        self.checksum = crc32(csum, &block.data[..size_of::<DirEntryTail>()]);
    }
}

/// The block that stores an array of `DirEntry`.
pub struct DirBlock(Block);

impl DirBlock {
    /// Wrap a data block to a directory block.
    pub fn new(block: Block) -> Self {
        DirBlock(block)
    }

    /// Get the wrapped block.
    pub fn block(&self) -> &Block {
        &self.0
    }

    /// Initialize a directory block, create an unused entry
    /// and the dir entry tail.
    pub fn init(&mut self) {
        let tail_offset = BLOCK_SIZE - size_of::<DirEntryTail>();
        let entry = DirEntry::new(0, tail_offset as u16, "", FileType::Unknown);
        self.0.write_offset_as(0, &entry);
        let tail = DirEntryTail::new();
        self.0.write_offset_as(tail_offset, &tail);
    }

    /// Get a directory entry by name, return the inode id of the entry.
    pub fn get(&self, name: &str) -> Option<InodeId> {
        let mut offset = 0;
        while offset < BLOCK_SIZE {
            let de: DirEntry = self.0.read_offset_as(offset);
            if !de.unused() && de.compare_name(name) {
                return Some(de.inode);
            }
            offset += de.rec_len as usize;
        }
        None
    }

    /// Get all directory entries in the block.
    pub fn list(&self, entries: &mut Vec<DirEntry>) {
        let mut offset = 0;
        while offset < BLOCK_SIZE {
            let de: DirEntry = self.0.read_offset_as(offset);
            offset += de.rec_len as usize;
            if !de.unused() {
                trace!("Dir entry: {:?} {}", de.name(), de.inode);
                entries.push(de);
            }
        }
    }

    /// Insert a directory entry to the block. Return true if success or false
    /// if the block doesn't have enough space.
    pub fn insert(&mut self, name: &str, inode: InodeId, file_type: FileType) -> bool {
        let required_size = DirEntry::required_size(name.len());
        let mut offset = 0;
        while offset < BLOCK_SIZE {
            // Read a dir entry
            let mut de: DirEntry = self.0.read_offset_as(offset);
            let rec_len = de.rec_len as usize;
            // The size that `de` actually uses
            let used_size = de.used_size();
            // The rest size
            let free_size = rec_len - used_size;
            // Try splitting dir entry
            // Compare size
            if free_size < required_size {
                // No enough space, try next dir ent
                offset += rec_len;
                continue;
            }
            // Has enough space
            // Update the old entry
            de.rec_len = used_size as u16;
            self.0.write_offset_as(offset, &de);
            // Insert the new entry
            let new_entry = DirEntry::new(inode, free_size as u16, name, file_type);
            self.0.write_offset_as(offset + used_size, &new_entry);
            return true;
        }
        false
    }

    /// Remove a directory entry from the block. Return true if success or false
    /// if the entry doesn't exist.
    pub fn remove(&mut self, name: &str) -> bool {
        let mut offset = 0;
        while offset < BLOCK_SIZE {
            let mut de: DirEntry = self.0.read_offset_as(offset);
            if !de.unused() && de.compare_name(name) {
                // Mark the target entry as unused
                de.set_unused();
                self.0.write_offset_as(offset, &de);
                return true;
            }
            offset += de.rec_len as usize;
        }
        false
    }

    /// Replace the inode of an existing directory entry in place.
    /// This is the key operation for atomic rename when target exists.
    ///
    /// # Arguments
    /// * `name` - the name of the entry to replace
    /// * `new_inode` - the new inode number to point to
    /// * `new_type` - the new file type
    ///
    /// # Returns
    /// * `true` if entry found and replaced
    /// * `false` if entry not found
    pub fn replace(&mut self, name: &str, new_inode: InodeId, new_type: FileType) -> bool {
        let mut offset = 0;
        while offset < BLOCK_SIZE {
            let mut de: DirEntry = self.0.read_offset_as(offset);
            if !de.unused() && de.compare_name(name) {
                // In-place modification: only change inode and file_type
                // rec_len and name remain unchanged
                de.set_inode(new_inode);
                de.set_type(new_type);
                self.0.write_offset_as(offset, &de);
                return true;
            }
            offset += de.rec_len as usize;
        }
        false
    }

    /// Calc and set block checksum
    pub fn set_checksum(&mut self, uuid: &[u8], ino: InodeId, ino_gen: u32) {
        let tail_offset = BLOCK_SIZE - size_of::<DirEntryTail>();
        let mut tail: DirEntryTail = self.0.read_offset_as(tail_offset);
        tail.set_checksum(uuid, ino, ino_gen, &self.0);
        self.0.write_offset_as(tail_offset, &tail);
    }
}
