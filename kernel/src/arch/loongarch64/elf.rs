use crate::{arch::MMArch, libs::elf::ElfArch, mm::MemoryManagementArch};

#[derive(Debug, Clone, Copy, Hash)]
pub struct LoongArch64ElfArch;

impl ElfArch for LoongArch64ElfArch {
    const ELF_ET_DYN_BASE: usize = MMArch::USER_END_VADDR.data() / 3 * 2;

    const ELF_PAGE_SIZE: usize = MMArch::PAGE_SIZE;
}
