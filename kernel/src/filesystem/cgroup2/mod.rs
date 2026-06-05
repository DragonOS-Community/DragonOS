use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::any::Any;
use core::sync::atomic::Ordering;

use hashbrown::{HashMap, HashSet};
use system_error::SystemError;

use crate::filesystem::vfs::FSMAKER;
use crate::{
    cgroup::{
        cgroup_accounting_lock, cgroup_common_ancestor, cgroup_migrate_vet_dst_with_src,
        cgroup_root, CgroupNode,
    },
    filesystem::{
        sysfs::sysfs_instance,
        vfs::{
            file::{FileFlags, FilePrivateData},
            permission::PermissionMask,
            vcore::generate_inode_id,
            FileSystem, FileSystemMakerData, FileType, FsInfo, IndexNode, InodeFlags, InodeMode,
            Magic, Metadata, MountableFileSystem, SuperBlock,
        },
    },
    libs::{mutex::MutexGuard, once::Once, rwsem::RwSem, spinlock::SpinLock},
    process::ProcessManager,
    register_mountable_fs,
    time::PosixTimeSpec,
};
use linkme::distributed_slice;

const CGROUP2_MAX_NAMELEN: usize = 255;
const CGROUP2_BLOCK_SIZE: u64 = 512;
const AVAILABLE_CONTROLLERS: [&str; 3] = ["cpu", "memory", "pids"];
const DOMAIN_CONTROLLERS: [&str; 1] = ["memory"];

#[derive(Debug)]
pub struct Cgroup2Fs {
    root_inode: Arc<Cgroup2Inode>,
    nsdelegate: bool,
}

#[derive(Debug)]
struct Cgroup2MountData {
    root_cgroup: Arc<CgroupNode>,
    nsdelegate: bool,
}

impl FileSystemMakerData for Cgroup2MountData {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
struct Cgroup2Inode {
    self_ref: Weak<Cgroup2Inode>,
    fs: RwSem<Weak<Cgroup2Fs>>,
    inner: SpinLock<Cgroup2InodeInner>,
}

#[derive(Debug)]
struct Cgroup2InodeInner {
    parent: Weak<Cgroup2Inode>,
    metadata: Metadata,
    name: String,
    kind: Cgroup2InodeKind,
}

#[derive(Debug)]
enum Cgroup2InodeKind {
    Dir {
        cgroup: Arc<CgroupNode>,
        children: HashMap<String, Arc<Cgroup2Inode>>,
    },
    File {
        cgroup: Arc<CgroupNode>,
        ty: CgroupCoreFile,
        data: Vec<u8>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CgroupCoreFile {
    Procs,
    Controllers,
    SubtreeControl,
    Events,
    Type,
    Freeze,
    CpuStat,
    CpuWeight,
    CpuMax,
    MemoryCurrent,
    MemoryPeak,
    MemoryMin,
    MemoryLow,
    MemoryHigh,
    MemoryMax,
    MemoryEvents,
    MemoryStat,
    MemorySwapCurrent,
    MemorySwapPeak,
    MemorySwapHigh,
    MemorySwapMax,
    MemorySwapEvents,
    PidsCurrent,
    PidsMax,
    PidsEvents,
}

#[derive(Clone, Copy)]
struct CgroupFileSpec {
    name: &'static str,
    ty: CgroupCoreFile,
    init: &'static [u8],
    mode: u16,
}

const BASE_FILE_SPECS: [CgroupFileSpec; 3] = [
    CgroupFileSpec {
        name: "cgroup.procs",
        ty: CgroupCoreFile::Procs,
        init: b"",
        mode: 0o644,
    },
    CgroupFileSpec {
        name: "cgroup.controllers",
        ty: CgroupCoreFile::Controllers,
        init: b"\n",
        mode: 0o444,
    },
    CgroupFileSpec {
        name: "cgroup.subtree_control",
        ty: CgroupCoreFile::SubtreeControl,
        init: b"\n",
        mode: 0o644,
    },
];

const NON_ROOT_CORE_FILE_SPECS: [CgroupFileSpec; 3] = [
    CgroupFileSpec {
        name: "cgroup.events",
        ty: CgroupCoreFile::Events,
        init: b"",
        mode: 0o444,
    },
    CgroupFileSpec {
        name: "cgroup.type",
        ty: CgroupCoreFile::Type,
        init: b"domain\n",
        mode: 0o444,
    },
    CgroupFileSpec {
        name: "cgroup.freeze",
        ty: CgroupCoreFile::Freeze,
        init: b"0\n",
        mode: 0o644,
    },
];

const ROOT_CONTROLLER_FILE_SPECS: [CgroupFileSpec; 2] = [
    CgroupFileSpec {
        name: "cpu.stat",
        ty: CgroupCoreFile::CpuStat,
        init: b"",
        mode: 0o444,
    },
    CgroupFileSpec {
        name: "memory.stat",
        ty: CgroupCoreFile::MemoryStat,
        init: b"",
        mode: 0o444,
    },
];

const CPU_FILE_SPECS: [CgroupFileSpec; 3] = [
    CgroupFileSpec {
        name: "cpu.stat",
        ty: CgroupCoreFile::CpuStat,
        init: b"",
        mode: 0o444,
    },
    CgroupFileSpec {
        name: "cpu.weight",
        ty: CgroupCoreFile::CpuWeight,
        init: b"100\n",
        mode: 0o644,
    },
    CgroupFileSpec {
        name: "cpu.max",
        ty: CgroupCoreFile::CpuMax,
        init: b"max 100000\n",
        mode: 0o644,
    },
];

const MEMORY_FILE_SPECS: [CgroupFileSpec; 13] = [
    CgroupFileSpec {
        name: "memory.current",
        ty: CgroupCoreFile::MemoryCurrent,
        init: b"0\n",
        mode: 0o444,
    },
    CgroupFileSpec {
        name: "memory.peak",
        ty: CgroupCoreFile::MemoryPeak,
        init: b"0\n",
        mode: 0o444,
    },
    CgroupFileSpec {
        name: "memory.min",
        ty: CgroupCoreFile::MemoryMin,
        init: b"0\n",
        mode: 0o644,
    },
    CgroupFileSpec {
        name: "memory.low",
        ty: CgroupCoreFile::MemoryLow,
        init: b"0\n",
        mode: 0o644,
    },
    CgroupFileSpec {
        name: "memory.high",
        ty: CgroupCoreFile::MemoryHigh,
        init: b"max\n",
        mode: 0o644,
    },
    CgroupFileSpec {
        name: "memory.max",
        ty: CgroupCoreFile::MemoryMax,
        init: b"max\n",
        mode: 0o644,
    },
    CgroupFileSpec {
        name: "memory.events",
        ty: CgroupCoreFile::MemoryEvents,
        init: b"",
        mode: 0o444,
    },
    CgroupFileSpec {
        name: "memory.stat",
        ty: CgroupCoreFile::MemoryStat,
        init: b"",
        mode: 0o444,
    },
    CgroupFileSpec {
        name: "memory.swap.current",
        ty: CgroupCoreFile::MemorySwapCurrent,
        init: b"0\n",
        mode: 0o444,
    },
    CgroupFileSpec {
        name: "memory.swap.peak",
        ty: CgroupCoreFile::MemorySwapPeak,
        init: b"0\n",
        mode: 0o444,
    },
    CgroupFileSpec {
        name: "memory.swap.high",
        ty: CgroupCoreFile::MemorySwapHigh,
        init: b"max\n",
        mode: 0o644,
    },
    CgroupFileSpec {
        name: "memory.swap.max",
        ty: CgroupCoreFile::MemorySwapMax,
        init: b"max\n",
        mode: 0o644,
    },
    CgroupFileSpec {
        name: "memory.swap.events",
        ty: CgroupCoreFile::MemorySwapEvents,
        init: b"",
        mode: 0o444,
    },
];

const PIDS_FILE_SPECS: [CgroupFileSpec; 3] = [
    CgroupFileSpec {
        name: "pids.current",
        ty: CgroupCoreFile::PidsCurrent,
        init: b"0\n",
        mode: 0o444,
    },
    CgroupFileSpec {
        name: "pids.max",
        ty: CgroupCoreFile::PidsMax,
        init: b"max\n",
        mode: 0o644,
    },
    CgroupFileSpec {
        name: "pids.events",
        ty: CgroupCoreFile::PidsEvents,
        init: b"max 0\n",
        mode: 0o444,
    },
];

impl CgroupCoreFile {
    fn controller(self) -> Option<&'static str> {
        match self {
            CgroupCoreFile::CpuStat | CgroupCoreFile::CpuWeight | CgroupCoreFile::CpuMax => {
                Some("cpu")
            }
            CgroupCoreFile::MemoryCurrent
            | CgroupCoreFile::MemoryPeak
            | CgroupCoreFile::MemoryMin
            | CgroupCoreFile::MemoryLow
            | CgroupCoreFile::MemoryHigh
            | CgroupCoreFile::MemoryMax
            | CgroupCoreFile::MemoryEvents
            | CgroupCoreFile::MemoryStat
            | CgroupCoreFile::MemorySwapCurrent
            | CgroupCoreFile::MemorySwapPeak
            | CgroupCoreFile::MemorySwapHigh
            | CgroupCoreFile::MemorySwapMax
            | CgroupCoreFile::MemorySwapEvents => Some("memory"),
            CgroupCoreFile::PidsCurrent | CgroupCoreFile::PidsMax | CgroupCoreFile::PidsEvents => {
                Some("pids")
            }
            _ => None,
        }
    }
}

impl Cgroup2Fs {
    fn new(root_cg: Arc<CgroupNode>, nsdelegate: bool) -> Arc<Self> {
        let root_inode = Cgroup2Inode::new_dir(String::new(), root_cg);

        let fs = Arc::new(Self {
            root_inode: root_inode.clone(),
            nsdelegate,
        });
        *root_inode.fs.write() = Arc::downgrade(&fs);

        Cgroup2Inode::populate_core_files(&root_inode)
            .expect("cgroup2: populate root files failed");
        fs
    }

    fn nsdelegate(&self) -> bool {
        self.nsdelegate
    }
}

pub fn cgroup2_init() -> Result<(), SystemError> {
    static INIT: Once = Once::new();
    let mut result = None;
    INIT.call_once(|| {
        result = Some((|| -> Result<(), SystemError> {
            sysfs_instance()
                .ensure_mount_point_path(&["fs", "cgroup"], InodeMode::from_bits_truncate(0o755))?;

            let root_inode = ProcessManager::current_mntns().root_inode();
            let sys = root_inode.find("sys")?;
            let fs_dir = sys.find("fs")?;
            let cgroup_dir = fs_dir.find("cgroup")?;

            let cgroup_fs = Cgroup2Fs::new(cgroup_root().root(), false);
            cgroup_dir.mount(
                cgroup_fs,
                crate::filesystem::vfs::mount::MountFlags::empty(),
            )?;

            ::log::info!("Cgroup2 mounted at /sys/fs/cgroup");
            Ok(())
        })());
    });
    result.unwrap_or(Ok(()))
}

impl Cgroup2Inode {
    fn cgroup_of_dir(inode: &Arc<Cgroup2Inode>) -> Option<Arc<CgroupNode>> {
        let inner = inode.inner.lock();
        match &inner.kind {
            Cgroup2InodeKind::Dir { cgroup, .. } => Some(cgroup.clone()),
            _ => None,
        }
    }

    fn fs_root_cgroup(fs_root: &Arc<dyn IndexNode>) -> Result<Arc<CgroupNode>, SystemError> {
        let root_inode = fs_root
            .as_any_ref()
            .downcast_ref::<Cgroup2Inode>()
            .ok_or(SystemError::EINVAL)?;
        let inner = root_inode.inner.lock();
        match &inner.kind {
            Cgroup2InodeKind::Dir { cgroup, .. } => Ok(cgroup.clone()),
            _ => Err(SystemError::EINVAL),
        }
    }

    fn prune_stale_dir_cache(parent: &Arc<Cgroup2Inode>) -> Result<(), SystemError> {
        let (parent_cgroup, entries) = {
            let inner = parent.inner.lock();
            match &inner.kind {
                // 先获取 parent cgroup 和缓存的 entries，避免长时间持有锁。
                Cgroup2InodeKind::Dir { cgroup, children } => {
                    let entries = children
                        .iter()
                        .map(|(name, inode)| (name.clone(), inode.clone()))
                        .collect::<Vec<_>>();
                    (cgroup.clone(), entries)
                }
                _ => return Err(SystemError::ENOTDIR),
            }
        };

        let mut stale = Vec::new();
        for (name, inode) in entries {
            let Some(cached_cgroup) = Self::cgroup_of_dir(&inode) else {
                continue;
            };
            match parent_cgroup.child(&name) {
                Some(real) if Arc::ptr_eq(&real, &cached_cgroup) => {}
                _ => stale.push(name),
            }
        }

        if stale.is_empty() {
            return Ok(());
        }

        let mut inner = parent.inner.lock();
        if let Cgroup2InodeKind::Dir { children, .. } = &mut inner.kind {
            for name in stale {
                children.remove(&name);
            }
            return Ok(());
        }

        Err(SystemError::ENOTDIR)
    }
    //把全局cgroup node路径转换为fs内路径，并找到对应inode
    fn find_cgroup_dir_from_fs_root(
        fs_root: Arc<dyn IndexNode>,
        cgroup: &Arc<CgroupNode>,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let view_root = Self::fs_root_cgroup(&fs_root)?;
        if !view_root.is_ancestor_of(cgroup) {
            return Err(SystemError::ENOENT);
        }
        let rel = crate::cgroup::cgroup_path_relative_to_node(cgroup, &view_root);
        if rel == "/" {
            return Ok(fs_root);
        }

        let mut cur = fs_root;
        for comp in rel.trim_start_matches('/').split('/') {
            if comp.is_empty() {
                continue;
            }
            if comp == ".." {
                return Err(SystemError::ENOENT);
            }
            cur = cur.find(comp)?;
        }
        Ok(cur)
    }

    fn cgroup_procs_inode_from_fs_root(
        fs_root: Arc<dyn IndexNode>,
        cgroup: &Arc<CgroupNode>,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let dir = Self::find_cgroup_dir_from_fs_root(fs_root, cgroup)?;
        dir.find("cgroup.procs")
    }

    fn check_attach_permissions(
        fs_root: Arc<dyn IndexNode>,
        src_cgroup: &Arc<CgroupNode>,
        dst_cgroup: &Arc<CgroupNode>,
    ) -> Result<(), SystemError> {
        let current = ProcessManager::current_pcb();
        let cred = current.cred();
        //目标cgroup的procs可写
        let dst_procs = Self::cgroup_procs_inode_from_fs_root(fs_root.clone(), dst_cgroup)?;
        let dst_md = dst_procs.metadata()?;
        cred.inode_permission(&dst_md, PermissionMask::MAY_WRITE.bits())
            .map_err(|_| SystemError::EACCES)?;
        //共同祖先的procs可写
        let common = cgroup_common_ancestor(src_cgroup, dst_cgroup);
        let common_procs = Self::cgroup_procs_inode_from_fs_root(fs_root, &common)?;
        let common_md = common_procs.metadata()?;
        cred.inode_permission(&common_md, PermissionMask::MAY_WRITE.bits())
            .map_err(|_| SystemError::EACCES)?;

        Ok(())
    }

    fn available_controllers_for(cgroup: &Arc<CgroupNode>) -> Vec<&'static str> {
        let Some(parent) = cgroup.parent() else {
            return AVAILABLE_CONTROLLERS.to_vec();
        };
        let parent_enabled: HashSet<String> = parent.subtree_control().into_iter().collect();
        AVAILABLE_CONTROLLERS
            .iter()
            .copied()
            .filter(|name| parent_enabled.contains(*name))
            .collect()
    }

    fn is_known_controller(name: &str) -> bool {
        AVAILABLE_CONTROLLERS.contains(&name)
    }

    fn controller_specs(name: &str) -> &'static [CgroupFileSpec] {
        match name {
            "cpu" => &CPU_FILE_SPECS,
            "memory" => &MEMORY_FILE_SPECS,
            "pids" => &PIDS_FILE_SPECS,
            _ => &[],
        }
    }

    fn desired_controller_specs(cgroup: &Arc<CgroupNode>) -> Vec<CgroupFileSpec> {
        if cgroup.parent().is_none() {
            return ROOT_CONTROLLER_FILE_SPECS.to_vec();
        }

        let mut specs = Vec::new();
        for controller in Self::available_controllers_for(cgroup) {
            specs.extend_from_slice(Self::controller_specs(controller));
        }
        specs
    }

    fn encode_controller_list(items: &[String]) -> Vec<u8> {
        if items.is_empty() {
            return b"\n".to_vec();
        }
        let mut sorted = items.to_vec();
        sorted.sort();
        let mut line = sorted.join(" ");
        line.push('\n');
        line.into_bytes()
    }

    fn parse_subtree_control_ops(input: &str) -> Result<Vec<(bool, &str)>, SystemError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }

        let mut ops = Vec::new();
        for token in trimmed.split_whitespace() {
            let mut chars = token.chars();
            let op = chars.next().ok_or(SystemError::EINVAL)?;
            let enable = match op {
                '+' => true,
                '-' => false,
                _ => return Err(SystemError::EINVAL),
            };
            let name = chars.as_str();
            if name.is_empty() || name.contains('/') {
                return Err(SystemError::EINVAL);
            }
            ops.push((enable, name));
        }
        Ok(ops)
    }

    fn fold_subtree_control_ops(input: &str) -> Result<HashMap<String, bool>, SystemError> {
        let mut folded = HashMap::new();
        for (enable, name) in Self::parse_subtree_control_ops(input)? {
            if !Self::is_known_controller(name) {
                return Err(SystemError::EINVAL);
            }
            folded.insert(name.to_string(), enable);
        }
        Ok(folded)
    }

    fn encode_pids_max(limit: Option<usize>) -> Vec<u8> {
        match limit {
            Some(v) => format!("{}\n", v).into_bytes(),
            None => b"max\n".to_vec(),
        }
    }

    fn parse_pids_max(input: &str) -> Result<Option<usize>, SystemError> {
        let trimmed = input.trim();
        if trimmed == "max" {
            return Ok(None);
        }
        let value = trimmed.parse::<u64>().map_err(|_| SystemError::EINVAL)?;
        let value = usize::try_from(value).map_err(|_| SystemError::EINVAL)?;
        Ok(Some(value))
    }

    fn encode_max_u64(value: Option<u64>) -> Vec<u8> {
        match value {
            Some(v) => format!("{}\n", v).into_bytes(),
            None => b"max\n".to_vec(),
        }
    }

    fn parse_max_u64(input: &str) -> Result<Option<u64>, SystemError> {
        let trimmed = input.trim();
        if trimmed == "max" {
            return Ok(None);
        }
        let value = trimmed.parse::<u64>().map_err(|_| SystemError::EINVAL)?;
        Ok(Some(value))
    }

    fn encode_cpu_max(quota: Option<u64>, period_us: u64) -> Vec<u8> {
        match quota {
            Some(quota) => format!("{} {}\n", quota, period_us).into_bytes(),
            None => format!("max {}\n", period_us).into_bytes(),
        }
    }

    fn parse_cpu_max(
        input: &str,
        current_period_us: u64,
    ) -> Result<(Option<u64>, u64), SystemError> {
        let mut parts = input.split_whitespace();
        let quota_raw = parts.next().ok_or(SystemError::EINVAL)?;
        let quota = if quota_raw == "max" {
            None
        } else {
            Some(quota_raw.parse::<u64>().map_err(|_| SystemError::EINVAL)?)
        };
        let period = match parts.next() {
            Some(raw) => raw.parse::<u64>().map_err(|_| SystemError::EINVAL)?,
            None => current_period_us,
        };
        if parts.next().is_some() || period == 0 {
            return Err(SystemError::EINVAL);
        }
        Ok((quota, period))
    }

    fn cpu_stat() -> Vec<u8> {
        // P1 exposes Linux-compatible cgroup v2 files, but CPU accounting
        // and bandwidth enforcement are not wired to the scheduler yet.
        b"usage_usec 0\nuser_usec 0\nsystem_usec 0\nnr_periods 0\nnr_throttled 0\nthrottled_usec 0\n"
            .to_vec()
    }

    fn memory_events() -> Vec<u8> {
        b"low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\noom_group_kill 0\n".to_vec()
    }

    fn memory_stat() -> Vec<u8> {
        // P1 keeps memory controller knobs as compat state only. The keys
        // mirror common Linux v2 memory.stat names while all counters stay 0.
        b"anon 0\nfile 0\nkernel_stack 0\npagetables 0\npercpu 0\nsock 0\nshmem 0\nfile_mapped 0\nfile_dirty 0\nfile_writeback 0\nswapcached 0\nanon_thp 0\nfile_thp 0\nshmem_thp 0\ninactive_anon 0\nactive_anon 0\ninactive_file 0\nactive_file 0\nunevictable 0\nslab_reclaimable 0\nslab_unreclaimable 0\nslab 0\nworkingset_refault_anon 0\nworkingset_refault_file 0\nworkingset_activate_anon 0\nworkingset_activate_file 0\nworkingset_restore_anon 0\nworkingset_restore_file 0\nworkingset_nodereclaim 0\npgfault 0\npgmajfault 0\npgrefill 0\npgscan 0\npgsteal 0\npgactivate 0\npgdeactivate 0\npglazyfree 0\npglazyfreed 0\nthp_fault_alloc 0\nthp_collapse_alloc 0\n"
            .to_vec()
    }

    fn memory_swap_events() -> Vec<u8> {
        b"high 0\nmax 0\nfail 0\n".to_vec()
    }

    fn replace_file_data(this: &Arc<Cgroup2Inode>, new_data: &[u8]) -> Result<(), SystemError> {
        let mut inner = this.inner.lock();
        match &mut inner.kind {
            Cgroup2InodeKind::File { data, .. } => {
                data.clear();
                data.extend_from_slice(new_data);
                inner.metadata.size = data.len() as i64;
                Ok(())
            }
            _ => Err(SystemError::EISDIR),
        }
    }

    fn validate_enable_controller(cgroup: &Arc<CgroupNode>, name: &str) -> Result<(), SystemError> {
        let available = Self::available_controllers_for(cgroup);
        if !available.contains(&name) {
            return Err(SystemError::ENOENT);
        }

        if DOMAIN_CONTROLLERS.contains(&name) && cgroup.parent().is_some() && cgroup.has_tasks() {
            return Err(SystemError::EBUSY);
        }
        Ok(())
    }

    fn apply_subtree_control(
        cgroup: &Arc<CgroupNode>,
        input: &str,
    ) -> Result<Vec<u8>, SystemError> {
        let ops = Self::fold_subtree_control_ops(input)?;
        let mut enabled: HashSet<String> = cgroup.subtree_control().into_iter().collect();

        for (name, is_enable) in ops {
            if is_enable {
                if enabled.contains(&name) {
                    continue;
                }
                Self::validate_enable_controller(cgroup, &name)?;
                enabled.insert(name);
            } else {
                for child in cgroup.children() {
                    if child.subtree_control().iter().any(|ctrl| ctrl == &name) {
                        return Err(SystemError::EBUSY);
                    }
                }
                enabled.remove(&name);
            }
        }

        cgroup.set_subtree_control(enabled.clone());
        let mut out: Vec<String> = enabled.into_iter().collect();
        out.sort();
        Ok(Self::encode_controller_list(&out))
    }

    fn new_dir(name: String, cgroup: Arc<CgroupNode>) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            self_ref: weak.clone(),
            fs: RwSem::new(Weak::new()),
            inner: SpinLock::new(Cgroup2InodeInner {
                parent: Weak::new(),
                metadata: Metadata {
                    size: 0,
                    mode: InodeMode::from_bits_truncate(0o755),
                    uid: 0,
                    gid: 0,
                    blk_size: CGROUP2_BLOCK_SIZE as usize,
                    blocks: 0,
                    atime: PosixTimeSpec::default(),
                    mtime: PosixTimeSpec::default(),
                    ctime: PosixTimeSpec::default(),
                    btime: PosixTimeSpec::default(),
                    dev_id: 0,
                    inode_id: generate_inode_id(),
                    file_type: FileType::Dir,
                    nlinks: 2,
                    raw_dev: Default::default(),
                    flags: InodeFlags::empty(),
                },
                name,
                kind: Cgroup2InodeKind::Dir {
                    cgroup,
                    children: HashMap::new(),
                },
            }),
        })
    }

    fn new_file(
        name: String,
        cgroup: Arc<CgroupNode>,
        ty: CgroupCoreFile,
        init: &[u8],
        mode: u16,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            self_ref: weak.clone(),
            fs: RwSem::new(Weak::new()),
            inner: SpinLock::new(Cgroup2InodeInner {
                parent: Weak::new(),
                metadata: Metadata {
                    size: init.len() as i64,
                    mode: InodeMode::from_bits_truncate(mode as u32),
                    uid: 0,
                    gid: 0,
                    blk_size: CGROUP2_BLOCK_SIZE as usize,
                    blocks: 0,
                    atime: PosixTimeSpec::default(),
                    mtime: PosixTimeSpec::default(),
                    ctime: PosixTimeSpec::default(),
                    btime: PosixTimeSpec::default(),
                    dev_id: 0,
                    inode_id: generate_inode_id(),
                    file_type: FileType::File,
                    nlinks: 1,
                    raw_dev: Default::default(),
                    flags: InodeFlags::empty(),
                },
                name,
                kind: Cgroup2InodeKind::File {
                    cgroup,
                    ty,
                    data: init.to_vec(),
                },
            }),
        })
    }

    fn add_child(
        parent: &Arc<Cgroup2Inode>,
        name: &str,
        child: Arc<Cgroup2Inode>,
    ) -> Result<(), SystemError> {
        let fs_weak = parent.fs.read().clone();
        *child.fs.write() = fs_weak;
        child.inner.lock().parent = Arc::downgrade(parent);

        let mut inner = parent.inner.lock();
        match &mut inner.kind {
            Cgroup2InodeKind::Dir { children, .. } => {
                children.insert(name.to_string(), child);
                Ok(())
            }
            _ => Err(SystemError::ENOTDIR),
        }
    }

    fn add_file_from_spec(
        dir: &Arc<Cgroup2Inode>,
        cgroup: Arc<CgroupNode>,
        spec: CgroupFileSpec,
    ) -> Result<(), SystemError> {
        let file =
            Cgroup2Inode::new_file(spec.name.to_string(), cgroup, spec.ty, spec.init, spec.mode);
        Self::add_child(dir, spec.name, file)
    }

    fn sync_controller_files(dir: &Arc<Cgroup2Inode>) -> Result<(), SystemError> {
        let (cgroup, desired) = {
            let inner = dir.inner.lock();
            match &inner.kind {
                Cgroup2InodeKind::Dir { cgroup, .. } => {
                    (cgroup.clone(), Self::desired_controller_specs(cgroup))
                }
                _ => return Err(SystemError::ENOTDIR),
            }
        };

        let desired_names: HashSet<String> =
            desired.iter().map(|spec| spec.name.to_string()).collect();
        {
            let mut inner = dir.inner.lock();
            if let Cgroup2InodeKind::Dir { children, .. } = &mut inner.kind {
                children.retain(|_, child| {
                    let child_inner = child.inner.lock();
                    match &child_inner.kind {
                        Cgroup2InodeKind::File { ty, .. } => {
                            ty.controller().is_none() || desired_names.contains(&child_inner.name)
                        }
                        Cgroup2InodeKind::Dir { .. } => true,
                    }
                });
            } else {
                return Err(SystemError::ENOTDIR);
            }
        }

        for spec in desired {
            let exists = {
                let inner = dir.inner.lock();
                match &inner.kind {
                    Cgroup2InodeKind::Dir { children, .. } => children.contains_key(spec.name),
                    _ => return Err(SystemError::ENOTDIR),
                }
            };
            if !exists {
                Self::add_file_from_spec(dir, cgroup.clone(), spec)?;
            }
        }

        Ok(())
    }

    fn sync_cached_child_controller_files(dir: &Arc<Cgroup2Inode>) -> Result<(), SystemError> {
        let cached_dirs = {
            let inner = dir.inner.lock();
            match &inner.kind {
                Cgroup2InodeKind::Dir { children, .. } => children
                    .values()
                    .filter(|child| {
                        let child_inner = child.inner.lock();
                        matches!(&child_inner.kind, Cgroup2InodeKind::Dir { .. })
                    })
                    .cloned()
                    .collect::<Vec<_>>(),
                _ => return Err(SystemError::ENOTDIR),
            }
        };

        for child in cached_dirs {
            Self::sync_controller_files(&child)?;
        }
        Ok(())
    }

    fn lookup_child(
        parent: &Arc<Cgroup2Inode>,
        name: &str,
    ) -> Result<Arc<Cgroup2Inode>, SystemError> {
        if name == "." {
            return Ok(parent.clone());
        }
        if name == ".." {
            return Ok(parent
                .inner
                .lock()
                .parent
                .upgrade()
                .unwrap_or_else(|| parent.clone()));
        }

        Self::prune_stale_dir_cache(parent)?;
        Self::sync_controller_files(parent)?;

        {
            let inner = parent.inner.lock();
            match &inner.kind {
                Cgroup2InodeKind::Dir { children, .. } => {
                    if let Some(inode) = children.get(name).cloned() {
                        return Ok(inode);
                    }
                }
                _ => return Err(SystemError::ENOTDIR),
            }
        }

        let parent_cgroup = {
            let inner = parent.inner.lock();
            match &inner.kind {
                Cgroup2InodeKind::Dir { cgroup, .. } => cgroup.clone(),
                _ => return Err(SystemError::ENOTDIR),
            }
        };

        let child_cgroup = parent_cgroup.child(name).ok_or(SystemError::ENOENT)?;
        let child = Cgroup2Inode::new_dir(name.to_string(), child_cgroup);
        Cgroup2Inode::add_child(parent, name, child.clone())?;
        Cgroup2Inode::populate_core_files(&child)?;
        //以父目录当前缓存为准
        let inner = parent.inner.lock();
        match &inner.kind {
            Cgroup2InodeKind::Dir { children, .. } => {
                children.get(name).cloned().ok_or(SystemError::ENOENT)
            }
            _ => Err(SystemError::ENOTDIR),
        }
    }

    fn populate_core_files(dir: &Arc<Cgroup2Inode>) -> Result<(), SystemError> {
        let cgroup = {
            let inner = dir.inner.lock();
            match &inner.kind {
                Cgroup2InodeKind::Dir { cgroup, .. } => cgroup.clone(),
                _ => return Err(SystemError::ENOTDIR),
            }
        };

        for spec in BASE_FILE_SPECS {
            Self::add_file_from_spec(dir, cgroup.clone(), spec)?;
        }

        if cgroup.parent().is_some() {
            for spec in NON_ROOT_CORE_FILE_SPECS {
                Self::add_file_from_spec(dir, cgroup.clone(), spec)?;
            }
        }

        Self::sync_controller_files(dir)?;

        Ok(())
    }

    /// Check if a cgroup has any tasks or populated descendants.
    ///
    /// Uses depth-limited recursion to prevent stack overflow.
    /// 判断 cgroup 及其子树是否包含任务
    /// 利用 subtree_task_counter 实现 O(1) 查询
    fn is_populated(cgroup: &Arc<CgroupNode>) -> bool {
        cgroup.has_tasks() || cgroup.subtree_task_counter().load(Ordering::Acquire) > 0
    }

    fn read_file(
        inner: &mut Cgroup2InodeInner,
        offset: usize,
        len: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        let bytes = match &mut inner.kind {
            Cgroup2InodeKind::File {
                cgroup,
                ty,
                data: _,
            } => match ty {
                CgroupCoreFile::Procs => {
                    let mut lines = String::new();
                    for pid in cgroup.tasks() {
                        lines.push_str(&format!("{}\n", pid.data()));
                    }
                    lines.into_bytes()
                }
                CgroupCoreFile::Controllers => {
                    let items: Vec<String> = Self::available_controllers_for(cgroup)
                        .into_iter()
                        .map(|s| s.to_string())
                        .collect();
                    Self::encode_controller_list(&items)
                }
                CgroupCoreFile::SubtreeControl => {
                    let items = cgroup.subtree_control();
                    Self::encode_controller_list(&items)
                }
                CgroupCoreFile::Events => {
                    let populated = if Self::is_populated(cgroup) { 1 } else { 0 };
                    format!("populated {}\nfrozen 0\n", populated).into_bytes()
                }
                CgroupCoreFile::Type => b"domain\n".to_vec(),
                CgroupCoreFile::Freeze => {
                    format!("{}\n", if cgroup.freeze_requested() { 1 } else { 0 }).into_bytes()
                }
                CgroupCoreFile::CpuStat => Self::cpu_stat(),
                CgroupCoreFile::CpuWeight => {
                    format!("{}\n", cgroup.cpu_state().weight()).into_bytes()
                }
                CgroupCoreFile::CpuMax => {
                    let (quota, period) = cgroup.cpu_state().max();
                    Self::encode_cpu_max(quota, period)
                }
                CgroupCoreFile::MemoryCurrent | CgroupCoreFile::MemoryPeak => b"0\n".to_vec(),
                CgroupCoreFile::MemoryMin => Self::encode_max_u64(cgroup.memory_state().min()),
                CgroupCoreFile::MemoryLow => Self::encode_max_u64(cgroup.memory_state().low()),
                CgroupCoreFile::MemoryHigh => Self::encode_max_u64(cgroup.memory_state().high()),
                CgroupCoreFile::MemoryMax => Self::encode_max_u64(cgroup.memory_state().max()),
                CgroupCoreFile::MemoryEvents => Self::memory_events(),
                CgroupCoreFile::MemoryStat => Self::memory_stat(),
                CgroupCoreFile::MemorySwapCurrent | CgroupCoreFile::MemorySwapPeak => {
                    b"0\n".to_vec()
                }
                CgroupCoreFile::MemorySwapHigh => {
                    Self::encode_max_u64(cgroup.memory_state().swap_high())
                }
                CgroupCoreFile::MemorySwapMax => {
                    Self::encode_max_u64(cgroup.memory_state().swap_max())
                }
                CgroupCoreFile::MemorySwapEvents => Self::memory_swap_events(),
                CgroupCoreFile::PidsCurrent => {
                    format!("{}\n", cgroup.pids_current_count()).into_bytes()
                }
                CgroupCoreFile::PidsMax => Self::encode_pids_max(cgroup.pids_max()),
                CgroupCoreFile::PidsEvents => {
                    format!("max {}\n", cgroup.pids_events_max()).into_bytes()
                }
            },
            _ => return Err(SystemError::EISDIR),
        };

        let start = core::cmp::min(offset, bytes.len());
        let end = core::cmp::min(offset + len, bytes.len());
        let n = end.saturating_sub(start);
        if n > buf.len() {
            return Err(SystemError::ENOBUFS);
        }
        buf[..n].copy_from_slice(&bytes[start..end]);
        Ok(n)
    }

    // Writes must not keep the inode's inner lock across permission checks or
    // task migration, otherwise a write to cgroup.procs can re-enter metadata()
    // on the same inode and self-deadlock.
    fn write_file(
        this: &Arc<Cgroup2Inode>,
        offset: usize,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        let (cgroup, ty) = {
            let inner = this.inner.lock();
            match &inner.kind {
                Cgroup2InodeKind::File { cgroup, ty, .. } => (cgroup.clone(), *ty),
                _ => return Err(SystemError::EISDIR),
            }
        };

        match ty {
            CgroupCoreFile::Procs => {
                if offset != 0 {
                    return Err(SystemError::EINVAL);
                }
                let input = core::str::from_utf8(buf).map_err(|_| SystemError::EINVAL)?;
                let pid_str = input.trim();
                let current = ProcessManager::current_pcb();
                let task = if pid_str == "0" {
                    current.clone()
                } else {
                    let pid_num = pid_str.parse::<usize>().map_err(|_| SystemError::EINVAL)?;
                    ProcessManager::find_task_by_vpid(crate::process::RawPid::new(pid_num))
                        .ok_or(SystemError::ESRCH)?
                };
                let src = task.task_cgroup_node();
                let fs_nsdelegate = this
                    .fs()
                    .as_any_ref()
                    .downcast_ref::<Cgroup2Fs>()
                    .map(|fs| fs.nsdelegate())
                    .unwrap_or(false);
                if fs_nsdelegate {
                    let ns_root = current.nsproxy().cgroup_ns.root_cgroup().clone();
                    if !ns_root.is_ancestor_of(&cgroup) {
                        return Err(SystemError::ENOENT);
                    }
                    if !ns_root.is_ancestor_of(&src) {
                        return Err(SystemError::ENOENT);
                    }
                }
                if Arc::ptr_eq(&src, &cgroup) {
                    return Ok(buf.len());
                }
                Self::check_attach_permissions(this.fs().root_inode(), &src, &cgroup)?;
                let leader = {
                    let ti = task.threads_read_irqsave();
                    ti.group_leader().unwrap_or_else(|| task.clone())
                };
                let others = leader.threads_read_irqsave().group_tasks_clone();

                let mut to_move = Vec::new();
                if !leader.is_exited() {
                    to_move.push(leader.clone());
                }
                for weak in others {
                    if let Some(t) = weak.upgrade() {
                        if !t.is_exited() {
                            to_move.push(t);
                        }
                    }
                }
                if to_move.is_empty() {
                    return Err(SystemError::ESRCH);
                }
                let moved_tasks = to_move.len();

                let _cgroup_guard = cgroup_accounting_lock().lock();
                cgroup_migrate_vet_dst_with_src(&src, &cgroup, moved_tasks)?;

                for t in to_move {
                    t.set_task_cgroup_node(cgroup.clone());
                }
                Ok(buf.len())
            }
            CgroupCoreFile::SubtreeControl => {
                if offset != 0 {
                    return Err(SystemError::EINVAL);
                }
                let input = core::str::from_utf8(buf).map_err(|_| SystemError::EINVAL)?;
                if !ProcessManager::current_pcb().cred().has_cap_sys_admin() {
                    return Err(SystemError::EPERM);
                }
                let dir = this
                    .inner
                    .lock()
                    .parent
                    .upgrade()
                    .ok_or(SystemError::ENOENT)?;
                let _cgroup_guard = cgroup_accounting_lock().lock();
                let new_data = Self::apply_subtree_control(&cgroup, input)?;
                Self::replace_file_data(this, &new_data)?;
                Self::sync_cached_child_controller_files(&dir)?;
                Ok(buf.len())
            }
            CgroupCoreFile::Freeze => {
                if offset != 0 {
                    return Err(SystemError::EINVAL);
                }
                let input = core::str::from_utf8(buf).map_err(|_| SystemError::EINVAL)?;
                let value = input
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| SystemError::EINVAL)?;
                if value > 1 {
                    return Err(SystemError::ERANGE);
                }
                cgroup.set_freeze_requested(value == 1);
                let new_data = format!("{}\n", value).into_bytes();
                Self::replace_file_data(this, &new_data)?;
                Ok(buf.len())
            }
            CgroupCoreFile::CpuWeight => {
                if offset != 0 {
                    return Err(SystemError::EINVAL);
                }
                let input = core::str::from_utf8(buf).map_err(|_| SystemError::EINVAL)?;
                let weight = input
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| SystemError::EINVAL)?;
                if !(1..=10_000).contains(&weight) {
                    return Err(SystemError::ERANGE);
                }
                cgroup.set_cpu_weight(weight);
                let new_data = format!("{}\n", weight).into_bytes();
                Self::replace_file_data(this, &new_data)?;
                Ok(buf.len())
            }
            CgroupCoreFile::CpuMax => {
                if offset != 0 {
                    return Err(SystemError::EINVAL);
                }
                let input = core::str::from_utf8(buf).map_err(|_| SystemError::EINVAL)?;
                let (_, current_period) = cgroup.cpu_state().max();
                let (quota, period) = Self::parse_cpu_max(input, current_period)?;
                cgroup.set_cpu_max(quota, period);
                let new_data = Self::encode_cpu_max(quota, period);
                Self::replace_file_data(this, &new_data)?;
                Ok(buf.len())
            }
            CgroupCoreFile::MemoryMin
            | CgroupCoreFile::MemoryLow
            | CgroupCoreFile::MemoryHigh
            | CgroupCoreFile::MemoryMax
            | CgroupCoreFile::MemorySwapHigh
            | CgroupCoreFile::MemorySwapMax => {
                if offset != 0 {
                    return Err(SystemError::EINVAL);
                }
                let input = core::str::from_utf8(buf).map_err(|_| SystemError::EINVAL)?;
                let value = Self::parse_max_u64(input)?;
                match ty {
                    CgroupCoreFile::MemoryMin => cgroup.set_memory_min(value),
                    CgroupCoreFile::MemoryLow => cgroup.set_memory_low(value),
                    CgroupCoreFile::MemoryHigh => cgroup.set_memory_high(value),
                    CgroupCoreFile::MemoryMax => cgroup.set_memory_max(value),
                    CgroupCoreFile::MemorySwapHigh => cgroup.set_memory_swap_high(value),
                    CgroupCoreFile::MemorySwapMax => cgroup.set_memory_swap_max(value),
                    _ => unreachable!(),
                }
                let new_data = Self::encode_max_u64(value);
                Self::replace_file_data(this, &new_data)?;
                Ok(buf.len())
            }
            CgroupCoreFile::PidsMax => {
                if offset != 0 {
                    return Err(SystemError::EINVAL);
                }
                let input = core::str::from_utf8(buf).map_err(|_| SystemError::EINVAL)?;
                let new_limit = Self::parse_pids_max(input)?;
                cgroup.set_pids_max(new_limit);
                let new_data = Self::encode_pids_max(new_limit);
                Self::replace_file_data(this, &new_data)?;
                Ok(buf.len())
            }
            CgroupCoreFile::Controllers
            | CgroupCoreFile::Events
            | CgroupCoreFile::Type
            | CgroupCoreFile::CpuStat
            | CgroupCoreFile::MemoryCurrent
            | CgroupCoreFile::MemoryPeak
            | CgroupCoreFile::MemoryEvents
            | CgroupCoreFile::MemoryStat
            | CgroupCoreFile::MemorySwapCurrent
            | CgroupCoreFile::MemorySwapPeak
            | CgroupCoreFile::MemorySwapEvents
            | CgroupCoreFile::PidsCurrent
            | CgroupCoreFile::PidsEvents => Err(SystemError::EPERM),
        }
    }
}

pub fn cgroup2_check_attach_permissions(
    fs_root: Arc<dyn IndexNode>,
    src_cgroup: &Arc<CgroupNode>,
    dst_cgroup: &Arc<CgroupNode>,
) -> Result<(), SystemError> {
    Cgroup2Inode::check_attach_permissions(fs_root, src_cgroup, dst_cgroup)
}
//转换inode为cgroup node
pub fn cgroup2_inode_to_node(inode: &Arc<dyn IndexNode>) -> Result<Arc<CgroupNode>, SystemError> {
    let cgroup_inode = inode
        .as_any_ref()
        .downcast_ref::<Cgroup2Inode>()
        .ok_or(SystemError::EINVAL)?;
    let inner = cgroup_inode.inner.lock();
    match &inner.kind {
        Cgroup2InodeKind::Dir { cgroup, .. } => Ok(cgroup.clone()),
        _ => Err(SystemError::ENOTDIR),
    }
}

impl FileSystem for Cgroup2Fs {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        self.root_inode.clone()
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            blk_dev_id: 0,
            max_name_len: CGROUP2_MAX_NAMELEN,
        }
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "cgroup2"
    }

    fn super_block(&self) -> SuperBlock {
        SuperBlock::new(
            Magic::CGROUP2_SUPER_MAGIC,
            CGROUP2_BLOCK_SIZE,
            CGROUP2_MAX_NAMELEN as u64,
        )
    }
}

impl MountableFileSystem for Cgroup2Fs {
    fn make_mount_data(
        raw_data: Option<&str>,
        _source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        let mut nsdelegate = false;
        if let Some(opts) = raw_data {
            for raw in opts.split(',') {
                let token = raw.trim();
                if token.is_empty() {
                    continue;
                }
                match token {
                    "nsdelegate" => nsdelegate = true,
                    "nsdelegate=0" => nsdelegate = false,
                    "nsdelegate=1" => nsdelegate = true,
                    _ => return Err(SystemError::EINVAL),
                }
            }
        }

        let root_cgroup = ProcessManager::current_pcb()
            .nsproxy()
            .cgroup_ns
            .root_cgroup()
            .clone();
        Ok(Some(Arc::new(Cgroup2MountData {
            root_cgroup,
            nsdelegate,
        })))
    }

    fn make_fs(
        data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let mount_data = data.and_then(|d| d.as_any().downcast_ref::<Cgroup2MountData>());
        let root_cgroup = mount_data
            .map(|d| d.root_cgroup.clone())
            .unwrap_or_else(|| cgroup_root().root());
        let nsdelegate = mount_data.map(|d| d.nsdelegate).unwrap_or(false);
        Ok(Cgroup2Fs::new(root_cgroup, nsdelegate))
    }
}

impl IndexNode for Cgroup2Inode {
    fn open(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        Ok(())
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let mut inner = self.inner.lock();
        Cgroup2Inode::read_file(&mut inner, offset, len, buf)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let n = core::cmp::min(len, buf.len());
        let this = self.self_ref.upgrade().unwrap();
        Cgroup2Inode::write_file(&this, offset, &buf[..n])
    }

    fn resize(&self, len: usize) -> Result<(), SystemError> {
        match &self.inner.lock().kind {
            Cgroup2InodeKind::File { .. } if len == 0 => Ok(()),
            Cgroup2InodeKind::File { .. } => Err(SystemError::EINVAL),
            Cgroup2InodeKind::Dir { .. } => Err(SystemError::EISDIR),
        }
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(self.inner.lock().metadata.clone())
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        self.inner.lock().metadata = metadata.clone();
        Ok(())
    }

    fn create(
        &self,
        name: &str,
        file_type: FileType,
        _mode: InodeMode,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if file_type != FileType::Dir {
            return Err(SystemError::EINVAL);
        }

        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains('/')
            || name.contains('\n')
        {
            return Err(SystemError::EINVAL);
        }

        let this = self.self_ref.upgrade().unwrap();
        Cgroup2Inode::prune_stale_dir_cache(&this)?;

        let cgroup = {
            let inner = self.inner.lock();
            match &inner.kind {
                Cgroup2InodeKind::Dir { cgroup, children } => {
                    if children.contains_key(name) || cgroup.child(name).is_some() {
                        return Err(SystemError::EEXIST);
                    }
                    cgroup.clone()
                }
                _ => return Err(SystemError::ENOTDIR),
            }
        };

        let child_cgroup = cgroup_root().create_child(&cgroup, name)?;
        let child = Cgroup2Inode::new_dir(name.to_string(), child_cgroup);
        Cgroup2Inode::add_child(&this, name, child.clone())?;
        Cgroup2Inode::populate_core_files(&child)?;
        Ok(child)
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        if name == "." || name == ".." || name.starts_with("cgroup.") {
            return Err(SystemError::EINVAL);
        }
        let this = self.self_ref.upgrade().unwrap();
        Cgroup2Inode::prune_stale_dir_cache(&this)?;
        let child = Cgroup2Inode::lookup_child(&this, name)?;

        // 先检查 child 状态，与 parent lock 解耦以避免 ABBA 死锁
        let _cgroup = {
            let inner = child.inner.lock();
            match &inner.kind {
                Cgroup2InodeKind::Dir { cgroup, .. } => {
                    if cgroup.has_children() {
                        return Err(SystemError::ENOTEMPTY);
                    }
                    if cgroup.has_tasks() {
                        return Err(SystemError::EBUSY);
                    }
                    cgroup.clone()
                }
                _ => return Err(SystemError::ENOTDIR),
            }
        }; // child lock 在此释放

        // 再按 parent -> child 的顺序获取 parent lock 执行删除
        {
            let parent_cgroup = this.inner.lock();
            if let Cgroup2InodeKind::Dir { cgroup: p, .. } = &parent_cgroup.kind {
                cgroup_root().remove_child(p, name)?;
            }
        }

        let mut inner = self.inner.lock();
        match &mut inner.kind {
            Cgroup2InodeKind::Dir { children, .. } => {
                children.remove(name);
                Ok(())
            }
            _ => Err(SystemError::ENOTDIR),
        }
    }

    fn unlink(&self, _name: &str) -> Result<(), SystemError> {
        // cgroup core files are always present and managed by kernel.
        Err(SystemError::EPERM)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        let this = self.self_ref.upgrade().unwrap();
        let inode = Cgroup2Inode::lookup_child(&this, name)?;
        Ok(inode)
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.fs.read().upgrade().unwrap()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        let this = self.self_ref.upgrade().unwrap();
        Cgroup2Inode::prune_stale_dir_cache(&this)?;
        Cgroup2Inode::sync_controller_files(&this)?;
        let inner = self.inner.lock();
        match &inner.kind {
            Cgroup2InodeKind::Dir { cgroup, children } => {
                let mut names = vec![".".to_string(), "..".to_string()];
                for child in cgroup.children_names() {
                    if !names.iter().any(|n| n == &child) {
                        names.push(child);
                    }
                }
                names.extend(children.keys().cloned());
                names.sort();
                names.dedup();
                Ok(names)
            }
            _ => Err(SystemError::ENOTDIR),
        }
    }

    fn dname(&self) -> Result<crate::filesystem::vfs::utils::DName, SystemError> {
        Ok(self.inner.lock().name.clone().into())
    }
}

register_mountable_fs!(Cgroup2Fs, CGROUP2FSMAKER, "cgroup2");
