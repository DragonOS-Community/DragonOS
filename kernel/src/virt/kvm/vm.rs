use crate::arch::kvm::vmx::vcpu::VmxVcpu;
use crate::arch::MMArch;
use crate::libs::mutex::Mutex;
use crate::mm::MemoryManagementArch;
use crate::{arch::KVMArch, kdebug};
use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

// use super::HOST_STACK_SIZE;
use super::host_mem::{
    KvmMemoryChange, KvmMemorySlot, KvmMemorySlots, KvmUserspaceMemoryRegion,
    KVM_ADDRESS_SPACE_NUM, KVM_MEM_LOG_DIRTY_PAGES, KVM_MEM_MAX_NR_PAGES, KVM_MEM_READONLY,
    KVM_MEM_SLOTS_NUM, KVM_USER_MEM_SLOTS, PAGE_SHIFT,
};
// use crate::kdebug;

#[derive(Debug, Clone)]
pub struct Vm {
    pub id: usize,
    // vcpu config
    pub nr_vcpus: u32, /* Number of cpus to run */
    pub vcpu: Vec<Arc<Mutex<VmxVcpu>>>,
    // memory config
    pub nr_mem_slots: u32, /* Number of memory slots in each address space */
    pub memslots: [KvmMemorySlots; KVM_ADDRESS_SPACE_NUM],
    // arch related config
    pub arch: KVMArch,
}

impl Vm {
    pub fn new(id: usize) -> Result<Self, SystemError> {
        let vcpu = Vec::new();
        // Allocate stack for vm-exit handlers and fill it with garbage data
        let instance = Self {
            id,
            nr_vcpus: 0,
            vcpu,
            nr_mem_slots: KVM_MEM_SLOTS_NUM,
            memslots: [KvmMemorySlots::default(); KVM_ADDRESS_SPACE_NUM],
            arch: Default::default(),
        };
        Ok(instance)
    }

    /// Allocate some memory and give it an address in the guest physical address space.
    pub fn set_user_memory_region(
        &mut self,
        mem: &KvmUserspaceMemoryRegion,
    ) -> Result<(), SystemError> {
        kdebug!("set_user_memory_region");
        let id: u16 = mem.slot as u16; // slot id
        let as_id = mem.slot >> 16; // address space id
        kdebug!("id={}, as_id={}", id, as_id);

        // 检查slot是否合法
        if mem.slot as usize >= self.nr_mem_slots as usize {
            return Err(SystemError::EINVAL);
        }
        // 检查flags是否合法
        self.check_memory_region_flag(mem)?;
        // 内存大小和地址必须是页对齐的
        if (mem.memory_size & (MMArch::PAGE_SIZE - 1) as u64) != 0
            || (mem.guest_phys_addr & (MMArch::PAGE_SIZE - 1) as u64) != 0
        {
            return Err(SystemError::EINVAL);
        }
        // 检查地址空间是否合法
        if as_id >= (KVM_ADDRESS_SPACE_NUM as u32) || id >= KVM_MEM_SLOTS_NUM as u16 {
            return Err(SystemError::EINVAL);
        }
        // if mem.memory_size < 0 {
        //     return Err(SystemError::EINVAL);
        // }
        let slot = &self.memslots[as_id as usize].memslots[id as usize];
        let base_gfn = mem.guest_phys_addr >> PAGE_SHIFT;
        let npages = mem.memory_size >> PAGE_SHIFT;
        if npages > KVM_MEM_MAX_NR_PAGES as u64 {
            return Err(SystemError::EINVAL);
        }
        let change: KvmMemoryChange;

        let old_slot = slot;
        let mut new_slot = KvmMemorySlot {
            base_gfn, // 虚机内存区间起始物理页框号
            npages,   // 虚机内存区间页数，即内存区间的大小
            // dirty_bitmap: old_slot.dirty_bitmap,
            userspace_addr: mem.userspace_addr, // 虚机内存区间对应的主机虚拟地址
            flags: mem.flags,                   // 虚机内存区间属性
            id,                                 // 虚机内存区间id
        };

        // 判断新memoryslot的类型
        if npages != 0 {
            //映射内存有大小，不是删除内存条
            if old_slot.npages == 0 {
                //内存槽号没有虚拟内存条，意味内存新创建
                change = KvmMemoryChange::Create;
            } else {
                //修改已存在的内存,表示修改标志或者平移映射地址
                // 检查内存条是否可以修改
                if mem.userspace_addr != old_slot.userspace_addr
                    || npages != old_slot.npages
                    || (new_slot.flags ^ old_slot.flags & KVM_MEM_READONLY) != 0
                {
                    return Err(SystemError::EINVAL);
                }
                if new_slot.base_gfn != old_slot.base_gfn {
                    //guest地址不同，内存条平移
                    change = KvmMemoryChange::Move;
                } else if new_slot.flags != old_slot.flags {
                    //内存条标志不同，修改标志
                    change = KvmMemoryChange::FlagsOnly;
                } else {
                    return Ok(());
                }
            }
        } else {
            if old_slot.npages == 0 {
                //内存槽号没有虚拟内存条，不可以删除
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
                if memslot.id == id || memslot.id as u32 >= KVM_USER_MEM_SLOTS {
                    continue;
                }
                // 当前已有的slot与new在guest物理地址上有交集
                if !(base_gfn + npages <= memslot.base_gfn
                    || memslot.base_gfn + memslot.npages <= base_gfn)
                {
                    return Err(SystemError::EEXIST);
                }
            }
        }

        if (new_slot.flags & KVM_MEM_LOG_DIRTY_PAGES) == 0 {
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
            let mut memslots = self.memslots[as_id as usize].memslots;
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
}
