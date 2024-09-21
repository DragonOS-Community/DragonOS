use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{intrinsics::unlikely, ops::Index};
use x86::vmx::vmcs::{guest, host};

use system_error::SystemError;

use crate::{
    arch::{vm::{
        asm::VmxAsm,
        kvm_host::{EmulType, KVM_PFN_NOSLOT},
        mmu::{
            mmu::{PFRet, PageLevel},
        },
        mtrr::kvm_mtrr_check_gfn_range_consistency,
        vmx::{ept::EptPageMapper, PageFaultErr},
    }, MMArch},
    kdebug, kwarn,
    libs::spinlock::SpinLockGuard,
    mm::{page::PageFlags, syscall::ProtFlags, virt_2_phys, PhysAddr},
    virt::{
        kvm::host_mem::PAGE_SHIFT,
        vm::kvm_host::{
            mem::{LockedKvmMemSlot, LockedVmMemSlotSet, UserMemRegionFlag, __gfn_to_pfn_memslot},
            search_memslots,
            vcpu::VirtCpu,
            Vm,
        },
    },
};

use super::mmu::{gfn_round_for_level, is_tdp_mmu_enabled, KvmMmuPageRole};

#[derive(Debug, Default)]
pub struct KvmMmuPage {
    pub tdp_mmu_page: bool, // 标记是否为 TDP（Two-Dimensional Paging）页表页
    pub gfn: u64,           // 客户机帧号（Guest Frame Number）

    /*
     * The following two entries are used to key the shadow page in the
     * hash table.暫時沒看出來
     */
    pub role: KvmMmuPageRole,
    pub spt: u64, // 指向页表条目（SPTE）的指针
    pub mmu_seq: u64,
    pub map_writable: bool,
    pub write_fault_to_shadow_pgtable: bool,
}

#[derive(Debug, Default)]
pub struct KvmPageFault {
    // vcpu.do_page_fault 的参数

    // addr是guestOS传进来的gpa
    addr: PhysAddr,
    error_code: u32,
    prefetch: bool,

    // 从 error_code 派生
    exec: bool,
    write: bool,
    present: bool,
    rsvd: bool,
    user: bool,

    // 从 mmu 和全局状态派生
    is_tdp: bool,
    nx_huge_page_workaround_enabled: bool,

    // 是否可以创建大于 4KB 的映射，或由于 NX 大页被禁止
    huge_page_disallowed: bool,

    // 此故障可以创建的最大页面大小
    max_level: u8,

    // 基于 max_level 和主机映射使用的页面大小可以创建的页面大小
    req_level: u8,

    // 基于 req_level 和 huge_page_disallowed 将创建的页面大小
    goal_level: u8,

    // 移位后的 addr，或如果 addr 是 gva 则是访客页表遍历的结果
    gfn: u64, // gfn_t 通常是一个 64 位地址

    // 包含 gfn 的 memslot。可能为 None
    slot: Option<Arc<LockedKvmMemSlot>>,

    // kvm_faultin_pfn 的输出
    mmu_seq: u64,

    // kvm_pfn_t 通常是一个 64 位地址,相当于知道了hpa
    pfn: u64,
    hva: u64, // hva_t 通常是一个 64 位地址
    map_writable: bool,

    // 表示访客正在尝试写入包含用于翻译写入本身的一个或多个 PTE 的 gfn
    write_fault_to_shadow_pgtable: bool,
}
impl KvmPageFault {
    pub fn pfn(&self) -> u64 {
        self.pfn
    }
    pub fn gfn(&self) -> u64 {
        self.gfn
    }
    pub fn gpa(&self) -> u64 {
        self.addr.data() as u64
    }
}

impl VirtCpu {
    #[inline(never)]
    pub fn page_fault(
        &mut self,
        vm: &Vm,
        cr2_or_gpa: u64,
        mut error_code: u64,
        insn: Option<u64>,
        insn_len: usize,
    ) -> Result<u64, SystemError> {
        let mut emulation_type = EmulType::PF;
        let direct = self.arch.mmu().root_role.get_direct();
        // IMPLICIT_ACCESS 是一个 KVM 定义的标志，用于在模拟触发隐式访问的指令时正确执行 SMAP 检查。
        // 防止内核态代码（超级用户模式）访问用户态内存。它是通过设置 CR4 寄存器中的 SMAP 位来启用的。
        // 如果硬件生成的错误代码与 KVM 定义的值冲突，则发出警告。
        // 清除该标志并继续，不终止虚拟机，因为 KVM 不可能依赖于 KVM 不知道的标志。
        if error_code & PageFaultErr::PFERR_IMPLICIT_ACCESS.bits() != 0 {
            kwarn!("Implicit access error code detected");
            error_code &= !PageFaultErr::PFERR_IMPLICIT_ACCESS.bits();
        }

        //if self.arch.mmu().root.hpa != KvmMmu::INVALID_PAGE {
        //    return Ok(PFRet::Retry as u64);
        //}

        let mut r = PFRet::Invalid;
        if unlikely(error_code & PageFaultErr::PFERR_RSVD.bits() != 0) {
            todo!();
            // r = self.handle_mmio_page_fault(cr2_or_gpa, direct)?;
            // if r == PFRes::Emulate{
            //    return x86_emulate_instruction(vcpu, cr2_or_gpa, emulation_type, insn,insn_len)	       insn_len);
            // }
        }

        if r == PFRet::Invalid {
            r = self
                .do_page_fault(vm, cr2_or_gpa, error_code as u32, false, emulation_type)?
                .into();
            if r == PFRet::Invalid {
                return Err(SystemError::EIO);
            }
        }

        if r == PFRet::Err {
            //return SystemError::EFAULT;
            todo!()
        }
        if r != PFRet::Emulate {
            return Ok(1);
        }

        // 在模拟指令之前，检查错误代码是否由于在翻译客户机页面时的只读（RO）违规。
        // 这可能发生在使用嵌套虚拟化和嵌套分页的情况下。如果是这样，我们只需取消页面保护并恢复客户机。
        let pferr_nested_guest_page = PageFaultErr::PFERR_GUEST_PAGE
            | PageFaultErr::PFERR_WRITE
            | PageFaultErr::PFERR_PRESENT;
        if self.arch.mmu().root_role.get_direct()
            && (error_code & pferr_nested_guest_page.bits()) == pferr_nested_guest_page.bits()
        {
            todo!()
        }

        // self.arch.mmu.page_fault 返回 RET_PF_EMULATE，但我们仍然可以乐观地尝试取消页面保护，
        // 并让处理器重新执行导致页面故障的指令。不允许重试 MMIO 模拟，因为这不仅毫无意义，
        // 而且可能导致进入无限循环，因为处理器会不断在不存在的 MMIO 地址上发生故障。
        // 重试来自嵌套客户机的指令也是毫无意义且危险的，因为我们只显式地影子 L1 的页表，
        // 即为 L1 取消保护并不会神奇地修复导致 L2 失败的问题。
        // if !self.mmio_info_in_cache(cr2_or_gpa, direct) && !self.arch.is_guest_mode() {
        //     emulation_type |= EmulType::ALLOW_RETRY_PF;
        // }

        // self.emulate_instruction(cr2_or_gpa, emulation_type, insn, insn_len)
        todo!("emulate_instruction")
    }

    fn do_page_fault(
        &mut self,
        vm: &Vm,
        cr2_or_gpa: u64,
        error_code: u32,
        prefetch: bool,
        mut emultype: EmulType,
    ) -> Result<u64, SystemError> {
        //初始化page fault
        let mut page_fault = KvmPageFault {
            addr: PhysAddr::new(cr2_or_gpa as usize),
            error_code,
            exec: error_code & PageFaultErr::PFERR_FETCH.bits() as u32 != 0,
            write: error_code & PageFaultErr::PFERR_WRITE.bits() as u32 != 0,
            present: error_code & PageFaultErr::PFERR_PRESENT.bits() as u32 != 0,
            rsvd: error_code & PageFaultErr::PFERR_RSVD.bits() as u32 != 0,
            user: error_code & PageFaultErr::PFERR_USER.bits() as u32 != 0,
            prefetch,
            is_tdp: true,
            nx_huge_page_workaround_enabled: false, //todo
            max_level: PageLevel::Level1G as u8,
            req_level: PageLevel::Level4K as u8,
            goal_level: PageLevel::Level4K as u8,
            ..Default::default()
        };
        //处理直接映射
        if self.arch.mmu().root_role.get_direct() {
            page_fault.gfn = (page_fault.addr.data() >> PAGE_SHIFT) as u64;
            page_fault.slot = self.gfn_to_memslot(page_fault.gfn, vm); //kvm_vcpu_gfn_to_memslot(vcpu, fault.gfn);没完成
        }
        //异步页面错误（Async #PF），也称为预取错误（prefetch faults），
        //从客机（guest）的角度来看并不是错误，并且已经在原始错误发生时被计数。
        if !prefetch {
            self.stat.pf_taken += 1;
        }

        let r = if page_fault.is_tdp {
            self.tdp_page_fault(vm, &mut page_fault).unwrap()
        } else {
            let handle = self.arch.mmu().page_fault.unwrap();
            handle(self, &page_fault).unwrap()
        };

        if page_fault.write_fault_to_shadow_pgtable {
            emultype |= EmulType::WRITE_PF_TO_SP;
        }
        //类似于上面的情况，预取错误并不是真正的虚假错误，并且异步页面错误路径不会进行仿真。
        //然而，确实要统计由异步页面错误处理程序修复的错误，否则它们将永远不会被统计。
        match PFRet::from(r) {
            PFRet::Fixed => self.stat.pf_fixed += 1,
            PFRet::Emulate => self.stat.pf_emulate += 1,
            PFRet::Spurious => self.stat.pf_spurious += 1,
            _ => {}
        }

        Ok(r)
    }

    pub fn gfn_to_memslot(&self, gfn: u64, vm: &Vm) -> Option<Arc<LockedKvmMemSlot>> {
        let slot_set: Arc<LockedVmMemSlotSet> = self.kvm_vcpu_memslots(vm);
        //...todo

        search_memslots(slot_set, gfn)
    }
    pub fn kvm_vcpu_memslots(&self, vm: &Vm) -> Arc<LockedVmMemSlotSet> {
        vm.memslots.index(0).clone()
    }
    fn tdp_page_fault(
        &mut self,
        vm: &Vm,
        page_fault: &mut KvmPageFault,
    ) -> Result<u64, SystemError> {
        // 如果 shadow_memtype_mask 为真，并且虚拟机有非一致性 DMA
        //if shadow_memtype_mask != 0 && self.kvm().lock().arch.noncoherent_dma_count > 0 {
        while page_fault.max_level > PageLevel::Level4K as u8 {
            let page_num = PageLevel::kvm_pages_per_hpage(page_fault.max_level);

            //低地址对齐
            let base = gfn_round_for_level(page_fault.gfn, page_fault.max_level);

            //检查给定 GFN 范围内的内存类型是否一致，暂未实现
            if kvm_mtrr_check_gfn_range_consistency(self, base, page_num) {
                break;
            }

            page_fault.max_level -= 1;
        }
        //}

        if is_tdp_mmu_enabled() {
            return self.kvm_tdp_mmu_page_fault(vm, page_fault);
        }

        self.direct_page_fault(page_fault)
    }
    fn kvm_tdp_mmu_page_fault(
        &self,
        vm: &Vm,
        page_fault: &mut KvmPageFault,
    ) -> Result<u64, SystemError> {
        //page_fault_handle_page_track(page_fault)
        //fast_page_fault(page_fault);
        //mmu_topup_memory_caches(false);//补充内存缓存
        let mut r = self
            .kvm_faultin_pfn(vm, page_fault, 1 | 1 << 1 | 1 << 2)
            .unwrap();
        if r != PFRet::Continue {
            return Ok(r.into());
        }

        r = PFRet::Retry;
        //实际的映射
        self.tdp_map(page_fault);
        Ok(r.into())
    }
    fn tdp_map(&self, page_fault: &mut KvmPageFault) -> Result<u64, SystemError> {
        //没有实现SPTE，huge page相关
        let mmu = self.arch.mmu();
        let kvm = self.kvm();
        let ret = PFRet::Retry;
        let mut mapper = EptPageMapper::lock();
        if mapper.is_mapped(page_fault) {
            kdebug!("page fault is already mapped");
            return Ok(PFRet::Continue.into());
        };
        let page_flags = PageFlags::from_prot_flags(ProtFlags::from_bits_truncate(0x7_u64), false);
        mapper.map(PhysAddr::new(page_fault.gpa() as usize), page_flags);
        if mapper.is_mapped(page_fault) {
            kdebug!("page fault is mapped now");
        };
        kdebug!("The ept_root_addr is {:?}", EptPageMapper::root_page_addr());
        //todo: 一些参数的更新
        Ok(PFRet::Fixed.into())
        //todo!()
    }

    fn direct_page_fault(&self, page_fault: &KvmPageFault) -> Result<u64, SystemError> {
        todo!()
    }

    fn kvm_faultin_pfn(
        &self,
        vm: &Vm,
        page_fault: &mut KvmPageFault,
        access: u32,
    ) -> Result<PFRet, SystemError> {
        page_fault.mmu_seq = vm.mmu_invalidate_seq;
        self.__kvm_faultin_pfn(page_fault)
    }
    fn __kvm_faultin_pfn(&self, page_fault: &mut KvmPageFault) -> Result<PFRet, SystemError> {
        let slot = &page_fault.slot;
        let mut is_async = false;
        if slot.is_none() {
            return Err(SystemError::KVM_HVA_ERR_BAD);
        }
        let slot = slot.as_ref().unwrap().read();

        if slot.get_flags().bits() & UserMemRegionFlag::KVM_MEMSLOT_INVALID.bits() != 0 {
            return Ok(PFRet::Retry);
        }
        if !slot.is_visible() {
            /* 不要将私有内存槽暴露给 L2。 */
            if self.arch.is_guest_mode() {
                drop(slot);
                page_fault.slot = None;
                page_fault.pfn = KVM_PFN_NOSLOT;
                page_fault.map_writable = false;
                return Ok(PFRet::Continue);
            }
            /*
             * 如果 APIC 访问页面存在但被禁用，则直接进行仿真，
             * 而不缓存 MMIO 访问或创建 MMIO SPTE。
             * 这样，当 AVIC 重新启用时，不需要清除缓存。
             */
            // if slot.get_id() == APIC_ACCESS_PAGE_PRIVATE_MEMSLOT && !self.kvm_apicv_activated()
            // {
            //     return PFRet::Emulate;
            // }
        }

        // 尝试将 GFN 转换为 PFN
        let guest_cr3 = VmxAsm::vmx_vmread(guest::CR3);
        let host_cr3 = VmxAsm::vmx_vmread(host::CR3);
        kdebug!("guest_cr3={:x}, host_cr3={:x}", guest_cr3, host_cr3);
        page_fault.pfn = __gfn_to_pfn_memslot(
            Some(&slot),
            page_fault.gfn,
            false,
            false,
            &mut is_async,
            page_fault.write,
            &mut page_fault.map_writable,
            &mut page_fault.hva,
        )?;

        if !is_async {
            return Ok(PFRet::Continue); /* *pfn 已经有正确的页面 */
        }

        // if !page_fault.prefetch && self.kvm_can_do_async_pf() {
        //     self.trace_kvm_try_async_get_page(page_fault.addr, page_fault.gfn);
        //     if self.kvm_find_async_pf_gfn(page_fault.gfn) {
        //         self.trace_kvm_async_pf_repeated_fault(page_fault.addr, page_fault.gfn);
        //         self.kvm_make_request(KVM_REQ_APF_HALT);
        //         return Ok(PFRet::Retry);
        //     } else if self.kvm_arch_setup_async_pf(page_fault.addr, page_fault.gfn) {
        //         return Ok(PFRet::Retry);
        //     }
        // }
        Ok(PFRet::Continue)
    }
}
