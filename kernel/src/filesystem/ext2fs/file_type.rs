use core::fmt::Debug;

use log::debug;
use system_error::SystemError;

use crate::filesystem::vfs::{syscall::ModeType, FileType};

/// 文件的类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]

pub enum Ext2FileType {
    /// 文件系统中的 FIFO（管道）
    FIFO = 0x1000,
    /// 字符设备
    CharacterDevice = 0x2000,
    /// 目录
    Directory = 0x4000,
    /// 块设备
    BlockDevice = 0x6000,
    /// 普通文件
    RegularFile = 0x8000,
    /// 符号链接
    SymbolicLink = 0xA000,
    /// Unix 套接字
    UnixSocket = 0xC000,
}

impl Ext2FileType {
    pub fn type_from_mode(mode: &u16) -> Result<Self, SystemError> {
        match mode & 0xF000 {
            0x1000 => Ok(Ext2FileType::FIFO),
            0x2000 => Ok(Ext2FileType::CharacterDevice),
            0x4000 => Ok(Ext2FileType::Directory),
            0x6000 => Ok(Ext2FileType::BlockDevice),
            0x8000 => Ok(Ext2FileType::RegularFile),
            0xA000 => Ok(Ext2FileType::SymbolicLink),
            0xC000 => Ok(Ext2FileType::UnixSocket),
            _ => Err(SystemError::EINVAL),
        }
    }
    pub fn covert_type(&self) -> FileType {
        match self {
            Ext2FileType::FIFO => FileType::Pipe,
            Ext2FileType::CharacterDevice => FileType::CharDevice,
            Ext2FileType::Directory => FileType::Dir,
            Ext2FileType::BlockDevice => FileType::BlockDevice,
            Ext2FileType::RegularFile => FileType::File,
            Ext2FileType::SymbolicLink => FileType::SymLink,
            Ext2FileType::UnixSocket => FileType::Socket,
        }
    }
    pub fn from_file_type(file_type: &FileType) -> Result<Self, SystemError> {
        match file_type {
            FileType::Pipe => Ok(Ext2FileType::FIFO),
            FileType::CharDevice | FileType::FramebufferDevice | FileType::KvmDevice => {
                Ok(Ext2FileType::CharacterDevice)
            }
            FileType::Dir => Ok(Ext2FileType::Directory),
            FileType::BlockDevice => Ok(Ext2FileType::BlockDevice),
            FileType::File => Ok(Ext2FileType::RegularFile),
            FileType::SymLink => Ok(Ext2FileType::SymbolicLink),
            FileType::Socket => Ok(Ext2FileType::UnixSocket),
        }
    }
}
impl Into<u16> for Ext2FileType {
    fn into(self) -> u16 {
        match self {
            Ext2FileType::FIFO => 0x1000,
            Ext2FileType::CharacterDevice => 0x2000,
            Ext2FileType::Directory => 0x4000,
            Ext2FileType::BlockDevice => 0x6000,
            Ext2FileType::RegularFile => 0x8000,
            Ext2FileType::SymbolicLink => 0xA000,
            Ext2FileType::UnixSocket => 0xC000,
        }
    }
}

bitflags! {

    pub struct Ext2FileMode:u16 {
            /// 文件系统中的 FIFO（管道）
    const  FIFO = 0x1000;
        /// 字符设备
    const  CHARACTER_DEVICE = 0x2000;
        /// 目录
    const  DIRECTORY = 0x4000;
        /// 块设备
    const    BLOCK_DEVICE = 0x6000;
        /// 普通文件
    const   REGULAR_FILE = 0x8000;
        /// 符号链接
    const  SYMBOLIC_LINK = 0xA000;
        /// Unix 套接字
    const   UNIX_SOCKET = 0xC000;

        /// 文件所有者具有写权限
    const OX = 0x001;
    /// 文件所有者具有写权限
    const    OW = 0x002;
    /// 文件所有者具有写权限
    const    OR = 0x004;
    /// 文件组所有者具有写权限
    const   GX = 0x008;
    /// 文件组所有者具有写权限
    const    GW = 0x010;
    /// 文件组所有者具有写权限
    const   GR = 0x020;
    /// 文件所有者具有写权限
    const    UX = 0x040;
    /// 文件所有者具有写权限
    const    UW = 0x080;
    /// 文件所有者具有写权限
    const    UR = 0x100;
    /// 文件所有者具有写权限
    const    STICKY_BIT = 0x200;
    /// 文件所有者具有写权限
    const   SET_GROUP_ID = 0x400;
    /// 文件所有者具有写权限
    const   SET_USER_ID = 0x800;
    const OXRW  =Self::OX.bits() | Self::OR.bits()  | Self::OW.bits() ;
    const GXRW = Self::GX.bits() | Self::GR.bits() | Self::GW.bits() ;
    const UXRW = Self::UX.bits() | Self::UR.bits() | Self::UW.bits() ;

}

}

impl Ext2FileMode {
    pub fn type_from_mode(t: &u16) -> Result<Ext2FileType, SystemError> {
        Ext2FileType::type_from_mode(t)
    }
    pub fn file_type(&self) -> Result<Ext2FileType, SystemError> {
        Ext2FileType::type_from_mode(&self.bits())
    }
    pub fn convert_mode(mode: &u16) -> Result<ModeType, SystemError> {
        let mut mode_type = ModeType::empty();
        todo!()
    }
    pub fn from_common_type(mode: ModeType) -> Result<Self, SystemError> {
        // TODO 转换
        match Ext2FileMode::from_bits(mode.bits() as u16) {
            Some(v) => Ok(v),
            None => Err(SystemError::EINVAL),
        }
    }
    pub fn set_file_type(&mut self, file_type: Ext2FileType) {
        self.bits = self.bits() | file_type as u16;
    }
}

#[cfg(test)]
mod tests {
    use crate::filesystem::ext2fs::file_type::{Ext2FileMode, Ext2FileType};
    #[test]
    fn test_ext2_file_mode() {
        let mut mode = Ext2FileMode::from_bits(0o777).unwrap();
        mode.set_file_type(Ext2FileType::RegularFile);
        println!("{:?}", mode)
    }
}
