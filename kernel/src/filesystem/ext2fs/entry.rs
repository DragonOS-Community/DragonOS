use alloc::string::String;

const EXT2_NAME_LEN: usize = 255;
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
    pub fn new() -> Self {
        Self {
            inode: 0,
            record_length: 0,
            name_length: 0,
            file_type: 0,
            name: [0; EXT2_NAME_LEN],
        }
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
