use alloc::{fmt, string::String};
use core::fmt::Debug;
use system_error::SystemError;
const EXT2_NAME_LEN: usize = 255;

#[derive(Clone)]
#[repr(C, align(1))]
pub struct Ext2DirEntry {
    /// Inode number of the file
    inode: u32,
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
    pub fn new(inode: u32, file_type: u8, file_name: &str) -> Result<Self, SystemError> {
        if file_name.len() > EXT2_NAME_LEN
            || inode == 0
            || Ext2DirEntryType::is_valid(file_type) == false
        {
            return Err(SystemError::EINVAL);
        }
        let mut record_length: u16 = 8 + file_name.len() as u16;
        if record_length % 4 != 0 {
            record_length += 4 - (record_length % 4);
        }
        let mut name = [0u8; EXT2_NAME_LEN];
        name[..file_name.len()].copy_from_slice(file_name.as_bytes());
        Ok(Self {
            inode,
            record_length,
            name_length: file_name.len() as u8,
            file_type,
            name,
        })
    }
    pub fn get_name(&self) -> String {
        String::from_utf8(self.name.to_vec()).expect("Invalid UTF-8 in entry name")
    }
    pub fn get_inode(&self) -> usize {
        self.inode as usize
    }
    pub fn get_file_type(&self) -> Ext2DirEntryType {
        Ext2DirEntryType::from(self.file_type)
    }
    pub fn if_used(&self) -> bool {
        self.inode == 0
    }
    pub fn get_rec_len(&self) -> usize {
        self.record_length as usize
    }
}
impl Debug for Ext2DirEntry {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Ext2DirEntry")
            .field("inode", &self.inode)
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
    pub fn is_valid(value: u8) -> bool {
        if value <= 7 {
            return true;
        }
        false
    }
}
