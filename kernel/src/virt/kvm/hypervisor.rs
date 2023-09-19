use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch::KVMArch;
use crate::arch::kvm::vmx::vcpu::VmxVcpu;
use crate::libs::mutex::Mutex;
use crate::syscall::SystemError;

use super::HOST_STACK_SIZE;
use super::host_mem::{KvmUserspaceMemoryRegion, KVM_MEM_LOG_DIRTY_PAGES,KVM_ADDRESS_SPACE_NUM, KVM_MEM_SLOTS_NUM,
    KvmMemorySlots, KvmMemorySlot, 
    PAGE_SHIFT, KVM_MEM_MAX_NR_PAGES, KvmMemoryChange, KVM_MEM_READONLY, KVM_USER_MEM_SLOTS,
};
use crate::arch::kvm::vmx::vmcs::PAGE_SIZE;
// use crate::kdebug;

pub struct Hypervisor {
    pub nr_vcpus: u32,  /* Number of cpus to run */
    pub vcpu: Vec<Arc<Mutex<Box<VmxVcpu>>>>,
    pub host_stack: Vec<u8>,
    pub mem_slots_num: u64,
    pub memslots: [KvmMemorySlots; KVM_ADDRESS_SPACE_NUM],
    pub arch: KVMArch,
    // 	vm_fd: u32,  	/* For VM ioctls() */
//     timerid: u32,   /* Posix timer for interrupts */
//     mem_slots: u32, /* for KVM_SET_USER_MEMORY_REGION */
//     ram_size: u64,  /* Guest memory size, in bytes */
//     ram_start: *u64,
//     ram_pagesize: u64,
//     mem_banks_lock: SpinLock<()>,
//     // mem_banks: Box<[kvm_mem_bank]>,

//     vm_state: u32,
}

impl Hypervisor {
    pub fn new(nr_vcpus: u32, _host_stack: u64, mem_slots_num: u64) -> Result<Self, SystemError> {
        let vcpu = Vec::new();
        // for i in 0..nr_vcpus {
        //     vcpu.push(Vcpu::new(i, Arc::new(hypervisor))?);
        // }
        // Allocate stack for vm-exit handlers and fill it with garbage data
        let instance = Self {
            nr_vcpus,
            vcpu,
            host_stack: vec![0xCC; HOST_STACK_SIZE],
            mem_slots_num,
            memslots: [KvmMemorySlots::default();KVM_ADDRESS_SPACE_NUM],
            arch: Default::default(),
        };
        Ok(instance)
    }


    /// Allocate some memory and give it an address in the guest physical address space.
    pub fn set_user_memory_region(&mut self, mem: &KvmUserspaceMemoryRegion) -> Result<(), SystemError>{
        let id: u16 = mem.slot as u16;      // slot id
        let as_id = mem.slot >> 16;    // address space id

        // 检查slot是否合法
        if mem.slot as usize >= self.mem_slots_num as usize {
            return Err(SystemError::EINVAL);
        }
        // 检查flags是否合法
        self.check_memory_region_flag(mem)?;
        // 内存大小和地址必须是页对齐的
        if (mem.memory_size & (PAGE_SIZE-1) as u64) !=0 || (mem.guest_phys_addr & (PAGE_SIZE-1) as u64) !=0 {
            return Err(SystemError::EINVAL);
        }
        // 检查地址空间是否合法
        if as_id >= (KVM_ADDRESS_SPACE_NUM as u32) || id >= KVM_MEM_SLOTS_NUM as u16 {
            return Err(SystemError::EINVAL);
        }
        if mem.memory_size < 0 {
            return Err(SystemError::EINVAL);
        }

        let slot = &self.memslots[as_id as usize].memslots[id as usize];
        let base_gfn = mem.guest_phys_addr >> PAGE_SHIFT;
        let npages = mem.memory_size >> PAGE_SHIFT;
        if npages > KVM_MEM_MAX_NR_PAGES as u64 {
            return Err(SystemError::EINVAL);
        }
        let change: KvmMemoryChange;

        let old_slot = slot;
        let mut new_slot = KvmMemorySlot{
            base_gfn, // 虚机内存区间起始物理页框号
            npages,   // 虚机内存区间页数，即内存区间的大小
            // dirty_bitmap: old_slot.dirty_bitmap, 
            userspace_addr: old_slot.userspace_addr,  // 虚机内存区间对应的主机虚拟地址
            flags: mem.flags,    // 虚机内存区间属性
            id,       // 虚机内存区间id
        };
        

        // 判断新memoryslot的类型
        if npages != 0 { //映射内存有大小，不是删除内存条
            if old_slot.npages == 0 { //内存槽号没有虚拟内存条，意味内存新创建
                change = KvmMemoryChange::Create;
            }
            else { //修改已存在的内存,表示修改标志或者平移映射地址
                // 检查内存条是否可以修改
                if mem.userspace_addr != old_slot.userspace_addr ||
                    npages !=old_slot.npages ||
                    (new_slot.flags ^ old_slot.flags & KVM_MEM_READONLY) != 0 {
                    return Err(SystemError::EINVAL);
                }
                if new_slot.base_gfn != old_slot.base_gfn { //guest地址不同，内存条平移
                    change = KvmMemoryChange::Move;
                } else if new_slot.flags != old_slot.flags { //内存条标志不同，修改标志
                    change = KvmMemoryChange::FlagsOnly;
                } else {
                    return Ok(());
                }
            }
        } else {
            if old_slot.npages == 0 { //内存槽号没有虚拟内存条，不可以删除
                return Err(SystemError::EINVAL);
            }
            //申请插入的内存为0，而内存槽上有内存，意味删除
            change = KvmMemoryChange::Delete;
            new_slot.base_gfn = 0;
            new_slot.flags = 0;
        }
        
        if change == KvmMemoryChange::Create || change == KvmMemoryChange::Move {
            // 检查内存区域是否重叠
            for i in 0..KVM_MEM_SLOTS_NUM {
                let memslot = &self.memslots[as_id as usize].memslots[i as usize];
                if memslot.id == id || memslot.id as u32 >=KVM_USER_MEM_SLOTS {
                    continue;
                }
                // 当前已有的slot与new在guest物理地址上有交集
                if !(base_gfn + npages <= memslot.base_gfn || memslot.base_gfn + memslot.npages <= base_gfn) {
                    return Err(SystemError::EEXIST);
                }
            }
        }

        if !(new_slot.flags & KVM_MEM_LOG_DIRTY_PAGES != 0) {
            // new_slot.dirty_bitmap = 0;
        }

        // 根据flags的值，决定是否创建内存脏页
        // if (new_slot.flags & KVM_MEM_LOG_DIRTY_PAGES)!=0 && new_slot.dirty_bitmap == 0 {
        //     let type_size = core::mem::size_of::<u64>() as u64;
        //     let dirty_bytes = 2 * ((new_slot.npages+type_size-1) / type_size) / 8;
            // new_slot.dirty_bitmap = Box::new(vec![0; dirty_bytes as u8]);
        // }

        if change == KvmMemoryChange::Create {
            new_slot.userspace_addr = mem.userspace_addr;
            let mut memslots = self.memslots[as_id as usize].memslots.clone();
            memslots[id as usize] = new_slot;
            self.memslots[as_id as usize].memslots = memslots;
            self.memslots[as_id as usize].used_slots += 1;
            // KVMArch::kvm_arch_create_memslot(&mut new_slot, npages);
            // KVMArch::kvm_arch_commit_memory_region(mem, &new_slot, old_slot, change);
        }
        // TODO--KvmMemoryChange::Delete & Move
        Ok(())


    }

    fn check_memory_region_flag(&self, mem: &KvmUserspaceMemoryRegion) -> Result<(), SystemError> {
        let valid_flags = KVM_MEM_LOG_DIRTY_PAGES;
        // 除了valid_flags之外的flags被置1了，就返回错误
        if mem.flags & !valid_flags != 0 {
            return Err(SystemError::EINVAL);
        }
        Ok(())
    }

    

    // pub fn virtualize_cpu(&self) -> Result<(), SystemError> {
    //     let mut vcpu = self.vcpu[0].lock();
    //     vcpu.virtualize_cpu();
    // }
}

