use crate::arch::mm::X86_64MMArch;
use crate::arch::vm::asm::VmxAsm;
use crate::arch::vm::kvm_host::page::KVM_MIN_FREE_MMU_PAGES;
use crate::mm::PhysAddr;
use crate::{
    arch::{mm::LockedFrameAllocator, MMArch, VirtCpuArch},
    libs::spinlock::{SpinLock, SpinLockGuard},
    mm::{page::PageMapper, MemoryManagementArch, PageTableKind},
    virt::vm::kvm_host::{vcpu::VirtCpu, Vm},
};
use alloc::{sync::Arc, vec::Vec};
use bitfield_struct::bitfield;
use core::intrinsics::likely;
use core::ops::{Add, Sub};
use log::{debug, error, warn};
use raw_cpuid::CpuId;
use system_error::SystemError;
use x86::controlregs::{Cr0, Cr4};
use x86::vmx::vmcs::guest;
use x86_64::registers::control::EferFlags;

use super::super::{vmx::vmx_info, x86_kvm_ops};
use super::mmu_internal::KvmPageFault;

const PT64_ROOT_5LEVEL: usize = 5;
const PT64_ROOT_4LEVEL: usize = 4;
const PT32_ROOT_LEVEL: usize = 2;
const PT32E_ROOT_LEVEL: usize = 3;

static mut TDP_ENABLED: bool = false;
static mut TDP_MMU_ENABLED: bool = true;
static mut TDP_MMU_ALLOWED: bool = unsafe { TDP_MMU_ENABLED };

static mut TDP_ROOT_LEVEL: usize = 0;
static mut MAX_TDP_LEVEL: usize = 0;
static mut SHADOW_ACCESSED_MASK: usize = 0;

static mut MAX_HUGE_PAGE_LEVEL: PageLevel = PageLevel::None;
pub const PAGE_SHIFT: u32 = 12;
pub const PAGE_SIZE: u64 = 1 << PAGE_SHIFT;

pub fn is_tdp_mmu_enabled() -> bool {
    unsafe { TDP_MMU_ENABLED }
}

#[allow(dead_code)]
#[repr(u8)]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum PageLevel {
    None,
    Level4K,
    Level2M,
    Level1G,
    Level512G,
    LevelNum,
}
// 实现 Add trait
impl Add<usize> for PageLevel {
    type Output = Self;

    fn add(self, other: usize) -> Self {
        let result = self as usize + other;
        match result {
            0 => PageLevel::None,
            1 => PageLevel::Level4K,
            2 => PageLevel::Level2M,
            3 => PageLevel::Level1G,
            4 => PageLevel::Level512G,
            5 => PageLevel::LevelNum,
            _ => PageLevel::LevelNum, // 超出范围时返回 LevelNum
        }
    }
}
// 实现 Sub trait
impl Sub<usize> for PageLevel {
    type Output = Self;

    fn sub(self, other: usize) -> Self {
        let result = self as isize - other as isize;
        match result {
            0 => PageLevel::None,
            1 => PageLevel::Level4K,
            2 => PageLevel::Level2M,
            3 => PageLevel::Level1G,
            4 => PageLevel::Level512G,
            5 => PageLevel::LevelNum,
            _ => PageLevel::None, // 超出范围时返回 None
        }
    }
}
impl PageLevel {
    fn kvm_hpage_gfn_shift(level: u8) -> u32 {
        ((level - 1) * 9) as u32
    }

    fn kvm_hpage_shift(level: u8) -> u32 {
        PAGE_SHIFT + Self::kvm_hpage_gfn_shift(level)
    }

    fn kvm_hpage_size(level: u8) -> u64 {
        1 << Self::kvm_hpage_shift(level)
    }
    /// 计算每个大页包含的页数
    ///
    /// # 参数
    /// - `level`: 页级别
    ///
    /// # 返回值
    /// 返回每个大页包含的页数
    pub fn kvm_pages_per_hpage(level: u8) -> u64 {
        Self::kvm_hpage_size(level) / PAGE_SIZE
    }
}
///计算给定 GFN（Guest Frame Number）在指定级别上的对齐值
pub fn gfn_round_for_level(gfn: u64, level: u8) -> u64 {
    gfn & !(PageLevel::kvm_pages_per_hpage(level) - 1)
}

#[derive(Debug)]
pub struct LockedKvmMmu {
    inner: SpinLock<KvmMmu>,
}

impl LockedKvmMmu {
    pub fn new(mmu: KvmMmu) -> Arc<Self> {
        Arc::new(Self {
            inner: SpinLock::new(mmu),
        })
    }

    pub fn lock(&self) -> SpinLockGuard<KvmMmu> {
        self.inner.lock()
    }
}

pub type KvmMmuPageFaultHandler =
    fn(vcpu: &mut VirtCpu, page_fault: &KvmPageFault) -> Result<i32, SystemError>;

#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct KvmMmu {
    pub root: KvmMmuRootInfo,
    pub cpu_role: KvmCpuRole,
    pub root_role: KvmMmuPageRole,
    pub page_fault: Option<KvmMmuPageFaultHandler>,

    pkru_mask: u32,

    prev_roots: [KvmMmuRootInfo; Self::KVM_MMU_NUM_PREV_ROOTS],

    pae_root: Vec<u64>,

    pub pdptrs: [u64; 4],
}

impl KvmMmu {
    pub fn _save_pdptrs(&mut self) {
        self.pdptrs[0] = VmxAsm::vmx_vmread(guest::PDPTE0_FULL);
        self.pdptrs[1] = VmxAsm::vmx_vmread(guest::PDPTE1_FULL);
        self.pdptrs[2] = VmxAsm::vmx_vmread(guest::PDPTE2_FULL);
        self.pdptrs[3] = VmxAsm::vmx_vmread(guest::PDPTE3_FULL);
    }
    const KVM_MMU_NUM_PREV_ROOTS: usize = 3;
    pub const INVALID_PAGE: u64 = u64::MAX;

    #[inline]
    pub fn tdp_enabled() -> bool {
        unsafe { TDP_ENABLED }
    }

    #[inline]
    pub fn tdp_root_level() -> usize {
        unsafe { TDP_ROOT_LEVEL }
    }

    #[inline]
    pub fn max_tdp_level() -> usize {
        unsafe { MAX_TDP_LEVEL }
    }

    #[inline]
    pub fn ad_enabled() -> bool {
        unsafe { SHADOW_ACCESSED_MASK != 0 }
    }

    /// 初始化mmu的配置，因为其是无锁的，所以该函数只能在初始化vmx时调用
    pub fn kvm_configure_mmu(
        enable_tdp: bool,
        tdp_forced_root_level: usize,
        tdp_max_root_level: usize,
        tdp_huge_page_level: PageLevel,
    ) {
        unsafe {
            TDP_ENABLED = enable_tdp;
            TDP_ROOT_LEVEL = tdp_forced_root_level;
            MAX_TDP_LEVEL = tdp_max_root_level;

            TDP_MMU_ENABLED = TDP_MMU_ALLOWED && TDP_ENABLED;

            if TDP_ENABLED {
                MAX_HUGE_PAGE_LEVEL = tdp_huge_page_level;
            } else if CpuId::new()
                .get_extended_processor_and_feature_identifiers()
                .unwrap()
                .has_1gib_pages()
            {
                MAX_HUGE_PAGE_LEVEL = PageLevel::Level1G;
            } else {
                MAX_HUGE_PAGE_LEVEL = PageLevel::Level2M;
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct KvmMmuRootInfo {
    pub pgd: u64,
    pub hpa: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct KvmCpuRole {
    base: KvmMmuPageRole,
    extend: KvmMmuExtenedRole,
}

impl PartialEq for KvmCpuRole {
    fn eq(&self, other: &Self) -> bool {
        self.base.0 == other.base.0 && self.extend.0 == other.extend.0
    }
}

/// ### 用于跟踪影子页（包括 TDP 页）的属性，以确定页面是否可以在给定的 MMU 上下文中使用。
#[bitfield(u32)]
pub struct KvmMmuPageRole {
    /// 表示页表级别，占用 4 位。对于普通的页表，取值是 2（二级页表）、3（三级页表）、4（四级页表）和 5（五级页表）
    #[bits(4)]
    pub level: u32,
    /// 页表项是否为 4 字节，占用 1 位。在非 PAE 分页模式下，该值为 1
    has_4_byte_gpte: bool,
    /// 表示页表项所在的象限，占用 2 位。该字段仅在 has_4_byte_gpte 为 1 时有效。
    #[bits(2)]
    quadrant: u32,
    /// 页面是否直接映射
    direct: bool,
    /// 页面的访问权限
    #[bits(3)]
    access: u32,
    /// 页面是否无效
    invalid: bool,
    /// 页面是否启用 NX（不可执行）位
    efer_nx: bool,
    /// CR0 寄存器中的写保护位(WP)是否被置位
    cr0_wp: bool,
    /// SMEP（Supervisor Mode Execution Protection）和非写保护位的组合
    smep_andnot_wp: bool,
    /// SMAP（Supervisor Mode Access Prevention）和非写保护位的组合
    smap_andnot_wp: bool,
    /// 页面是否禁用访问位（Accessed Bit）
    ad_disabled: bool,
    /// 当前页是否处于客户机模式
    guest_mode: bool,
    /// 是否将此页透传给客户机
    passthrough: bool,
    /// 未使用位域
    #[bits(5)]
    unused: u32,
    /// 表示 SMM（System Management Mode）模式
    #[bits(8)]
    pub smm: u32,
}

impl KvmMmuPageRole {
    pub fn is_cr0_pg(&self) -> bool {
        self.level() > 0
    }

    pub fn is_cr4_pae(&self) -> bool {
        !self.has_4_byte_gpte()
    }
    pub fn get_direct(&self) -> bool {
        self.direct()
    }
}

#[bitfield(u32)]
pub struct KvmMmuExtenedRole {
    valid: bool,
    execonly: bool,
    cr4_pse: bool,
    cr4_pke: bool,
    cr4_smap: bool,
    cr4_smep: bool,
    cr4_la57: bool,
    efer_lma: bool,
    #[bits(24)]
    unused: u32,
}

pub struct KvmMmuRoleRegs {
    pub cr0: Cr0,
    pub cr4: Cr4,
    pub efer: EferFlags,
}

/// page falut的返回值, 用于表示页面错误的处理结果
/// 应用在handle_mmio_page_fault()、mmu.page_fault()、fast_page_fault()和
/// kvm_mmu_do_page_fault()等
#[derive(Debug, Eq, PartialEq, FromPrimitive, Clone)]
#[repr(u32)]
pub enum PFRet {
    Continue,       // RET_PF_CONTINUE: 到目前为止一切正常，继续处理页面错误。
    Retry,          // RET_PF_RETRY: 让 CPU 再次对该地址发生页面错误。
    Emulate,        // RET_PF_EMULATE: MMIO 页面错误，直接模拟指令。
    Invalid,        // RET_PF_INVALID: SPTE 无效，让实际的页面错误路径更新它。
    Fixed,          // RET_PF_FIXED: 故障的条目已经被修复
    Spurious,       // RET_PF_SPURIOUS: 故障的条目已经被修复，例如由另一个 vCPU 修复。
    Err = u32::MAX, // 错误
}
impl From<PFRet> for i32 {
    fn from(pf_ret: PFRet) -> Self {
        pf_ret as i32
    }
}
impl From<i32> for PFRet {
    fn from(value: i32) -> Self {
        match value {
            0 => PFRet::Continue,
            1 => PFRet::Retry,
            2 => PFRet::Emulate,
            3 => PFRet::Invalid,
            4 => PFRet::Fixed,
            5 => PFRet::Spurious,
            _ => PFRet::Err, // 默认返回 Invalid
        }
    }
}
impl VirtCpuArch {
    pub fn kvm_init_mmu(&mut self) {
        let regs = self.role_regs();
        let cpu_role = self.calc_cpu_role(&regs);

        if self.walk_mmu.is_some()
            && self.nested_mmu.is_some()
            && Arc::ptr_eq(
                self.walk_mmu.as_ref().unwrap(),
                self.nested_mmu.as_ref().unwrap(),
            )
        {
            todo!()
        } else if KvmMmu::tdp_enabled() {
            self.init_tdp_mmu(cpu_role);
        } else {
            todo!()
        }
    }

    fn unload_mmu(&mut self) {
        // TODO
    }

    pub fn reset_mmu_context(&mut self) {
        self.unload_mmu();
        self.kvm_init_mmu();
    }

    fn role_regs(&mut self) -> KvmMmuRoleRegs {
        KvmMmuRoleRegs {
            cr0: self.read_cr0_bits(Cr0::CR0_ENABLE_PAGING | Cr0::CR0_WRITE_PROTECT),
            cr4: self.read_cr4_bits(
                Cr4::CR4_ENABLE_PSE
                    | Cr4::CR4_ENABLE_PAE
                    | Cr4::CR4_ENABLE_LA57
                    | Cr4::CR4_ENABLE_SMEP
                    | Cr4::CR4_ENABLE_SMAP
                    | Cr4::CR4_ENABLE_PROTECTION_KEY,
            ),
            efer: self.efer,
        }
    }

    fn calc_cpu_role(&self, regs: &KvmMmuRoleRegs) -> KvmCpuRole {
        let mut role = KvmCpuRole::default();
        let base = &mut role.base;
        let ext = &mut role.extend;
        base.set_access(0b111);
        base.set_smm(self.is_smm() as u32);
        base.set_guest_mode(self.is_guest_mode());
        ext.set_valid(true);

        if !regs.cr0.contains(Cr0::CR0_ENABLE_PAGING) {
            base.set_direct(true);
            return role;
        }

        base.set_efer_nx(regs.efer.contains(EferFlags::NO_EXECUTE_ENABLE));
        base.set_cr0_wp(regs.cr0.contains(Cr0::CR0_WRITE_PROTECT));
        base.set_smep_andnot_wp(
            regs.cr4.contains(Cr4::CR4_ENABLE_SMEP) && !regs.cr0.contains(Cr0::CR0_WRITE_PROTECT),
        );
        base.set_smap_andnot_wp(
            regs.cr4.contains(Cr4::CR4_ENABLE_SMAP) && !regs.cr0.contains(Cr0::CR0_WRITE_PROTECT),
        );

        base.set_has_4_byte_gpte(!regs.cr4.contains(Cr4::CR4_ENABLE_PAE));

        if regs.efer.contains(EferFlags::LONG_MODE_ACTIVE) {
            let level = if regs.cr4.contains(Cr4::CR4_ENABLE_LA57) {
                PT64_ROOT_5LEVEL as u32
            } else {
                PT64_ROOT_4LEVEL as u32
            };
            base.set_level(level);
        } else if regs.cr4.contains(Cr4::CR4_ENABLE_PAE) {
            base.set_level(PT32E_ROOT_LEVEL as u32);
        } else {
            base.set_level(PT32_ROOT_LEVEL as u32);
        }

        ext.set_cr4_smep(regs.cr4.contains(Cr4::CR4_ENABLE_SMEP));
        ext.set_cr4_smap(regs.cr4.contains(Cr4::CR4_ENABLE_SMAP));
        ext.set_cr4_pse(regs.cr4.contains(Cr4::CR4_ENABLE_PSE));
        ext.set_cr4_pke(
            regs.efer.contains(EferFlags::LONG_MODE_ACTIVE)
                && regs.cr4.contains(Cr4::CR4_ENABLE_PROTECTION_KEY),
        );
        ext.set_cr4_la57(
            regs.efer.contains(EferFlags::LONG_MODE_ACTIVE)
                && regs.cr4.contains(Cr4::CR4_ENABLE_LA57),
        );
        ext.set_efer_lma(regs.efer.contains(EferFlags::LONG_MODE_ACTIVE));

        role
    }

    /// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kvm/mmu/mmu.c#6019
    pub fn vcpu_arch_mmu_create(&mut self) {
        if vmx_info().tdp_enabled() {
            self.guset_mmu = Some(self._mmu_create());
        }

        self.root_mmu = Some(self._mmu_create());
        self.mmu = self.root_mmu.clone();
        self.walk_mmu = self.root_mmu.clone();
    }

    fn _mmu_create(&self) -> Arc<LockedKvmMmu> {
        let mut mmu = KvmMmu::default();

        mmu.root.hpa = KvmMmu::INVALID_PAGE;
        mmu.root.pgd = 0;

        for role in &mut mmu.prev_roots {
            role.hpa = KvmMmu::INVALID_PAGE;
            role.pgd = KvmMmu::INVALID_PAGE;
        }

        if KvmMmu::tdp_enabled() && self.mmu_get_tdp_level() > PT32E_ROOT_LEVEL {
            return LockedKvmMmu::new(mmu);
        }

        mmu.pae_root
            .resize(MMArch::PAGE_SIZE / core::mem::size_of::<u64>(), 0);

        return LockedKvmMmu::new(mmu);
    }

    fn mmu_get_tdp_level(&self) -> usize {
        if KvmMmu::tdp_root_level() != 0 {
            return KvmMmu::tdp_root_level();
        }

        if KvmMmu::max_tdp_level() == 5 && self.max_phyaddr <= 48 {
            return 4;
        }

        return KvmMmu::max_tdp_level();
    }

    pub fn init_tdp_mmu(&mut self, cpu_role: KvmCpuRole) {
        let context = self.root_mmu();
        let mut context = context.lock();
        let root_role = self.calc_tdp_mmu_root_page_role(cpu_role);

        if cpu_role == context.cpu_role && root_role.0 == context.root_role.0 {
            return;
        }

        context.cpu_role = cpu_role;
        context.root_role = root_role;

        // todo 设置函数集

        if !context.cpu_role.base.is_cr0_pg() {
            // todo: context->gva_to_gpa = nonpaging_gva_to_gpa;
            warn!("context->gva_to_gpa = nonpaging_gva_to_gpa todo!");
        } else if context.cpu_role.base.is_cr4_pae() {
            // todo: context->gva_to_gpa = paging64_gva_to_gpa;
            warn!("context->gva_to_gpa = paging64_gva_to_gpa todo!");
        } else {
            // todo: context->gva_to_gpa = paging32_gva_to_gpa;
            warn!("context->gva_to_gpa = paging32_gva_to_gpa todo!");
        }

        // todo:
        // reset_guest_paging_metadata(vcpu, context);
        // reset_tdp_shadow_zero_bits_mask(context);
    }

    #[inline]
    pub fn root_mmu(&self) -> &Arc<LockedKvmMmu> {
        self.root_mmu.as_ref().unwrap()
    }

    #[inline]
    pub fn mmu(&self) -> SpinLockGuard<KvmMmu> {
        self.mmu.as_ref().unwrap().lock()
    }

    fn calc_tdp_mmu_root_page_role(&self, cpu_role: KvmCpuRole) -> KvmMmuPageRole {
        let mut role = KvmMmuPageRole::default();

        role.set_access(0b111);
        role.set_cr0_wp(true);
        role.set_efer_nx(true);
        role.set_smm(cpu_role.base.smm());
        role.set_guest_mode(cpu_role.base.guest_mode());
        role.set_ad_disabled(!KvmMmu::ad_enabled());
        role.set_level(self.mmu_get_tdp_level() as u32);
        role.set_direct(true);
        role.set_has_4_byte_gpte(false);

        role
    }
}

impl VirtCpu {
    pub fn kvm_mmu_reload(&mut self, vm: &Vm) -> Result<(), SystemError> {
        if likely(self.arch.mmu().root.hpa != KvmMmu::INVALID_PAGE) {
            return Ok(());
        }

        return self.kvm_mmu_load(vm);
    }

    pub fn kvm_mmu_load(&mut self, vm: &Vm) -> Result<(), SystemError> {
        let direct = self.arch.mmu().root_role.direct();
        self.mmu_topup_memory_caches(!direct)?;
        self.mmu_alloc_special_roots()?;

        if direct {
            self.mmu_alloc_direct_roots(vm)?;
        } else {
            self.mmu_alloc_shadow_roots(vm)?;
        }

        // TODO: kvm_mmu_sync_roots

        self.kvm_mmu_load_pgd(vm);

        Ok(())
    }

    pub fn kvm_mmu_load_pgd(&mut self, vm: &Vm) {
        let root_hpa = self.arch.mmu().root.hpa;
        debug!("kvm_mmu_load_pgd::root_hpa = {:#x}", root_hpa);
        if root_hpa == KvmMmu::INVALID_PAGE {
            return;
        }

        let level = self.arch.mmu().root_role.level();
        x86_kvm_ops().load_mmu_pgd(self, vm, root_hpa, level);
    }

    fn mmu_topup_memory_caches(&mut self, _maybe_indirect: bool) -> Result<(), SystemError> {
        // TODO
        Ok(())
    }

    fn mmu_alloc_special_roots(&mut self) -> Result<(), SystemError> {
        // TODO
        Ok(())
    }

    fn mmu_alloc_direct_roots(&mut self, vm: &Vm) -> Result<(), SystemError> {
        let shadow_root_level = self.arch.mmu().root_role.level();
        let _r: Result<(), SystemError> = self.make_mmu_pages_available(vm);
        let root: PhysAddr;
        if KvmMmu::tdp_enabled() {
            root = self.kvm_tdp_mmu_get_vcpu_root_hpa().unwrap();
            let mut mmu = self.arch.mmu();
            mmu.root.hpa = root.data() as u64;
        } else if shadow_root_level >= PT64_ROOT_4LEVEL as u32 {
            todo!()
        } else if shadow_root_level == PT32E_ROOT_LEVEL as u32 {
            todo!()
        } else {
            error!("Bad TDP root level = {}", shadow_root_level);
            return Err(SystemError::EIO);
        }
        /* root.pgd is ignored for direct MMUs. */
        self.arch.mmu().root.pgd = 0;
        Ok(())
    }

    fn mmu_alloc_shadow_roots(&mut self, _vm: &Vm) -> Result<(), SystemError> {
        todo!();
    }
    fn make_mmu_pages_available(&mut self, vm: &Vm) -> Result<(), SystemError> {
        let avail = Self::kvm_mmu_available_pages(vm);
        if likely(avail >= KVM_MIN_FREE_MMU_PAGES) {
            return Ok(());
        }
        //kvm_mmu_zap_oldest_mmu_pages(vm, KVM_REFILL_PAGES - avail);
        if Self::kvm_mmu_available_pages(vm) == 0 {
            return Err(SystemError::ENOSPC);
        }
        Ok(())
    }
    fn kvm_mmu_available_pages(vm: &Vm) -> usize {
        if vm.arch.n_max_mmu_pages > vm.arch.n_used_mmu_pages {
            return vm.arch.n_max_mmu_pages - vm.arch.n_used_mmu_pages;
        }
        return 0;
    }
    fn kvm_tdp_mmu_get_vcpu_root_hpa(&self) -> Result<PhysAddr, SystemError> {
        //todo Check for an existing root before allocating a new one.  Note, the
        // role check prevents consuming an invalid root.
        let root = self.tdp_mmu_alloc_sp().unwrap();
        Ok(PhysAddr::new(root as usize))
    }
    fn tdp_mmu_alloc_sp(&self) -> Result<u64, SystemError> {
        // 申请并创建新的页表
        let mapper: crate::mm::page::PageMapper<X86_64MMArch, LockedFrameAllocator> = unsafe {
            PageMapper::create(PageTableKind::EPT, LockedFrameAllocator)
                .ok_or(SystemError::ENOMEM)?
        };

        let ept_root_hpa = mapper.table().phys();

        self.arch.mmu().root.hpa = ept_root_hpa.data() as u64;

        debug!("ept_root_hpa:{:x}!", ept_root_hpa.data() as u64);

        return Ok(self.arch.mmu().root.hpa);
    }
}
