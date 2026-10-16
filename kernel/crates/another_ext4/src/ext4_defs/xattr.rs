//! Extended attributes (xattrs) are typically stored in a separate data block
//! on the disk and referenced from inodes via `inode.file_acl*`.
//!
//! There are two places where extended attributes can be found. The first place
//! is between the end of each inode entry and the beginning of the next inode
//! entry. The second place where extended attributes can be found is in the block
//! pointed to by `inode.file_acl`.
//!
//! We only implement the seperate data block storage of extended attributes.

use super::{crc::crc32, AsBytes, Block, MetadataChecksumSeed};
use crate::constants::*;
use crate::prelude::*;
use core::cmp::Ordering;

/// The beginning of an extended attribute block.
#[repr(C)]
#[derive(Debug)]
pub struct XattrHeader {
    /// Magic number for identification, 0xEA020000.
    magic: u32,
    /// Reference count.
    refcount: u32,
    /// Number of disk blocks used.
    blocks: u32,
    /// Hash value of all attributes. (UNUSED by now)
    hash: u32,
    /// Checksum of the extended attribute block.
    checksum: u32,
    /// Reserved for future use.
    reserved: [u32; 3],
}

unsafe impl AsBytes for XattrHeader {}

impl XattrHeader {
    pub const XATTR_MAGIC: u32 = 0xEA020000;

    pub fn new() -> Self {
        XattrHeader {
            magic: Self::XATTR_MAGIC,
            refcount: 1,
            blocks: 1,
            hash: 0,
            checksum: 0,
            reserved: [0; 3],
        }
    }

    pub fn magic(&self) -> u32 {
        self.magic
    }

    pub fn refcount(&self) -> u32 {
        self.refcount
    }

    pub fn set_refcount(&mut self, refcount: u32) {
        self.refcount = refcount;
    }

    pub fn blocks(&self) -> u32 {
        self.blocks
    }

    pub fn checksum(&self) -> u32 {
        self.checksum
    }

    pub fn set_checksum(&mut self, checksum: u32) {
        self.checksum = checksum;
    }
}

/// Compute an ext4 external-xattr block checksum using the Linux 6.6 layout.
/// `seed` is the filesystem metadata checksum seed (normally CRC32C(uuid)).
pub fn xattr_block_checksum(
    seed: MetadataChecksumSeed,
    block_id: PBlockId,
    block: &[u8],
) -> Option<u32> {
    if block.len() != BLOCK_SIZE {
        return None;
    }
    let checksum_offset = core::mem::offset_of!(XattrHeader, checksum);
    let mut checksum = seed.crc32c(&block_id.to_le_bytes());
    checksum = crc32(checksum, &block[..checksum_offset]);
    checksum = crc32(checksum, &[0; core::mem::size_of::<u32>()]);
    Some(crc32(
        checksum,
        &block[checksum_offset + core::mem::size_of::<u32>()..],
    ))
}

#[inline]
fn xattr_value_storage_size(size: usize) -> Option<usize> {
    size.checked_add(3).map(|size| size & !3)
}

/// Validate the fixed header and entry table needed before releasing a block.
/// Returns whether any value is stored in an EA inode, which this crate cannot
/// reclaim yet.
pub fn validate_xattr_block_for_release(block: &[u8]) -> Option<(XattrHeader, bool)> {
    if block.len() != BLOCK_SIZE || block.len() < size_of::<XattrHeader>() {
        return None;
    }
    let header = XattrHeader::from_bytes(&block[..size_of::<XattrHeader>()]);
    if header.magic() != XattrHeader::XATTR_MAGIC || header.refcount() == 0 || header.blocks() != 1
    {
        return None;
    }

    let mut entry_start = size_of::<XattrHeader>();
    let mut has_ea_inode = false;
    let mut inline_values = Vec::new();
    loop {
        let terminator_end = entry_start.checked_add(size_of::<u32>())?;
        if terminator_end > BLOCK_SIZE {
            return None;
        }
        if block[entry_start..terminator_end] == [0; 4] {
            break;
        }
        let fixed_end = entry_start.checked_add(size_of::<FakeXattrEntry>())?;
        if fixed_end > BLOCK_SIZE {
            return None;
        }
        let entry = FakeXattrEntry::from_bytes(
            &block[entry_start..entry_start + size_of::<FakeXattrEntry>()],
        );
        let name_end = fixed_end.checked_add(entry.name_len as usize)?;
        if name_end > BLOCK_SIZE || block[fixed_end..name_end].contains(&0) {
            return None;
        }
        let entry_size = size_of::<FakeXattrEntry>()
            .checked_add(entry.name_len as usize)?
            .checked_add(3)?
            & !3;
        let next_entry = entry_start.checked_add(entry_size)?;
        // `next_entry == BLOCK_SIZE` cannot leave the mandatory four-byte
        // terminator. This mirrors Linux's `EXT4_XATTR_NEXT(e) >= end` check.
        if next_entry
            .checked_add(size_of::<u32>())
            .is_none_or(|end| end > BLOCK_SIZE)
        {
            return None;
        }
        has_ea_inode |= entry.value_inum != 0;
        if entry.value_inum != 0 {
            if entry.value_size == 0 {
                return None;
            }
        } else if entry.value_size != 0 {
            let value_start = entry.value_offset as usize;
            let value_size = entry.value_size as usize;
            let padded_size = value_size.checked_add(3)? & !3;
            let value_end = value_start.checked_add(padded_size)?;
            if value_start & 3 != 0 || value_end > BLOCK_SIZE {
                return None;
            }
            inline_values.push((value_start, value_end));
        }
        entry_start = next_entry;
    }

    let names_end = entry_start.checked_add(size_of::<u32>())?;
    for &(start, end) in &inline_values {
        if start < names_end || end > BLOCK_SIZE {
            return None;
        }
    }
    inline_values.sort_unstable_by_key(|range| range.0);
    if inline_values
        .windows(2)
        .any(|ranges| ranges[1].0 < ranges[0].1)
    {
        return None;
    }
    Some((header, has_ea_inode))
}

/// Following the struct `XattrHeader` is an array of `XattrEntry`.
#[repr(C)]
#[derive(Debug)]
pub struct XattrEntry {
    /// Length of name.
    name_len: u8,
    /// Attribute name index.
    ///
    /// To reduce the amount of on-disk space that the keys consume, the
    /// beginningof the key string is matched against the attribute name
    /// index. If a match is found, the attribute name index field is set,
    /// and matching string is removed from the key name.
    name_index: u8,
    /// Location of this attribute's value on the disk block where
    /// it is stored. For a block this value is relative to the start
    /// of the block (i.e. the header).
    /// value = `block[value_offset..value_offset + value_size]`
    value_offset: u16,
    /// The inode where the value is stored. Zero indicates the value
    /// is in the same block as this entry (FIXED 0 by now)
    value_inum: u32,
    /// Length of attribute value.
    value_size: u32,
    /// Linux hash of the stored name suffix and padded inline value
    hash: u32,
    /// Attribute name, max 255 bytes.
    name: [u8; 255],
}

/// Fake xattr entry. A normal entry without `name` field.
#[repr(C)]
pub struct FakeXattrEntry {
    name_len: u8,
    name_index: u8,
    value_offset: u16,
    value_inum: u32,
    value_size: u32,
    hash: u32,
}
unsafe impl AsBytes for FakeXattrEntry {}

/// The actual size of the extended attribute entry is determined by `name_len`.
/// So we need to implement `AsBytes` methods specifically for `XattrEntry`.
unsafe impl AsBytes for XattrEntry {
    fn from_bytes(bytes: &[u8]) -> Self {
        let fake_entry = FakeXattrEntry::from_bytes(bytes);
        let mut entry = XattrEntry {
            name_len: fake_entry.name_len,
            name_index: fake_entry.name_index,
            value_offset: fake_entry.value_offset,
            value_inum: fake_entry.value_inum,
            value_size: fake_entry.value_size,
            hash: fake_entry.hash,
            name: [0; 255],
        };
        let name_len = entry.name_len as usize;
        let name_offset = size_of::<FakeXattrEntry>();
        entry.name[..name_len].copy_from_slice(&bytes[name_offset..name_offset + name_len]);
        entry
    }
    fn to_bytes(&self) -> &[u8] {
        let name_len = self.name_len as usize;
        unsafe {
            core::slice::from_raw_parts(
                self as *const Self as *const u8,
                size_of::<FakeXattrEntry>() + name_len,
            )
        }
    }
}

impl XattrEntry {
    /// Create a new xattr entry.
    pub fn new(name: &[u8], value_size: usize, value_offset: usize) -> Self {
        let mut name_bytes = [0u8; 255];
        let (name_index, name) = Self::match_name(name);
        let name_len = name.len();
        name_bytes[..name_len].copy_from_slice(name);
        Self {
            name_len: name.len() as u8,
            name_index,
            value_offset: value_offset as u16,
            value_inum: 0,
            value_size: value_size as u32,
            hash: 0,
            name: name_bytes,
        }
    }

    /// Namespace prefixes are bytes: on-disk suffixes need not be UTF-8.
    fn prefix(&self) -> Option<&'static [u8]> {
        match self.name_index {
            1 => Some(b"user."),
            2 if self.name_len == 0 => Some(b"system.posix_acl_access"),
            3 if self.name_len == 0 => Some(b"system.posix_acl_default"),
            4 => Some(b"trusted."),
            6 => Some(b"security."),
            7 => Some(b"gnu."),
            _ => None,
        }
    }

    /// Get the required size to save a xattr entry, 4-byte aligned
    pub fn required_size(name: &[u8]) -> usize {
        let (_, name) = Self::match_name(name);
        let name_len = name.len();
        // FakeXattrEntry + name -> align to 4
        (core::mem::size_of::<FakeXattrEntry>() + name_len).div_ceil(4) * 4
    }

    /// Get the used size of this xattr entry, 4-bytes alighed
    pub fn used_size(&self) -> usize {
        (core::mem::size_of::<FakeXattrEntry>() + self.name_len as usize).div_ceil(4) * 4
    }

    /// Compare the name of the xattr entry with a given name
    pub fn compare_name(&self, name: &[u8]) -> Ordering {
        let (name_index, name) = Self::match_name(name);
        match self.name_index.cmp(&name_index) {
            Ordering::Equal => {}
            ordering => return ordering,
        };
        match self.name_len.cmp(&(name.len() as u8)) {
            Ordering::Equal => {}
            ordering => return ordering,
        }
        self.name[..self.name_len as usize].cmp(name)
    }

    /// Match the attribute name prefix to get name index. If one is found,
    /// return the name index and the string with the prefix removed.
    fn match_name(name: &[u8]) -> (u8, &[u8]) {
        if name == b"system.posix_acl_access" {
            return (2, b"");
        }
        if name == b"system.posix_acl_default" {
            return (3, b"");
        }
        let prefixes: &[(&[u8], u8)] = &[
            (b"user.", 1),
            (b"trusted.", 4),
            (b"security.", 6),
            (b"gnu.", 7),
        ];
        for &(prefix, index) in prefixes {
            if let Some(suffix) = name.strip_prefix(prefix) {
                return (index, suffix);
            }
        }
        (0, name)
    }

    /// Full syscall names are limited to 255 bytes, including the namespace.
    pub(crate) fn validate_name(name: &[u8]) -> Result<()> {
        if name.is_empty() || name.len() > 255 || name.contains(&0) {
            return Err(Ext4Error::new(ErrCode::ERANGE));
        }
        let (index, suffix) = Self::match_name(name);
        if index == 0 {
            return Err(Ext4Error::new(ErrCode::ENOTSUP));
        }
        if suffix.is_empty() && index != 2 && index != 3 {
            return Err(Ext4Error::new(ErrCode::EINVAL));
        }
        Ok(())
    }
}

/// The block that stores extended attributes for an inode. The block is
/// pointed to `by inode.file_acl`.
///
/// `XattrHeader` is the beginning of an extended attribute block. Following
/// the struct `XattrHeader` is an array of `XattrEntry`. Attribute values
/// follow the end of the entry table. The values are stored starting at the
/// end of the block and grow towards the xattr_header/xattr_entry table. When
/// the two collide, the disk block fills up, and the filesystem returns `ENOSPC`.
pub struct XattrBlock(Block);

impl XattrBlock {
    /// Wrap a data block as `XattrBlock`.
    pub fn new(block: Block) -> Self {
        XattrBlock(block)
    }

    /// Get the wrapped block.
    pub fn block(self) -> Block {
        self.0
    }

    /// Initialize a xattr block, write a `XattrHeader` to the
    /// beginning of the block.
    pub fn init(&mut self) {
        self.0.data.fill(0);
        let header = XattrHeader::new();
        self.0.write_offset_as(0, &header);
    }

    /// Verify the Linux ext4 external-xattr block checksum.
    pub fn verify_checksum(&self, seed: MetadataChecksumSeed, block_id: PBlockId) -> bool {
        let header: XattrHeader = self.0.read_offset_as(0);
        xattr_block_checksum(seed, block_id, &*self.0.data)
            .is_some_and(|checksum| checksum == header.checksum())
    }

    /// Recompute the Linux ext4 external-xattr block checksum after mutation.
    pub fn update_checksum(&mut self, seed: MetadataChecksumSeed, block_id: PBlockId) -> bool {
        let mut header: XattrHeader = self.0.read_offset_as(0);
        header.set_checksum(0);
        self.0.write_offset_as(0, &header);
        let Some(checksum) = xattr_block_checksum(seed, block_id, &*self.0.data) else {
            return false;
        };
        header.set_checksum(checksum);
        self.0.write_offset_as(0, &header);
        true
    }

    /// Decode one entry only after checking its variable-length disk bounds.
    fn entry_at(&self, offset: usize) -> Result<Option<XattrEntry>> {
        let bad = || Ext4Error::new(ErrCode::EIO);
        let marker = self
            .0
            .data
            .get(offset..offset.checked_add(4).ok_or_else(bad)?)
            .ok_or_else(bad)?;
        if marker == [0; 4] {
            return Ok(None);
        }
        let fixed_end = offset
            .checked_add(size_of::<FakeXattrEntry>())
            .ok_or_else(bad)?;
        let fixed = self.0.data.get(offset..fixed_end).ok_or_else(bad)?;
        let raw = FakeXattrEntry::from_bytes(fixed);
        let end = fixed_end
            .checked_add(raw.name_len as usize)
            .ok_or_else(bad)?;
        let name = self.0.data.get(fixed_end..end).ok_or_else(bad)?;
        if name.contains(&0) {
            return Err(bad());
        }
        let next = offset
            .checked_add((size_of::<FakeXattrEntry>() + name.len()).div_ceil(4) * 4)
            .ok_or_else(bad)?;
        if next.checked_add(4).is_none_or(|end| end > BLOCK_SIZE) {
            return Err(bad());
        }
        Ok(Some(XattrEntry::from_bytes(&self.0.data[offset..end])))
    }

    /// Validate once before accessing or moving table/value regions. No name
    /// collection or value copy is needed; the block snapshot owns all bytes.
    pub(crate) fn validate(&self) -> Result<()> {
        let bad = || Ext4Error::new(ErrCode::EIO);
        let header: XattrHeader = self.0.read_offset_as(0);
        if header.magic() != XattrHeader::XATTR_MAGIC
            || header.blocks() != 1
            || header.refcount() == 0
        {
            return Err(bad());
        }
        let mut offset = size_of::<XattrHeader>();
        let mut first_value = BLOCK_SIZE;
        while let Some(entry) = self.entry_at(offset)? {
            if entry.value_inum == 0 && entry.value_size != 0 {
                let start = entry.value_offset as usize;
                let padded = xattr_value_storage_size(entry.value_size as usize).ok_or_else(bad)?;
                if start & 3 != 0 || start.checked_add(padded).is_none_or(|end| end > BLOCK_SIZE) {
                    return Err(bad());
                }
                first_value = first_value.min(start);
            } else if entry.value_inum != 0 && entry.value_size == 0 {
                return Err(bad());
            }
            offset += entry.used_size();
        }
        if offset + 4 > first_value {
            return Err(bad());
        }
        Ok(())
    }

    pub(crate) fn validate_inline(&self) -> Result<()> {
        self.validate()?;
        let mut offset = size_of::<XattrHeader>();
        while let Some(entry) = self.entry_at(offset)? {
            if entry.value_inum != 0 {
                return Err(Ext4Error::new(ErrCode::ENOTSUP));
            }
            let next = offset + entry.used_size();
            if entry.value_size != 0 {
                let start = entry.value_offset as usize;
                let end = start + xattr_value_storage_size(entry.value_size as usize).unwrap();
                let mut later = next;
                while let Some(other) = self.entry_at(later)? {
                    if other.value_inum == 0 && other.value_size != 0 {
                        let other_start = other.value_offset as usize;
                        let other_end = other_start
                            + xattr_value_storage_size(other.value_size as usize).unwrap();
                        if start < other_end && other_start < end {
                            return Err(Ext4Error::new(ErrCode::EIO));
                        }
                    }
                    later += other.used_size();
                }
            }
            offset = next;
        }
        Ok(())
    }

    pub fn is_empty(&self) -> Result<bool> {
        self.validate()?;
        Ok(self.entry_at(size_of::<XattrHeader>())?.is_none())
    }

    /// Get a borrowed inline value. External EA inode values are unsupported.
    pub fn get(&self, name: &[u8]) -> Result<Option<&[u8]>> {
        self.validate()?;
        let mut offset = size_of::<XattrHeader>();
        while let Some(entry) = self.entry_at(offset)? {
            if entry.compare_name(name).is_eq() {
                if entry.value_inum != 0 {
                    return Err(Ext4Error::new(ErrCode::ENOTSUP));
                }
                let start = entry.value_offset as usize;
                // Empty values do not require a meaningful disk offset.
                return Ok(Some(if entry.value_size == 0 {
                    &[]
                } else {
                    &self.0.data[start..start + entry.value_size as usize]
                }));
            }
            offset += entry.used_size();
        }
        Ok(None)
    }

    /// Write NUL-separated names directly; an empty buffer is a size probe.
    /// Validate and check total size before touching the caller's output.
    pub fn list(&self, buf: &mut [u8]) -> Result<usize> {
        self.validate()?;
        let mut total = 0usize;
        let mut offset = size_of::<XattrHeader>();
        while let Some(entry) = self.entry_at(offset)? {
            if let Some(prefix) = entry.prefix() {
                total = total
                    .checked_add(prefix.len() + entry.name_len as usize + 1)
                    .ok_or_else(|| Ext4Error::new(ErrCode::E2BIG))?;
            }
            offset += entry.used_size();
        }
        if buf.is_empty() {
            return Ok(total);
        }
        if buf.len() < total {
            return Err(Ext4Error::new(ErrCode::ERANGE));
        }
        offset = size_of::<XattrHeader>();
        let mut written = 0;
        while let Some(entry) = self.entry_at(offset)? {
            if let Some(prefix) = entry.prefix() {
                buf[written..written + prefix.len()].copy_from_slice(prefix);
                written += prefix.len();
                let suffix = &entry.name[..entry.name_len as usize];
                buf[written..written + suffix.len()].copy_from_slice(suffix);
                written += suffix.len();
                buf[written] = 0;
                written += 1;
            }
            offset += entry.used_size();
        }
        Ok(total)
    }

    /// Insert a sorted entry, allocating its value from the free area's end.
    /// Value placement is independent of entry ordering, as on Linux ext4.
    pub fn insert(&mut self, name: &[u8], value: &[u8]) -> bool {
        if XattrEntry::validate_name(name).is_err() || self.validate_inline().is_err() {
            return false;
        }
        let mut table_end = size_of::<XattrHeader>();
        let mut value_start = BLOCK_SIZE;
        let mut insert_at = None;
        while let Ok(Some(entry)) = self.entry_at(table_end) {
            if insert_at.is_none() && entry.compare_name(name).is_gt() {
                insert_at = Some(table_end);
            }
            if entry.value_size != 0 {
                value_start = value_start.min(entry.value_offset as usize);
            }
            table_end += entry.used_size();
        }
        let insert_at = insert_at.unwrap_or(table_end);
        let entry_size = XattrEntry::required_size(name);
        let Some(storage_size) = xattr_value_storage_size(value.len()) else {
            return false;
        };
        let Some(new_value_start) = value_start.checked_sub(storage_size) else {
            return false;
        };
        if table_end
            .checked_add(entry_size + 4)
            .is_none_or(|end| end > new_value_start)
        {
            return false;
        }
        // Include the four-byte terminator in the table move.
        self.0
            .data
            .copy_within(insert_at..table_end + 4, insert_at + entry_size);
        self.0.data[insert_at..insert_at + entry_size].fill(0);
        self.0.data[new_value_start..value_start].fill(0);
        let entry = XattrEntry::new(
            name,
            value.len(),
            if value.is_empty() { 0 } else { new_value_start },
        );
        self.0.write_offset_as(insert_at, &entry);
        self.0.write_offset(new_value_start, value);
        self.rehash();
        true
    }

    /// Linux ext4_xattr_hash_entry/ext4_xattr_rehash. The namespace prefix
    /// is encoded separately and is not part of the entry hash. Inline values
    /// are hashed as zero-padded little-endian words, including the final word.
    fn rehash(&mut self) {
        let mut offset = size_of::<XattrHeader>();
        let mut block_hash = 0u32;
        let mut shareable = true;
        while self.0.data[offset..offset + 4] != [0; 4] {
            let mut entry: XattrEntry = self.0.read_offset_as(offset);
            let mut hash = 0u32;
            for byte in &entry.name[..entry.name_len as usize] {
                hash = hash.rotate_left(5) ^ u32::from(*byte);
            }
            if entry.value_size != 0 {
                let start = entry.value_offset as usize;
                let end = start + (entry.value_size as usize).div_ceil(4) * 4;
                for word in self.0.data[start..end].chunks_exact(4) {
                    hash = hash.rotate_left(16) ^ u32::from_le_bytes(word.try_into().unwrap());
                }
            }
            entry.hash = hash;
            shareable &= hash != 0;
            block_hash = block_hash.rotate_left(16) ^ hash;
            self.0.write_offset_as(offset, &entry);
            offset += entry.used_size();
        }
        let mut header: XattrHeader = self.0.read_offset_as(0);
        header.hash = if shareable { block_hash } else { 0 };
        self.0.write_offset_as(0, &header);
    }

    /// Remove an entry and compact the physical value region independently
    /// from the sorted entry table. Linux images need not order the two alike.
    pub fn remove(&mut self, name: &[u8]) -> bool {
        if self.validate_inline().is_err() {
            return false;
        }
        let mut table_end = size_of::<XattrHeader>();
        let mut value_start = BLOCK_SIZE;
        let mut removed = None;
        while let Ok(Some(entry)) = self.entry_at(table_end) {
            if entry.value_size != 0 {
                value_start = value_start.min(entry.value_offset as usize);
            }
            let next = table_end + entry.used_size();
            if entry.compare_name(name).is_eq() {
                removed = Some((table_end, entry));
            }
            table_end = next;
        }
        let Some((entry_start, entry)) = removed else {
            return false;
        };
        let entry_size = entry.used_size();
        let removed_start = entry.value_offset as usize;
        let storage_size = xattr_value_storage_size(entry.value_size as usize).unwrap();
        self.0
            .data
            .copy_within(entry_start + entry_size..table_end + 4, entry_start);
        self.0.data[table_end + 4 - entry_size..table_end + 4].fill(0);
        if storage_size != 0 {
            self.0
                .data
                .copy_within(value_start..removed_start, value_start + storage_size);
            self.0.data[value_start..value_start + storage_size].fill(0);
            let mut offset = size_of::<XattrHeader>();
            while let Some(mut other) = self.entry_at(offset).unwrap() {
                if other.value_size != 0 && (other.value_offset as usize) < removed_start {
                    other.value_offset += storage_size as u16;
                    self.0.write_offset_as(offset, &other);
                }
                offset += other.used_size();
            }
        }
        self.rehash();
        true
    }
}

#[cfg(test)]
mod release_tests {
    use super::*;

    #[test]
    fn mutation_hashes_name_suffix_and_padded_values_like_linux() {
        let mut block = XattrBlock::new(Block::new(9, Box::new([0; BLOCK_SIZE])));
        block.init();
        assert!(block.insert(b"user.a", &[1, 2, 3]));
        let first: XattrEntry = block.0.read_offset_as(size_of::<XattrHeader>());
        assert_eq!(first.hash, 0x0062_0201);
        let header: XattrHeader = block.0.read_offset_as(0);
        assert_eq!(header.hash, first.hash);
        assert!(block.remove(b"user.a"));
        assert_eq!(block.0.read_offset_as::<XattrHeader>(0).hash, 0);
        assert!(block.insert(b"user.a", &[]));
        assert_eq!(
            block
                .0
                .read_offset_as::<XattrEntry>(size_of::<XattrHeader>())
                .hash,
            97
        );
    }

    #[test]
    fn byte_names_list_without_conversion_and_preserve_output_on_range_error() {
        let mut block = XattrBlock::new(Block::new(9, Box::new([0; BLOCK_SIZE])));
        block.init();
        assert!(block.insert(b"user.\xff", b"one"));
        assert!(block.insert(b"user.\xfe", b"two"));
        assert_eq!(block.get(b"user.\xff").unwrap(), Some(&b"one"[..]));
        let size = block.list(&mut []).unwrap();
        assert_eq!(size, 14);
        let mut short = [0xa5; 13];
        assert_eq!(block.list(&mut short).unwrap_err().code(), ErrCode::ERANGE);
        assert_eq!(short, [0xa5; 13]);
        let mut output = [0xa5; 18];
        assert_eq!(block.list(&mut output).unwrap(), size);
        assert_eq!(&output[..size], b"user.\xfe\0user.\xff\0");
        assert_eq!(&output[size..], &[0xa5; 4]);
    }

    #[test]
    fn mutation_uses_physical_value_offsets_and_accepts_empty_offset_zero() {
        let mut block = XattrBlock::new(Block::new(9, Box::new([0; BLOCK_SIZE])));
        block.init();
        // A Linux-valid preexisting image: sorted entries, independently
        // placed values with a gap, and an empty value with offset zero.
        let next = add_inline_entry(&mut block.0.data, size_of::<XattrHeader>(), b"a", 4080, 5);
        let next = add_inline_entry(&mut block.0.data, next, b"z", 4092, 4);
        add_inline_entry(&mut block.0.data, next, b"empty", 0, 0);
        block.0.data[4080..4085].copy_from_slice(b"first");
        block.0.data[4092..4096].copy_from_slice(b"last");
        assert!(block.insert(b"user.m", b"middle"));
        assert_eq!(block.get(b"user.empty").unwrap(), Some(&b""[..]));
        assert!(block.remove(b"user.z"));
        assert_eq!(block.get(b"user.a").unwrap(), Some(&b"first"[..]));
        assert_eq!(block.get(b"user.m").unwrap(), Some(&b"middle"[..]));
        assert!(block.remove(b"user.empty"));
        assert!(block.insert(b"user.b", b"new"));
        assert!(block.remove(b"user.a"));
        assert_eq!(block.get(b"user.m").unwrap(), Some(&b"middle"[..]));
        assert_eq!(block.get(b"user.b").unwrap(), Some(&b"new"[..]));
        assert!(validate_xattr_block_for_release(&block.0.data[..]).is_some());
    }

    #[test]
    fn zero_suffix_acl_is_not_a_terminator_and_namespace_names_are_exact() {
        let mut block = XattrBlock::new(Block::new(9, Box::new([0; BLOCK_SIZE])));
        block.init();
        assert!(block.insert(b"system.posix_acl_access", b"acl"));
        assert!(block.insert(b"security.tail", b"present"));
        assert!(block.insert(b"gnu.test", b"gnu"));
        let mut names = [0; 128];
        let size = block.list(&mut names).unwrap();
        assert_eq!(
            &names[..size],
            b"system.posix_acl_access\0security.tail\0gnu.test\0"
        );
        assert_eq!(block.get(b"security.tail").unwrap(), Some(&b"present"[..]));
        assert!(block.remove(b"system.posix_acl_access"));
        assert_eq!(block.get(b"security.tail").unwrap(), Some(&b"present"[..]));
        assert_eq!(
            XattrEntry::validate_name(b"system.unknown")
                .unwrap_err()
                .code(),
            ErrCode::ENOTSUP
        );
    }

    #[test]
    fn external_values_can_be_listed_but_are_never_read_as_inline_data() {
        let mut block = XattrBlock::new(Block::new(9, Box::new([0; BLOCK_SIZE])));
        block.init();
        assert!(block.insert(b"user.external", b"data"));
        let offset = size_of::<XattrHeader>();
        let mut entry: XattrEntry = block.0.read_offset_as(offset);
        entry.value_inum = 123;
        entry.value_offset = 0;
        block.0.write_offset_as(offset, &entry);
        let mut names = [0; 32];
        let size = block.list(&mut names).unwrap();
        assert_eq!(&names[..size], b"user.external\0");
        assert_eq!(
            block.get(b"user.external").unwrap_err().code(),
            ErrCode::ENOTSUP
        );
        assert_eq!(
            block.validate_inline().unwrap_err().code(),
            ErrCode::ENOTSUP
        );
    }

    #[test]
    fn malformed_blocks_fail_before_copy_or_mutation_and_unknown_indices_are_hidden() {
        let mut block = XattrBlock::new(Block::new(9, Box::new([0; BLOCK_SIZE])));
        block.init();
        assert!(block.insert(b"user.a", b"value"));
        let offset = size_of::<XattrHeader>();
        let mut entry: XattrEntry = block.0.read_offset_as(offset);
        entry.name_index = 99;
        block.0.write_offset_as(offset, &entry);
        assert_eq!(block.list(&mut []).unwrap(), 0);
        entry.value_offset = 40; // Overlaps the table.
        block.0.write_offset_as(offset, &entry);
        let mut output = [0xa5; 32];
        assert_eq!(block.list(&mut output).unwrap_err().code(), ErrCode::EIO);
        assert_eq!(output, [0xa5; 32]);
        assert!(!block.remove(b"user.a"));

        block.init();
        assert!(block.insert(b"user.empty", b""));
        let mut empty: XattrEntry = block.0.read_offset_as(offset);
        empty.value_offset = u16::MAX;
        block.0.write_offset_as(offset, &empty);
        assert!(block.insert(b"user.a", b"data"));
        assert!(block.remove(b"user.a"));
        assert!(block.remove(b"user.empty"));

        block.init();
        assert!(block.insert(b"user.a", b"data"));
        assert!(block.insert(b"user.b", b"more"));
        let first: XattrEntry = block.0.read_offset_as(offset);
        let second_offset = offset + first.used_size();
        let mut second: XattrEntry = block.0.read_offset_as(second_offset);
        second.value_offset = first.value_offset;
        block.0.write_offset_as(second_offset, &second);
        assert_eq!(block.validate_inline().unwrap_err().code(), ErrCode::EIO);
        assert!(!block.insert(b"user.c", b"new"));

        // Walk a valid fixed-width prefix up to the end of the block. Unknown
        // indices with empty suffixes are real entries, not terminators.
        for (remaining, tail_name_len) in [(4, 0), (8, 0), (12, 0), (16, 0), (20, 5), (20, 4)] {
            block.init();
            let tail = BLOCK_SIZE - remaining;
            let mut cursor = size_of::<XattrHeader>();
            let extra = (tail - cursor) % size_of::<FakeXattrEntry>();
            let mut raw = FakeXattrEntry {
                name_len: extra as u8,
                name_index: 99,
                value_offset: 0,
                value_inum: 0,
                value_size: 0,
                hash: 0,
            };
            if extra != 0 {
                block.0.write_offset_as(cursor, &raw);
                block.0.data[cursor + size_of::<FakeXattrEntry>()
                    ..cursor + size_of::<FakeXattrEntry>() + extra]
                    .fill(b'a');
                cursor += size_of::<FakeXattrEntry>() + extra;
            }
            raw.name_len = 0;
            while cursor < tail {
                block.0.write_offset_as(cursor, &raw);
                cursor += size_of::<FakeXattrEntry>();
            }
            assert_eq!(cursor, tail);
            // The final item respectively lacks fixed bytes, name bytes, or
            // the mandatory four bytes after its complete aligned entry.
            raw.name_len = tail_name_len;
            let fixed = raw.to_bytes();
            let copied = remaining.min(fixed.len());
            block.0.data[tail..tail + copied].copy_from_slice(&fixed[..copied]);
            if remaining > fixed.len() {
                block.0.data[tail + fixed.len()..].fill(b'b');
            }
            let mut output = [0xa5; 32];
            assert_eq!(block.get(b"user.a").unwrap_err().code(), ErrCode::EIO);
            assert_eq!(block.list(&mut output).unwrap_err().code(), ErrCode::EIO);
            assert_eq!(output, [0xa5; 32]);
        }

        block.init();
        assert!(block.insert(b"user.a", b"data"));
        let mut entry: XattrEntry = block.0.read_offset_as(offset);
        entry.value_size = 8; // Offset 4092 plus eight bytes exceeds the block.
        block.0.write_offset_as(offset, &entry);
        let mut output = [0xa5; 32];
        assert_eq!(block.get(b"user.a").unwrap_err().code(), ErrCode::EIO);
        assert_eq!(block.list(&mut output).unwrap_err().code(), ErrCode::EIO);
        assert_eq!(output, [0xa5; 32]);
    }

    fn add_inline_entry(
        bytes: &mut [u8; BLOCK_SIZE],
        start: usize,
        name: &[u8],
        value_offset: u16,
        value_size: u32,
    ) -> usize {
        let entry = FakeXattrEntry {
            name_len: name.len() as u8,
            name_index: 1,
            value_offset,
            value_inum: 0,
            value_size,
            hash: 0,
        };
        bytes[start..start + size_of::<FakeXattrEntry>()].copy_from_slice(entry.to_bytes());
        let name_start = start + size_of::<FakeXattrEntry>();
        bytes[name_start..name_start + name.len()].copy_from_slice(name);
        start + (size_of::<FakeXattrEntry>() + name.len()).div_ceil(4) * 4
    }

    fn checksummed_block(
        block_id: PBlockId,
        refcount: u32,
    ) -> ([u8; BLOCK_SIZE], MetadataChecksumSeed) {
        let mut bytes = [0; BLOCK_SIZE];
        let mut header = XattrHeader::new();
        header.set_refcount(refcount);
        bytes[..size_of::<XattrHeader>()].copy_from_slice(header.to_bytes());
        let seed = MetadataChecksumSeed::from_raw(0x1234_5678);
        let checksum = xattr_block_checksum(seed, block_id, &bytes).unwrap();
        header.set_checksum(checksum);
        bytes[..size_of::<XattrHeader>()].copy_from_slice(header.to_bytes());
        (bytes, seed)
    }

    #[test]
    fn release_validation_checks_fixed_header_fields() {
        let (mut bytes, _) = checksummed_block(17, 2);
        let (header, has_ea_inode) = validate_xattr_block_for_release(&bytes).unwrap();
        assert_eq!(header.refcount(), 2);
        assert!(!has_ea_inode);

        bytes[..4].copy_from_slice(&0u32.to_le_bytes());
        assert!(validate_xattr_block_for_release(&bytes).is_none());
        let (mut bytes, _) = checksummed_block(17, 1);
        bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
        assert!(validate_xattr_block_for_release(&bytes).is_none());
    }

    #[test]
    fn checksum_binds_block_number_and_detects_damage() {
        let (mut bytes, seed) = checksummed_block(17, 2);
        let header = XattrHeader::from_bytes(&bytes[..size_of::<XattrHeader>()]);
        assert_eq!(
            xattr_block_checksum(seed, 17, &bytes).unwrap(),
            header.checksum()
        );
        assert_ne!(
            xattr_block_checksum(seed, 18, &bytes).unwrap(),
            header.checksum()
        );
        bytes[BLOCK_SIZE - 1] ^= 1;
        assert_ne!(
            xattr_block_checksum(seed, 17, &bytes).unwrap(),
            header.checksum()
        );
    }

    #[test]
    fn xattr_block_mutation_recomputes_linux_checksum() {
        let seed = MetadataChecksumSeed::from_raw(0x1234_5678);
        let block_id = 17;
        let mut block = XattrBlock::new(Block::new(block_id, Box::new([0; BLOCK_SIZE])));
        block.init();
        assert!(block.insert(b"user.test", b"value"));
        assert!(validate_xattr_block_for_release(&block.0.data[..]).is_some());
        assert!(block.update_checksum(seed, block_id));
        assert!(block.verify_checksum(seed, block_id));

        assert!(block.remove(b"user.test"));
        assert!(validate_xattr_block_for_release(&block.0.data[..]).is_some());
        assert!(!block.verify_checksum(seed, block_id));
        assert!(block.update_checksum(seed, block_id));
        assert!(block.verify_checksum(seed, block_id));
        assert!(!block.verify_checksum(seed, block_id + 1));
    }

    #[test]
    fn unaligned_values_keep_linux_padded_layout_across_insert_and_remove() {
        let mut block = XattrBlock::new(Block::new(17, Box::new([0; BLOCK_SIZE])));
        block.init();

        assert!(block.insert(b"user.first", b"1234567"));
        assert!(block.insert(b"user.second", b"abc"));
        assert!(block.insert(b"user.third", b"12345"));
        assert_eq!(block.get(b"user.first").unwrap(), Some(&b"1234567"[..]));
        assert_eq!(block.get(b"user.second").unwrap(), Some(&b"abc"[..]));
        assert_eq!(block.get(b"user.third").unwrap(), Some(&b"12345"[..]));
        assert!(validate_xattr_block_for_release(&block.0.data[..]).is_some());

        assert!(block.remove(b"user.first"));
        assert!(block.remove(b"user.third"));
        assert_eq!(block.get(b"user.first").unwrap(), None);
        assert_eq!(block.get(b"user.second").unwrap(), Some(&b"abc"[..]));
        assert_eq!(block.get(b"user.third").unwrap(), None);
        assert!(validate_xattr_block_for_release(&block.0.data[..]).is_some());
    }

    #[test]
    fn release_validation_reports_ea_inode_entries() {
        let (mut bytes, seed) = checksummed_block(17, 1);
        bytes[size_of::<XattrHeader>()] = 1; // name_len
        bytes[size_of::<XattrHeader>() + 4..size_of::<XattrHeader>() + 8]
            .copy_from_slice(&42u32.to_le_bytes()); // value_inum
        bytes[size_of::<XattrHeader>() + 8..size_of::<XattrHeader>() + 12]
            .copy_from_slice(&1u32.to_le_bytes()); // value_size
        bytes[size_of::<XattrHeader>() + size_of::<FakeXattrEntry>()] = b'x';
        let checksum = xattr_block_checksum(seed, 17, &bytes).unwrap();
        bytes[core::mem::offset_of!(XattrHeader, checksum)
            ..core::mem::offset_of!(XattrHeader, checksum) + 4]
            .copy_from_slice(&checksum.to_le_bytes());
        assert_eq!(validate_xattr_block_for_release(&bytes).unwrap().1, true);
    }

    #[test]
    fn release_validation_rejects_malformed_layout_without_checksum_help() {
        let (template, _) = checksummed_block(17, 1);
        let first = size_of::<XattrHeader>();

        let mut bytes = template;
        let next = add_inline_entry(&mut bytes, first, b"a\0b", 4088, 4);
        bytes[next..next + 4].fill(0);
        assert!(validate_xattr_block_for_release(&bytes).is_none());

        let mut bytes = template;
        let next = add_inline_entry(&mut bytes, first, b"a", 33, 4);
        bytes[next..next + 4].fill(0);
        assert!(validate_xattr_block_for_release(&bytes).is_none());

        let mut bytes = template;
        let next = add_inline_entry(&mut bytes, first, b"a", first as u16, 4);
        bytes[next..next + 4].fill(0);
        assert!(validate_xattr_block_for_release(&bytes).is_none());

        let mut bytes = template;
        let second = add_inline_entry(&mut bytes, first, b"a", 4088, 8);
        let end = add_inline_entry(&mut bytes, second, b"b", 4092, 4);
        bytes[end..end + 4].fill(0);
        assert!(validate_xattr_block_for_release(&bytes).is_none());

        let mut bytes = template;
        bytes[first..].fill(0xff);
        assert!(validate_xattr_block_for_release(&bytes).is_none());
    }
}
