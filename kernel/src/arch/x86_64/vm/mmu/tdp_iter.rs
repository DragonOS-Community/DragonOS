// use crate::{
//     arch::vm::mmu::mmu::gfn_round_for_level,
//     mm::{virt_2_phys, PhysAddr, VirtAddr},
//     time::sleep,
//     virt::kvm::host_mem::PAGE_SHIFT,
// };

// use super::{
//     mmu::{PageLevel, PAGE_SIZE},
//     mmu_internal::KvmMmuPage,
// };

// pub const PT64_ROOT_MAX_LEVEL: usize = 5; //通常只用到4级，但是确实有5级的情况
// pub const PT_LEVEL_BITS: u8 = 9; // 每个页表级别的位数
// pub const PT64_ENT_PER_PAGE: u32 = 1 << 9;
// pub const PTE_LEN: usize = 64;

// //Bits 51:12 are from the EPT PDPTE
// pub const PT64_BASE_ADDR_MASK: u64 = ((1u64 << 52) - 1) & !(PAGE_SIZE - 1);

// pub fn shadow_pt_index(addr: u64, level: u8) -> u64 {
//     (addr >> (PAGE_SHIFT as u8 + (level - 1) * PT_LEVEL_BITS)) & ((1 << PT_LEVEL_BITS) - 1)
// }
// pub fn is_last_spte(pte: u64, level: u8) -> bool {
//     level == PageLevel::Level4K as u8 || is_large_pte(pte)
// }
// pub fn is_shadow_present_pte(pte: u64) -> bool {
//     pte & 1 << 11 != 0 //在intel手冊中：ept PTE:11 Ignored.不是很懂
// }
// pub fn is_large_pte(pte: u64) -> bool {
//     pte & 1 << 7 != 0 //在intel手冊中：ept PTE:7 Ignored.
// }
// ///Bits 51:12 are from the EPT PDPTE
// pub fn spte_to_pfn(pte: u64) -> u64 {
//     (pte & PT64_BASE_ADDR_MASK) >> PAGE_SHIFT
// }

// #[derive(Default)]
// pub struct TdpIter {
//     inner: TdpIterInner,
// }

// impl TdpIter {
//     pub fn start(
//         &self,
//         root_pt: usize,
//         root_level: u8,
//         min_level: u8,
//         next_last_level_gfn: u64,
//     ) -> Self {
//         let mut inner = self.inner.clone();
//         inner.start(root_pt, root_level, min_level, next_last_level_gfn);
//         TdpIter { inner }
//     }
// }
// ///迭代器将遍历分页结构，直到找到此 GFN 的映射。
// #[derive(Default, Clone)]
// pub struct TdpIterInner {
//     next_last_level_gfn: u64,
//     /// 线程上次让出时的 next_last_level_gfn。
//     /// 仅当 next_last_level_gfn != yielded_gfn 时让出，有助于确保前进。
//     pub yielded_gfn: u64,

//     ///指向遍历到当前 SPTE 的页表的指针
//     pt_path: [u64; PT64_ROOT_MAX_LEVEL],

//     ///指向当前 SPTE 的指针  是hva吗？
//     sptep: PhysAddr,

//     /// 当前 SPTE 映射的最低 GFN  hpa>>shift?
//     pub gfn: u64,

//     ///给迭代器的根页级别
//     pub root_level: u8,

//     ///迭代器应遍历到的最低级别
//     pub min_level: u8,

//     ///迭代器在分页结构中的当前级别
//     pub level: u8,

//     ///sptep 处值的快照
//     pub old_spte: u64,

//     ///迭代器是否具有有效状态。如果迭代器走出分页结构的末端，则为 false。
//     ///
//     pub valid: bool,
// }
// impl TdpIterInner {
//     ///初始化ept iter
//     #[inline(never)]
//     pub fn start(
//         &mut self,
//         root_pt: usize,
//         root_level: u8,
//         min_level: u8,
//         next_last_level_gfn: u64,
//     ) {
//         // if root_pt.role.level() == 0 || root_pt.role.level() > PT64_ROOT_MAX_LEVEL as u32  {
//         //     self.valid = false;
//         //     return;
//         // }

//         if root_level < 1 || root_level > PT64_ROOT_MAX_LEVEL as u8 {
//             self.valid = false;
//             return;
//         }
//         self.next_last_level_gfn = next_last_level_gfn;
//         self.root_level = root_level as u8;
//         self.min_level = min_level as u8;
//         self.pt_path[(self.root_level - 1) as usize] = root_pt as u64;
//         self.yielded_gfn = self.next_last_level_gfn;
//         self.level = self.root_level;

//         self.gfn = gfn_round_for_level(self.next_last_level_gfn, self.level);
//         self.tdp_iter_refresh_sptep();
//         self.valid = true;
//     }

//     /*
//      * 重新计算当前GFN和level和SPTE指针，并重新读取SPTE。
//      */
//     fn tdp_iter_refresh_sptep(&mut self) {
//         // self.sptep = PhysAddr::new(
//         //     (self.pt_path[self.level as usize - 1]
//         //         + shadow_pt_index(self.gfn << PAGE_SHIFT, self.level)) as usize,
//         // );
//         // self.old_spte = read_sptep(self.sptep);
//     }

//     pub fn _next(&mut self) {
//         if self.try_step_down() {
//             return;
//         }
//         loop {
//             if self.try_step_side() {
//                 return;
//             }
//             if !self.try_step_up() {
//                 break;
//             }
//         }
//         self.valid = false;
//     }
//     ///在分页结构中向目标GFN下降一级。如果迭代器能够下降一级，则返回true，否则返回false。
//     fn try_step_down(&mut self) -> bool {
//         if self.level == self.min_level {
//             return false;
//         }
//         //在下降之前重新读取SPTE，以避免遍历到不再从此条目链接的页表中。
//         self.old_spte = read_sptep(self.sptep);

//         match spte_to_child_pt(self.old_spte, self.level) {
//             Some(child_pt) => {
//                 self.level -= 1;
//                 self.pt_path[self.level as usize - 1] = child_pt.data() as u64;
//                 self.gfn = gfn_round_for_level(self.gfn, self.level);
//                 self.tdp_iter_refresh_sptep();
//                 true
//             }
//             None => false,
//         }
//     }
//     fn try_step_up(&mut self) -> bool {
//         if self.level == self.root_level {
//             return false;
//         }
//         self.level += 1;
//         self.gfn = gfn_round_for_level(self.gfn, self.level);
//         self.tdp_iter_refresh_sptep();
//         true
//     }
//     ///在当前页表的当前级别中，移动到下一个条目。下一个条目可以指向一个page backing guest memory ，
//     ///或者另一个页表，或者它可能是不存在的。如果迭代器能够移动到页表中的下一个条目，则返回true，
//     ///如果迭代器已经在当前页表的末尾，则返回false。
//     fn try_step_side(&mut self) -> bool {
//         //检查迭代器是否已经在当前页表的末尾。
//         if shadow_pt_index(self.gfn << PAGE_SHIFT, self.level) == (PT64_ENT_PER_PAGE - 1) as u64 {
//             return false;
//         }

//         self.gfn += PageLevel::kvm_pages_per_hpage(self.level);
//         self.next_last_level_gfn = self.gfn;
//         self.sptep.add(PTE_LEN); //指向下一个spte，一个spte占64位
//         self.old_spte = read_sptep(self.sptep);
//         true
//     }
// }
// impl Iterator for TdpIter {
//     type Item = TdpIterInner; // 返回 (gfn, spte) 元组

//     fn next(&mut self) -> Option<Self::Item> {
//         let inner = &mut self.inner;
//         if !inner.valid {
//             return None;
//         }
//         inner._next();
//         if inner.valid {
//             Some(inner.clone())
//         } else {
//             None
//         }
//     }
// }
// ///给定一个 SPTE 及其级别，返回一个指针，该指针包含 SPTE 所引用的子页表的hva。
// ///如果没有这样的条目，则返回 null。
// ///
// fn spte_to_child_pt(spte: u64, level: u8) -> Option<VirtAddr> {
//     //没有子页表
//     if !is_shadow_present_pte(spte) || is_last_spte(spte, level) {
//         return None;
//     }
//     Some(VirtAddr::new(virt_2_phys//__va
//         ((spte_to_pfn(spte)<<PAGE_SHIFT) as usize
//     )))
// }
// pub fn read_sptep(sptep: PhysAddr) -> u64 {
//     unsafe { *(sptep.data() as *const u64) }
// }
