//! /proc 根目录实现
//!
//! 这个文件实现了 /proc 的根目录，包含静态条目和动态的进程目录

use core::{any::Any, fmt};

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
            thread_self::ThreadSelfSymOps,
            version::VersionFileOps,
            version_signature::VersionSignatureFileOps,
            vmstat::VmstatFileOps,
            Builder, PROCFS_BLOCK_SIZE, PROCFS_MAX_NAMELEN,
        },
        vfs::{FileSystemMakerData, IndexNode, InodeMode, FSMAKER},
    },
    process::{namespace::pid_namespace::PidNamespace, ProcessManager, RawPid},
    register_mountable_fs,
};
use alloc::{
    string::ToString,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

/// /proc 根目录的 DirOps 实现
pub struct RootDirOps {
    pid_ns: Arc<PidNamespace>,
}

impl fmt::Debug for RootDirOps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RootDirOps").finish()
    }
}

struct ProcMountData {
    pid_ns: Arc<PidNamespace>,
}

impl fmt::Debug for ProcMountData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcMountData").finish()
    }
}

impl FileSystemMakerData for ProcMountData {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

//  drop 的时候把对应pid的文件夹删除
impl RootDirOps {
    pub fn new_inode(fs: Weak<ProcFS>, pid_ns: Arc<PidNamespace>) -> Arc<dyn IndexNode> {
        //todo 这里要注册一个observer，用于动态创建进程目录

        ProcDirBuilder::new(Self { pid_ns }, InodeMode::from_bits_truncate(0o555))
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
        ("thread-self", ThreadSelfSymOps::new_inode),
        ("version", VersionFileOps::new_inode),
        ("version_signature", VersionSignatureFileOps::new_inode),
        ("vmstat", VmstatFileOps::new_inode),
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
            if let Some(target) = crate::filesystem::procfs::pid::ProcPidTarget::from_tgid_in_ns(
                self.pid_ns.clone(),
                pid,
            ) {
                let mut cached_children = dir.cached_children().write();

                if let Some(child) = cached_children.get(name) {
                    if self.validate_child(child.as_ref()) {
                        return Ok(child.clone());
                    }
                }

                let inode = PidDirOps::new_inode(target, dir.self_ref_weak().clone());
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
        let pid_targets = self
            .pid_ns
            .collect_pids()
            .into_iter()
            .filter_map(|pid| {
                crate::filesystem::procfs::pid::ProcPidTarget::from_tgid_in_ns(
                    self.pid_ns.clone(),
                    pid.pid_nr_ns(&self.pid_ns),
                )
            })
            .collect::<Vec<_>>();

        // 获取缓存写锁并填充
        let mut cached_children = dir.cached_children().write();

        cached_children.retain(|name, child| {
            name.parse::<RawPid>().is_err() || self.validate_child(child.as_ref())
        });

        // 填充进程目录（只传递 PID）
        for target in pid_targets {
            let name = target.vpid().to_string();
            let needs_refresh = cached_children
                .get(&name)
                .map(|child| !self.validate_child(child.as_ref()))
                .unwrap_or(true);
            if needs_refresh {
                cached_children.insert(
                    name,
                    PidDirOps::new_inode(target.clone(), dir.self_ref_weak().clone()),
                );
            }
        }

        // 填充静态条目
        populate_children_from_table(&mut cached_children, Self::STATIC_ENTRIES, |f| {
            (f)(dir.self_ref_weak().clone())
        });
        // 写锁在这里自动释放
    }

    fn validate_child(&self, child: &dyn IndexNode) -> bool {
        if let Some(pid_dir) = child.downcast_ref::<ProcDir<PidDirOps>>() {
            return pid_dir.ops().is_current_target();
        }
        true
    }
}

use crate::filesystem::vfs::{FileSystem, FsInfo, Magic, MountableFileSystem, SuperBlock};
use crate::libs::rwsem::RwSem;
use linkme::distributed_slice;

/// ProcFS 文件系统
pub struct ProcFS {
    pid_ns: Arc<PidNamespace>,
    /// procfs 的 root inode
    root_inode: Arc<dyn IndexNode>,
    super_block: RwSem<SuperBlock>,
}

impl fmt::Debug for ProcFS {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcFS").finish()
    }
}

impl ProcFS {
    pub fn new(pid_ns: Arc<PidNamespace>) -> Arc<Self> {
        let super_block = SuperBlock::new(
            Magic::PROC_MAGIC,
            PROCFS_BLOCK_SIZE,
            PROCFS_MAX_NAMELEN as u64,
        );

        let fs: Arc<ProcFS> = Arc::new_cyclic(|weak_fs| ProcFS {
            pid_ns: pid_ns.clone(),
            super_block: RwSem::new(super_block),
            root_inode: RootDirOps::new_inode(weak_fs.clone(), pid_ns.clone()),
        });

        fs
    }

    pub fn pid_ns(&self) -> &Arc<PidNamespace> {
        &self.pid_ns
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
        Ok(Some(Arc::new(ProcMountData {
            pid_ns: ProcessManager::current_pcb().active_pid_ns(),
        })))
    }

    fn make_fs(
        data: Option<&dyn crate::filesystem::vfs::FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let mount_data = data
            .and_then(|d| d.as_any().downcast_ref::<ProcMountData>())
            .ok_or(SystemError::EINVAL)?;
        let fs = ProcFS::new(mount_data.pid_ns.clone());
        Ok(fs)
    }
}

// 注册 procfs 为可挂载文件系统
register_mountable_fs!(ProcFS, PROCFSMAKER, "proc");
