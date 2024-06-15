use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use bitmap::AllocBitmap;
use hashbrown::HashMap;
use system_error::SystemError;

use crate::{
    arch::MMArch,
    libs::{
        rbtree::RBTree,
        rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
    mm::{MemoryManagementArch, VirtAddr},
    virt::vm::{kvm_host::KVM_ADDRESS_SPACE_NUM, user_api::KvmUserspaceMemoryRegion},
};

use super::{LockedVm, Vm};

pub const KVM_USER_MEM_SLOTS: u16 = u16::MAX;
pub const KVM_INTERNAL_MEM_SLOTS: u16 = 3;
pub const KVM_MEM_SLOTS_NUM: u16 = KVM_USER_MEM_SLOTS - KVM_INTERNAL_MEM_SLOTS;
pub const KVM_MEM_MAX_NR_PAGES: usize = (1 << 31) - 1;

#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct KvmMmuMemoryCache {
    gfp_zero: u32,
    gfp_custom: u32,
    capacity: usize,
    nobjs: usize,
    objects: Option<Box<Vec<u8>>>,
}
impl KvmMmuMemoryCache {
    #[allow(dead_code)]
    pub fn kvm_mmu_totup_memory_cache(
        &mut self,
        _capacity: usize,
        _min: usize,
    ) -> Result<(), SystemError> {
        // let gfp = if self.gfp_custom != 0 {
        //     self.gfp_custom
        // } else {
        //     todo!();
        // };

        // if self.nobjs >= min {
        //     return Ok(());
        // }

        // if unlikely(self.objects.is_none()) {
        //     if self.capacity == 0 {
        //         return Err(SystemError::EIO);
        //     }

        //     // self.objects = Some(Box::new)
        // }

        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Default)]
pub struct AddrRange {
    pub start: VirtAddr,
    pub last: VirtAddr,
}

#[derive(Debug, Default)]
pub struct KvmMemSlotSet {
    /// 最后一次使用到的内存插槽
    pub last_use: Option<Arc<LockedKvmMemSlot>>,
    /// 存储虚拟地址（hva）和内存插槽之间的映射关系
    hva_tree: RBTree<AddrRange, Arc<LockedKvmMemSlot>>,
    /// 用于存储全局页帧号（gfn）和内存插槽之间的映射关系
    gfn_tree: RBTree<u64, Arc<LockedKvmMemSlot>>,
    /// 将内存插槽的ID映射到对应的内存插槽。
    slots: HashMap<u16, Arc<LockedKvmMemSlot>>,

    pub node_idx: usize,
    pub generation: u64,
}

impl KvmMemSlotSet {
    pub fn get_slot(&self, id: u16) -> Option<Arc<LockedKvmMemSlot>> {
        self.slots.get(&id).cloned()
    }
}

#[derive(Debug)]
pub struct LockedKvmMemSlot {
    inner: RwLock<KvmMemSlot>,
}

impl LockedKvmMemSlot {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(KvmMemSlot::default()),
        })
    }

    #[inline]
    pub fn read(&self) -> RwLockReadGuard<KvmMemSlot> {
        self.inner.read()
    }

    #[inline]
    pub fn write(&self) -> RwLockWriteGuard<KvmMemSlot> {
        self.inner.write()
    }

    #[inline]
    pub fn copy_from(&self, other: &Arc<LockedKvmMemSlot>) {
        let mut guard = self.write();
        let other = other.read();

        guard.base_gfn = other.base_gfn;
        guard.npages = other.npages;

        guard.dirty_bitmap = other.dirty_bitmap.clone();
        guard.arch = other.arch;
        guard.userspace_addr = other.userspace_addr;
        guard.flags = other.flags;
        guard.id = other.id;
        guard.as_id = other.as_id;
    }
}

#[derive(Debug, Default)]
pub struct KvmMemSlot {
    /// 首个gfn
    base_gfn: u64,
    /// 页数量
    npages: usize,
    /// 脏页位图
    dirty_bitmap: Option<AllocBitmap>,
    /// 架构相关
    arch: (),
    userspace_addr: VirtAddr,
    flags: UserMemRegionFlag,
    id: u16,
    as_id: u16,

    hva_node_key: [AddrRange; 2],
}

#[derive(Debug)]
pub struct LockedVmMemSlotSet {
    inner: SpinLock<KvmMemSlotSet>,
}

impl LockedVmMemSlotSet {
    pub fn new(slots: KvmMemSlotSet) -> Arc<Self> {
        Arc::new(Self {
            inner: SpinLock::new(slots),
        })
    }

    pub fn lock(&self) -> SpinLockGuard<KvmMemSlotSet> {
        self.inner.lock()
    }
}

#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct GfnToHvaCache {
    generation: u64,
    /// 客户机对应物理地址（Guest Physical Address）
    gpa: u64,
    /// 主机用户空间虚拟地址（User Host Virtual Address）
    uhva: Option<u64>,
    /// 主机内核空间虚拟地址（Kernel Host Virtual Address）
    khva: u64,
    /// 对应内存插槽
    memslot: Option<Arc<LockedKvmMemSlot>>,
    /// 对应物理页帧号(Page Frame Number)
    pfn: Option<u64>,
    /// 缓存项的使用情况
    usage: PfnCacheUsage,
    /// 是否处于活动状态
    active: bool,
    /// 是否有效
    valid: bool,
    vm: Option<Weak<LockedVm>>,
}

impl GfnToHvaCache {
    pub fn init(vm: Weak<LockedVm>, usage: PfnCacheUsage) -> Self {
        // check_stack_usage();
        // let mut ret: Box<GfnToHvaCache> = unsafe { Box::new_zeroed().assume_init() };
        // ret.usage = usage;
        // ret.vm = Some(vm);
        // *ret
        Self {
            usage,
            vm: Some(vm),
            ..Default::default()
        }
    }
}

bitflags! {
    #[derive(Default)]
    pub struct PfnCacheUsage: u8 {
        const GUEST_USES_PFN = 1 << 0;
        const HOST_USES_PFN = 1 << 1;
        const GUEST_AND_HOST_USES_PFN = Self::GUEST_USES_PFN.bits | Self::HOST_USES_PFN.bits;
    }

    pub struct UserMemRegionFlag: u32 {
        /// 用来开启内存脏页
        const LOG_DIRTY_PAGES = 1 << 0;
        /// 开启内存只读
        const READONLY = 1 << 1;
        /// 标记invalid
        const KVM_MEMSLOT_INVALID = 1 << 16;
    }
}

impl Default for UserMemRegionFlag {
    fn default() -> Self {
        Self::empty()
    }
}

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub enum KvmMemoryChangeMode {
    Create,
    Delete,
    Move,
    FlagsOnly,
}

impl Vm {
    #[inline(never)]
    pub fn set_memory_region(&mut self, mem: KvmUserspaceMemoryRegion) -> Result<(), SystemError> {
        if mem.slot >= u16::MAX as u32 {
            return Err(SystemError::EINVAL);
        }

        let as_id = mem.slot >> 16;
        let id = mem.slot as u16;

        // 检查内存对齐以及32位检测（虽然现在没什么用<）
        if (mem.memory_size as usize & MMArch::PAGE_SIZE != 0)
            || mem.memory_size != mem.memory_size as usize as u64
        {
            return Err(SystemError::EINVAL);
        }

        if !mem.guest_phys_addr.check_aligned(MMArch::PAGE_SIZE) {
            return Err(SystemError::EINVAL);
        }

        if !mem.userspace_addr.check_aligned(MMArch::PAGE_SIZE) {
            // 这里应该还需要判断从userspace_addr->userspace_addr+memory_size这段区间都是合法的
            return Err(SystemError::EINVAL);
        }

        if as_id >= KVM_ADDRESS_SPACE_NUM as u32 || id >= KVM_MEM_SLOTS_NUM {
            return Err(SystemError::EINVAL);
        }

        if (mem.memory_size >> MMArch::PAGE_SHIFT) > KVM_MEM_MAX_NR_PAGES as u64 {
            return Err(SystemError::EINVAL);
        }

        let slots = self.memslot_set(as_id as usize).clone();

        let slots_guard = slots.lock();
        let old = slots_guard.get_slot(id);
        if mem.memory_size == 0 {
            if let Some(old) = &old {
                let old_npages = old.read().npages;
                if old_npages == 0 {
                    return Err(SystemError::EINVAL);
                }

                if self.nr_memslot_pages < old_npages {
                    return Err(SystemError::EIO);
                }
                drop(slots_guard);
                return self.set_memslot(Some(&old), None, KvmMemoryChangeMode::Delete);
            } else {
                return Err(SystemError::EINVAL);
            }
        }

        let base_gfn = (mem.guest_phys_addr.data() >> MMArch::PAGE_SHIFT) as u64;
        let npages = mem.memory_size >> MMArch::PAGE_SHIFT;

        let change;
        if let Some(old) = &old {
            let old_guard = old.read();
            if old_guard.npages == 0 {
                change = KvmMemoryChangeMode::Create;
                // 避免溢出
                if self.nr_memslot_pages + (npages as usize) < self.nr_memslot_pages {
                    return Err(SystemError::EINVAL);
                }
            } else {
                if mem.userspace_addr != old_guard.userspace_addr
                    || npages != old_guard.npages as u64
                    || (mem.flags ^ old_guard.flags).contains(UserMemRegionFlag::READONLY)
                {
                    return Err(SystemError::EINVAL);
                }

                if base_gfn != old_guard.base_gfn {
                    change = KvmMemoryChangeMode::Move;
                } else if mem.flags != old_guard.flags {
                    change = KvmMemoryChangeMode::FlagsOnly;
                } else {
                    return Ok(());
                }
            }
        } else {
            change = KvmMemoryChangeMode::Create;
            // 避免溢出
            if self.nr_memslot_pages + (npages as usize) < self.nr_memslot_pages {
                return Err(SystemError::EINVAL);
            }
        };

        if change == KvmMemoryChangeMode::Create || change == KvmMemoryChangeMode::Move {
            if slots_guard.gfn_tree.contains_key(&base_gfn) {
                return Err(SystemError::EEXIST);
            }
        }

        let new = LockedKvmMemSlot::new();
        let mut new_guard = new.write();

        new_guard.as_id = as_id as u16;
        new_guard.id = id;
        new_guard.base_gfn = base_gfn;
        new_guard.npages = npages as usize;
        new_guard.flags = mem.flags;
        new_guard.userspace_addr = mem.userspace_addr;

        drop(new_guard);
        drop(slots_guard);
        return self.set_memslot(old.as_ref(), Some(&new), change);
    }

    #[inline]
    /// 获取活动内存插槽
    fn memslot_set(&self, id: usize) -> &Arc<LockedVmMemSlotSet> {
        // 避免越界
        let id = id % KVM_ADDRESS_SPACE_NUM;
        &self.memslots[id]
    }

    #[inline(never)]
    fn set_memslot(
        &mut self,
        old: Option<&Arc<LockedKvmMemSlot>>,
        new: Option<&Arc<LockedKvmMemSlot>>,
        change: KvmMemoryChangeMode,
    ) -> Result<(), SystemError> {
        let invalid_slot = LockedKvmMemSlot::new();
        if change == KvmMemoryChangeMode::Delete || change == KvmMemoryChangeMode::Move {
            self.invalidate_memslot(old.unwrap(), &invalid_slot)
        }

        match self.prepare_memory_region(old, new, change) {
            Ok(_) => {}
            Err(e) => {
                if change == KvmMemoryChangeMode::Delete || change == KvmMemoryChangeMode::Move {
                    self.active_memslot(Some(&invalid_slot), old)
                }
                return Err(e);
            }
        }

        match change {
            KvmMemoryChangeMode::Create => self.create_memslot(new),
            KvmMemoryChangeMode::Delete => self.delete_memslot(old, &invalid_slot),
            KvmMemoryChangeMode::Move => self.move_memslot(old, new, &invalid_slot),
            KvmMemoryChangeMode::FlagsOnly => self.update_flags_memslot(old, new),
        }

        // TODO:kvm_commit_memory_region(kvm, old, new, change);
        Ok(())
    }

    fn create_memslot(&mut self, new: Option<&Arc<LockedKvmMemSlot>>) {
        self.replace_memslot(None, new);
        self.active_memslot(None, new);
    }

    fn delete_memslot(
        &mut self,
        old: Option<&Arc<LockedKvmMemSlot>>,
        invalid_slot: &Arc<LockedKvmMemSlot>,
    ) {
        self.replace_memslot(old, None);
        self.active_memslot(Some(invalid_slot), None);
    }

    fn move_memslot(
        &mut self,
        old: Option<&Arc<LockedKvmMemSlot>>,
        new: Option<&Arc<LockedKvmMemSlot>>,
        invalid_slot: &Arc<LockedKvmMemSlot>,
    ) {
        self.replace_memslot(old, new);
        self.active_memslot(Some(invalid_slot), new);
    }

    fn update_flags_memslot(
        &mut self,
        old: Option<&Arc<LockedKvmMemSlot>>,
        new: Option<&Arc<LockedKvmMemSlot>>,
    ) {
        self.replace_memslot(old, new);
        self.active_memslot(old, new);
    }

    fn prepare_memory_region(
        &self,
        old: Option<&Arc<LockedKvmMemSlot>>,
        new: Option<&Arc<LockedKvmMemSlot>>,
        change: KvmMemoryChangeMode,
    ) -> Result<(), SystemError> {
        if change != KvmMemoryChangeMode::Delete {
            let new = new.unwrap();
            let mut new_guard = new.write();
            if !new_guard.flags.contains(UserMemRegionFlag::LOG_DIRTY_PAGES) {
                new_guard.dirty_bitmap = None;
            } else if old.is_some() {
                let old_guard = old.unwrap().read();
                if old_guard.dirty_bitmap.is_some() {
                    new_guard.dirty_bitmap = old_guard.dirty_bitmap.clone();
                } else {
                    new_guard.dirty_bitmap = Some(AllocBitmap::new(new_guard.npages * 2));
                }
            }
        }

        return self.arch_prepare_memory_region(old, new, change);
    }

    fn invalidate_memslot(
        &mut self,
        old: &Arc<LockedKvmMemSlot>,
        invalid_slot: &Arc<LockedKvmMemSlot>,
    ) {
        invalid_slot.copy_from(old);

        let mut old_guard = old.write();
        let mut invalid_slot_guard = invalid_slot.write();
        invalid_slot_guard
            .flags
            .insert(UserMemRegionFlag::KVM_MEMSLOT_INVALID);

        self.swap_active_memslots(old_guard.as_id as usize);

        old_guard.arch = invalid_slot_guard.arch;
    }

    #[inline(never)]
    fn active_memslot(
        &mut self,
        old: Option<&Arc<LockedKvmMemSlot>>,
        new: Option<&Arc<LockedKvmMemSlot>>,
    ) {
        let as_id = if let Some(slot) = old.or(new) {
            slot.read().as_id
        } else {
            0
        };

        self.swap_active_memslots(as_id as usize);

        self.replace_memslot(old, new);
    }

    #[inline(never)]
    fn replace_memslot(
        &self,
        old: Option<&Arc<LockedKvmMemSlot>>,
        new: Option<&Arc<LockedKvmMemSlot>>,
    ) {
        let as_id = if let Some(slot) = old.or(new) {
            slot.read().as_id
        } else {
            0
        };

        let slot_set = self.get_inactive_memslot_set(as_id as usize);

        let mut slots_guard = slot_set.lock();
        let idx = slots_guard.node_idx;

        if let Some(old) = old {
            slots_guard.hva_tree.remove(&old.read().hva_node_key[idx]);

            if let Some(last) = &slots_guard.last_use {
                if Arc::ptr_eq(last, old) {
                    slots_guard.last_use = new.map(|x| x.clone());
                }
            }

            if new.is_none() {
                slots_guard.gfn_tree.remove(&old.read().base_gfn);
                return;
            }
        }

        let new = new.unwrap();
        let mut new_guard = new.write();
        new_guard.hva_node_key[idx].start = new_guard.userspace_addr;
        new_guard.hva_node_key[idx].last =
            new_guard.userspace_addr + VirtAddr::new((new_guard.npages << MMArch::PAGE_SHIFT) - 1);

        slots_guard
            .hva_tree
            .insert(new_guard.hva_node_key[idx], new.clone());

        if let Some(old) = old {
            slots_guard.gfn_tree.remove(&old.read().base_gfn);
        }

        slots_guard.gfn_tree.insert(new_guard.base_gfn, new.clone());
    }

    fn get_inactive_memslot_set(&self, as_id: usize) -> Arc<LockedVmMemSlotSet> {
        let active = self.memslot_set(as_id);

        let inactive_idx = active.lock().node_idx ^ 1;
        return self.memslots_set[as_id][inactive_idx].clone();
    }

    fn swap_active_memslots(&mut self, as_id: usize) {
        self.memslots[as_id] = self.get_inactive_memslot_set(as_id);
    }
}
