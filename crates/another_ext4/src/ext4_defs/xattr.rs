//! Extended attributes (xattrs) are typically stored in a separate data block
//! on the disk and referenced from inodes via `inode.file_acl*`.
//!
//! There are two places where extended attributes can be found. The first place
//! is between the end of each inode entry and the beginning of the next inode
//! entry. The second place where extended attributes can be found is in the block
//! pointed to by `inode.file_acl`.
//!
//! We only implement the seperate data block storage of extended attributes.

use super::{AsBytes, Block};
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
    const XATTR_MAGIC: u32 = 0xEA020000;

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
    /// Hash value of attribute name and attribute value (UNUSED by now)
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
    pub fn new(name: &str, value_size: usize, value_offset: usize) -> Self {
        let mut name_bytes = [0u8; 255];
        let (name_index, name) = Self::match_name(name);
        let name_len = name.len();
        name_bytes[..name_len].copy_from_slice(name.as_bytes());
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

    /// Get the name of the xattr entry
    pub fn name(&self) -> String {
        let prefix = match self.name_index {
            1 => "user.",
            2 => "system.posix_acl_access.",
            3 => "system.posix_acl_default.",
            4 => "trusted.",
            6 => "security.",
            7 => "system.",
            _ => "",
        };
        let name_bytes = &self.name[..self.name_len as usize];
        let name = unsafe { String::from_utf8_unchecked(name_bytes.to_vec()) };
        prefix.to_string() + &name
    }

    /// Get the required size to save a xattr entry, 4-byte aligned
    pub fn required_size(name: &str) -> usize {
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
    pub fn compare_name(&self, name: &str) -> Ordering {
        let (name_index, name) = Self::match_name(name);
        match self.name_index.cmp(&name_index) {
            Ordering::Equal => {}
            ordering => return ordering,
        };
        match self.name_len.cmp(&(name.len() as u8)) {
            Ordering::Equal => {}
            ordering => return ordering,
        }
        self.name[..self.name_len as usize].cmp(name.as_bytes())
    }

    /// Match the attribute name prefix to get name index. If one is found,
    /// return the name index and the string with the prefix removed.
    fn match_name(name: &str) -> (u8, &str) {
        let prefixes = [
            ("user.", 1),
            ("system.posix_acl_access.", 2),
            ("system.posix_acl_default.", 3),
            ("trusted.", 4),
            ("security.", 6),
            ("system.", 7),
        ];
        for (prefix, index) in prefixes {
            if let Some(stripped) = name.strip_prefix(prefix) {
                return (index, stripped);
            }
        }
        (0, name)
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
        let header = XattrHeader::new();
        self.0.write_offset_as(0, &header);
    }

    /// Get a xattr by name, return the value.
    pub fn get(&self, name: &str) -> Option<&[u8]> {
        let mut entry_start = size_of::<XattrHeader>();
        // Iterate over entry table
        while entry_start < BLOCK_SIZE {
            // Check `name_len`, 0 indicates the end of the entry table.
            if self.0.data[entry_start] == 0 {
                // Target xattr not found
                break;
            }
            let entry: XattrEntry = self.0.read_offset_as(entry_start);
            if entry.compare_name(name).is_eq() {
                let offset = entry.value_offset as usize;
                let size = entry.value_size as usize;
                return Some(&self.0.data[offset..offset + size]);
            }
            entry_start += entry.used_size();
        }
        None
    }

    /// List all xattr names
    pub fn list(&self) -> Vec<String> {
        let mut entry_start = size_of::<XattrHeader>();
        let mut names = Vec::new();
        // Iterate over entry table
        while entry_start < BLOCK_SIZE {
            // Check `name_len`, 0 indicates the end of the entry table.
            if self.0.data[entry_start] == 0 {
                break;
            }
            let entry: XattrEntry = self.0.read_offset_as(entry_start);
            names.push(entry.name());
            entry_start += entry.used_size();
        }
        names
    }

    /// Insert a xattr entry into the block. Return true if success.
    pub fn insert(&mut self, name: &str, value: &[u8]) -> bool {
        let mut p_entry = size_of::<XattrHeader>();
        let mut p_value = BLOCK_SIZE;

        let mut is_ins_pos_found = false;
        let mut ins_entry_pos = p_entry;
        let mut ins_value_pos = p_value;
        let ins_entry_size = XattrEntry::required_size(name);
        let ins_value_size = value.len();

        // Iterate over entry table, find the position to insert entry
        // and the end of entry table
        while p_entry < BLOCK_SIZE {
            // Check `name_len`, 0 indicates the end of the entry table.
            if self.0.data[p_entry] == 0 {
                // Reach the end of table
                break;
            }
            let entry: XattrEntry = self.0.read_offset_as(p_entry);
            if !is_ins_pos_found && entry.compare_name(name).is_gt() {
                // Insert before this entry
                ins_entry_pos = p_entry;
                ins_value_pos = p_value;
                is_ins_pos_found = true;
            }
            p_value = entry.value_offset as usize;
            p_entry += entry.used_size();
        }
        if !is_ins_pos_found {
            // Insert at the end of table
            ins_entry_pos = p_entry;
            ins_value_pos = p_value;
        }

        // `ins_entry_pos` points to the position to insert entry,
        // `ins_value_pos - ins_value_size` points to the position to insert value.
        // `p_entry` points to the start of blank area,
        // `p_value` points to the last value,
        // `[p_entry, p_value)` is the blank area.

        // Check space, '+1' is reserved for blank area
        if p_value - p_entry < ins_entry_size + ins_value_size + 1 {
            // Not enough space
            return false;
        }

        // Move the entries from `ins_entry_pos` to `p_entry`
        // Copy `[ins_entry_pos, p_entry)` to `[ins_entry_pos+ins_entry_size, p_entry+ins_entry_size)`
        self.0
            .data
            .copy_within(ins_entry_pos..p_entry, ins_entry_pos + ins_entry_size);
        // Set `[ins_entry_pos..ins_entry_pos+entry_req_size)` to 0
        self.0.data[ins_entry_pos..ins_entry_pos + ins_entry_size].fill(0);

        // Move the corresponding values
        // Copy `[p_value, ins_value_pos)` to `[p_value-ins_value_size, ins_value_pos-ins_value_size)`
        self.0
            .data
            .copy_within(p_value..ins_value_pos, p_value - ins_value_size);
        // Set `[ins_value_pos-ins_value_size, ins_value_pos)` to 0
        self.0.data[ins_value_pos - ins_value_size..ins_value_pos].fill(0);

        // Update the value offset of the moved entries
        let mut p_entry2 = ins_entry_pos + ins_entry_size;
        while p_entry2 < p_entry + ins_entry_size {
            let mut entry: XattrEntry = self.0.read_offset_as(p_entry2);
            entry.value_offset -= ins_value_size as u16;
            self.0.write_offset_as(p_entry2, &entry);
            p_entry2 += entry.used_size();
        }

        // Insert entry to `[ins_entry_pos, ins_entry_pos+ins_entry_size)`
        let entry = XattrEntry::new(name, value.len(), ins_value_pos - ins_value_size);
        self.0.write_offset_as(ins_entry_pos, &entry);
        // Insert value to `[ins_value_pos-ins_value_size, ins_value_pos)`
        self.0.write_offset(ins_value_pos - ins_value_size, value);

        true
    }

    /// Remove a xattr entry from the block. Return true if success.
    pub fn remove(&mut self, name: &str) -> bool {
        let mut p_entry = size_of::<XattrHeader>();
        let mut p_value = BLOCK_SIZE;

        let mut is_rem_pos_found = false;
        let mut rem_entry_pos = p_entry;
        let mut rem_value_pos = p_value;
        let mut rem_entry_size = 0;
        let mut rem_value_size = 0;

        // Iterate over entry table, find the entry to remove
        while p_entry < BLOCK_SIZE {
            // Check `name_len`, 0 indicates the end of the entry table.
            if self.0.data[p_entry] == 0 {
                break;
            }
            let entry: XattrEntry = self.0.read_offset_as(p_entry);
            p_value = entry.value_offset as usize;
            // Compare name
            if !is_rem_pos_found && entry.compare_name(name).is_eq() {
                rem_entry_pos = p_entry;
                rem_value_pos = p_value;
                rem_entry_size = entry.used_size();
                rem_value_size = entry.value_size as usize;
                is_rem_pos_found = true;
            }
            p_entry += entry.used_size();
        }
        if !is_rem_pos_found {
            return false;
        }

        // `rem_entry_pos` points to the entry to remove,
        // `rem_value_pos` points to the value to remove.
        // `p_entry` points to the start of blank area,
        // `p_value` points to the last value,
        // `[p_entry, p_value)` is the blank area.

        // Move the following entries
        // Copy `[rem_entry_pos + rem_entry_size, p_entry)` to `[rem_entry_pos, p_entry - rem_entry_size)`
        self.0
            .data
            .copy_within(rem_entry_pos + rem_entry_size..p_entry, rem_entry_pos);
        // Set `[p_entry - rem_entry_size, p_entry)` to 0
        self.0.data[p_entry - rem_entry_size..p_entry].fill(0);

        // Move the corresponding values
        // Copy `[p_value, rem_value_pos)` to `[p_value + rem_value_size, rem_value_pos + rem_value_size)`
        self.0
            .data
            .copy_within(p_value..rem_value_pos, p_value + rem_value_size);
        // Set `[p_value, p_value + rem_value_size)` to 0
        self.0.data[p_value..p_value + rem_value_size].fill(0);

        // Update the value offset of the moved entries
        let mut p_entry2 = rem_entry_pos;
        while p_entry2 < p_entry - rem_entry_size {
            let mut entry: XattrEntry = self.0.read_offset_as(p_entry2);
            entry.value_offset += rem_value_size as u16;
            self.0.write_offset_as(p_entry2, &entry);
            p_entry2 += entry.used_size();
        }

        true
    }
}
