use core::{
    cmp::min,
    fmt::Debug,
    mem::{self, transmute, ManuallyDrop},
};

use alloc::{
    string::String, sync::{Arc, Weak}, vec::Vec
};
use system_error::SystemError;

use super::{file_type::Ext2FileType, fs::EXT2_SB_INFO};
use crate::{
    driver::base::block::{block_device::LBA_SIZE, disk_info::Partition},
    filesystem::{
        ext2fs::file_type,
        vfs::{syscall::ModeType, FileSystem, FileType, IndexNode, Metadata},
    },
    libs::{rwlock::RwLock, spinlock::SpinLock},
};

const EXT2_NDIR_BLOCKS: usize = 12;
const EXT2_DIND_BLOCK: usize = 13;
const EXT2_TIND_BLOCK: usize = 14;
const EXT2_BP_NUM: usize = 15;

#[derive(Debug)]
pub struct LockedExt2Inode(SpinLock<Ext2Inode>);

/// inode中根据不同系统的保留值
#[repr(C, align(1))]
pub union OSD1 {
    linux_reserved: u32,
    hurd_tanslator: u32,
    masix_reserved: u32,
}
impl Debug for OSD1 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "OSD1:{}", unsafe { self.linux_reserved })
    }
}
#[derive(Debug)]
#[repr(C, align(1))]
struct MasixOsd2 {
    frag_num: u8,
    frag_size: u8,
    pad: u16,
    reserved: [u32; 2],
}
#[derive(Debug)]
#[repr(C, align(1))]
struct LinuxOsd2 {
    frag_num: u8,
    frag_size: u8,
    pad: u16,
    uid_high: u16,
    gid_high: u16,
    reserved: u32,
}

#[repr(C, align(1))]
#[derive(Debug)]
struct HurdOsd2 {
    frag_num: u8,
    frag_size: u8,
    mode_high: u16,
    uid_high: u16,
    gid_high: u16,
    author: u32,
}

/// inode中根据不同系统的保留值
pub union OSD2 {
    linux: ManuallyDrop<LinuxOsd2>,
    hurd: ManuallyDrop<HurdOsd2>,
    masix: ManuallyDrop<MasixOsd2>,
}
impl Debug for OSD2 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "OSD2:{:?}", unsafe { &self.linux })
    }
}
#[derive(Debug)]
#[repr(C, align(1))]
/// 磁盘中存储的inode
pub struct Ext2Inode {
    /// 文件类型和权限，高四位代表文件类型，其余代表权限
    pub mode: u16,
    /// 文件所有者
    pub uid: u16,
    /// 文件大小
    pub lower_size: u32,
    /// 文件访问时间
    pub access_time: u32,
    /// 文件创建时间
    pub create_time: u32,
    /// 文件修改时间
    pub modify_time: u32,
    /// 文件删除时间
    pub delete_time: u32,
    /// 文件组
    pub gid: u16,
    /// 文件链接数
    pub hard_link_num: u16,
    /// 文件在磁盘上的扇区
    pub disk_sector: u32,
    /// 文件属性
    pub flags: u32,
    /// 操作系统依赖
    pub os_dependent_1: OSD1,

    pub blocks: [u32; EXT2_BP_NUM],

    /// Generation number (Primarily used for NFS)
    pub generation_num: u32,

    /// In Ext2 version 0, this field is reserved.
    /// In version >= 1, Extended attribute block (File ACL).
    pub file_acl: u32,

    /// In Ext2 version 0, this field is reserved.
    /// In version >= 1, Upper 32 bits of file size (if feature bit set) if it's a file,
    /// Directory ACL if it's a directory
    pub directory_acl: u32,

    /// 片段地址
    pub fragment_addr: u32,
    /// 操作系统依赖
    pub os_dependent_2: OSD2,
}
impl Ext2Inode {
    // TODO 刷新磁盘中的inode
}

impl LockedExt2Inode {
    pub fn get_block_group(inode: usize) -> usize {
        let sb = &EXT2_SB_INFO.read().ext2_super_block.upgrade().unwrap();
        let inodes_per_group = sb.inodes_per_group;
        return ((inode as u32 - 1) / inodes_per_group) as usize;
    }

    pub fn get_index_in_group(inode: usize) -> usize {
        let sb = &EXT2_SB_INFO.read().ext2_super_block.upgrade().unwrap();

        let inodes_per_group = sb.inodes_per_group;
        return ((inode as u32 - 1) % inodes_per_group) as usize;
    }

    pub fn get_block_addr(inode: usize) -> usize {
        let sb = &EXT2_SB_INFO.read().ext2_super_block.upgrade().unwrap();
        let mut inode_size = sb.inode_size as usize;
        let block_size = sb.block_size as usize;

        if sb.major_version < 1 {
            inode_size = 128;
        }
        return (inode * inode_size) / block_size;
    }
}
#[derive(Debug)]
pub struct DataBlock {
    data: [u8; 4 * 1024],
}
pub struct LockedDataBlock(RwLock<DataBlock>);

#[derive(Debug, Default, Clone)]
pub(crate) struct Ext2Indirect {
    pub self_ref: Weak<Ext2Indirect>,
    pub next_point: Vec<Option<Arc<Ext2Indirect>>>,
    // TODO datablock应该改为block地址
    pub data_block: Option<u32>,
}
#[derive(Debug)]
pub struct LockedExt2InodeInfo(pub SpinLock<Ext2InodeInfo>);

#[derive(Debug)]
/// 存储在内存中的inode
pub struct Ext2InodeInfo {
    // TODO 将ext2iode内容和meta联系在一起，可自行设计
    // data: Vec<Option<Ext2Indirect>>,
    i_data: [u32; 15],
    meta: Metadata,
    // block_group: u32,
    mode: ModeType,
    file_type: FileType,
    i_mode: u16,
    // file_size: u32,
    // disk_sector: u32,
}

impl Ext2InodeInfo {
    pub fn new(inode: &Ext2Inode) -> Self {
        // let inode_grade = inode.0.lock();
        let mode = inode.mode;
        let file_type = Ext2FileType::type_from_mode(&mode).unwrap().covert_type();
        // TODO 根据inode mode转换modetype
        let fs_mode = ModeType::from_bits_truncate(0o755);
        let meta = Metadata::new(file_type, fs_mode);
        // TODO 获取block group
        let mut d: Vec<Option<Ext2Indirect>> = Vec::with_capacity(15);
        for i in 0..12 as usize {
            let mut idir = Ext2Indirect::default();
            idir.data_block = Some(inode.blocks[i]);
            idir.self_ref = Arc::downgrade(&Arc::new(idir.clone()));
            d[i] = Some(idir);
        }
        // TODO 间接地址
        Self {
            i_data: inode.blocks,
            i_mode: mode,
            meta,
            mode: fs_mode,
            file_type,
        }
    }
    // TODO 更新当前inode的元数据
}

impl IndexNode for LockedExt2InodeInfo {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: &mut crate::filesystem::vfs::FilePrivateData,
    ) -> Result<usize, system_error::SystemError> {
        let inode_grade = self.0.lock();
        let superb = EXT2_SB_INFO.read();
        // TODO 需要根据不同的文件类型，选择不同的读取方式，将读的行为集成到file type
        match inode_grade.file_type {
            FileType::File => {
                // TODO 判断是否有空指针
                // 起始读取块
                let mut start_block = offset / LBA_SIZE;
                // 需要读取的块
                let read_block_num = min(len, buf.len()) / LBA_SIZE + 1;
                // 已经读的块数
                let mut already_read_block: usize = 0;
                // 已经读的字节
                let mut already_read_byte: usize = 0;
                let mut start_pos: usize = 0;
                // 读取的字节
                let mut end_len: usize = min(LBA_SIZE, buf.len());
                // 读取直接块
                while already_read_block < read_block_num && start_block <= 11 {
                    // 每次读一个块
                    let r: usize = superb.partition.upgrade().unwrap().disk().read_at(
                        inode_grade.i_data[start_block] as usize,
                        1,
                        &mut buf[start_pos..start_pos + end_len],
                    )?;
                    already_read_block += 1;
                    start_block += 1;
                    already_read_byte += r;
                    start_pos += end_len;
                    end_len = min(buf.len() - already_read_byte, LBA_SIZE);
                }

                if already_read_block == read_block_num {
                    return Ok(already_read_byte);
                }

                // 读取一级间接块
                // 获取地址块
                let mut address_block: [u8; 512] = [0; 512];
                let _ = superb.partition.upgrade().unwrap().disk().read_at(
                    inode_grade.i_data[12] as usize,
                    1,
                    &mut address_block[0..],
                );
                let address: [u32; 128] =
                    unsafe { mem::transmute::<[u8; 512], [u32; 128]>(address_block) };

                // 读取数据块
                while already_read_block < read_block_num && start_block <= 127 + 12 {
                    // 每次读一个块
                    let r: usize = superb.partition.upgrade().unwrap().disk().read_at(
                        address[start_block - 12] as usize,
                        1,
                        &mut buf[start_pos..start_pos + end_len],
                    )?;
                    already_read_block += 1;
                    start_block += 1;
                    already_read_byte += r;
                    start_pos += end_len;
                    end_len = min(buf.len() - already_read_byte, LBA_SIZE);
                }

                if already_read_block == read_block_num {
                    return Ok(already_read_byte);
                }

                // FIXME partition clone一下，升级成arc之后一直clone用
                // 读取二级间接块
                let indir_block = get_address_block(
                    superb.partition.upgrade().unwrap(),
                    inode_grade.i_data[13] as usize,
                );

                for i in 0..128 {
                    // 根据二级间接块，获取读取间接块
                    let address = get_address_block(
                        superb.partition.upgrade().unwrap(),
                        indir_block[i] as usize,
                    );
                    for j in 0..128 {
                        if already_read_block == read_block_num {
                            return Ok(already_read_byte);
                        }

                        let r = superb.partition.upgrade().unwrap().disk().read_at(
                            address[j] as usize,
                            1,
                            &mut buf[start_pos..start_pos + end_len],
                        )?;
                        already_read_block += 1;
                        start_block += 1;
                        already_read_byte += r;
                        start_pos += end_len;
                        end_len = min(buf.len() - already_read_byte, LBA_SIZE);
                    }
                }

                // 读取三级间接块
                let thdir_block = get_address_block(
                    superb.partition.upgrade().unwrap(),
                    inode_grade.i_data[14] as usize,
                );

                for i in 0..128 {
                    // 根据二级间接块，获取读取间接块
                    let indir_block = get_address_block(
                        superb.partition.upgrade().unwrap(),
                        thdir_block[i] as usize,
                    );
                    for second in 0..128 {
                        let address = get_address_block(
                            superb.partition.upgrade().unwrap(),
                            indir_block[second] as usize,
                        );
                        for j in 0..128 {
                            if already_read_block == read_block_num {
                                return Ok(already_read_byte);
                            }

                            let r = superb.partition.upgrade().unwrap().disk().read_at(
                                address[j] as usize,
                                1,
                                &mut buf[start_pos..start_pos + end_len],
                            )?;
                            already_read_block += 1;
                            start_block += 1;
                            already_read_byte += r;
                            start_pos += end_len;
                            end_len = min(buf.len() - already_read_byte, LBA_SIZE);
                        }
                    }
                }

                Ok(already_read_byte)
            }
            FileType::Dir => {
                // TODO 读取目录
                unimplemented!()
            }
            _ => Err(SystemError::EINVAL),
        }
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: &mut crate::filesystem::vfs::FilePrivateData,
    ) -> Result<usize, system_error::SystemError> {
        let inode_grade = self.0.lock();
        let superb = EXT2_SB_INFO.read();
        // 判断inode的文件类型
        let file_type = Ext2FileType::type_from_mode(&inode_grade.i_mode);
        if file_type.is_err() {
            return Err(SystemError::EINVAL);
        }
        let file_type = file_type.unwrap();
        // TODO 根据不用类型文件写入数据
        match file_type {
            Ext2FileType::FIFO => todo!(),
            Ext2FileType::CharacterDevice => todo!(),
            Ext2FileType::Directory => todo!(),
            Ext2FileType::BlockDevice => todo!(),
            Ext2FileType::RegularFile => todo!(),
            Ext2FileType::SymbolicLink => todo!(),
            Ext2FileType::UnixSocket => todo!(),
        }
        // TODO write_at
        todo!()
    }

    fn fs(&self) -> alloc::sync::Arc<dyn FileSystem> {
        todo!()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, system_error::SystemError> {
        let guard = self.0.lock();
        let file_type = Ext2FileType::type_from_mode(&guard.i_mode);
        if file_type.is_err() {
            kerror!("{:?}", file_type.clone().err());
            return Err(SystemError::EINVAL);
        }
        let file_type = file_type.unwrap();
        match file_type {
            Ext2FileType::Directory=> {
                let mut ret: Vec<String> = Vec::new();
                // TODO 列出目录项
                // 获取inode数据块

                // 解析为entry数组

                // 遍历entry数组

                // 将entry添加到ret中
                return Ok(ret);
            }
            _ => {
                return Err(SystemError::ENOTDIR);
            }
        }
    }
    fn metadata(&self) -> Result<Metadata, SystemError> {
        return Ok(self.0.lock().meta.clone());
    }
}

pub fn get_address_block(partition: Arc<Partition>, ptr: usize) -> [u32; 128] {
    let mut address_block: [u8; 512] = [0; 512];
    let _ = partition.disk().read_at(ptr, 1, &mut address_block[0..]);
    let address: [u32; 128] = unsafe { mem::transmute::<[u8; 512], [u32; 128]>(address_block) };
    address
}
