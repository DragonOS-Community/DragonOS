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
        // Hash all directory entries up to (but not including) this tail struct.
        // Linux: csum = ext4_chksum(sbi, i_csum_seed, dirent, (char*)tail - block_start)
        let tail_offset = BLOCK_SIZE - size_of::<DirEntryTail>();
        self.checksum = crc32(csum, &block.data[..tail_offset]);
    }
}

/// The block that stores an array of `DirEntry`.
pub struct DirBlock(Block);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirBlockLayout {
    Leaf,
    Htree,
}

impl DirBlock {
    /// Wrap a data block to a directory block.
    pub fn new(block: Block) -> Self {
        DirBlock(block)
    }

    /// Get the wrapped block.
    pub fn block(&self) -> &Block {
        &self.0
    }

    /// Validate the complete linear directory layout before any entry is
    /// interpreted.  The checksum tail is not a directory entry and every
    /// record must end at or before that tail with a bounded name payload.
    pub fn validate(
        &self,
        uuid: &[u8],
        ino: InodeId,
        ino_gen: u32,
        metadata_csum: bool,
        indexed: bool,
        htree_root: bool,
    ) -> Result<DirBlockLayout> {
        if indexed && htree_root {
            self.validate_htree(uuid, ino, ino_gen, metadata_csum)?;
            return Ok(DirBlockLayout::Htree);
        }
        if indexed
            && self
                .validate_htree(uuid, ino, ino_gen, metadata_csum)
                .is_ok()
        {
            return Ok(DirBlockLayout::Htree);
        }
        if let Ok(()) = self.validate_leaf(uuid, ino, ino_gen, metadata_csum) {
            return Ok(DirBlockLayout::Leaf);
        }
        Err(Ext4Error::new(ErrCode::EIO))
    }

    fn validate_leaf(
        &self,
        uuid: &[u8],
        ino: InodeId,
        ino_gen: u32,
        metadata_csum: bool,
    ) -> Result<()> {
        let data_end = if metadata_csum {
            BLOCK_SIZE - size_of::<DirEntryTail>()
        } else {
            BLOCK_SIZE
        };
        if metadata_csum {
            let tail: DirEntryTail = self.0.read_offset_as(data_end);
            if tail.reserved_zero1 != 0
                || tail.rec_len as usize != size_of::<DirEntryTail>()
                || tail.reserved_zero2 != 0
                || tail.reserved_ft != 0xDE
            {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            let mut expected = tail;
            expected.set_checksum(uuid, ino, ino_gen, &self.0);
            if expected.checksum != tail.checksum {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
        }

        let mut offset = 0usize;
        while offset < data_end {
            if data_end - offset < size_of::<FakeDirEntry>() {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            // Read primitive fields byte-wise.  Constructing FakeDirEntry
            // before checking file_type would create an invalid Rust enum for
            // malicious on-disk discriminants.
            let rec_len =
                u16::from_le_bytes([self.0.data[offset + 4], self.0.data[offset + 5]]) as usize;
            let name_len = self.0.data[offset + 6] as usize;
            let file_type = self.0.data[offset + 7];
            if rec_len < size_of::<FakeDirEntry>()
                || rec_len % 4 != 0
                || offset.checked_add(rec_len).is_none_or(|end| end > data_end)
                || name_len > rec_len - size_of::<FakeDirEntry>()
                || file_type > FileType::SymLink as u8
            {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            offset += rec_len;
        }
        if offset != data_end {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        Ok(())
    }

    /// Validate an ext4 htree root or internal node using the same count/limit
    /// layout and checksum coverage as Linux `ext4_dx_csum_verify()`.
    fn validate_htree(
        &self,
        uuid: &[u8],
        ino: InodeId,
        ino_gen: u32,
        metadata_csum: bool,
    ) -> Result<()> {
        let first_rec_len = u16::from_le_bytes([self.0.data[4], self.0.data[5]]) as usize;
        if self.0.data[7] > FileType::SymLink as u8 {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        let count_offset = if first_rec_len == BLOCK_SIZE {
            8usize
        } else if first_rec_len == 12 {
            let second_rec_len = u16::from_le_bytes([self.0.data[16], self.0.data[17]]) as usize;
            if second_rec_len != BLOCK_SIZE - 12
                || self.0.data[24..28] != [0; 4]
                || self.0.data[29] as usize != 8
                || self.0.data[19] > FileType::SymLink as u8
            {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            32usize
        } else {
            return Err(Ext4Error::new(ErrCode::EIO));
        };

        let limit =
            u16::from_le_bytes([self.0.data[count_offset], self.0.data[count_offset + 1]]) as usize;
        let count =
            u16::from_le_bytes([self.0.data[count_offset + 2], self.0.data[count_offset + 3]])
                as usize;
        if count == 0 || count > limit {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        let tail_offset = count_offset
            .checked_add(
                limit
                    .checked_mul(8)
                    .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?,
            )
            .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
        let max_tail_offset = if metadata_csum {
            BLOCK_SIZE - 8
        } else {
            BLOCK_SIZE
        };
        if tail_offset > max_tail_offset {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        if !metadata_csum {
            return Ok(());
        }
        let stored = u32::from_le_bytes(
            self.0.data[tail_offset + 4..tail_offset + 8]
                .try_into()
                .map_err(|_| Ext4Error::new(ErrCode::EIO))?,
        );
        let used_end = count_offset
            .checked_add(count * 8)
            .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
        let mut checksum = crc32(CRC32_INIT, uuid);
        checksum = crc32(checksum, &ino.to_le_bytes());
        checksum = crc32(checksum, &ino_gen.to_le_bytes());
        checksum = crc32(checksum, &self.0.data[..used_end]);
        checksum = crc32(checksum, &self.0.data[tail_offset..tail_offset + 4]);
        checksum = crc32(checksum, &[0; 4]);
        if checksum != stored {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        Ok(())
    }

    /// Initialize a directory block, create an unused entry
    /// and the dir entry tail.
    pub fn init(&mut self, metadata_csum: bool) {
        let data_end = if metadata_csum {
            BLOCK_SIZE - size_of::<DirEntryTail>()
        } else {
            BLOCK_SIZE
        };
        let entry = DirEntry::new(0, data_end as u16, "", FileType::Unknown);
        self.0.write_offset_as(0, &entry);
        if metadata_csum {
            let tail = DirEntryTail::new();
            self.0.write_offset_as(data_end, &tail);
        }
    }

    /// Get a directory entry by name, return the inode id of the entry.
    pub fn get(&self, name: &str, metadata_csum: bool) -> Option<InodeId> {
        let tail_offset = if metadata_csum {
            BLOCK_SIZE - size_of::<DirEntryTail>()
        } else {
            BLOCK_SIZE
        };
        let mut offset = 0;
        while offset < tail_offset {
            let de: DirEntry = self.0.read_offset_as(offset);
            if !de.unused() && de.compare_name(name) {
                return Some(de.inode);
            }
            offset += de.rec_len as usize;
        }
        None
    }

    /// Get all directory entries in the block.
    pub fn list(&self, entries: &mut Vec<DirEntry>, metadata_csum: bool) {
        let tail_offset = if metadata_csum {
            BLOCK_SIZE - size_of::<DirEntryTail>()
        } else {
            BLOCK_SIZE
        };
        let mut offset = 0;
        while offset < tail_offset {
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
    pub fn insert(
        &mut self,
        name: &str,
        inode: InodeId,
        file_type: FileType,
        metadata_csum: bool,
    ) -> bool {
        let required_size = DirEntry::required_size(name.len());
        let tail_offset = if metadata_csum {
            BLOCK_SIZE - size_of::<DirEntryTail>()
        } else {
            BLOCK_SIZE
        };
        let mut offset = 0;
        while offset < tail_offset {
            // Read a dir entry
            let mut de: DirEntry = self.0.read_offset_as(offset);
            let rec_len = de.rec_len as usize;
            // An unused record is itself free space.  Reuse it in place
            // instead of preserving an artificial 8-byte inode-zero prefix;
            // ext4 directories must start with their first real entry, and
            // e2fsck rejects the prefix once deletion makes a directory empty.
            if de.unused() && rec_len >= required_size {
                let new_entry = DirEntry::new(inode, rec_len as u16, name, file_type);
                self.0.write_offset_as(offset, &new_entry);
                return true;
            }
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
    pub fn remove(&mut self, name: &str, metadata_csum: bool) -> bool {
        let tail_offset = if metadata_csum {
            BLOCK_SIZE - size_of::<DirEntryTail>()
        } else {
            BLOCK_SIZE
        };
        let mut offset = 0;
        let mut previous_offset = None;
        while offset < tail_offset {
            let mut de: DirEntry = self.0.read_offset_as(offset);
            if !de.unused() && de.compare_name(name) {
                // Match ext4_generic_delete_entry(): coalesce the deleted
                // record into its predecessor when one exists.  Merely
                // clearing i_ino leaves a typed, named hole which e2fsck
                // rejects for directory entries on metadata_csum filesystems.
                if let Some(previous_offset) = previous_offset {
                    let mut previous: DirEntry = self.0.read_offset_as(previous_offset);
                    previous.rec_len += de.rec_len;
                    self.0.write_offset_as(previous_offset, &previous);
                } else {
                    de.set_unused();
                    de.set_type(FileType::Unknown);
                    self.0.write_offset_as(offset, &de);
                }
                return true;
            }
            previous_offset = Some(offset);
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
    pub fn replace(
        &mut self,
        name: &str,
        new_inode: InodeId,
        new_type: FileType,
        metadata_csum: bool,
    ) -> bool {
        let tail_offset = if metadata_csum {
            BLOCK_SIZE - size_of::<DirEntryTail>()
        } else {
            BLOCK_SIZE
        };
        let mut offset = 0;
        while offset < tail_offset {
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

    pub fn set_htree_checksum(&mut self, uuid: &[u8], ino: InodeId, ino_gen: u32) -> Result<()> {
        let first_rec_len = u16::from_le_bytes([self.0.data[4], self.0.data[5]]) as usize;
        let count_offset = if first_rec_len == BLOCK_SIZE {
            8usize
        } else if first_rec_len == 12 {
            32usize
        } else {
            return Err(Ext4Error::new(ErrCode::EIO));
        };
        let limit =
            u16::from_le_bytes([self.0.data[count_offset], self.0.data[count_offset + 1]]) as usize;
        let count =
            u16::from_le_bytes([self.0.data[count_offset + 2], self.0.data[count_offset + 3]])
                as usize;
        if count == 0 || count > limit {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        let tail_offset = count_offset
            .checked_add(
                limit
                    .checked_mul(8)
                    .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?,
            )
            .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
        if tail_offset > BLOCK_SIZE - 8 {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        let used_end = count_offset
            .checked_add(count * 8)
            .ok_or_else(|| Ext4Error::new(ErrCode::EIO))?;
        let mut checksum = crc32(CRC32_INIT, uuid);
        checksum = crc32(checksum, &ino.to_le_bytes());
        checksum = crc32(checksum, &ino_gen.to_le_bytes());
        checksum = crc32(checksum, &self.0.data[..used_end]);
        checksum = crc32(checksum, &self.0.data[tail_offset..tail_offset + 4]);
        checksum = crc32(checksum, &[0; 4]);
        self.0.data[tail_offset + 4..tail_offset + 8].copy_from_slice(&checksum.to_le_bytes());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_block() -> DirBlock {
        let mut block = DirBlock::new(Block::new(7, Box::new([0; BLOCK_SIZE])));
        block.init(true);
        assert!(block.insert("entry", 12, FileType::RegularFile, true));
        block.set_checksum(&[0x41; 16], 2, 9);
        block
    }

    fn valid_htree_root() -> DirBlock {
        let uuid = [0x41; 16];
        let ino = 2u32;
        let generation = 9u32;
        let mut raw = Block::new(8, Box::new([0; BLOCK_SIZE]));
        raw.write_offset_as(0, &DirEntry::new(ino, 12, ".", FileType::Directory));
        raw.write_offset_as(
            12,
            &DirEntry::new(ino, (BLOCK_SIZE - 12) as u16, "..", FileType::Directory),
        );
        raw.data[28] = 1; // hash version
        raw.data[29] = 8; // dx_root_info length
        let count_offset = 32usize;
        let limit = ((BLOCK_SIZE - count_offset - 8) / 8) as u16;
        raw.data[count_offset..count_offset + 2].copy_from_slice(&limit.to_le_bytes());
        raw.data[count_offset + 2..count_offset + 4].copy_from_slice(&2u16.to_le_bytes());
        raw.data[count_offset + 4..count_offset + 8].copy_from_slice(&1u32.to_le_bytes());
        raw.data[count_offset + 8..count_offset + 12]
            .copy_from_slice(&0x8765_4321u32.to_le_bytes());
        raw.data[count_offset + 12..count_offset + 16].copy_from_slice(&2u32.to_le_bytes());
        let tail_offset = count_offset + limit as usize * 8;
        raw.data[tail_offset..tail_offset + 4].copy_from_slice(&[12, 0, 0, 0xde]);
        let used_end = count_offset + 2 * 8;
        let mut checksum = crc32(CRC32_INIT, &uuid);
        checksum = crc32(checksum, &ino.to_le_bytes());
        checksum = crc32(checksum, &generation.to_le_bytes());
        checksum = crc32(checksum, &raw.data[..used_end]);
        checksum = crc32(checksum, &raw.data[tail_offset..tail_offset + 4]);
        checksum = crc32(checksum, &[0; 4]);
        raw.data[tail_offset + 4..tail_offset + 8].copy_from_slice(&checksum.to_le_bytes());
        DirBlock::new(raw)
    }

    #[test]
    fn validated_directory_rejects_zero_and_out_of_bounds_record_lengths() {
        let block = valid_block();
        block
            .validate(&[0x41; 16], 2, 9, true, false, false)
            .unwrap();

        let mut zero = block.0.clone();
        zero.data[4..6].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(
            DirBlock::new(zero)
                .validate(&[0x41; 16], 2, 9, false, false, false)
                .unwrap_err()
                .code(),
            ErrCode::EIO
        );

        let mut oversized = block.0.clone();
        oversized.data[4..6].copy_from_slice(&((BLOCK_SIZE - 4) as u16).to_le_bytes());
        assert_eq!(
            DirBlock::new(oversized)
                .validate(&[0x41; 16], 2, 9, false, false, false)
                .unwrap_err()
                .code(),
            ErrCode::EIO
        );
    }

    #[test]
    fn validated_directory_rejects_name_overrun_and_bad_tail_checksum() {
        let block = valid_block();
        let mut bad_name = block.0.clone();
        bad_name.data[4..6].copy_from_slice(&8u16.to_le_bytes());
        bad_name.data[6] = 255;
        assert_eq!(
            DirBlock::new(bad_name)
                .validate(&[0x41; 16], 2, 9, false, false, false)
                .unwrap_err()
                .code(),
            ErrCode::EIO
        );

        let mut bad_type = block.0.clone();
        bad_type.data[7] = 0xff;
        assert_eq!(
            DirBlock::new(bad_type)
                .validate(&[0x41; 16], 2, 9, false, false, false)
                .unwrap_err()
                .code(),
            ErrCode::EIO
        );

        let mut bad_checksum = block.0.clone();
        bad_checksum.data[BLOCK_SIZE - 1] ^= 0x80;
        assert_eq!(
            DirBlock::new(bad_checksum)
                .validate(&[0x41; 16], 2, 9, true, false, false)
                .unwrap_err()
                .code(),
            ErrCode::EIO
        );
    }

    #[test]
    fn missing_entry_scan_stops_before_checksum_tail() {
        let mut block = valid_block();
        block
            .validate(&[0x41; 16], 2, 9, true, false, false)
            .unwrap();

        assert_eq!(block.get("missing", true), None);
        let mut entries = Vec::new();
        block.list(&mut entries, true);
        assert_eq!(entries.len(), 1);
        assert!(!block.remove("missing", true));
        assert!(!block.replace("missing", 99, FileType::RegularFile, true));
        let long = "x".repeat(255);
        while block.insert(&long, 99, FileType::RegularFile, true) {}
        while block.insert("a", 99, FileType::RegularFile, true) {}
        assert!(!block.insert("another", 99, FileType::RegularFile, true));
    }

    #[test]
    fn indexed_directory_root_uses_dx_checksum_layout() {
        let block = valid_htree_root();
        assert_eq!(
            block.validate(&[0x41; 16], 2, 9, true, true, true).unwrap(),
            DirBlockLayout::Htree
        );
        assert_eq!(block.get(".", true), Some(2));

        let mut no_csum = block.0.clone();
        no_csum.data[32..34].copy_from_slice(&508u16.to_le_bytes());
        assert_eq!(
            DirBlock::new(no_csum)
                .validate(&[0x41; 16], 2, 9, false, true, true)
                .unwrap(),
            DirBlockLayout::Htree
        );

        let mut corrupted = block.0.clone();
        corrupted.data[BLOCK_SIZE - 1] ^= 0x80;
        assert_eq!(
            DirBlock::new(corrupted)
                .validate(&[0x41; 16], 2, 9, true, true, true)
                .unwrap_err()
                .code(),
            ErrCode::EIO
        );

        let mut invalid_type = block.0.clone();
        invalid_type.data[7] = 0xff;
        assert_eq!(
            DirBlock::new(invalid_type)
                .validate(&[0x41; 16], 2, 9, false, true, true)
                .unwrap_err()
                .code(),
            ErrCode::EIO
        );
    }

    #[test]
    fn directory_without_metadata_checksum_uses_the_full_block() {
        let mut block = DirBlock::new(Block::new(9, Box::new([0; BLOCK_SIZE])));
        block.init(false);
        assert!(block.insert("entry", 12, FileType::RegularFile, false));
        assert_eq!(
            block
                .validate(&[0x41; 16], 2, 9, false, false, false)
                .unwrap(),
            DirBlockLayout::Leaf
        );
    }
}
