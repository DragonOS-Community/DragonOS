use std::fs::File;

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};

use crate::{
    filesystem::vfs::{core::generate_inode_id, FileType},
    include::bindings::bindings::{
        process_control_block, process_find_pcb_by_pid, EEXIST, EINVAL, EISDIR, ENOBUFS, ENOENT,
        ENOTDIR, ENOTEMPTY, EPERM,
    },
    libs::spinlock::{SpinLock, SpinLockGuard},
    print, println,
    time::TimeSpec,
};

use super::vfs::{FileSystem, FsInfo, IndexNode, InodeId, Metadata, PollStatus};

//进程文件夹下的各类文件
#[derive(Debug)]
#[repr(u8)]
pub enum ProcFileType {
    ProcStatus = 0,
    //todo: other file types
    Default,
}

impl From<u8> for ProcFileType {
    fn from(value: u8) -> Self {
        match value {
            0 => ProcFileType::ProcStatus,
            _ => ProcFileType::Default,
        }
    }
}
///节点私有信息结构体
#[derive(Debug)]
pub struct InodeInfo {
    //进程pid
    pid: i64,
    //文件类型
    ftype: ProcFileType,
}

/// procfs的inode名称的最大长度
const PROCFS_MAX_NAMELEN: usize = 64;

/// @brief procfs文件系统的Inode结构体
#[derive(Debug)]
struct LockedProcFSInode(SpinLock<ProcFSInode>);

/// @brief procfs文件系统结构体
#[derive(Debug)]
pub struct ProcFS {
    /// procfs的root inode
    root_inode: Arc<LockedProcFSInode>,
}

/// @brief procfs文件系统的Inode结构体(不包含锁)
#[derive(Debug)]
pub struct ProcFSInode {
    /// 指向父Inode的弱引用
    parent: Weak<LockedProcFSInode>,
    /// 指向自身的弱引用
    self_ref: Weak<LockedProcFSInode>,
    /// 子Inode的B树
    children: BTreeMap<String, Arc<LockedProcFSInode>>,
    /// 当前inode的数据部分
    data: Vec<u8>,
    /// 当前inode的元数据
    metadata: Metadata,
    /// 指向inode所在的文件系统对象的指针
    fs: Weak<ProcFS>,
    /// 储存私有信息
    fdata: InodeInfo,
}

impl ProcFSInode {
    // 获取进程status信息
    fn get_info_status(&self) {
        //获取pcb
        let pid_t = &self.fdata.pid;
        let pcb_t = unsafe { process_find_pcb_by_pid(pid_t) };
        //
        let mut pdata = &self.data;
        pdata.push("Name:\t".to_string());
        pdata.push(pcb_t.name);
        pdata.push("\nstate:\t".to_string());
        pdata.push(pcb_t.state);
        pdata.push("\npid:\t".to_string());
        pdata.push(pcb_t.pid);
        pdata.push("\nPpid:\t".to_string());
        pdata.push(pcb_t.parent_pcb.pid);
        pdata.push("\ncpu_id:\t".to_string());
        pdata.push(pcb_t.cpu_id);
        pdata.push("\npriority:\t".to_string());
        pdata.push(pcb_t.priority);
        pdata.push("\npreempt:\t".to_string());
        pdata.push(pcb_t.preempt_count);
        pdata.push("\nvrtime:\t".to_string());
        pdata.push(pcb_t.virtual_runtime);

        let hiwater_vm = pcb_t.mm.vmas.vm_end - pcb_t.mm.vmas.vm_start;
        let text = pcb_t.mm.code_addr_end - pcb_t.mm.code_addr_start;
        let data = pcb_t.mm.data_addr_end - pcb_t.mm.data_addr_start;

        pdata.push("\nVmPeak:".to_string());
        pdata.push(hiwater_vm);
        pdata.push(" kB".to_string());
        pdata.push("\nVmData:".to_string());
        pdata.push(data);
        pdata.push(" kB".to_string());
        pdata.push("\nVmExe:".to_string());
        pdata.push(text);
        pdata.push(" kB\n".to_string());
    }

    //todo:other files info

}

impl FileSystem for ProcFS {
    fn get_root_inode(&self) -> Arc<dyn super::vfs::IndexNode> {
        return self.root_inode.clone();
    }

    fn info(&self) -> FsInfo {
        return FsInfo {
            blk_dev_id: 0,
            max_name_len: PROCFS_MAX_NAMELEN,
        };
    }
}

impl ProcFS {
    pub fn new() -> Arc<Self> {
        // 初始化root inode
        let root: Arc<LockedProcFSInode> =
            Arc::new(LockedProcFSInode(SpinLock::new(ProcFSInode {
                parent: Weak::default(),
                self_ref: Weak::default(),
                children: BTreeMap::new(),
                data: Vec::new(),
                metadata: Metadata {
                    dev_id: 0,
                    inode_id: generate_inode_id(),
                    size: 0,
                    blk_size: 0,
                    blocks: 0,
                    atime: TimeSpec::default(),
                    mtime: TimeSpec::default(),
                    ctime: TimeSpec::default(),
                    file_type: FileType::Dir,
                    mode: 0o777,
                    nlinks: 1,
                    uid: 0,
                    gid: 0,
                    raw_dev: 0,
                },
                fs: Weak::default(),
                fdata: InodeInfo {
                    pid: 0,
                    ftype: ProcFileType::Default,
                },
            })));

        let result: Arc<ProcFS> = Arc::new(ProcFS { root_inode: root });

        // 对root inode加锁，并继续完成初始化工作
        let mut root_guard: SpinLockGuard<ProcFSInode> = result.root_inode.0.lock();
        root_guard.parent = Arc::downgrade(&result.root_inode);
        root_guard.self_ref = Arc::downgrade(&result.root_inode);
        root_guard.fs = Arc::downgrade(&result);
        // 释放锁
        drop(root_guard);

        return result;
    }

    pub fn root(&self) -> Arc<ProcINode> {
        self.root_inode.clone()
    }

    ///@brief 进程注册
    pub fn procfs_register_pid(&self, pid: i64) -> Result<(), i32> {
        
        //获取当前inode
        let proc = self.get_root_inode();
        //创建对应进程文件夹
        let pf = proc.create(&pid.to_string(), Dir, 0).unwrap();
        //创建相关文件
        //status文件
        let sf: ProcFSInode = pf.create("status", File, 0).unwrap().as_any_ref();
        sf.fdata.pid = pid;
        
        //todo: other files

        return Ok(());
    }
}

impl IndexNode for LockedProcFSInode {
    fn read_at(&self, offset: usize, len: usize, buf: &mut [u8]) -> Result<usize, i32> {
        if buf.len() < len {
            return Err(-(EINVAL as i32));
        }
        // 加锁
        let inode: SpinLockGuard<ProcFSInode> = self.0.lock();

        // 检查当前inode是否为一个文件夹，如果是的话，就返回错误
        if inode.metadata.file_type == FileType::Dir {
            return Err(-(EISDIR as i32));
        }

        //根据文件类型获取相应数据
        match inode.ptype {
            ProcFileType::ProcStatus => inode.get_info_status(),
            ProcFileType::Default => (),
        }

        let start = inode.data.len().min(offset);
        let end = inode.data.len().min(offset + len);

        // buffer空间不足
        if buf.len() < (end - start) {
            return Err(-(ENOBUFS as i32));
        }

        // 拷贝数据
        let src = &inode.data[start..end];
        buf[0..src.len()].copy_from_slice(src);
        return Ok(src.len());
    }

    fn write_at(&self, offset: usize, len: usize, buf: &mut [u8]) -> Result<usize, i32> {
        if buf.len() < len {
            return Err(-(EINVAL as i32));
        }

        // 加锁
        let mut inode: SpinLockGuard<ProcFSInode> = self.0.lock();

        // 检查当前inode是否为一个文件夹，如果是的话，就返回错误
        if inode.metadata.file_type == FileType::Dir {
            return Err(-(EISDIR as i32));
        }

        let data: &mut Vec<u8> = &mut inode.data;

        // 如果文件大小比原来的大，那就resize这个数组
        if offset + len > data.len() {
            data.resize(offset + len, 0);
        }

        let target = &mut data[offset..offset + len];
        target.copy_from_slice(&buf[0..len]);
        return Ok(len);
    }

    fn poll(&self) -> Result<PollStatus, i32> {
        // 加锁
        let inode: SpinLockGuard<ProcFSInode> = self.0.lock();

        // 检查当前inode是否为一个文件夹，如果是的话，就返回错误
        if inode.metadata.file_type == FileType::Dir {
            return Err(-(EISDIR as i32));
        }

        return Ok(PollStatus {
            flags: PollStatus::READ_MASK | PollStatus::WRITE_MASK,
        });
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        return self.0.lock().fs.upgrade().unwrap();
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn metadata(&self) -> Result<Metadata, i32> {
        let inode = self.0.lock();
        let mut metadata = inode.metadata.clone();
        metadata.size = inode.data.len() as i64;

        return Ok(metadata);
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), i32> {
        let mut inode = self.0.lock();
        inode.metadata.atime = metadata.atime;
        inode.metadata.mtime = metadata.mtime;
        inode.metadata.ctime = metadata.ctime;
        inode.metadata.mode = metadata.mode;
        inode.metadata.uid = metadata.uid;
        inode.metadata.gid = metadata.gid;

        return Ok(());
    }

    fn resize(&self, len: usize) -> Result<(), i32> {
        let mut inode = self.0.lock();
        if inode.metadata.file_type == FileType::File {
            inode.data.resize(len, 0);
            return Ok(());
        } else {
            return Err(-(EINVAL as i32));
        }
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        mode: u32,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, i32> {
        // 获取当前inode
        let mut inode = self.0.lock();
        // 如果当前inode不是文件夹，则返回
        if inode.metadata.file_type != FileType::Dir {
            return Err(-(ENOTDIR as i32));
        }
        // 如果有重名的，则返回
        if inode.children.contains_key(name) {
            return Err(-(EEXIST as i32));
        }

        // 创建inode
        let result: Arc<LockedProcFSInode> =
            Arc::new(LockedProcFSInode(SpinLock::new(ProcFSInode {
                parent: inode.self_ref.clone(),
                self_ref: Weak::default(),
                children: BTreeMap::new(),
                data: Vec::new(),
                metadata: Metadata {
                    dev_id: 0,
                    inode_id: generate_inode_id(),
                    size: 0,
                    blk_size: 0,
                    blocks: 0,
                    atime: TimeSpec::default(),
                    mtime: TimeSpec::default(),
                    ctime: TimeSpec::default(),
                    file_type: file_type,
                    mode: mode,
                    nlinks: 1,
                    uid: 0,
                    gid: 0,
                    raw_dev: data,
                },
                fs: inode.fs.clone(),
                fdata: InodeInfo {
                    pid: 0,
                    ftype: ProcFileType::Default,
                },
            })));

        // 初始化inode的自引用的weak指针
        result.0.lock().self_ref = Arc::downgrade(&result);

        // 将子inode插入父inode的B树中
        inode.children.insert(String::from(name), result.clone());

        return Ok(result);
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), i32> {
        let other: &LockedProcFSInode = other
            .downcast_ref::<LockedProcFSInode>()
            .ok_or(-(EPERM as i32))?;
        let mut inode: SpinLockGuard<ProcFSInode> = self.0.lock();
        let mut other_locked: SpinLockGuard<ProcFSInode> = other.0.lock();

        // 如果当前inode不是文件夹，那么报错
        if inode.metadata.file_type != FileType::Dir {
            return Err(-(ENOTDIR as i32));
        }

        // 如果另一个inode是文件夹，那么也报错
        if other_locked.metadata.file_type == FileType::Dir {
            return Err(-(EISDIR as i32));
        }

        // 如果当前文件夹下已经有同名文件，也报错。
        if inode.children.contains_key(name) {
            return Err(-(EEXIST as i32));
        }

        inode
            .children
            .insert(String::from(name), other_locked.self_ref.upgrade().unwrap());

        // 增加硬链接计数
        other_locked.metadata.nlinks += 1;
        return Ok(());
    }

    fn unlink(&self, name: &str) -> Result<(), i32> {
        let mut inode: SpinLockGuard<ProcFSInode> = self.0.lock();
        // 如果当前inode不是目录，那么也没有子目录/文件的概念了，因此要求当前inode的类型是目录
        if inode.metadata.file_type != FileType::Dir {
            return Err(-(ENOTDIR as i32));
        }
        // 不允许删除当前文件夹，也不允许删除上一个目录
        if name == "." || name == ".." {
            return Err(-(ENOTEMPTY as i32));
        }

        // 获得要删除的文件的inode
        let to_delete = inode.children.get(name).ok_or(-(ENOENT as i32))?;
        // 减少硬链接计数
        to_delete.0.lock().metadata.nlinks -= 1;
        // 在当前目录中删除这个子目录项
        inode.children.remove(name);
        return Ok(());
    }

    fn move_(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
    ) -> Result<(), i32> {
        let old_inode: Arc<dyn IndexNode> = self.find(old_name)?;

        // 在新的目录下创建一个硬链接
        target.link(new_name, &old_inode)?;
        // 取消现有的目录下的这个硬链接
        if let Err(err) = self.unlink(old_name) {
            // 如果取消失败，那就取消新的目录下的硬链接
            target.unlink(new_name)?;
            return Err(err);
        }
        return Ok(());
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, i32> {
        let inode = self.0.lock();

        if inode.metadata.file_type != FileType::Dir {
            return Err(-(ENOTDIR as i32));
        }

        match name {
            "" | "." => {
                return Ok(inode.self_ref.upgrade().ok_or(-(ENOENT as i32))?);
            }

            ".." => {
                return Ok(inode.parent.upgrade().ok_or(-(ENOENT as i32))?);
            }
            name => {
                // 在子目录项中查找
                return Ok(inode.children.get(name).ok_or(-(ENOENT as i32))?.clone());
            }
        }
    }

    fn get_entry_name(&self, ino: InodeId) -> Result<String, i32> {
        let inode: SpinLockGuard<ProcFSInode> = self.0.lock();
        if inode.metadata.file_type != FileType::Dir {
            return Err(-(ENOTDIR as i32));
        }

        match ino {
            0 => {
                return Ok(String::from("."));
            }
            1 => {
                return Ok(String::from(".."));
            }
            ino => {
                // 暴力遍历所有的children，判断inode id是否相同
                // TODO: 优化这里，这个地方性能很差！
                let mut key: Vec<String> = inode
                    .children
                    .keys()
                    .filter(|k| inode.children.get(*k).unwrap().0.lock().metadata.inode_id == ino)
                    .cloned()
                    .collect();

                assert_eq!(key.len(), 1);

                return Ok(key.remove(0));
            }
        }
    }
}
