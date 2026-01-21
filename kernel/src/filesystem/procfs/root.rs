//! /proc 根目录实现
//!
//! 这个文件实现了 /proc 的根目录，包含静态条目和动态的进程目录

use crate::{
    filesystem::{
        procfs::{
            cmdline::CmdlineFileOps,
            cpuinfo::CpuInfoFileOps,
            kmsg_file::KmsgFileOps,
            loadavg::LoadavgFileOps,
            meminfo::MeminfoFileOps,
            mounts::MountsFileOps,
            net::NetDirOps,
            pid::PidDirOps,
            self_::SelfSymOps,
            stat::StatFileOps,
            sys::SysDirOps,
            template::{
                lookup_child_from_table, populate_children_from_table, DirOps, ProcDir,
                ProcDirBuilder,
            },
            thread_self::ThreadSelfDirOps,
            version::VersionFileOps,
            version_signature::VersionSignatureFileOps,
            Builder, PROCFS_BLOCK_SIZE, PROCFS_MAX_NAMELEN,
        },
        vfs::{FileSystemMakerData, IndexNode, InodeMode, FSMAKER},
    },
    process::{ProcessManager, RawPid},
    register_mountable_fs,
};
use alloc::{
    string::ToString,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

/// /proc 根目录的 DirOps 实现
#[derive(Debug)]
pub struct RootDirOps;

//  drop 的时候把对应pid的文件夹删除
impl RootDirOps {
    pub fn new_inode(fs: Weak<ProcFS>) -> Arc<dyn IndexNode> {
        //todo 这里要注册一个observer，用于动态创建进程目录

        ProcDirBuilder::new(Self, InodeMode::from_bits_truncate(0o555))
            .fs(fs)
            .build()
            .expect("Failed to create RootDirOps")
    }

    /// 静态条目表
    /// 包含所有非进程目录的 /proc 条目
    #[expect(clippy::type_complexity)]
    const STATIC_ENTRIES: &'static [(
        &'static str,
        fn(Weak<dyn IndexNode>) -> Arc<dyn IndexNode>,
    )] = &[
        ("cmdline", CmdlineFileOps::new_inode),
        ("cpuinfo", CpuInfoFileOps::new_inode),
        ("kmsg", KmsgFileOps::new_inode),
        ("loadavg", LoadavgFileOps::new_inode),
        ("meminfo", MeminfoFileOps::new_inode),
        ("mounts", MountsFileOps::new_inode),
        ("net", NetDirOps::new_inode),
        ("self", SelfSymOps::new_inode),
        ("stat", StatFileOps::new_inode),
        ("sys", SysDirOps::new_inode),
        ("thread-self", ThreadSelfDirOps::new_inode),
        ("version", VersionFileOps::new_inode),
        ("version_signature", VersionSignatureFileOps::new_inode),
    ];
}

impl DirOps for RootDirOps {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 首先检查是否是 PID 目录
        if let Ok(pid) = name.parse::<RawPid>() {
            // 检查进程是否存在
            if ProcessManager::find(pid).is_some() {
                let mut cached_children = dir.cached_children().write();

                // 检查缓存中是否已存在
                if let Some(child) = cached_children.get(name) {
                    return Ok(child.clone());
                }

                // 创建新的 PID 目录（只传递 PID，不传递进程引用）
                let inode = PidDirOps::new_inode(pid, dir.self_ref_weak().clone());
                cached_children.insert(name.to_string(), inode.clone());
                return Ok(inode);
            } else {
                return Err(SystemError::ENOENT);
            }
        }

        // 查找静态条目
        let mut cached_children = dir.cached_children().write();

        if let Some(child) =
            lookup_child_from_table(name, &mut cached_children, Self::STATIC_ENTRIES, |f| {
                (f)(dir.self_ref_weak().clone())
            })
        {
            return Ok(child);
        }

        Err(SystemError::ENOENT)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        // 先收集进程 PID，然后立即释放进程表锁
        let pid_list = {
            let all_processes = crate::process::all_process().lock_irqsave();
            if let Some(process_map) = all_processes.as_ref() {
                process_map.keys().cloned().collect::<Vec<_>>()
            } else {
                Vec::new()
            }
        };
        // 进程表锁已经释放

        // 获取缓存写锁并填充
        let mut cached_children = dir.cached_children().write();

        // 填充进程目录（只传递 PID）
        for pid in pid_list {
            cached_children
                .entry(pid.to_string())
                .or_insert_with(|| PidDirOps::new_inode(pid, dir.self_ref_weak().clone()));
        }

        // 填充静态条目
        populate_children_from_table(&mut cached_children, Self::STATIC_ENTRIES, |f| {
            (f)(dir.self_ref_weak().clone())
        });
        // 写锁在这里自动释放
    }
}

use crate::filesystem::vfs::{FileSystem, FsInfo, Magic, MountableFileSystem, SuperBlock};
use crate::libs::rwsem::RwSem;
use linkme::distributed_slice;

/// ProcFS 文件系统
#[derive(Debug)]
pub struct ProcFS {
    /// procfs 的 root inode
    root_inode: Arc<dyn IndexNode>,
    super_block: RwSem<SuperBlock>,
}

impl ProcFS {
    pub fn new() -> Arc<Self> {
        let super_block = SuperBlock::new(
            Magic::PROC_MAGIC,
            PROCFS_BLOCK_SIZE,
            PROCFS_MAX_NAMELEN as u64,
        );

        let fs: Arc<ProcFS> = Arc::new_cyclic(|weak_fs| ProcFS {
            super_block: RwSem::new(super_block),
            root_inode: RootDirOps::new_inode(weak_fs.clone()),
        });

        fs
    }
}

impl FileSystem for ProcFS {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        self.root_inode.clone()
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            blk_dev_id: 0,
            max_name_len: PROCFS_MAX_NAMELEN,
        }
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn name(&self) -> &str {
        "procfs"
    }

    fn super_block(&self) -> SuperBlock {
        self.super_block.read().clone()
    }
}

impl MountableFileSystem for ProcFS {
    /// 创建 procfs 挂载数据
    ///
    /// procfs 是一个虚拟文件系统，不需要任何挂载数据。
    /// 与需要挂载选项的文件系统（如带有大小限制的 tmpfs）不同，
    /// procfs 的行为完全由内核状态决定，不需要额外的配置参数。
    fn make_mount_data(
        _raw_data: Option<&str>,
        _source: &str,
    ) -> Result<Option<Arc<dyn crate::filesystem::vfs::FileSystemMakerData + 'static>>, SystemError>
    {
        // procfs 不需要任何额外的挂载数据
        Ok(None)
    }

    fn make_fs(
        _data: Option<&dyn crate::filesystem::vfs::FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let fs = ProcFS::new();
        Ok(fs)
    }
}

// 注册 procfs 为可挂载文件系统
register_mountable_fs!(ProcFS, PROCFSMAKER, "proc");
