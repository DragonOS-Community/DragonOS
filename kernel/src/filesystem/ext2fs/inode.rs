use core::{
    cmp::min,
    fmt::Debug,
    mem::{self, transmute, ManuallyDrop},
};

use alloc::{
    fmt,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;
use uefi::data_types;

use super::{entry::Ext2DirEntry, file_type::Ext2FileType, fs::EXT2_SB_INFO};
use crate::{
    driver::base::block::{
        block_device::{__bytes_to_lba, LBA_SIZE},
        disk_info::Partition,
    },
    filesystem::{
        ext2fs::{ext2fs_instance, file_type, inode},
        vfs::{syscall::ModeType, FilePrivateData, FileSystem, FileType, IndexNode, Metadata},
    },
    libs::{rwlock::RwLock, spinlock::SpinLock, vec_cursor::VecCursor},
};

const EXT2_NDIR_BLOCKS: usize = 12;
const EXT2_DIND_BLOCK: usize = 13;
const EXT2_TIND_BLOCK: usize = 14;
const EXT2_BP_NUM: usize = 15;

#[derive(Debug)]
pub struct LockedExt2Inode(SpinLock<Ext2Inode>);

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

#[derive(Default, Clone)]
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
    pub _os_dependent_1: [u8; 4],
    /// 数组块指针
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
    pub _os_dependent_2: [u8; 12],
}
impl Ext2Inode {
    pub fn new() -> Self {
        Self {
            hard_link_num: 1,
            ..Default::default()
        }
    }
    pub fn new_from_bytes(data: &Vec<u8>) -> Result<Ext2Inode, SystemError> {
        let mut cursor = VecCursor::new(data.to_vec());

        let inode = Ext2Inode {
            mode: cursor.read_u16()?,
            uid: cursor.read_u16()?,
            lower_size: cursor.read_u32()?,
            access_time: cursor.read_u32()?,
            create_time: cursor.read_u32()?,
            modify_time: cursor.read_u32()?,
            delete_time: cursor.read_u32()?,
            gid: cursor.read_u16()?,
            hard_link_num: cursor.read_u16()?,
            disk_sector: cursor.read_u32()?,
            flags: cursor.read_u32()?,
            _os_dependent_1: {
                let mut data = [0u8; 4];
                cursor.read_exact(&mut data)?;
                data
            },
            blocks: {
                let mut data = [0u8; EXT2_BP_NUM * 4];
                cursor.read_exact(&mut data)?;
                let mut ret = [0u32; EXT2_BP_NUM];
                let mut start: usize = 0;
                for i in 0..EXT2_BP_NUM {
                    ret[i] = u32::from_le_bytes(data[start..start + 4].try_into().unwrap());
                    start += 4;
                }
                ret
            },
            generation_num: cursor.read_u32()?,
            file_acl: cursor.read_u32()?,
            directory_acl: cursor.read_u32()?,
            fragment_addr: cursor.read_u32()?,
            _os_dependent_2: {
                let mut data = [0u8; 12];
                cursor.read_exact(&mut data)?;
                data
            },
        };
        Ok(inode)
    }
    pub fn flush(&self) {
        // TODO 刷新磁盘中的inode
        todo!()
    }
}

impl Debug for Ext2Inode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Inode")
            .field("mode", &format_args!("{:?}\n", &self.mode))
            .field("uid", &format_args!("{:?}\n", &self.uid))
            .field("lower_size", &format_args!("{:?}\n", &self.lower_size))
            .field("access_time", &format_args!("{:?}\n", &self.access_time))
            .field("create_time", &format_args!("{:?}\n", &self.create_time))
            .field("modify_time", &format_args!("{:?}\n", &self.modify_time))
            .field("delete_time", &format_args!("{:?}\n", &self.delete_time))
            .field("gid", &format_args!("{:?}\n", &self.gid))
            .field(
                "hard_link_num",
                &format_args!("{:?}\n", &self.hard_link_num),
            )
            .field("disk_sector", &format_args!("{:?}\n", &self.disk_sector))
            .field("flags", &format_args!("{:?}\n", &self.flags))
            .field(
                "_os_dependent_1",
                &format_args!("{:?}\n", &self._os_dependent_1),
            )
            .field("blocks", &format_args!("{:?}\n", &self.blocks))
            .field(
                "generation_num",
                &format_args!("{:?}\n", &self.generation_num),
            )
            .field("file_acl", &format_args!("{:?}\n", &self.file_acl))
            .field(
                "directory_acl",
                &format_args!("{:?}\n", &self.directory_acl),
            )
            .field(
                "fragment_addr",
                &format_args!("{:?}\n", &self.fragment_addr),
            )
            .field(
                "_os_dependent_2",
                &format_args!("{:?}\n", &self._os_dependent_2),
            )
            .finish()
    }
}
impl LockedExt2Inode {
    // TODO EXT2_SB_INFO要改
    pub fn get_block_group(inode: usize) -> usize {
        let binding = EXT2_SB_INFO.read();
        let sb = binding.ext2_super_block.as_ref().unwrap();
        let inodes_per_group = sb.inodes_per_group;
        return ((inode as u32 - 1) / inodes_per_group) as usize;
    }

    pub fn get_index_in_group(inode: usize) -> usize {
        let binding = EXT2_SB_INFO.read();
        let sb = &binding.ext2_super_block.as_ref().unwrap();

        let inodes_per_group = sb.inodes_per_group;
        return ((inode as u32 - 1) % inodes_per_group) as usize;
    }

    pub fn get_block_addr(inode: usize) -> usize {
        let binding = EXT2_SB_INFO.read();
        let sb = &binding.ext2_super_block.as_ref().unwrap();
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
    // entry: Ext2DirEntry,
    // data: Vec<Option<Ext2Indirect>>,
    i_data: [u32; 15],
    meta: Metadata,
    // block_group: u32,
    mode: ModeType,
    file_type: FileType,
    i_mode: u16,
    inode: Ext2Inode, // file_size: u32,
                      // disk_sector: u32,
}

impl Ext2InodeInfo {
    pub fn new(inode: &Ext2Inode) -> Self {
        // kinfo!("begin Ext2InodeInfo new");
        let mode = inode.mode;
        let file_type = Ext2FileType::type_from_mode(&mode).unwrap().covert_type();
        // kinfo!("file_type = {:?}", file_type);

        // TODO 根据inode mode转换modetype
        let fs_mode = ModeType::from_bits_truncate(mode as u32);
        let meta = Metadata::new(file_type, fs_mode);
        // TODO 获取block group

        // TODO 间接地址
        // kinfo!("end Ext2InodeInfo new");

        Self {
            inode: inode.clone(),
            i_data: inode.blocks,
            i_mode: mode,
            meta,
            mode: fs_mode,
            file_type,
            // entry: todo!(),
        }
    }
    // TODO 更新当前inode的元数据
}

impl IndexNode for LockedExt2InodeInfo {
    fn find(&self, _name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        // kinfo!("begin LockedExt2InodeInfo find");
        let guard = self.0.lock();
        let inode = &guard.inode;
        if Ext2FileType::type_from_mode(&inode.mode).unwrap() != Ext2FileType::Directory {
            return Err(SystemError::ENOTDIR);
        }
        let size: usize = ((inode.directory_acl as usize) << 32usize) + inode.lower_size as usize;
        let mut data_block: Vec<u8> = Vec::with_capacity(size);
        data_block.resize(size, 0);
        drop(guard);
        self.read_at(
            0,
            size,
            data_block.as_mut_slice(),
            &mut FilePrivateData::Unused,
        )?;
        let mut begin_pos = 0;
        loop {
            if begin_pos >= size {
                break;
            }
            let inode_num =
                u32::from_le_bytes(data_block[begin_pos..begin_pos + 4].try_into().unwrap());
            if inode_num == 0 {
                break;
            }
            let name_pos = begin_pos + 8;
            begin_pos += mem::size_of::<u32>();
            let rc_len: u16 =
                u16::from_le_bytes(data_block[begin_pos..begin_pos + 2].try_into().unwrap());
            let name_len: u8 = u8::from_le(data_block[begin_pos + 2]);
            let name = String::from_utf8_lossy(&data_block[name_pos..name_pos + name_len as usize]);
            if name == _name {
                let ext2 = ext2fs_instance();
                let sb = ext2.sb_info.0.lock();
                let i = sb.read_inode(inode_num).unwrap();
                return Ok(Arc::new(LockedExt2InodeInfo(SpinLock::new(
                    Ext2InodeInfo::new(&i),
                ))));
            }
            begin_pos += rc_len as usize - mem::size_of::<u32>();
        }
        // kinfo!("end LockedExt2InodeInfo find");

        return Err(SystemError::EINVAL);
    }
    fn close(&self, _data: &mut FilePrivateData) -> Result<(), SystemError> {
        // kdebug!("close inode");
        Ok(())
    }
    fn open(
        &self,
        _data: &mut FilePrivateData,
        _mode: &crate::filesystem::vfs::file::FileMode,
    ) -> Result<(), SystemError> {
        // kdebug!("open inode");
        Ok(())
    }
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: &mut crate::filesystem::vfs::FilePrivateData,
    ) -> Result<usize, system_error::SystemError> {
        // TODO 需要根据不同的文件类型，选择不同的读取方式，将读的行为集成到file type
        // kinfo!("begin LockedExt2InodeInfo read_at");
        let inode_grade = self.0.lock();
        let binding = ext2fs_instance();
        let superb = binding.sb_info.0.lock();
        match inode_grade.file_type {
            FileType::File | FileType::Dir => {
                // kinfo!("i data ={:?}", inode_grade.i_data);/
                let inode = &inode_grade.inode;
                // 计算文件大小
                let file_size =
                    ((inode.directory_acl as usize) << 32usize) + inode.lower_size as usize;
                // kinfo!("offset = {offset}");
                if offset >= file_size {
                    return Ok(0usize);
                }
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
                // let mut end_len: usize = min(LBA_SIZE, buf.len());
                // kdebug!(
                //     "read_block_num:{read_block_num},buf_len:{},len:{}",
                //     buf.len(),
                //     len
                // );
                // 需要读取的字节大小
                let mut read_buf_size = len;
                if len % superb.s_block_size as usize != 0 {
                    read_buf_size +=
                        superb.s_block_size as usize - len % superb.s_block_size as usize;
                }
                // kinfo!("read_buf_size = {read_buf_size}");
                let mut read_buf: Vec<u8> = Vec::with_capacity(read_buf_size);
                read_buf.resize(read_buf_size, 0);
                // 读取直接块
                // kdebug!("read direct, start_block = {start_block}");
                while start_block <= 11 {
                    if inode_grade.i_data[start_block] == 0 {
                        // if inode_grade.inode.lower_size == 28 {
                        // kinfo!("{:?}", String::from_utf8(read_buf[..file_size].to_vec()).unwrap());
                        // }
                        // kinfo!("end LockedExt2InodeInforead_at");
                        buf.copy_from_slice(&read_buf[..buf.len()]);
                        return Ok(min(file_size, len));
                    }
                    let start_addr =
                        (inode_grade.i_data[start_block] * superb.s_block_size) as usize;

                    // 每次读一个块
                    let r: usize = superb.partition.as_ref().unwrap().disk().read_at(
                        __bytes_to_lba(start_addr, LBA_SIZE),
                        superb.s_block_size as usize / LBA_SIZE,
                        &mut read_buf[start_pos..start_pos + superb.s_block_size as usize],
                    )?;
                    // kinfo!("r={r},superb.s_block_size={}", superb.s_block_size);
                    already_read_block += 1;
                    start_block += 1;
                    already_read_byte += r;
                    start_pos += superb.s_block_size as usize;
                    // kdebug!(
                    //     "already_read_byte:{already_read_byte},start_pos:{start_pos},start_addr:{start_addr},lbaid:{},block_num:{}",
                    //     __bytes_to_lba(start_addr, LBA_SIZE),inode_grade.i_data[start_block],
                    // );
                }

                if already_read_block == read_block_num || inode_grade.i_data[12] == 0 {
                    buf.copy_from_slice(&read_buf[..len]);
                    kdebug!("end read direct,end LockedExt2InodeInfo read_at, start_block = {start_block}");
                    return Ok(min(file_size, len));
                }

                kdebug!("read indirect, start_block = {start_block}");

                // 读取一级间接块
                // 获取地址块
                let start_addr = (inode_grade.i_data[start_block] * superb.s_block_size) as usize;
                let mut address_block: Vec<u8> = Vec::with_capacity(superb.s_block_size as usize);
                address_block.resize(superb.s_block_size as usize, 0);
                let _ = superb.partition.as_ref().unwrap().disk().read_at(
                    __bytes_to_lba(start_addr, LBA_SIZE),
                    superb.s_block_size as usize / LBA_SIZE,
                    &mut address_block[..],
                );
                let mut address: Vec<u32> = Vec::with_capacity(address_block.len() / 4);
                address = unsafe { core::mem::transmute_copy(&address_block) };
                let ever_read_count = superb.s_block_size as usize / LBA_SIZE;

                // 读取数据块
                while already_read_block < read_block_num && start_block <= 127 + 12 {
                    if address[start_block - 12] == 0 {
                        // kinfo!("end LockedExt2InodeInfo read_at");
                        buf.copy_from_slice(&read_buf[..len]);
                        return Ok(min(file_size, len));
                    }
                    // 每次读一个块
                    let r: usize = superb.partition.clone().unwrap().disk().read_at(
                        //  address[start_block - 12]里面可能是块号
                        __bytes_to_lba(
                            address[start_block - 12] as usize * superb.s_block_size as usize,
                            LBA_SIZE,
                        ),
                        ever_read_count,
                        &mut read_buf[start_pos..start_pos + superb.s_block_size as usize],
                    )?;
                    already_read_block += 1;
                    start_block += 1;
                    already_read_byte += r;
                    start_pos += superb.s_block_size as usize;
                }

                if inode_grade.i_data[13] == 0 || already_read_block == read_block_num {
                    buf.copy_from_slice(&read_buf[..len]);
                    kdebug!("end read indirect,end LockedExt2InodeInfo read_at, start_block = {start_block}");
                    return Ok(min(file_size, len));
                }
                kdebug!("read secondly direct, start_block = {start_block}");

                // 读取二级间接块

                let start_addr = (inode_grade.i_data[13] * superb.s_block_size) as usize;
                let mut address_block: Vec<u8> = Vec::with_capacity(superb.s_block_size as usize);
                address_block.resize(superb.s_block_size as usize, 0);
                let _ = superb.partition.as_ref().unwrap().disk().read_at(
                    __bytes_to_lba(start_addr, LBA_SIZE),
                    superb.s_block_size as usize / LBA_SIZE,
                    &mut address_block[..],
                );
                let mut indir_block: Vec<u32> = Vec::with_capacity(address_block.len() / 4);
                indir_block = unsafe { core::mem::transmute_copy(&address_block) };

                for i in 0..128 {
                    // 根据二级间接块，获取读取间接块

                    let mut addr_data: Vec<u8> = Vec::with_capacity(superb.s_block_size as usize);
                    addr_data.resize(superb.s_block_size as usize, 0);
                    let _ = superb.partition.as_ref().unwrap().disk().read_at(
                        // indir block 里面可能也是块号
                        __bytes_to_lba(
                            indir_block[i] as usize * superb.s_block_size as usize,
                            LBA_SIZE,
                        ),
                        ever_read_count,
                        addr_data.as_mut_slice(),
                    );
                    let mut data_address: Vec<u32> = Vec::with_capacity(addr_data.len() / 4);
                    data_address = unsafe { core::mem::transmute_copy(&addr_data) };

                    for j in 0..128 {
                        if already_read_block == read_block_num {
                            buf.copy_from_slice(&read_buf[..len]);
                            return Ok(min(file_size, len));
                        }

                        let r = superb.partition.clone().unwrap().disk().read_at(
                            __bytes_to_lba(
                                data_address[j] as usize * superb.s_block_size as usize,
                                LBA_SIZE,
                            ),
                            ever_read_count,
                            &mut read_buf[start_pos..start_pos + superb.s_block_size as usize],
                        )?;

                        already_read_block += 1;
                        start_block += 1;
                        already_read_byte += r;
                        start_pos += superb.s_block_size as usize;
                    }
                }

                if inode_grade.i_data[14] == 0 || already_read_block == read_block_num {
                    kdebug!("end read secondly direct,end LockedExt2InodeInfo read_at, start_block = {start_block}");
                    buf.copy_from_slice(&read_buf[..len]);
                    return Ok(min(file_size, len));
                }
                kdebug!("read thirdly direct, start_block = {start_block}");

                // 读取三级间接块

                let start_addr = (inode_grade.i_data[14] * superb.s_block_size) as usize;
                let mut address_block: Vec<u8> = Vec::with_capacity(superb.s_block_size as usize);
                address_block.resize(superb.s_block_size as usize, 0);
                let _ = superb.partition.as_ref().unwrap().disk().read_at(
                    __bytes_to_lba(start_addr, LBA_SIZE),
                    superb.s_block_size as usize / LBA_SIZE,
                    &mut address_block[..],
                );
                let mut thdir_block: Vec<u32> = Vec::with_capacity(address_block.len() / 4);
                thdir_block = unsafe { core::mem::transmute_copy(&address_block) };

                for i in 0..128 {
                    // 根据二级间接块，获取读取间接块
                    // let indir_block = get_address_block(
                    //     superb.partition.clone().unwrap(),
                    //     thdir_block[i] as usize,
                    // );

                    let mut block: Vec<u8> = Vec::with_capacity(superb.s_block_size as usize);
                    block.resize(superb.s_block_size as usize, 0);
                    let _ = superb.partition.as_ref().unwrap().disk().read_at(
                        // indir block 里面可能也是块号
                        __bytes_to_lba(
                            thdir_block[i] as usize * superb.s_block_size as usize,
                            LBA_SIZE,
                        ),
                        ever_read_count,
                        &mut block[..],
                    );
                    let mut indir_block: Vec<u32> = Vec::with_capacity(block.len() / 4);
                    indir_block = unsafe { core::mem::transmute_copy(&block) };

                    for second in 0..128 {
                        let mut dir_data: Vec<u8> =
                            Vec::with_capacity(superb.s_block_size as usize);
                        dir_data.resize(superb.s_block_size as usize, 0);
                        let _ = superb.partition.as_ref().unwrap().disk().read_at(
                            // indir block 里面可能也是块号
                            __bytes_to_lba(
                                indir_block[second] as usize * superb.s_block_size as usize,
                                LBA_SIZE,
                            ),
                            ever_read_count,
                            &mut dir_data[..],
                        );
                        let mut dir_block: Vec<u32> = Vec::with_capacity(block.len() / 4);
                        dir_block = unsafe { core::mem::transmute_copy(&block) };

                        for j in 0..128 {
                            if already_read_block == read_block_num {
                                buf.copy_from_slice(&read_buf[..len]);
                                return Ok(min(file_size, len));
                            }

                            let r = superb.partition.as_ref().unwrap().disk().read_at(
                                __bytes_to_lba(
                                    dir_block[j] as usize * superb.s_block_size as usize,
                                    LBA_SIZE,
                                ),
                                ever_read_count,
                                &mut read_buf[start_pos..start_pos + superb.s_block_size as usize],
                            )?;
                            already_read_block += 1;
                            start_block += 1;
                            already_read_byte += r;
                            start_pos += superb.s_block_size as usize;
                        }
                    }
                }
                kdebug!(
                    "end read thirdly direct,end LockedExt2InodeInfo read_at, start_block = {start_block}"
                );
                buf.copy_from_slice(&read_buf[..len]);
                Ok(min(file_size, len))
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
        // TODO 根据不同类型文件写入数据
        match file_type {
            Ext2FileType::FIFO | Ext2FileType::Directory => {
                let mut start_block = offset / LBA_SIZE;
                todo!()
            }
            Ext2FileType::CharacterDevice => todo!(),
            Ext2FileType::BlockDevice => todo!(),
            Ext2FileType::RegularFile => todo!(),
            Ext2FileType::SymbolicLink => todo!(),
            Ext2FileType::UnixSocket => todo!(),
        }
        // TODO write_at
    }

    fn fs(&self) -> alloc::sync::Arc<dyn FileSystem> {
        ext2fs_instance()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, system_error::SystemError> {
        kdebug!("begin ext2 list");
        let guard = self.0.lock();
        let file_type = Ext2FileType::type_from_mode(&guard.i_mode);
        if file_type.is_err() {
            kerror!("{:?}", file_type.clone().err());
            return Err(SystemError::EINVAL);
        }
        let file_type = file_type.unwrap();
        kdebug!("file type = {file_type:?}");
        let mut names: Vec<String> = Vec::new();
        match file_type {
            Ext2FileType::Directory => {
                // 获取inode数据
                // // kinfo!("list inode : {:?}", guard.inode);
                let inode = &guard.inode;
                // 解析为entry数组
                let meta = &guard.meta;
                // BUG 获取文件大小失败。
                let size: usize =
                    ((inode.directory_acl as usize) << 32usize) + inode.lower_size as usize;
                // kinfo!("size = {size}");
                let mut data_block: Vec<u8> = Vec::with_capacity(size);
                data_block.resize(size, 0);
                drop(guard);
                kdebug!("enter read at");
                let _read_size = self.read_at(
                    0,
                    size,
                    data_block.as_mut_slice(),
                    &mut FilePrivateData::Unused,
                )?;
                // 遍历entry数组
                let mut begin_pos = 0;
                loop {
                    if begin_pos >= size {
                        break;
                    }
                    let inode_num = u32::from_le_bytes(
                        data_block[begin_pos..begin_pos + 4].try_into().unwrap(),
                    );
                    if inode_num == 0 {
                        break;
                    }
                    let name_pos = begin_pos + 8;
                    begin_pos += mem::size_of::<u32>();
                    let rc_len: u16 = u16::from_le_bytes(
                        data_block[begin_pos..begin_pos + 2].try_into().unwrap(),
                    );
                    let name_len: u8 = u8::from_le(data_block[begin_pos + 2]);
                    let name = String::from_utf8_lossy(
                        &data_block[name_pos..name_pos + name_len as usize],
                    );
                    // kinfo!("rc_len:{rc_len},name_len:{name_len},name_pos:{name_pos},name:{name}");
                    names.push(name.to_string());
                    begin_pos += rc_len as usize - mem::size_of::<u32>();
                }
                kdebug!("end ext2 list");

                // 将entry添加到ret中
                return Ok(names);
            }
            _ => {
                return Err(SystemError::ENOTDIR);
            }
        }
    }
    fn metadata(&self) -> Result<Metadata, SystemError> {
        return Ok(self.0.lock().meta.clone());
    }
    fn create(
        &self,
        name: &str,
        file_type: FileType,
        mode: ModeType,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        todo!()
    }
}

pub fn get_address_block(partition: Arc<Partition>, ptr: usize) -> [u32; 128] {
    // kinfo!("begin get address block");
    let mut address_block: [u8; 512] = [0; 512];
    let _ = partition
        .disk()
        .read_at(__bytes_to_lba(ptr, LBA_SIZE), 1, &mut address_block[0..]);
    let address: [u32; 128] = unsafe { mem::transmute::<[u8; 512], [u32; 128]>(address_block) };
    // kinfo!("end get address block");
    address
}
