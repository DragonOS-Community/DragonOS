use core::{
    cmp::min,
    fmt::Debug,
    mem::{self, transmute, ManuallyDrop},
};

use super::{
    entry::Ext2DirEntry,
    file_type::{Ext2FileMode, Ext2FileType},
    fs::EXT2_SB_INFO,
};
use crate::{
    driver::base::block::{
        block_device::{__bytes_to_lba, LBA_SIZE},
        disk_info::Partition,
    },
    filesystem::{
        ext2fs::{block_group_desc::Ext2BlockGroupDescriptor, ext2fs_instance, file_type, inode},
        vfs::{syscall::ModeType, FilePrivateData, FileSystem, FileType, IndexNode, Metadata},
    },
    libs::{
        rwlock::RwLock,
        spinlock::{SpinLock, SpinLockGuard},
        vec_cursor::VecCursor,
    },
    time::PosixTimeSpec,
};
use alloc::{
    fmt,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use elf::endian::LittleEndian;
use log::{debug, error, info};
use system_error::SystemError;
use uefi::data_types;

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
    pub fn new(mode: u16) -> Self {
        let now = PosixTimeSpec::now().tv_sec as u32;
        Self {
            hard_link_num: 1,
            access_time: now,
            create_time: now,
            modify_time: now,
            mode,

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
    pub fn to_bytes(&self) -> Vec<u8> {
        // TODO 优化
        let mut data = Vec::with_capacity(mem::size_of::<Ext2Inode>());
        data.extend_from_slice(&self.mode.to_le_bytes());
        data.extend_from_slice(&self.uid.to_le_bytes());
        data.extend_from_slice(&self.lower_size.to_le_bytes());
        data.extend_from_slice(&self.access_time.to_le_bytes());
        data.extend_from_slice(&self.create_time.to_le_bytes());
        data.extend_from_slice(&self.modify_time.to_le_bytes());
        data.extend_from_slice(&self.delete_time.to_le_bytes());
        data.extend_from_slice(&self.gid.to_le_bytes());
        data.extend_from_slice(&self.hard_link_num.to_le_bytes());
        data.extend_from_slice(&self.disk_sector.to_le_bytes());
        data.extend_from_slice(&self.flags.to_le_bytes());
        data.extend_from_slice(&self._os_dependent_1);
        for i in self.blocks.iter() {
            data.extend_from_slice(&i.to_le_bytes());
        }
        data.extend_from_slice(&self.generation_num.to_le_bytes());
        data.extend_from_slice(&self.file_acl.to_le_bytes());
        data.extend_from_slice(&self.directory_acl.to_le_bytes());
        data.extend_from_slice(&self.fragment_addr.to_le_bytes());
        data.extend_from_slice(&self._os_dependent_2);
        data
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
    inode: Ext2Inode,
    // file_size: u32,
    // disk_sector: u32,
    inode_num: u32,
}

impl Ext2InodeInfo {
    pub fn new(inode: &Ext2Inode, inode_num: u32) -> Self {
        // info!("begin Ext2InodeInfo new");
        let mode = inode.mode;
        let file_type = Ext2FileType::type_from_mode(&mode).unwrap().covert_type();
        // info!("file_type = {:?}", file_type);

        // TODO 根据inode mode转换modetype
        let fs_mode = ModeType::from_bits_truncate(mode as u32);
        let mut meta = Metadata::new(file_type, fs_mode);
        // TODO 获取block group

        // TODO 间接地址
        // info!("end Ext2InodeInfo new");

        Self {
            inode: inode.clone(),
            i_data: inode.blocks,
            i_mode: mode,
            meta,
            mode: fs_mode,
            file_type,
            inode_num,
            // entry: todo!(),
        }
    }
    // TODO 更新当前inode的元数据
}

impl IndexNode for LockedExt2InodeInfo {
    fn find(&self, _name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        info!("begin LockedExt2InodeInfo find");
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
            SpinLock::new(FilePrivateData::Unused).lock(),
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
                    Ext2InodeInfo::new(&i, inode_num),
                ))));
            }
            begin_pos += rc_len as usize - mem::size_of::<u32>();
        }
        info!("end LockedExt2InodeInfo find");
        // return self.create_with_data(_name, FileType::File, ModeType::all(), 0);

        return Err(SystemError::ENOENT);
    }
    fn close(&self, _data: SpinLockGuard<'_, FilePrivateData>) -> Result<(), SystemError> {
        debug!("close inode");
        Ok(())
    }
    fn open(
        &self,
        _data: SpinLockGuard<'_, FilePrivateData>,
        _mode: &crate::filesystem::vfs::file::FileMode,
    ) -> Result<(), SystemError> {
        debug!("open inode");
        Ok(())
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: SpinLockGuard<'_, FilePrivateData>,
    ) -> Result<usize, system_error::SystemError> {
        // TODO 需要根据不同的文件类型，选择不同的读取方式，将读的行为集成到file type
        // info!("begin LockedExt2InodeInfo read_at");
        let inode_grade = self.0.lock();
        let binding = ext2fs_instance();
        let superb = binding.sb_info.0.lock();
        match inode_grade.file_type {
            FileType::File | FileType::Dir => {
                // info!("i data ={:?}", inode_grade.i_data);/
                let inode = &inode_grade.inode;
                // 计算文件大小
                let file_size =
                    ((inode.directory_acl as usize) << 32usize) + inode.lower_size as usize;
                // info!("offset = {offset}");
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
                // debug!(
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
                // info!("read_buf_size = {read_buf_size}");
                let mut read_buf: Vec<u8> = Vec::with_capacity(read_buf_size);
                read_buf.resize(read_buf_size, 0);
                // 读取直接块
                // debug!("read direct, start_block = {start_block}");
                while start_block <= 11 {
                    if inode_grade.i_data[start_block] == 0 {
                        // TODO 修改拷贝的起点为 offset % block_size
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
                    // info!("r={r},superb.s_block_size={}", superb.s_block_size);
                    already_read_block += 1;
                    start_block += 1;
                    already_read_byte += r;
                    start_pos += superb.s_block_size as usize;
                    // debug!(
                    //     "already_read_byte:{already_read_byte},start_pos:{start_pos},start_addr:{start_addr},lbaid:{},block_num:{}",
                    //     __bytes_to_lba(start_addr, LBA_SIZE),inode_grade.i_data[start_block],
                    // );
                }

                if already_read_block == read_block_num || inode_grade.i_data[12] == 0 {
                    buf.copy_from_slice(&read_buf[..len]);
                    debug!("end read direct,end LockedExt2InodeInfo read_at, start_block = {start_block}");
                    return Ok(min(file_size, len));
                }

                debug!("read indirect, start_block = {start_block}");

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
                        // info!("end LockedExt2InodeInfo read_at");
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
                    debug!("end read indirect,end LockedExt2InodeInfo read_at, start_block = {start_block}");
                    return Ok(min(file_size, len));
                }
                debug!("read secondly direct, start_block = {start_block}");

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
                    debug!("end read secondly direct,end LockedExt2InodeInfo read_at, start_block = {start_block}");
                    buf.copy_from_slice(&read_buf[..len]);
                    return Ok(min(file_size, len));
                }
                debug!("read thirdly direct, start_block = {start_block}");

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
                debug!(
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
        _data: SpinLockGuard<'_, FilePrivateData>,
    ) -> Result<usize, system_error::SystemError> {
        let inode_grade = self.0.lock();
        // 判断inode的文件类型
        let file_type = Ext2FileType::type_from_mode(&inode_grade.i_mode);
        if file_type.is_err() {
            return Err(SystemError::EINVAL);
        }
        let file_type = file_type.unwrap();
        // TODO 根据不同类型文件写入数据
        match file_type {
            Ext2FileType::FIFO | Ext2FileType::Directory => {
                let sb = ext2fs_instance().super_block();
                let super_block = sb.0.lock();
                let partition = ext2fs_instance().partition.clone();
                let block_size = super_block.s_block_size as usize;

                let mut block_offset = offset / block_size;
                let inode = &inode_grade.inode;
                let mut inode_clone = inode.clone();
                let group_id = (inode_grade.inode_num / super_block.s_inodes_per_group) as usize;
                let group_desc = &super_block.group_desc_table.as_ref().unwrap()[group_id].clone();

                let mut block_id = 0usize;
                let id_per_block = block_size / mem::size_of::<u32>();

                // 读block bitmap
                let count = (super_block.s_blocks_per_group as usize / 8) / block_size;
                let mut bitmap_buf: Vec<u8> = Vec::with_capacity(count * block_size);
                bitmap_buf.resize(count * block_size, 0);
                let _ = partition.disk().read_at(
                    group_desc.block_bitmap_address as usize,
                    count * (block_size / LBA_SIZE),
                    bitmap_buf.as_mut_slice(),
                );

                let mut start_buf_offset = 0usize;
                let mut inode_flush = false;
                // 通过file size 判断是否要分配新的块
                while start_buf_offset < len {
                    // TODO 如果是要分配新块，就不找块号。直接找bitmap，写块。

                    // TODO 判断block_offset对应的块存不存在，如果不存在就分配
                    let write_len = min(block_size, len);

                    // 找到起始要插入的块
                    if block_offset < 12 {
                        block_id = inode.blocks[block_offset] as usize;
                        if block_id == 0 {
                            inode_flush = true;
                            // TODO 分配新块，将新块id写到inode中
                            let new_block = group_desc.alloc_one_block(
                                bitmap_buf.as_slice(),
                                group_id,
                                block_size / mem::size_of::<Ext2BlockGroupDescriptor>(),
                            );
                            inode_clone.blocks[block_offset] = new_block as u32;
                            block_id = new_block;
                        }
                        let count = if write_len % LBA_SIZE != 0 {
                            write_len / LBA_SIZE + 1
                        } else {
                            write_len / LBA_SIZE
                        };
                        partition.disk().write_at(
                            block_id * block_size / LBA_SIZE,
                            count,
                            &buf[start_buf_offset..start_buf_offset + write_len],
                        )?;
                    } else if block_offset < id_per_block + 12 {
                        // 一级间接
                        let mut id = inode.blocks[12] as usize;
                        if id == 0 {
                            // TODO 分配新块 作为地址块 并修改id 将id写到inode中
                            let new_block = group_desc.alloc_one_block(
                                bitmap_buf.as_slice(),
                                group_id,
                                block_size / mem::size_of::<Ext2BlockGroupDescriptor>(),
                            );
                            inode_clone.blocks[12] = new_block as u32;
                            id = new_block;
                        }
                        let mut address_block: Vec<u8> = Vec::with_capacity(block_size);
                        address_block.resize(block_size, 0);
                        let _ = partition.disk().read_at(
                            id * block_size / LBA_SIZE,
                            block_size / LBA_SIZE,
                            &mut address_block[..],
                        );
                        let mut address_block_data: Vec<u32> = Vec::with_capacity(block_size / 4);
                        address_block_data = unsafe { core::mem::transmute_copy(&address_block) };
                        block_id = address_block_data[block_offset - 12] as usize;
                        if block_id == 0 {
                            // TODO 分配新块 将新块id写到address_block中
                            let new_block = group_desc.alloc_one_block(
                                bitmap_buf.as_slice(),
                                group_id,
                                block_size / mem::size_of::<Ext2BlockGroupDescriptor>(),
                            );
                            // TODO 将数据写到新块中
                            let count = if write_len % LBA_SIZE != 0 {
                                write_len / LBA_SIZE + 1
                            } else {
                                write_len / LBA_SIZE
                            };
                            partition.disk().write_at(
                                new_block * block_size / LBA_SIZE,
                                count,
                                &buf[start_buf_offset..start_buf_offset + write_len],
                            )?;
                            // TODO address_block写回磁盘
                            address_block_data[block_offset - 12] = new_block as u32;
                            partition.disk().write_at(
                                id * block_size / LBA_SIZE,
                                block_size / LBA_SIZE,
                                unsafe { core::mem::transmute(address_block_data.as_slice()) },
                            )?;
                        }
                    } else if block_offset < id_per_block.pow(2) + 12 {
                        // 二级间接
                        let mut id = inode.blocks[13] as usize;
                        if id == 0 {
                            // TODO 分配新块 作为地址块 并修改id 将id写到inode中
                            let new_block = group_desc.alloc_one_block(
                                bitmap_buf.as_slice(),
                                group_id,
                                block_size / mem::size_of::<Ext2BlockGroupDescriptor>(),
                            );
                            inode_clone.blocks[13] = new_block as u32;
                            id = new_block;
                        }
                        let mut address_block: Vec<u8> = Vec::with_capacity(block_size);
                        address_block.resize(block_size, 0);
                        let _ = partition.disk().read_at(
                            id * block_size / LBA_SIZE,
                            block_size / LBA_SIZE,
                            &mut address_block[..],
                        );
                        let mut address_block_data: Vec<u32> = Vec::with_capacity(block_size / 4);
                        address_block_data = unsafe { core::mem::transmute_copy(&address_block) };
                        // BUG 应该除 id_per_block的平方
                        let id = address_block_data
                            [(block_offset - id_per_block - 12) / id_per_block]
                            as usize;
                        if id == 0 {
                            // TODO 分配新块 将新块id写到address_block中
                            let new_block = group_desc.alloc_one_block(
                                bitmap_buf.as_slice(),
                                group_id,
                                block_size / mem::size_of::<Ext2BlockGroupDescriptor>(),
                            );
                            address_block_data[(block_offset - id_per_block - 12) / id_per_block] =
                                new_block as u32;
                            partition.disk().write_at(
                                id * block_size / LBA_SIZE,
                                block_size / LBA_SIZE,
                                unsafe { core::mem::transmute(address_block_data.as_slice()) },
                            )?;
                        }
                        address_block.clear();
                        address_block.resize(block_size, 0);
                        let _ = partition.disk().read_at(
                            id * block_size / LBA_SIZE,
                            block_size / LBA_SIZE,
                            &mut address_block[..],
                        );
                        address_block_data.clear();
                        address_block_data = unsafe { core::mem::transmute_copy(&address_block) };
                        let id_in_block = block_offset - id_per_block - 12;
                        block_id = address_block_data[id_in_block] as usize;
                        if block_id == 0 {
                            // TODO 分配新块 将新块id写到address_block中
                            let new_block = group_desc.alloc_one_block(
                                bitmap_buf.as_slice(),
                                group_id,
                                block_size / mem::size_of::<Ext2BlockGroupDescriptor>(),
                            );
                            // TODO address_block写回磁盘
                            let count = if write_len % LBA_SIZE != 0 {
                                write_len / LBA_SIZE + 1
                            } else {
                                write_len / LBA_SIZE
                            };
                            partition.disk().write_at(
                                new_block * block_size / LBA_SIZE,
                                count,
                                &buf[start_buf_offset..start_buf_offset + write_len],
                            )?;

                            // TODO address_block写回磁盘
                            address_block_data[(block_offset - id_per_block - 12) / id_per_block] =
                                new_block as u32;
                            partition.disk().write_at(
                                id * block_size / LBA_SIZE,
                                block_size / LBA_SIZE,
                                unsafe { core::mem::transmute(address_block_data.as_slice()) },
                            )?;
                        }
                    } else {
                        // 三级间接
                        let mut id = inode.blocks[14] as usize;
                        if id == 0 {
                            // TODO 分配新块 作为地址块 并修改id 将id写到inode中
                            let new_block = group_desc.alloc_one_block(
                                bitmap_buf.as_slice(),
                                group_id,
                                block_size / mem::size_of::<Ext2BlockGroupDescriptor>(),
                            );
                            inode_clone.blocks[14] = new_block as u32;
                            id = new_block;
                        }
                        let mut address_block: Vec<u8> = Vec::with_capacity(block_size);
                        address_block.resize(block_size, 0);
                        let _ = partition.disk().read_at(
                            id * block_size / LBA_SIZE,
                            block_size / LBA_SIZE,
                            &mut address_block[..],
                        );
                        let mut address_block_data: Vec<u32> = Vec::with_capacity(block_size / 4);
                        address_block_data = unsafe { core::mem::transmute_copy(&address_block) };
                        let id = address_block_data[(block_offset
                            - id_per_block
                            - 12
                            - id_per_block.pow(2))
                            / id_per_block.pow(2)] as usize;
                        if id == 0 {
                            // TODO 分配新块 将新块id写到address_block中
                            let new_block = group_desc.alloc_one_block(
                                bitmap_buf.as_slice(),
                                group_id,
                                block_size / mem::size_of::<Ext2BlockGroupDescriptor>(),
                            );
                            // TODO address_block写回磁盘

                            address_block_data[(block_offset
                                - id_per_block
                                - 12
                                - id_per_block.pow(2))
                                / id_per_block.pow(2)] = new_block as u32;

                            partition.disk().write_at(
                                id * block_size / LBA_SIZE,
                                block_size / LBA_SIZE,
                                unsafe { core::mem::transmute(address_block_data.as_slice()) },
                            )?;
                        }
                        address_block.clear();
                        address_block.resize(block_size, 0);
                        let _ = partition.disk().read_at(
                            id * block_size / LBA_SIZE,
                            block_size / LBA_SIZE,
                            &mut address_block[..],
                        );
                        address_block_data.clear();
                        address_block_data = unsafe { core::mem::transmute_copy(&address_block) };
                        let id = address_block_data[(block_offset
                            - id_per_block
                            - 12
                            - id_per_block.pow(2))
                            / id_per_block] as usize;
                        if id == 0 {
                            // TODO 分配新块 将新块id写到address_block中
                            let new_block = group_desc.alloc_one_block(
                                bitmap_buf.as_slice(),
                                group_id,
                                block_size / mem::size_of::<Ext2BlockGroupDescriptor>(),
                            );
                            // TODO address_block写回磁盘
                            address_block_data[(block_offset
                                - id_per_block
                                - 12
                                - id_per_block.pow(2))
                                / id_per_block] = new_block as u32;
                            partition.disk().write_at(
                                id * block_size / LBA_SIZE,
                                block_size / LBA_SIZE,
                                unsafe { core::mem::transmute(address_block_data.as_slice()) },
                            )?;
                        }
                        address_block.clear();
                        address_block.resize(block_size, 0);
                        let _ = partition.disk().read_at(
                            id * block_size / LBA_SIZE,
                            block_size / LBA_SIZE,
                            &mut address_block[..],
                        );
                        address_block_data.clear();
                        address_block_data = unsafe { core::mem::transmute_copy(&address_block) };
                        block_id = address_block_data
                            [block_offset - id_per_block - 12 - id_per_block.pow(2)]
                            as usize;
                        if block_id == 0 {
                            // TODO 分配新块 将新块id写到address_block中
                            let new_block = group_desc.alloc_one_block(
                                bitmap_buf.as_slice(),
                                group_id,
                                block_size / mem::size_of::<Ext2BlockGroupDescriptor>(),
                            );
                            // BUG 将数据写到新块中
                            let count = if write_len % LBA_SIZE != 0 {
                                write_len / LBA_SIZE + 1
                            } else {
                                write_len / LBA_SIZE
                            };
                            partition.disk().write_at(
                                new_block * block_size / LBA_SIZE,
                                count,
                                &buf[start_buf_offset..start_buf_offset + write_len],
                            )?;

                            address_block_data
                                [block_offset - id_per_block - 12 - id_per_block.pow(2)] =
                                new_block as u32;
                            partition.disk().write_at(
                                id * block_size / LBA_SIZE,
                                block_size / LBA_SIZE,
                                unsafe { core::mem::transmute(address_block_data.as_slice()) },
                            )?;
                            // TODO address_block写回磁盘
                        }
                    }
                    // // TODO 写一个块 更新offset

                    // // TODO 写块：1. 写直接块
                    block_offset += 1;
                    start_buf_offset += write_len;
                }
                if inode_flush {
                    // 写inode表
                    let mut table_num = group_desc.inode_table_start as usize;
                    let inode_per_block = block_size / mem::size_of::<Ext2BlockGroupDescriptor>();
                    let inode_id = inode_grade.inode_num as usize;
                    let block_offset = (inode_id - 1) / inode_per_block;
                    let inode_offset = (inode_id - 1) % inode_per_block;
                    table_num += block_offset;
                    let mut table_data: Vec<u8> = Vec::with_capacity(block_size);
                    table_data.resize(block_size, 0);
                    partition.disk().read_at(
                        table_num * block_size / LBA_SIZE,
                        block_size / LBA_SIZE,
                        &mut table_data,
                    )?;
                    let inode_byte = inode_clone.to_bytes();
                    // table_data[inode_offset + mem::size_of::<Ext2Inode>()
                    //     ..inode_offset + mem::size_of::<Ext2Inode>()] = inode_byte;
                    table_data.splice(
                        inode_offset * mem::size_of::<Ext2Inode>()
                            ..inode_offset * mem::size_of::<Ext2Inode>()
                                + mem::size_of::<Ext2Inode>(),
                        inode_byte,
                    );

                    partition.disk().write_at(
                        table_num * block_size / LBA_SIZE,
                        block_size / LBA_SIZE,
                        &mut table_data,
                    )?;
                }
                return Ok(start_buf_offset);
                // let start_block_id = offset
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
        debug!("begin ext2 list");
        let guard = self.0.lock();
        let file_type = Ext2FileType::type_from_mode(&guard.i_mode);
        if file_type.is_err() {
            error!("{:?}", file_type.clone().err());
            return Err(SystemError::EINVAL);
        }
        let file_type = file_type.unwrap();
        debug!("file type = {file_type:?}");
        let mut names: Vec<String> = Vec::new();
        match file_type {
            Ext2FileType::Directory => {
                // 获取inode数据
                // info!("list inode : {:?}", guard.inode);
                let inode = &guard.inode;
                // 解析为entry数组
                let meta = &guard.meta;
                // BUG 获取文件大小失败。
                let size: usize =
                    ((inode.directory_acl as usize) << 32usize) + inode.lower_size as usize;
                // info!("size = {size}");
                let mut data_block: Vec<u8> = Vec::with_capacity(size);
                data_block.resize(size, 0);
                drop(guard);
                debug!("enter read at");
                let _read_size = self.read_at(
                    0,
                    size,
                    data_block.as_mut_slice(),
                    SpinLock::new(FilePrivateData::Unused).lock(),
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
                    // info!("rc_len:{rc_len},name_len:{name_len},name_pos:{name_pos},name:{name}");
                    names.push(name.to_string());
                    begin_pos += rc_len as usize - mem::size_of::<u32>();
                }
                debug!("end ext2 list");

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
    fn create_with_data(
        &self,
        _name: &str,
        _file_type: FileType,
        _mode: ModeType,
        _data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if _data == 0 {
            debug!("create_with_data to create");
            return self.create(_name, _file_type, _mode);
        }
        return Err(SystemError::EINVAL);
    }
    fn create(
        &self,
        name: &str,
        file_type: FileType,
        mode: ModeType,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        debug!("begin ext2 create");
        let guard = self.0.lock();

        let ext2fs = ext2fs_instance();
        let sb: SpinLockGuard<super::fs::Ext2SuperBlockInfo> = ext2fs.sb_info.0.lock();
        let group_id = (guard.inode_num - 1) / sb.s_inodes_per_group;
        // let inode_index = guard.inode_num % sb.s_inodes_per_group;
        let block_size = sb.s_block_size as usize;

        let mut descriptor = &sb.group_desc_table.as_ref().unwrap()[group_id as usize];

        let i_bitmap = (descriptor.inode_bitmap_address as usize * block_size) / LBA_SIZE;
        // 读inode bitmap
        let bitmap_count =
            (((sb.s_inodes_per_group as usize / 8) / block_size) + 1) * (block_size / LBA_SIZE);
        let mut bitmap_buf: Vec<u8> = Vec::with_capacity(bitmap_count * LBA_SIZE);
        bitmap_buf.resize(bitmap_count * LBA_SIZE, 0);
        debug!("get bitmap");

        let _ = ext2fs_instance().partition.disk().read_at(
            i_bitmap,
            bitmap_count,
            bitmap_buf.as_mut_slice(),
        );

        // 获取新的inode index
        let mut bpos = 0usize;
        let mut new_bm = 0u8;
        let mut new_inode_index = group_id as usize * sb.s_inodes_per_group as usize;
        let mut index_offset = 0usize;
        debug!("alloc ext2 inode");

        for (p, i) in bitmap_buf.iter().enumerate() {
            if i == &0xFFu8 {
                if i & 0xF0 == 0xF0 {
                    // 1111 0000
                    let mut mask = 0b1000_0000u8;
                    for j in 0..4 {
                        if i & mask == 0 {
                            new_inode_index += p * 8 + j;
                            index_offset = p * 8 + j;
                            bpos = p;
                            new_bm = i | mask;
                            break;
                        }
                        mask >>= 1;
                    }
                } else if i & 0x0F == 0x0F {
                    // 0000 1111
                    let mut mask = 0b0000_1000u8;
                    for j in 4..8 {
                        if i & mask == 0 {
                            new_inode_index += p * 8 + j;
                            index_offset = p * 8 + j;
                            bpos = p;
                            new_bm = i | mask;
                            break;
                        }
                        mask >>= 1;
                    }
                } else {
                    let mut mask = 0b1000_0000u8;
                    for j in 0..8 {
                        if i & mask == 0 {
                            new_inode_index += p * 8 + j;
                            index_offset = p * 8 + j;
                            bpos = p;
                            new_bm = i | mask;
                            break;
                        }
                        mask >>= 1;
                    }
                }
            }
        }

        let mb = Ext2FileMode::from_common_type(mode)?;
        debug!("create ext2 inode");

        //  创建inode
        let new_inode = Ext2Inode::new(mb.bits());
        // 新inode所在块号，跟据inode table的起始位置+inode block offset
        let block_id = (descriptor.inode_table_start as usize
            + (index_offset * sb.s_inode_size as usize) / block_size)
            * (block_size / LBA_SIZE);
        // inode table所占块数
        // let table_block_num = {
        //     let size = sb.s_inodes_per_group * sb.s_inode_size;
        //     if size % sb.s_block_size == 0 {
        //         (size / sb.s_block_size) as usize * (sb.s_block_size as usize / LBA_SIZE)
        //     } else {
        //         (size / sb.s_block_size + 1) as usize * (sb.s_block_size as usize / LBA_SIZE)
        //     }
        // };
        // 读inode table
        // BUG 内存分配错误

        let mut table_buf: Vec<u8> = Vec::with_capacity(block_size / LBA_SIZE);
        table_buf.resize(block_size / LBA_SIZE, 0);
        debug!("read inode table");
        let _ = ext2fs_instance().partition.disk().read_at(
            block_id,
            block_size / LBA_SIZE,
            table_buf.as_mut_slice(),
        );

        let in_block_offset = (index_offset * sb.s_inode_size as usize) % block_size;
        table_buf[in_block_offset..in_block_offset + mem::size_of::<Ext2Inode>()]
            .copy_from_slice(&new_inode.to_bytes());
        // 写inode table
        debug!("write inode table");

        let _ = ext2fs_instance().partition.disk().write_at(
            block_id,
            block_size / LBA_SIZE,
            table_buf.as_slice(),
        );
        //  写inode bitmap
        debug!("write inode bitmap");

        bitmap_buf[bpos] = new_bm;
        let _ = ext2fs_instance().partition.disk().write_at(
            i_bitmap,
            bitmap_count,
            bitmap_buf.as_slice(),
        );
        // TODO 修改
        // descriptor.free_inodes_num-=1;
        descriptor.flush(&ext2fs_instance().partition, group_id as usize, block_size)?;

        // TODO 写descriptor

        // 获取descriptor

        // TODO 写entry

        let inode = &guard.inode;
        let e_file_t = Ext2FileType::from_file_type(&file_type)?;

        // let new_inode_num = 0;
        let new_entry = Ext2DirEntry::new(new_inode_index as u32, e_file_t.into(), name)?;
        // 读entry
        let file_size = if sb.major_version == 1 {
            ((inode.directory_acl as usize) << 32usize) + inode.lower_size as usize
        } else {
            inode.lower_size as usize
        };
        drop(guard);
        let entry_buf = new_entry.to_bytes();
        self.write_at(
            file_size,
            entry_buf.len(),
            &entry_buf,
            SpinLock::new(FilePrivateData::Unused).lock(),
        )?;
        // TODO 调用write at追加entry
        debug!("end ext2 create");

        Ok(Arc::new(LockedExt2InodeInfo(SpinLock::new(
            Ext2InodeInfo::new(&new_inode, new_inode_index.try_into().unwrap()),
        ))))
    }
}

pub fn get_address_block(partition: Arc<Partition>, ptr: usize) -> [u32; 128] {
    // info!("begin get address block");
    let mut address_block: [u8; 512] = [0; 512];
    let _ = partition
        .disk()
        .read_at(__bytes_to_lba(ptr, LBA_SIZE), 1, &mut address_block[0..]);
    let address: [u32; 128] = unsafe { mem::transmute::<[u8; 512], [u32; 128]>(address_block) };
    // info!("end get address block");
    address
}
