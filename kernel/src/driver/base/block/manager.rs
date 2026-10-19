use core::{fmt::Formatter, sync::atomic::AtomicU32};

use alloc::{string::ToString, sync::Arc, vec::Vec};
use hashbrown::HashMap;
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    driver::base::{
        block::gendisk::GenDisk,
        device::{
            device_number::{DeviceNumber, Major},
            DevName,
        },
        kobject::KObject,
    },
    filesystem::{
        devfs::{devfs_register, devfs_unregister},
        vfs::{utils::DName, IndexNode},
    },
    init::initcall::INITCALL_POSTCORE,
    libs::{
        mutex::{Mutex, MutexGuard},
        rwsem::RwSem,
    },
};

use super::{
    block_device::{BlockDevice, GeneralBlockRange},
    gendisk::GenDiskMap,
};

mod partition_table;

static mut BLOCK_DEV_MANAGER: Option<BlockDevManager> = None;

#[inline]
pub fn block_dev_manager() -> &'static BlockDevManager {
    unsafe { BLOCK_DEV_MANAGER.as_ref().unwrap() }
}

#[unified_init(INITCALL_POSTCORE)]
pub fn block_dev_manager_init() -> Result<(), SystemError> {
    unsafe {
        BLOCK_DEV_MANAGER = Some(BlockDevManager::new());
    }
    Ok(())
}

/// 磁盘设备管理器
pub struct BlockDevManager {
    inner: Mutex<InnerBlockDevManager>,
}

struct InnerBlockDevManager {
    disks: HashMap<DevName, Arc<dyn BlockDevice>>,
    /// 记录每个major对应的下一个可用的minor号
    minors: HashMap<Major, AtomicU32>,
}
impl BlockDevManager {
    pub fn new() -> Self {
        BlockDevManager {
            inner: Mutex::new(InnerBlockDevManager {
                disks: HashMap::new(),
                minors: HashMap::new(),
            }),
        }
    }

    fn inner(&self) -> MutexGuard<'_, InnerBlockDevManager> {
        self.inner.lock()
    }

    /// Retain a published disk by its native device number. The returned
    /// disk may be queried only after the registry and metadata locks drop:
    /// its backend can itself depend on another registered block device.
    pub fn lookup_gendisk_by_device_number(&self, number: DeviceNumber) -> Option<Arc<GenDisk>> {
        let inner = self.inner();
        for device in inner.disks.values() {
            let metadata = device.blkdev_meta().inner();
            for disk in metadata.gendisks.values() {
                if disk.device_num() == number {
                    return Some(disk.clone());
                }
            }
        }
        None
    }

    /// 注册磁盘设备
    pub fn register(&self, dev: Arc<dyn BlockDevice>) -> Result<(), SystemError> {
        // A node can be looked up by device number before devfs publication
        // finishes. Keep its open admission behind the same namespace gate.
        let _raw_gate = dev.blkdev_meta().raw_cache_gate.write();
        let dev_name = dev.dev_name();
        {
            let inner = self.inner();
            if inner.disks.contains_key(dev_name) {
                return Err(SystemError::EEXIST);
            }
        }

        let gendisks = self.prepare_gendisks(&dev)?;

        {
            let mut inner = self.inner();
            if inner.disks.contains_key(dev_name) {
                return Err(SystemError::EEXIST);
            }
            inner.disks.insert(dev_name.clone(), dev.clone());
        }

        if let Err(e) = self.publish_gendisks(&dev, &gendisks) {
            let mut inner = self.inner();
            inner.disks.remove(dev_name);
            drop(inner);
            dev.blkdev_meta().inner().gendisks.clear();
            return Err(e);
        }

        Ok(())
    }

    /// Retain every node across a mapping/namespace transition. The caller
    /// holds this device's raw_cache_gate exclusively; no metadata lock spans
    /// allocation, cache writeback, devfs work or backend I/O.
    pub(crate) fn snapshot_raw_nodes_locked(
        &self,
        dev: &dyn BlockDevice,
    ) -> Result<Vec<Arc<GenDisk>>, SystemError> {
        let count = dev.blkdev_meta().inner().gendisks.len();
        let mut nodes = Vec::new();
        nodes
            .try_reserve_exact(count)
            .map_err(|_| SystemError::ENOMEM)?;
        nodes.extend(dev.blkdev_meta().inner().gendisks.values().cloned());
        Ok(nodes)
    }

    /// Drop only partition nodes. A failed devfs removal restores the same
    /// node identities; neither part0 nor the driver's binding is removed.
    /// The caller holds raw_cache_gate.write(), including through rollback.
    pub(crate) fn remove_partitions_locked(
        &self,
        dev: &Arc<dyn BlockDevice>,
    ) -> Result<(), SystemError> {
        let mut nodes = self.snapshot_raw_nodes_locked(dev.as_ref())?;
        nodes.retain(|node| node.idx() != GenDisk::ENTIRE_DISK_IDX);
        if nodes.iter().any(|node| node.has_node_holders()) {
            return Err(SystemError::EBUSY);
        }
        for node in &nodes {
            node.set_online_locked(false);
        }
        for (index, node) in nodes.iter().enumerate() {
            let name = node.dname()?;
            if let Err(error) = devfs_unregister(name.as_ref(), node.clone()) {
                for old in &nodes[..index] {
                    if let Ok(name) = old.dname() {
                        if let Err(rollback) = devfs_register(name.as_ref(), old.clone()) {
                            log::error!(
                                "Failed to restore partition {}: {:?}",
                                name.as_ref(),
                                rollback
                            );
                        }
                    }
                }
                for old in &nodes {
                    old.set_online_locked(true);
                }
                return Err(error);
            }
        }
        {
            let mut metadata = dev.blkdev_meta().inner();
            for node in &nodes {
                metadata.gendisks.remove(&node.idx());
            }
        }
        for node in nodes {
            self.emit_gendisk_uevent(dev, node.dname()?.as_ref(), "remove");
        }
        Ok(())
    }

    /// Runtime partition scan, serialized with mapping changes and partition
    /// open admission by the caller's exclusive raw_cache_gate.
    pub(crate) fn scan_partitions_locked(
        &self,
        dev: &Arc<dyn BlockDevice>,
    ) -> Result<(), SystemError> {
        let nodes = self.snapshot_raw_nodes_locked(dev.as_ref())?;
        if nodes
            .iter()
            .any(|node| node.idx() != GenDisk::ENTIRE_DISK_IDX && node.has_node_holders())
        {
            return Err(SystemError::EBUSY);
        }
        let mut sync_error = None;
        for node in &nodes {
            if let Err(error) = node.sync_raw_cache_locked() {
                sync_error.get_or_insert(error);
            }
        }
        if nodes
            .iter()
            .any(|node| node.has_dirty_raw_cache_locked() || node.has_raw_writeback_locked())
        {
            return Err(sync_error.unwrap_or(SystemError::EBUSY));
        }
        if let Some(error) = sync_error {
            // Linux bdev_disk_changed ignores sync_blockdev's terminal error,
            // after the old requests have completed. Preserve its errseq.
            log::warn!("Block cache sync before partition scan failed: {:?}", error);
        }
        for node in &nodes {
            node.invalidate_raw_cache_locked()?;
        }
        self.remove_partitions_locked(dev)?;
        let partitions = self.prepare_partition_gendisks(dev)?;
        for partition in partitions {
            if let Err(error) = self.publish_gendisk(dev, partition.clone()) {
                log::warn!(
                    "Failed to publish partition {} on {}: {:?}",
                    partition.idx(),
                    dev.dev_name().name(),
                    error
                );
            }
        }
        Ok(())
    }

    /// Detect partitions without holding the global block manager lock.
    fn prepare_gendisks(
        &self,
        dev: &Arc<dyn BlockDevice>,
    ) -> Result<Vec<Arc<GenDisk>>, SystemError> {
        let disk_range = dev.disk_range();
        // Linux publishes part0 (the whole-disk node) before scanning the
        // partition table.  Keep that invariant here: a missing/malformed
        // table or a media read error must not make the disk itself disappear.
        let mut gendisks =
            vec![self.create_gendisk_with_range(dev, disk_range, GenDisk::ENTIRE_DISK_IDX)];

        // A zero-capacity device has no sector 0 to probe.  This is a normal
        // state for an unbound loop device, so let the device reject I/O until
        // media is attached.
        if disk_range.len() == 0 {
            return Ok(gendisks);
        }

        match self.prepare_partition_gendisks(dev) {
            Ok(partitions) => gendisks.extend(partitions),
            Err(SystemError::EINVAL | SystemError::EEXIST) => {}
            Err(err) => {
                log::warn!(
                    "Failed to scan partitions on block device {}: {:?}",
                    dev.dev_name().name(),
                    err
                );
            }
        }
        Ok(gendisks)
    }

    fn prepare_partition_gendisks(
        &self,
        dev: &Arc<dyn BlockDevice>,
    ) -> Result<Vec<Arc<GenDisk>>, SystemError> {
        let descriptors = partition_table::read_partitions(dev.as_ref())?;
        let mut disks = Vec::new();
        disks
            .try_reserve_exact(descriptors.len())
            .map_err(|_| SystemError::ENOMEM)?;
        for descriptor in descriptors {
            disks.push(self.create_gendisk_with_range(dev, descriptor.range, descriptor.index));
        }
        Ok(disks)
    }

    fn create_gendisk_with_range(
        &self,
        dev: &Arc<dyn BlockDevice>,
        range: GeneralBlockRange,
        idx: u32,
    ) -> Arc<GenDisk> {
        let weak_dev = Arc::downgrade(dev);

        // 这里先拿到硬盘的设备名，然后在根据idx来生成gendisk的名字
        // 如果是整个磁盘，则idx为 None，名字为/dev/sda
        // 如果是分区，例如idx为1，则名字为/dev/sda1
        // 以此类推
        let dev_name = dev.dev_name();
        let (idx, dev_name) = match idx {
            GenDisk::ENTIRE_DISK_IDX => (None, DName::from(dev_name.name())),
            id => {
                let separator = if dev_name
                    .name()
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_digit)
                {
                    "p"
                } else {
                    ""
                };
                (
                    Some(id),
                    DName::from(format!("{}{}{}", dev_name.name(), separator, idx)),
                )
            }
        };

        GenDisk::new(weak_dev, range, idx, dev_name)
    }

    fn publish_gendisks(
        &self,
        dev: &Arc<dyn BlockDevice>,
        gendisks: &[Arc<GenDisk>],
    ) -> Result<(), SystemError> {
        let Some((whole_disk, partitions)) = gendisks.split_first() else {
            return Err(SystemError::EINVAL);
        };
        debug_assert_eq!(whole_disk.idx(), GenDisk::ENTIRE_DISK_IDX);
        self.publish_gendisk(dev, whole_disk.clone())?;

        // Linux keeps part0 registered even if an individual partition cannot
        // be added.  Partition nodes are therefore best-effort additions and
        // must not roll back the usable whole-disk node.
        for partition in partitions {
            if let Err(err) = self.publish_gendisk(dev, partition.clone()) {
                log::warn!(
                    "Failed to publish partition {} on block device {}: {:?}",
                    partition.idx(),
                    dev.dev_name().name(),
                    err
                );
            }
        }
        Ok(())
    }

    fn publish_gendisk(
        &self,
        dev: &Arc<dyn BlockDevice>,
        gendisk: Arc<GenDisk>,
    ) -> Result<(), SystemError> {
        let blk_meta = dev.blkdev_meta();
        let idx = gendisk.idx();
        {
            let mut meta_inner = blk_meta.inner();
            if meta_inner.gendisks.contains_key(&idx) {
                return Err(SystemError::EEXIST);
            }

            meta_inner.gendisks.insert(idx, gendisk.clone());
        }

        if let Err(e) = dev.callback_gendisk_registered(&gendisk) {
            blk_meta.inner().gendisks.remove(&idx);
            return Err(e);
        }

        // 注册到devfs
        let dname = gendisk.dname()?;
        devfs_register(dname.as_ref(), gendisk.clone()).map_err(|e| {
            gendisk.set_online_locked(false);
            blk_meta.inner().gendisks.remove(&idx);
            log::error!(
                "Failed to register gendisk {:?} to devfs: {:?}",
                dname.as_ref(),
                e
            );
            e
        })?;
        self.emit_gendisk_uevent(dev, dname.as_ref(), "add");
        Ok(())
    }

    /// 卸载磁盘设备
    pub fn unregister(&self, dev: &Arc<dyn BlockDevice>) -> Result<(), SystemError> {
        // 回归注释（保持原子性/可恢复性）：
        // 这里不能在 devfs_unregister 之前就把设备从 manager 移除、或清空 blk_meta.gendisks。
        // 否则一旦 devfs_unregister 中途失败，会留下不可恢复的不一致状态：
        // - manager 中设备已消失（重试会 ENOENT）
        // - 元数据 gendisks 已清空（无法再找到需要卸载的节点）
        // - devfs 里可能还有残留节点
        //
        // 正确做法是“两阶段”：
        // 1) 锁外尝试卸载所有 devfs 节点（中途失败则回滚已卸载部分，确保可重试）
        // 2) 全部卸载成功后，才清理 manager/元数据

        // 先检查设备是否存在，但不要移除（避免失败后不可重试）
        {
            let inner = self.inner();
            if !inner.disks.contains_key(dev.dev_name()) {
                return Err(SystemError::ENOENT);
            }
        }

        let blk_meta = dev.blkdev_meta();
        let gendisks: Vec<Arc<GenDisk>> = {
            let meta_inner = blk_meta.inner();
            meta_inner.gendisks.values().cloned().collect()
        };

        let mut unregistered: Vec<Arc<GenDisk>> = Vec::new();
        for gendisk in &gendisks {
            let dname = gendisk.dname()?;
            if let Err(e) = devfs_unregister(dname.as_ref(), gendisk.clone()) {
                // 回滚：尽量把已卸载的重新注册回 devfs，保持系统可用/一致。
                for rg in unregistered.into_iter() {
                    if let Ok(rname) = rg.dname() {
                        let _ = devfs_register(rname.as_ref(), rg.clone());
                    }
                }
                return Err(e);
            }
            unregistered.push(gendisk.clone());
        }

        // 全部 devfs 卸载成功：再移除 manager 记录并清空 gendisks 元数据
        let mut inner = self.inner();
        if inner.disks.remove(dev.dev_name()).is_none() {
            // 理论上不应发生：在 devfs 卸载过程中该设备被并发移除
            // 尽力回滚 devfs，以免留下“manager 不存在但 devfs 存在/不存在”的混乱状态
            drop(inner);
            for rg in unregistered.into_iter() {
                if let Ok(rname) = rg.dname() {
                    let _ = devfs_register(rname.as_ref(), rg.clone());
                }
            }
            return Err(SystemError::ENOENT);
        }
        drop(inner);

        for gendisk in &gendisks {
            let dname = gendisk.dname()?;
            self.emit_gendisk_uevent(dev, dname.as_ref(), "remove");
        }

        let mut meta_inner = blk_meta.inner();
        meta_inner.gendisks.clear();
        Ok(())
    }

    /// Unpublish a block device after its hardware has irreversibly gone.
    ///
    /// Unlike [`Self::unregister`], this operation cannot be rolled back. It
    /// first detaches all gendisk metadata, removes every devfs node on a
    /// best-effort basis, and then drops the manager's strong reference so
    /// stale open nodes can only observe a dead weak device rather than keeping
    /// a controller alive. The caller serializes this operation against block
    /// publication and recoverable unregistration for the same device.
    pub fn unregister_detached(&self, dev: &Arc<dyn BlockDevice>) {
        let gendisks = core::mem::take(&mut dev.blkdev_meta().inner().gendisks);

        for gendisk in gendisks.values() {
            match gendisk.dname() {
                Ok(dname) => {
                    if let Err(err) = devfs_unregister(dname.as_ref(), gendisk.clone()) {
                        log::warn!(
                            "Failed to remove detached block node {}: {:?}",
                            dname.as_ref(),
                            err
                        );
                    }
                }
                Err(err) => log::warn!(
                    "Failed to resolve a detached block node for {}: {:?}",
                    dev.dev_name().name(),
                    err
                ),
            }
        }

        self.inner().disks.remove(dev.dev_name());
        for gendisk in gendisks.values() {
            if let Ok(dname) = gendisk.dname() {
                self.emit_gendisk_uevent(dev, dname.as_ref(), "remove");
            }
        }
    }

    fn emit_gendisk_uevent(&self, dev: &Arc<dyn BlockDevice>, devname: &str, action: &str) {
        let kobj = dev.device() as Arc<dyn KObject>;
        let Ok(parent_devpath) = <dyn KObject>::devpath(&kobj) else {
            return;
        };
        let devpath = format!("{parent_devpath}/block/{devname}");
        let _ = <dyn KObject>::emit_uevent(
            action,
            &devpath,
            &[
                ("SUBSYSTEM", "block".into()),
                ("DEVNAME", devname.to_string()),
            ],
        );
    }
    /// 通过路径查找gendisk
    ///
    /// # 参数
    ///
    /// - `path`: 分区路径 `/dev/sda1` 或者 `sda1`，或者是`/dev/sda`
    pub fn lookup_gendisk_by_path(&self, path: &str) -> Option<Arc<GenDisk>> {
        let raw = path.strip_prefix("/dev/").unwrap_or(path);
        let inner = self.inner();
        for dev in inner.disks.values() {
            let metadata = dev.blkdev_meta().inner();
            for node in metadata.gendisks.values() {
                // GenDisk::dname is an immutable stored-name clone, with no
                // I/O or secondary lock. Match what was actually published:
                // loop0p1/nvme0n1p1 and sda1 share no parsing convention.
                if node.dname().is_ok_and(|name| name.as_ref() == raw) {
                    return Some(node.clone());
                }
            }
        }
        None
    }

    /// 打印所有已发布gendisk的真实路径，不重新推导分区命名。
    pub fn print_gendisks(&self) {
        let mut disks = alloc::vec::Vec::new();
        let inner = self.inner();
        for dev in inner.disks.values() {
            let meta = dev.blkdev_meta().inner();
            for node in meta.gendisks.values() {
                if let Ok(name) = node.dname() {
                    disks.push(format!("/dev/{}", name.as_ref()));
                }
            }
        }
        drop(inner);
        log::debug!("All gendisks: {:?}", disks);
    }

    /// 获取对应major下一个可用的minor号
    pub(self) fn next_minor(&self, major: Major) -> u32 {
        let mut inner = self.inner();
        let base = inner
            .minors
            .entry(major)
            .or_insert_with(|| AtomicU32::new(0));
        let base_minor = base.load(core::sync::atomic::Ordering::SeqCst);
        base.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        base_minor
    }
}

pub struct BlockDevMeta {
    pub devname: DevName,
    pub major: Major,
    pub base_minor: u32,
    inner: Mutex<InnerBlockDevMeta>,
    /// Serialize raw-cache frontends/claims with mapping and partition changes.
    /// Backend payload never acquires this gate: an exclusive holder may flush
    /// and wait for already claimed I/O using the still-valid old mapping.
    pub(crate) raw_cache_gate: RwSem<()>,
}

pub struct InnerBlockDevMeta {
    pub gendisks: GenDiskMap,
    pub dev_idx: usize,
}

impl BlockDevMeta {
    pub fn new(devname: DevName, major: Major) -> Self {
        BlockDevMeta {
            devname,
            major,
            base_minor: block_dev_manager().next_minor(major),
            raw_cache_gate: RwSem::new(()),
            inner: Mutex::new(InnerBlockDevMeta {
                gendisks: GenDiskMap::new(),
                dev_idx: 0, // 默认索引为0
            }),
        }
    }

    pub(crate) fn inner(&self) -> MutexGuard<'_, InnerBlockDevMeta> {
        self.inner.lock()
    }
}

impl core::fmt::Debug for BlockDevMeta {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BlockDevMeta")
            .field("devname", &self.devname)
            .finish()
    }
}
