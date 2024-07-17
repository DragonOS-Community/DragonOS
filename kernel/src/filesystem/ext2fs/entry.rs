use alloc::{fmt, string::String, vec::Vec};
use core::fmt::Debug;
use log::debug;
use system_error::SystemError;

use super::file_type::Ext2FileType;
const EXT2_NAME_LEN: usize = 255;

#[derive(Clone)]
#[repr(C, align(1))]
pub struct Ext2DirEntry {
    /// Inode number of the file
    inode_num: u32,
    /// Length of the directory entry record
    record_length: u16,
    /// Length of the name
    name_length: u8,
    /// File type
    file_type: u8,
    /// Name of the file
    name: [u8; EXT2_NAME_LEN],
}

impl Ext2DirEntry {
    pub fn new(
        inode_num: u32,
        file_type: Ext2FileType,
        file_name: &str,
    ) -> Result<Self, SystemError> {
        // debug!("========== Ext2DirEntry::new ==========");
        let tp = Ext2DirEntryType::convert_file_type(file_type);

        if file_name.len() > EXT2_NAME_LEN || inode_num == 0 {
            debug!(
                "inum: {inode_num},type: {:?},name: {}",
                file_type, file_name
            );
            return Err(SystemError::EINVAL);
        }
        let mut record_length: u16 = 8 + file_name.len() as u16;
        if record_length % 4 != 0 {
            record_length += 4 - (record_length % 4);
        }
        let mut name = [0u8; EXT2_NAME_LEN];
        name[..file_name.len()].copy_from_slice(file_name.as_bytes());
        let ret = Self {
            inode_num,
            record_length,
            name_length: file_name.len() as u8,
            file_type: tp.to_u8(),
            name,
        };
        // debug!("========== Ext2DirEntry::new ==========");
        Ok(ret)
    }
    pub fn get_name(&self) -> String {
        let name = &self.name[..self.name_length as usize];
        String::from_utf8(name.to_vec()).expect("Invalid UTF-8 in entry name")
    }
    pub fn get_inode(&self) -> usize {
        self.inode_num as usize
    }
    pub fn get_file_type(&self) -> Ext2DirEntryType {
        Ext2DirEntryType::from(self.file_type)
    }
    pub fn if_used(&self) -> bool {
        self.inode_num == 0
    }
    pub fn get_rec_len(&self) -> usize {
        self.record_length as usize
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes: Vec<u8> = Vec::with_capacity(self.record_length as usize);
        bytes.resize(self.record_length as usize, 0);
        bytes[0..4].copy_from_slice(&self.inode_num.to_le_bytes());
        bytes[4..6].copy_from_slice(&self.record_length.to_le_bytes());

        bytes[6..7].copy_from_slice(&self.name_length.to_le_bytes());

        bytes[7..8].copy_from_slice(&self.file_type.to_le_bytes());
        bytes[8..8 + self.name_length as usize]
            .copy_from_slice(&self.name[0..self.name_length as usize]);
        // bytes.extend_from_slice(&self.name[0..self.name_length as usize]);

        bytes
    }
}
impl Debug for Ext2DirEntry {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Ext2DirEntry")
            .field("inode_num", &self.inode_num)
            .field("record_length", &self.record_length)
            .field("name_length", &self.name_length)
            .field("file_type", &Ext2DirEntryType::from(self.file_type))
            .field("name", &self.name)
            .finish()
    }
}
/// EXT2 目录项类型
pub enum Ext2DirEntryType {
    Unknown,
    RegularFile,
    Directory,
    CharacterDevice,
    BlockDevice,
    FIFO,
    Socket,
    Symlink,
}

impl From<u8> for Ext2DirEntryType {
    fn from(value: u8) -> Self {
        match value {
            0 => Ext2DirEntryType::Unknown,
            1 => Ext2DirEntryType::RegularFile,
            2 => Ext2DirEntryType::Directory,
            3 => Ext2DirEntryType::CharacterDevice,
            4 => Ext2DirEntryType::BlockDevice,
            5 => Ext2DirEntryType::FIFO,
            6 => Ext2DirEntryType::Socket,
            7 => Ext2DirEntryType::Symlink,
            _ => Ext2DirEntryType::Unknown,
        }
    }
}

impl Debug for Ext2DirEntryType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Ext2DirEntryType::Unknown => f.write_str("Unknown"),
            Ext2DirEntryType::RegularFile => f.write_str("RegularFile"),
            Ext2DirEntryType::Directory => f.write_str("Directory"),
            Ext2DirEntryType::CharacterDevice => f.write_str("CharacterDevice"),
            Ext2DirEntryType::BlockDevice => f.write_str("BlockDevice"),
            Ext2DirEntryType::FIFO => f.write_str("FIFO"),
            Ext2DirEntryType::Socket => f.write_str("Socket"),
            Ext2DirEntryType::Symlink => f.write_str("Symlink"),
        }
    }
}
impl Ext2DirEntryType {
    pub fn to_u8(&self) -> u8 {
        match self {
            Ext2DirEntryType::Unknown => 0,
            Ext2DirEntryType::RegularFile => 1,
            Ext2DirEntryType::Directory => 2,
            Ext2DirEntryType::CharacterDevice => 3,
            Ext2DirEntryType::BlockDevice => 4,
            Ext2DirEntryType::FIFO => 5,
            Ext2DirEntryType::Socket => 6,
            Ext2DirEntryType::Symlink => 7,
        }
    }
    pub fn convert_file_type(file_type: Ext2FileType) -> Self {
        match file_type {
            Ext2FileType::RegularFile => Ext2DirEntryType::RegularFile,
            Ext2FileType::Directory => Ext2DirEntryType::Directory,
            Ext2FileType::CharacterDevice => Ext2DirEntryType::CharacterDevice,
            Ext2FileType::BlockDevice => Ext2DirEntryType::BlockDevice,
            Ext2FileType::FIFO => Ext2DirEntryType::FIFO,
            Ext2FileType::SymbolicLink => Ext2DirEntryType::Socket,
            _ => Ext2DirEntryType::Unknown,
        }
    }
    pub fn is_valid(value: u8) -> bool {
        if value <= 7 {
            return true;
        }
        false
    }
}
