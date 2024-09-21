use crate::arch::mm::{LockedFrameAllocator, PageMapper};
use crate::arch::vm::asm::VmxAsm;
use crate::arch::vm::mmu::mmu::{max_huge_page_level, PageLevel};
use crate::arch::vm::mmu::mmu_internal::KvmPageFault;
use crate::arch::MMArch;
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::mm::allocator::page_frame::FrameAllocator;
use crate::{kdebug, kerror, kinfo, kwarn};
use crate::mm::page::{page_manager_lock_irqsave, Page, PageEntry, PageFlags, PageFlush, PageManager, PageTable};
use crate::mm::{MemoryManagementArch, PageTableKind, PhysAddr, VirtAddr};
use crate::smp::core::smp_get_processor_id;
use crate::smp::cpu::AtomicProcessorId;
use crate::smp::cpu::ProcessorId;
use core::marker::PhantomData;
use core::ops::Add;
use core::sync::atomic::{compiler_fence, AtomicUsize, Ordering};
use hashbrown::HashMap;
use system_error::SystemError;
use x86::msr;
use x86::vmx::vmcs::control;
use crate::arch::x86_64::mm::X86_64MMArch;
use crate::libs::rwlock::RwLock;
use super::vmx_info;

// pub const VMX_EPT_MT_EPTE_SHIFT:u64 = 3;
pub const VMX_EPT_RWX_MASK: u64 = 0x7 << 3;

// Exit Qualifications for EPT Violations
pub const EPT_VIOLATION_ACC_READ_BIT: u64 = 0;
pub const EPT_VIOLATION_ACC_WRITE_BIT: u64 = 1;
pub const EPT_VIOLATION_ACC_INSTR_BIT: u64 = 2;
pub const EPT_VIOLATION_RWX_SHIFT: u64 = 3;
pub const EPT_VIOLATION_GVA_IS_VALID_BIT: u64 = 7;
pub const EPT_VIOLATION_GVA_TRANSLATED_BIT: u64 = 8;

bitflags! {
    pub struct EptViolationExitQual :u64{
        const ACC_READ = 1 << EPT_VIOLATION_ACC_READ_BIT;
        const ACC_WRITE = 1 << EPT_VIOLATION_ACC_WRITE_BIT;
        const ACC_INSTR = 1 << EPT_VIOLATION_ACC_INSTR_BIT;
        const RWX_MASK = VMX_EPT_RWX_MASK << EPT_VIOLATION_RWX_SHIFT;
        const GVA_IS_VALID = 1 << EPT_VIOLATION_GVA_IS_VALID_BIT;
        const GVA_TRANSLATED = 1 << EPT_VIOLATION_GVA_TRANSLATED_BIT;
    }
}

// /// 全局EPT物理页信息管理器
// pub static mut EPT_PAGE_MANAGER: Option<SpinLock<EptPageManager>> = None;

// /// 初始化EPT_PAGE_MANAGER
// pub fn ept_page_manager_init() {
//     kinfo!("page_manager_init");
//     let page_manager = SpinLock::new(EptPageManager::new());

//     compiler_fence(Ordering::SeqCst);
//     unsafe { EPT_PAGE_MANAGER = Some(page_manager) };
//     compiler_fence(Ordering::SeqCst);

//     kinfo!("page_manager_init done");
// }

// pub fn ept_page_manager_lock_irqsave() -> SpinLockGuard<'static, EptPageManager> {
//     unsafe { EPT_PAGE_MANAGER.as_ref().unwrap().lock_irqsave() }
// }
// EPT 页表数据结构
#[derive(Debug)]
pub struct EptPageTable {
    /// 当前页表表示的虚拟地址空间的起始地址
    base: VirtAddr,
    /// 当前页表所在的物理地址
    phys: PhysAddr,
    /// 当前页表的层级
    /// PageLevel::4K = 1
    level: PageLevel,
}
impl EptPageTable{
    pub fn phys(&self) -> PhysAddr {
        self.phys
    }
    
    /// 设置当前页表的第i个页表项
    pub unsafe fn set_entry(&self, i: usize, entry: PageEntry<MMArch>) -> Option<()> {
        let entry_virt = self.entry_virt(i)?;
        MMArch::write::<usize>(entry_virt, entry.data());
        return Some(());
    }
    /// 判断当前页表的第i个页表项是否已经填写了值
    ///
    /// ## 参数
    /// - Some(true) 如果已经填写了值
    /// - Some(false) 如果未填写值
    /// - None 如果i超出了页表项的范围
    pub fn entry_mapped(&self, i: usize) -> Option<bool> {
        let etv = unsafe { self.entry_virt(i) }?;
        if unsafe { MMArch::read::<usize>(etv) } != 0 {
            return Some(true);
        } else {
            return Some(false);
        }
    }

      /// 获取当前页表的层级
      #[inline(always)]
      pub fn level(&self) -> PageLevel {
          self.level
      }

      /// 获取第i个页表项所表示的虚拟内存空间的起始地址
      pub fn entry_base(&self, i: usize) -> Option<VirtAddr> {
        if i < MMArch::PAGE_ENTRY_NUM {
            let shift = (self.level as usize - 1) * MMArch::PAGE_ENTRY_SHIFT + MMArch::PAGE_SHIFT;
            return Some(self.base.add(i << shift));
        } else {
            return None;
        }
    }
      /// 获取当前页表自身所在的虚拟地址
      #[inline(always)]
      pub unsafe fn virt(&self) -> VirtAddr {
          return MMArch::phys_2_virt(self.phys).unwrap();
      }
    /// 获取当前页表的第i个页表项所在的虚拟地址（注意与entry_base进行区分）
    pub unsafe fn entry_virt(&self, i: usize) -> Option<VirtAddr> {
     if i < MMArch::PAGE_ENTRY_NUM {
            return Some(self.virt().add(i * MMArch::PAGE_ENTRY_SIZE));
        } else {
           return None;
     }
    }
     /// 获取当前页表的第i个页表项
     pub unsafe fn entry(&self, i: usize) -> Option<PageEntry<MMArch>> {
        let entry_virt = self.entry_virt(i)?;
        return Some(PageEntry::from_usize(MMArch::read::<usize>(entry_virt)));
    }
    
    pub fn new(base:VirtAddr,phys: PhysAddr,level:PageLevel) -> Self {
        Self {
            base: base,
            phys,
            level
        }
    }
   /// 根据虚拟地址，获取对应的页表项在页表中的下标
    ///
    /// ## 参数
    ///
    /// - gpa: 虚拟地址
    ///
    /// ## 返回值
    ///
    /// 页表项在页表中的下标。如果addr不在当前页表所表示的虚拟地址空间中，则返回None
    pub unsafe fn index_of(&self, gpa: PhysAddr) -> Option<usize> {
        let addr = VirtAddr::new(gpa.data() & MMArch::PAGE_ADDRESS_MASK);
        let shift = (self.level - 1) as usize  * MMArch::PAGE_ENTRY_SHIFT + MMArch::PAGE_SHIFT;

        let mask = (MMArch::PAGE_ENTRY_NUM << shift) - 1;
        if addr < self.base || addr >= self.base.add(mask) {
            return None;
        } else {
            return Some((addr.data() >> shift) & MMArch::PAGE_ENTRY_MASK);
        }
    }

    pub fn next_level_table(&self, index: usize) -> Option<EptPageTable> {
        if self.level == PageLevel::Level4K {
            return None;
        }
        // 返回下一级页表
        return Some(EptPageTable::new(
            self.entry_base(index)?,
            unsafe { self.entry(index) }?.address().ok()?,
            self.level - 1,
        ));
    }
}

// // EPT物理页管理器
// pub struct EptPageManager {
//     phys2page: HashMap<PhysAddr, EptPageTable>,
// }

// impl EptPageManager {
//     pub fn new() -> Self {
//         Self {
//             phys2page: HashMap::new(),
//         }
//     }
    
// }

/// Check if MTRR is supported
pub fn check_ept_features() -> Result<(), SystemError> {
    const MTRR_ENABLE_BIT: u64 = 1 << 11;
    let ia32_mtrr_def_type = unsafe { msr::rdmsr(msr::IA32_MTRR_DEF_TYPE) };
    if (ia32_mtrr_def_type & MTRR_ENABLE_BIT) == 0 {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    Ok(())
}

/// 标志当前没有处理器持有内核映射器的锁
/// 之所以需要这个标志，是因为AtomicUsize::new(0)会把0当作一个处理器的id
const EPT_MAPPER_NO_PROCESSOR: ProcessorId = ProcessorId::INVALID;
/// 当前持有内核映射器锁的处理器
static EPT_MAPPER_LOCK_OWNER: AtomicProcessorId = AtomicProcessorId::new(EPT_MAPPER_NO_PROCESSOR);
/// 内核映射器的锁计数器
static EPT_MAPPER_LOCK_COUNT: AtomicUsize = AtomicUsize::new(0);

pub struct EptPageMapper {
    /// EPT页表映射器
    //mapper: PageMapper,//PageTableKind::EPT, LockedFrameAllocator
    /// 标记当前映射器是否为只读
    readonly: bool,
    // EPT页表根地址
    root_page_addr: PhysAddr,
    /// 页分配器
    frame_allocator: LockedFrameAllocator,
}

impl EptPageMapper{
    /// 返回最上层的ept页表
    pub fn table(&self) ->EptPageTable {
        EptPageTable::new(VirtAddr::new(0),
         self.root_page_addr,max_huge_page_level())
  }
    pub fn root_page_addr() -> PhysAddr {
        let eptp =VmxAsm::vmx_vmread(control::EPTP_FULL);
        PhysAddr::new(eptp as usize)
    }

    fn lock_cpu(cpuid: ProcessorId) -> Self {
        loop {
            match EPT_MAPPER_LOCK_OWNER.compare_exchange_weak(
                EPT_MAPPER_NO_PROCESSOR,
                cpuid,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                // 当前处理器已经持有了锁
                Err(id) if id == cpuid => break,
                // either CAS failed, or some other hardware thread holds the lock
                Err(_) => core::hint::spin_loop(),
            }
        }

        let prev_count = EPT_MAPPER_LOCK_COUNT.fetch_add(1, Ordering::Relaxed);
        compiler_fence(Ordering::Acquire);

        // 本地核心已经持有过锁，因此标记当前加锁获得的映射器为只读
        let readonly = prev_count > 0;
        let root_page_addr = Self::root_page_addr();
        kdebug!("EptPageMapper root_page_addr: {:?}", root_page_addr);
        return Self { 
            readonly,
            root_page_addr,
            frame_allocator: LockedFrameAllocator,
        };
    }

    /// @brief 锁定内核映射器, 并返回一个内核映射器对象
    /// 目前只有这一个办法可以获得EptPageMapper对象
    #[inline(always)]
    pub fn lock() -> Self {
        //fixme:得到的是cpuid还是vcpuid?
        let cpuid = smp_get_processor_id();
        return Self::lock_cpu(cpuid);
    }

    /// @brief: 检查有无gpa->hpa的映射
    #[no_mangle]
    pub fn is_mapped(&self,page_fault:&mut KvmPageFault) -> bool {
        let gpa = page_fault.gpa();
        let mut page_table = self.table();
        let mut next_page_table;
        loop {
            let index:usize = unsafe {
                if let Some(i) = page_table.index_of(PhysAddr::new(gpa as usize)){
                   i
                }else{
                    kerror!("ept page table index_of failed");
                    return false;
                }
            };
            if let Some(table)  = page_table.next_level_table(index) {
                kdebug!("ept page table next level table: {:?}", table);
                if table.level() == PageLevel::Level4K {
                    return true;
                }
                next_page_table = table;
            }else{
                return false;
            }
            page_table = next_page_table;
        
        }
    }

    /// 从当前EptPageMapper的页分配器中分配一个物理页(hpa)，并将其映射到指定的gpa
    pub fn map(
        &mut self,
        gpa: PhysAddr,
        flags: PageFlags<MMArch>,
    ) -> Option<PageFlush<MMArch>>{
        compiler_fence(Ordering::SeqCst);
        let hpa: PhysAddr = unsafe { self.frame_allocator.allocate_one() }?;
        compiler_fence(Ordering::SeqCst);

        let mut page_manager_guard: SpinLockGuard<'static, PageManager> =
            page_manager_lock_irqsave();
        if !page_manager_guard.contains(&hpa) {
            page_manager_guard.insert(hpa, Page::new(false));
        }
        self.map_gpa(gpa, hpa, flags)
    }


    ///映射一个hpa到指定的gpa
    pub fn map_gpa(
        &mut self,
        gpa: PhysAddr,
        hpa: PhysAddr,
        flags: PageFlags<MMArch>,
    ) -> Option<PageFlush<MMArch>> {
         // 验证虚拟地址和物理地址是否对齐
         if !(gpa.check_aligned(MMArch::PAGE_SIZE) && hpa.check_aligned(MMArch::PAGE_SIZE)) {
            kerror!(
                "Try to map unaligned page: gpa={:?}, hpa={:?}",
                gpa,
                hpa
            );
            return None;
        }

        let gpa = PhysAddr::new(gpa.data() & (!MMArch::PAGE_NEGATIVE_MASK));

        // TODO： 验证flags是否合法

         // 创建页表项
        let entry = PageEntry::new(hpa, flags);
        let mut table = self.table();
        kdebug!("ept page table: {:?}", table);
        kdebug!("Now eptp is : {:?}", VmxAsm::vmx_vmread(control::EPTP_FULL));
        loop{
            let i = unsafe { table.index_of(gpa).unwrap() };
            assert!(i < MMArch::PAGE_ENTRY_NUM);
            if table.level() == PageLevel::Level4K {
                //todo: 检查是否已经映射
                //fixme::按道理已经检查过了，不知道是否正确
                if table.entry_mapped(i).unwrap() {
                    kwarn!("Page gpa :: {:?} already mapped", gpa);
                }

                compiler_fence(Ordering::SeqCst);

                unsafe { table.set_entry(i, entry) };
                compiler_fence(Ordering::SeqCst);
                return Some(PageFlush::new(VirtAddr::new(gpa.data())));
            }else{
                let next_table = table.next_level_table(i);
                if let Some(next_table) = next_table {
                    table = next_table;
                } else {
                     // 分配下一级页表
                     let frame = unsafe { self.frame_allocator.allocate_one() }?;

                     // 清空这个页帧
                     unsafe { MMArch::write_bytes(MMArch::phys_2_virt(frame).unwrap(), 0, MMArch::PAGE_SIZE) };
 
                     // fixme::设置页表项的flags，可能有点问题
                     let flags: PageFlags<MMArch> =
                         unsafe { PageFlags::from_data(MMArch::ENTRY_FLAG_DEFAULT_TABLE | MMArch::ENTRY_FLAG_READWRITE) };
 
                        kdebug!("EptEntryFlags: {:?}", flags);


                        // 把新分配的页表映射到当前页表
                        unsafe { table.set_entry(i, PageEntry::new(frame, flags)) };

                        // 获取新分配的页表
                        table = table.next_level_table(i)?; 
                }
            }

        }
    }
}
