use core::{
    fmt::Debug,
    sync::atomic::{AtomicUsize, Ordering},
};

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use hashbrown::HashMap;
use log::debug;
use mem::LockedKvmMemSlot;
use system_error::SystemError;

use crate::{
    arch::{
        vm::{kvm_host::vcpu::VirtCpuRequest, vmx::KvmVmx, x86_kvm_manager},
        CurrentKvmManager, KvmArch, VirtCpuArch,
    },
    filesystem::vfs::file::{File, FileMode},
    libs::spinlock::{SpinLock, SpinLockGuard},
    mm::ucontext::AddressSpace,
    process::ProcessManager,
    smp::cpu::ProcessorId,
    virt::vm::{
        kvm_dev::KvmVcpuDev,
        kvm_host::vcpu::{LockedVirtCpu, VirtCpu},
    },
};

use self::{
    mem::{GfnToHvaCache, KvmMemSlotSet, LockedVmMemSlotSet, PfnCacheUsage},
    vcpu::{GuestDebug, VcpuMode},
};

pub mod mem;
pub mod vcpu;

const KVM_ADDRESS_SPACE_NUM: usize = 1;
pub const KVM_USERSAPCE_IRQ_SOURCE_ID: usize = 0;
pub const KVM_IRQFD_RESAMPLE_IRQ_SOURCE_ID: usize = 1;

#[derive(Debug)]
pub struct LockedVm {
    inner: SpinLock<Vm>,
}

static KVM_USAGE_COUNT: AtomicUsize = AtomicUsize::new(0);

impl LockedVm {
    pub fn lock(&self) -> SpinLockGuard<Vm> {
        self.inner.lock()
    }

    pub fn create(vm_type: usize) -> Result<Arc<Self>, SystemError> {
        let mut memslots_set = vec![];
        let mut memslots = vec![];
        for i in 0..KVM_ADDRESS_SPACE_NUM {
            let mut tmp = vec![];
            for j in 0..2 {
                let mut slots = KvmMemSlotSet::default();
                slots.last_use = None;
                slots.node_idx = j;
                slots.generation = i as u64;
                tmp.push(LockedVmMemSlotSet::new(slots));
            }
            memslots_set.push(tmp);
            memslots.push(memslots_set[i][0].clone());
        }

        let kvm = Vm {
            mm: ProcessManager::current_pcb()
                .basic()
                .user_vm()
                .unwrap()
                .write()
                .try_clone()?,
            max_vcpus: CurrentKvmManager::KVM_MAX_VCPUS,
            memslots_set,
            memslots,
            arch: KvmArch::init(vm_type)?,
            created_vcpus: 0,
            lock_vm_ref: Weak::new(),
            nr_memslot_pages: 0,
            online_vcpus: 0,
            dirty_ring_size: 0,
            dirty_ring_with_bitmap: false,
            vcpus: HashMap::new(),
            #[cfg(target_arch = "x86_64")]
            kvm_vmx: KvmVmx::default(),
            nr_memslots_dirty_logging: 0,
            mmu_invalidate_seq: 0,
        };

        let ret = Arc::new(Self {
            inner: SpinLock::new(kvm),
        });

        Self::hardware_enable_all()?;

        ret.lock().lock_vm_ref = Arc::downgrade(&ret);
        return Ok(ret);
    }

    fn hardware_enable_all() -> Result<(), SystemError> {
        KVM_USAGE_COUNT.fetch_add(1, Ordering::SeqCst);

        // 如果是第一个启动的，则需要对所有cpu都初始化硬件
        if KVM_USAGE_COUNT.load(Ordering::SeqCst) == 1 {
            // FIXME!!!!
            // 这里是要对每个cpu都进行初始化，目前这里只对当前cpu调用了初始化流程
            x86_kvm_manager().arch_hardware_enable()?;
        }

        Ok(())
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct Vm {
    lock_vm_ref: Weak<LockedVm>,
    mm: Arc<AddressSpace>,
    max_vcpus: usize,
    created_vcpus: usize,
    online_vcpus: usize,
    /// vcpu集合
    vcpus: HashMap<usize, Arc<LockedVirtCpu>>,
    // name: String,
    /// 对应活动和非活动内存槽,实际为：[[Arc<LockedVmMemSlots>; 2]; KVM_ADDRESS_SPACE_NUM]，这里暂时写Vec
    memslots_set: Vec<Vec<Arc<LockedVmMemSlotSet>>>,
    /// 当前活动内存槽，实际为：[Arc<LockedVmMemSlots>; KVM_ADDRESS_SPACE_NUM]，这里暂时写Vec
    pub memslots: Vec<Arc<LockedVmMemSlotSet>>,
    /// 内存槽对应的页数
    nr_memslot_pages: usize,

    pub arch: KvmArch,

    pub dirty_ring_size: u32,
    pub nr_memslots_dirty_logging: u32,
    dirty_ring_with_bitmap: bool,

    #[cfg(target_arch = "x86_64")]
    pub kvm_vmx: KvmVmx,

    pub mmu_invalidate_seq: u64, //用于表示内存管理单元（MMU）无效化序列号
}

impl Vm {
    #[inline(never)]
    pub fn create_vcpu(&mut self, id: usize) -> Result<usize, SystemError> {
        if id >= self.max_vcpus {
            return Err(SystemError::EINVAL);
        }

        if self.created_vcpus >= self.max_vcpus {
            return Err(SystemError::EINVAL);
        }

        self.created_vcpus += 1;

        let vcpu = self._create_vcpu(id)?;
        if self.dirty_ring_size != 0 {
            todo!()
        }

        vcpu.lock().vcpu_id = self.online_vcpus;

        self.vcpus.insert(self.online_vcpus, vcpu.clone());

        self.online_vcpus += 1;

        let vcpu_inode = KvmVcpuDev::new(vcpu);

        let file = File::new(vcpu_inode, FileMode::from_bits_truncate(0x777))?;

        let fd = ProcessManager::current_pcb()
            .fd_table()
            .write()
            .alloc_fd(file, None)?;

        Ok(fd as usize)
    }

    /// ### 创建一个vcpu，并且初始化部分数据
    #[inline(never)]
    pub fn _create_vcpu(&mut self, id: usize) -> Result<Arc<LockedVirtCpu>, SystemError> {
        let mut vcpu = self.new_vcpu(id);

        vcpu.init_arch(self, id)?;

        Ok(Arc::new(LockedVirtCpu::new(vcpu)))
    }

    #[inline(never)]
    pub fn new_vcpu(&self, id: usize) -> VirtCpu {
        return VirtCpu {
            cpu: ProcessorId::INVALID,
            kvm: Some(self.lock_vm_ref.clone()),
            vcpu_id: id,
            pid: None,
            _preempted: false,
            _ready: false,
            _last_used_slot: None,
            _stats_id: format!("kvm-{}/vcpu-{}", ProcessManager::current_pid().data(), id),
            _pv_time: GfnToHvaCache::init(self.lock_vm_ref.clone(), PfnCacheUsage::HOST_USES_PFN),
            arch: VirtCpuArch::new(),
            private: None,
            request: VirtCpuRequest::empty(),
            guest_debug: GuestDebug::empty(),
            run: unsafe { Some(Box::new_zeroed().assume_init()) },
            _vcpu_idx: 0,
            mode: VcpuMode::OutsideGuestMode,
            stat: Default::default(),
        };
    }

    #[cfg(target_arch = "x86_64")]
    pub fn kvm_vmx_mut(&mut self) -> &mut KvmVmx {
        &mut self.kvm_vmx
    }

    #[cfg(target_arch = "x86_64")]
    pub fn kvm_vmx(&self) -> &KvmVmx {
        &self.kvm_vmx
    }
}

/// ## 多处理器状态（有些状态在某些架构并不合法）
#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub enum MutilProcessorState {
    Runnable,
    Uninitialized,
    InitReceived,
    Halted,
    SipiReceived,
    Stopped,
    CheckStop,
    Operating,
    Load,
    ApResetHold,
    Suspended,
}
///返回包含 gfn 的 memslot 的指针。如果没有找到，则返回 NULL。
///当 "approx" 设置为 true 时，即使地址落在空洞中，也会返回 memslot。
///在这种情况下，将返回空洞边界的其中一个 memslot。
/// 先简陋完成，原本是二分，现在先遍历
pub fn search_memslots(
    slot_set: Arc<LockedVmMemSlotSet>,
    gfn: u64, /*_approx:bool*/
) -> Option<Arc<LockedKvmMemSlot>> {
    let slots = slot_set.lock();
    let node = &slots.gfn_tree;
    //let(start,end)=(0,node.len()-1);
    for (_gfn_num, slot) in node.iter() {
        let slot_guard = slot.read();
        debug!(
            "gfn:{gfn},slot base_gfn: {},slot npages: {}",
            slot_guard.base_gfn, slot_guard.npages
        );
        if gfn >= slot_guard.base_gfn && gfn < slot_guard.base_gfn + slot_guard.npages as u64 {
            return Some(slot.clone());
        }
    }
    return None;
}
