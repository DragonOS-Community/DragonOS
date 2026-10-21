use core::{
    cmp::{max, min},
    fmt::Debug,
    intrinsics::{likely, unlikely},
    ops::Range,
};

use alloc::{sync::Arc, vec::Vec};
use elf::{
    abi::{ET_DYN, ET_EXEC, PT_GNU_PROPERTY, PT_INTERP, PT_LOAD},
    endian::AnyEndian,
    file::FileHeader,
    segment::{ProgramHeader, SegmentTable},
};
use system_error::SystemError;

use crate::{
    arch::{CurrentElfArch, MMArch},
    driver::base::block::SeekFrom,
    filesystem::vfs::{
        fcntl::AtFlags, file::File, open::do_open_execat, write_access::InodeWriteGuard,
    },
    mm::{
        allocator::page_frame::{PageFrameCount, VirtPageFrame},
        syscall::{MapFlags, ProtFlags},
        ucontext::AddressSpace,
        MemoryManagementArch, VirtAddr,
    },
    process::{
        abi::AtType,
        exec::{BinaryLoader, BinaryLoaderResult, ExecError, ExecLoadMode, ExecParam},
        ProcessFlags, ProcessManager,
    },
    syscall::user_access::{clear_user_protected, copy_to_user_protected},
};

// 存放跟架构相关的Elf属性，
pub trait ElfArch: Clone + Copy + Debug {
    const ELF_ET_DYN_BASE: usize;
    const ELF_PAGE_SIZE: usize;
}

#[derive(Debug)]
pub struct ElfLoader;

pub const ELF_LOADER: ElfLoader = ElfLoader::new();

/// Per-exec metadata, prepared before the old image is committed. The table is
/// borrowed on demand so this owner never contains a self-reference.
struct PreparedElf {
    header: FileHeader<AnyEndian>,
    headers: Vec<u8>,
}

impl PreparedElf {
    fn prepare(file: &File, header: FileHeader<AnyEndian>) -> Result<Self, SystemError> {
        let headers = ElfLoader::read_program_headers(file, &header)?;
        Ok(Self { header, headers })
    }

    fn segments(&self) -> SegmentTable<'_, AnyEndian> {
        SegmentTable::new(self.header.endianness, self.header.class, &self.headers)
    }
}

/// The interpreter needs an executable file and metadata, not another process
/// argument/auxv builder. Release write exclusion before the file mount pin.
struct PreparedInterpreter {
    _write_guard: Arc<InodeWriteGuard>,
    file: Arc<File>,
    elf: PreparedElf,
}

impl PreparedInterpreter {
    fn prepare(file: Arc<File>, vm: &Arc<AddressSpace>) -> Result<Self, ExecError> {
        let guard = InodeWriteGuard::deny_write(file.inode()).map_err(ExecError::SystemError)?;
        ExecParam::would_dump_file(&file, vm).map_err(ExecError::SystemError)?;
        let mut bytes = [0; 64];
        if file
            .pread(0, bytes.len(), &mut bytes)
            .map_err(ExecError::SystemError)?
            != bytes.len()
        {
            return Err(ExecError::SystemError(SystemError::EIO));
        }
        let bad = || ExecError::SystemError(SystemError::ELIBBAD);
        let header = ElfLoader::parse_ehdr(&bytes).map_err(|_| bad())?;
        // Interpreter validation is complete before begin_new_exec(), as in
        // Linux load_elf_binary(), rather than in the mapping phase.
        #[cfg(target_arch = "x86_64")]
        let arch_matches = ElfMachine::from(header.e_machine) == ElfMachine::X86_64;
        #[cfg(target_arch = "riscv64")]
        let arch_matches = ElfMachine::from(header.e_machine) == ElfMachine::RiscV;
        #[cfg(target_arch = "loongarch64")]
        let arch_matches = ElfMachine::from(header.e_machine) == ElfMachine::LoongArch;
        if header.class != elf::file::Class::ELF64
            || !arch_matches
            || (header.e_type != ET_EXEC && header.e_type != ET_DYN)
        {
            return Err(bad());
        }
        // Linux load_elf_phdrs folds allocation/read failures into ELIBBAD for
        // an interpreter (and ENOEXEC for the main executable).
        let elf = PreparedElf::prepare(&file, header).map_err(|_| bad())?;
        Ok(Self {
            _write_guard: guard,
            file,
            elf,
        })
    }
}

impl ElfLoader {
    /// 读取文件的缓冲区大小
    pub const FILE_READ_BUF_SIZE: usize = 512 * 1024;

    pub const fn new() -> Self {
        Self
    }

    fn inner_probe_common(
        &self,
        param: &ExecParam,
        ehdr: &FileHeader<AnyEndian>,
    ) -> Result<(), ExecError> {
        // 只支持 64 位的 ELF 文件
        if ehdr.class != elf::file::Class::ELF64 {
            return Err(ExecError::WrongArchitecture);
        }

        // 判断是否以可执行文件的形式加载
        if param.load_mode() == ExecLoadMode::Exec {
            // 检查文件类型是否为可执行文件
            if ElfType::from(ehdr.e_type) != ElfType::Executable
                && ElfType::from(ehdr.e_type) != ElfType::DSO
            {
                return Err(ExecError::NotExecutable);
            }
        } else {
            return Err(ExecError::NotSupported);
        }

        return Ok(());
    }

    #[cfg(target_arch = "x86_64")]
    pub fn probe_x86_64(
        &self,
        param: &ExecParam,
        ehdr: &FileHeader<AnyEndian>,
    ) -> Result<(), ExecError> {
        // 判断架构是否匹配
        if ElfMachine::from(ehdr.e_machine) != ElfMachine::X86_64 {
            return Err(ExecError::WrongArchitecture);
        }
        return self.inner_probe_common(param, ehdr);
    }

    #[cfg(target_arch = "riscv64")]
    pub fn probe_riscv(
        &self,
        param: &ExecParam,
        ehdr: &FileHeader<AnyEndian>,
    ) -> Result<(), ExecError> {
        // 判断架构是否匹配
        if ElfMachine::from(ehdr.e_machine) != ElfMachine::RiscV {
            return Err(ExecError::WrongArchitecture);
        }
        return self.inner_probe_common(param, ehdr);
    }

    #[cfg(target_arch = "loongarch64")]
    pub fn probe_loongarch(
        &self,
        param: &ExecParam,
        ehdr: &FileHeader<AnyEndian>,
    ) -> Result<(), ExecError> {
        // 判断架构是否匹配
        if ElfMachine::from(ehdr.e_machine) != ElfMachine::LoongArch {
            return Err(ExecError::WrongArchitecture);
        }
        return self.inner_probe_common(param, ehdr);
    }

    /// 设置用户堆空间，映射[start, end)区间的虚拟地址，并把brk指针指向end
    ///
    /// ## 参数
    ///
    /// - `user_vm_guard` - 用户虚拟地址空间
    /// - `start` - 本次映射的起始地址
    /// - `end` - 本次映射的结束地址（不包含）
    /// - `prot_flags` - BSS 所属 PT_LOAD 段的权限
    fn set_elf_brk(
        &self,
        mm: &Arc<crate::mm::ucontext::AddressSpace>,
        start: VirtAddr,
        end: VirtAddr,
        prot_flags: ProtFlags,
    ) -> Result<(), ExecError> {
        let start = Self::elf_page_start(start);
        let end = Self::elf_page_align_up(end);
        // debug!("set_elf_brk: start={:?}, end={:?}", start, end);
        if end > start {
            mm.map_anonymous_wait(
                start,
                end - start,
                Self::elf_brk_prot_flags(prot_flags),
                MapFlags::MAP_ANONYMOUS | MapFlags::MAP_FIXED_NOREPLACE,
                false,
                true,
            )
            .map_err(ExecError::SystemError)?;
        }
        let mut user_vm_guard = mm.write();
        user_vm_guard.elf_brk_start = end;
        user_vm_guard.elf_brk = end;
        return Ok(());
    }

    /// 计算addr在ELF PAGE内的偏移
    fn elf_page_offset(addr: VirtAddr) -> usize {
        addr.data() & (CurrentElfArch::ELF_PAGE_SIZE - 1)
    }

    fn elf_page_start(addr: VirtAddr) -> VirtAddr {
        VirtAddr::new(addr.data() & (!(CurrentElfArch::ELF_PAGE_SIZE - 1)))
    }

    fn elf_page_align_up(addr: VirtAddr) -> VirtAddr {
        VirtAddr::new(
            (addr.data() + CurrentElfArch::ELF_PAGE_SIZE - 1)
                & (!(CurrentElfArch::ELF_PAGE_SIZE - 1)),
        )
    }

    /// 生成 ELF brk/BSS 匿名映射权限。
    ///
    /// Linux `vm_brk_flags()` 只从 PT_LOAD 权限中额外透传 exec 位；
    /// 基础 data VMA 权限来自 `VM_DATA_DEFAULT_FLAGS`，这里显式表达为可读写。
    fn elf_brk_prot_flags(load_prot_flags: ProtFlags) -> ProtFlags {
        let mut prot = ProtFlags::PROT_READ | ProtFlags::PROT_WRITE;
        if load_prot_flags.contains(ProtFlags::PROT_EXEC) {
            prot |= ProtFlags::PROT_EXEC;
        }
        prot
    }

    /// 根据ELF的p_flags生成对应的ProtFlags
    fn make_prot(p_flags: u32, _has_interpreter: bool, _is_interpreter: bool) -> ProtFlags {
        let mut prot = ProtFlags::empty();
        if p_flags & elf::abi::PF_R != 0 {
            prot |= ProtFlags::PROT_READ;
        }
        if p_flags & elf::abi::PF_W != 0 {
            prot |= ProtFlags::PROT_WRITE;
        }
        if p_flags & elf::abi::PF_X != 0 {
            prot |= ProtFlags::PROT_EXEC;
        }

        // todo: 增加与架构相关的处理
        // ref:  https://code.dragonos.org.cn/xref/linux-5.19.10/fs/binfmt_elf.c?r=&mo=22652&fi=824#572

        return prot;
    }

    /// Map a PT_LOAD through the normal file-mmap lifecycle. The caller must
    /// not hold the address-space lock: mmap hooks, population and BSS writes
    /// can fault or acquire that lock themselves.
    fn load_elf_segment(
        file: &Arc<File>,
        vm: &Arc<AddressSpace>,
        phent: &ProgramHeader,
        addr_to_map: VirtAddr,
        prot: ProtFlags,
        map_flags: MapFlags,
        total_size: usize,
    ) -> Result<VirtAddr, SystemError> {
        let page_offset = Self::elf_page_offset(VirtAddr::new(phent.p_vaddr as usize));
        let addr_to_map = Self::elf_page_start(addr_to_map);
        if phent.p_filesz > phent.p_memsz
            || phent.p_vaddr.checked_add(phent.p_memsz).is_none()
            || phent.p_vaddr.saturating_add(phent.p_memsz) > MMArch::USER_END_VADDR.data() as u64
        {
            return Err(SystemError::ENOEXEC);
        }
        let align_len = |len: usize| {
            len.checked_add(CurrentElfArch::ELF_PAGE_SIZE - 1)
                .map(|len| len & !(CurrentElfArch::ELF_PAGE_SIZE - 1))
                .ok_or(SystemError::ENOEXEC)
        };
        // A pure BSS segment has no file mapping, including its partial first
        // page. The existing BSS path maps the subsequent whole pages.
        let map_size = if phent.p_filesz == 0 {
            if phent.p_memsz == 0 {
                0
            } else {
                align_len(page_offset)?
            }
        } else {
            align_len(
                (phent.p_filesz as usize)
                    .checked_add(page_offset)
                    .ok_or(SystemError::ENOEXEC)?,
            )?
        };
        let map_len = if total_size != 0 {
            align_len(total_size)?
        } else {
            map_size
        };
        if map_len < map_size {
            return Err(SystemError::ENOEXEC);
        }
        if map_len == 0 {
            return Ok(addr_to_map);
        }
        let mapped = if phent.p_filesz == 0 {
            vm.map_anonymous_wait(
                addr_to_map,
                map_len,
                Self::elf_brk_prot_flags(prot),
                map_flags | MapFlags::MAP_ANONYMOUS,
                false,
                false,
            )?
            .virt_address()
        } else {
            let file_offset = (phent.p_offset as usize)
                .checked_sub(page_offset)
                .ok_or(SystemError::ENOEXEC)?;
            if file_offset & (CurrentElfArch::ELF_PAGE_SIZE - 1) != 0
                || file_offset
                    .checked_add(map_len)
                    .is_none_or(|end| end > isize::MAX as usize)
            {
                return Err(SystemError::ENOEXEC);
            }
            let load_prot = if MMArch::PAGE_FAULT_ENABLED {
                prot
            } else {
                prot | ProtFlags::PROT_WRITE
            };
            let mapped = vm
                .file_mapping_with_file(
                    file.clone(),
                    addr_to_map,
                    map_len,
                    load_prot,
                    map_flags,
                    file_offset,
                    false,
                    false,
                )?
                .virt_address();
            // Architectures without user page faults still need explicit
            // population. Copy the file-mapped prefix/tail too, not just the
            // segment bytes; zeroed allocation supplies bytes past EOF.
            if !MMArch::PAGE_FAULT_ENABLED {
                let file_size = file.metadata()?.size.max(0) as usize;
                let read_len = min(map_size, file_size.saturating_sub(file_offset));
                Self::do_load_file(mapped, read_len, file_offset, file)?;
                if load_prot != prot {
                    vm.mprotect_wait(
                        VirtPageFrame::new(mapped),
                        PageFrameCount::from_bytes(map_size).unwrap(),
                        prot,
                    )?;
                }
            }
            mapped
        };
        // Only the first PT_LOAD reserves the whole image for address choice.
        // For an aligned pure-BSS first segment, retain no page here; the BSS
        // path below will establish its anonymous range at the chosen address.
        if map_len > map_size {
            vm.munmap_wait(
                VirtPageFrame::new(mapped + map_size),
                PageFrameCount::from_bytes(map_len - map_size).unwrap(),
            )?;
        }
        Ok(mapped)
    }

    /// 加载elf动态链接器
    ///
    /// ## 参数
    ///
    /// - `interpreter`:已准备的动态链接器文件及元数据
    /// - `load_bias`偏移量
    ///
    /// ## TODO
    ///
    /// 添加一个Arch state抽象，描述架构相关的elf state(参考 https://code.dragonos.org.cn/xref/linux-6.1.9/fs/binfmt_elf.c#592)
    fn load_elf_interp(
        interpreter: &PreparedInterpreter,
        vm: &Arc<AddressSpace>,
        load_bias: usize,
    ) -> Result<(BinaryLoaderResult, VirtAddr), ExecError> {
        let interp_hdr = &interpreter.elf.header;
        let phdr_table = interpreter.elf.segments();
        //TODO 架构相关检查 https://code.dragonos.org.cn/xref/linux-6.1.9/fs/binfmt_elf.c#610
        let mut total_size = Self::total_mapping_size(&phdr_table);

        if total_size == 0 {
            return Err(ExecError::InvalidParemeter);
        }

        let mut load_addr_set = false;
        let mut load_addr: VirtAddr = VirtAddr::new(0);
        let mut elf_bss: VirtAddr = VirtAddr::new(0);
        let mut last_bss: VirtAddr = VirtAddr::new(0);
        let mut bss_prot: Option<ProtFlags> = None;
        for section in phdr_table {
            if section.p_type == PT_LOAD {
                // log::debug!("loading {:?}", section);
                let mut elf_type = MapFlags::MAP_PRIVATE;
                let elf_prot = Self::make_prot(section.p_flags, true, true);
                let vaddr = TryInto::<usize>::try_into(section.p_vaddr).unwrap();
                let mut addr_to_map = load_addr + vaddr;
                if interp_hdr.e_type == ET_EXEC || load_addr_set {
                    elf_type.insert(MapFlags::MAP_FIXED)
                } else if load_bias != 0 && interp_hdr.e_type == ET_DYN {
                    addr_to_map = VirtAddr::new(0);
                }
                let map_addr = Self::load_elf_segment(
                    &interpreter.file,
                    vm,
                    &section,
                    addr_to_map,
                    elf_prot,
                    elf_type,
                    total_size,
                )
                .map_err(ExecError::SystemError)?;
                total_size = 0;
                if !load_addr_set && interp_hdr.e_type == ET_DYN {
                    load_addr =
                        VirtAddr::new(map_addr - Self::elf_page_start(VirtAddr::new(vaddr)));
                    load_addr_set = true;
                }
                let addr = load_addr + TryInto::<usize>::try_into(section.p_vaddr).unwrap();
                // Validate segment parameters (aligned with main load() function)
                let seg_end = section.p_vaddr.checked_add(section.p_memsz);
                if seg_end.is_none()
                    || addr >= MMArch::USER_END_VADDR
                    || section.p_filesz > section.p_memsz
                    || (section.p_memsz as usize) > MMArch::USER_END_VADDR.data()
                    || MMArch::USER_END_VADDR.data() - (section.p_memsz as usize) < addr.data()
                {
                    return Err(ExecError::OutOfMemory);
                }

                // Safety: validation above ensures p_vaddr + p_filesz <= p_vaddr + p_memsz does not overflow
                let addr = load_addr
                    + TryInto::<usize>::try_into(section.p_vaddr + section.p_filesz).unwrap();
                if addr > elf_bss {
                    elf_bss = addr;
                }

                // Safety: validation above ensures p_vaddr + p_memsz does not overflow u64
                let addr = load_addr
                    + TryInto::<usize>::try_into(section.p_vaddr + section.p_memsz).unwrap();
                if addr > last_bss {
                    last_bss = addr;
                    bss_prot = Some(elf_prot);
                }
            }
        }
        Self::pad_zero(elf_bss).map_err(|_| return ExecError::BadAddress(Some(elf_bss)))?;
        elf_bss = Self::elf_page_align_up(elf_bss);
        last_bss = Self::elf_page_align_up(last_bss);
        if last_bss > elf_bss {
            if bss_prot.is_none() {
                return Err(ExecError::InvalidParemeter);
            }
            let bss_prot = Self::elf_brk_prot_flags(bss_prot.unwrap());
            vm.map_anonymous_wait(
                elf_bss,
                last_bss - elf_bss,
                bss_prot,
                MapFlags::MAP_ANONYMOUS | MapFlags::MAP_FIXED_NOREPLACE,
                false,
                true,
            )
            .map_err(ExecError::SystemError)?;
        }
        let interp_base = load_addr;
        let entry = load_addr + TryInto::<usize>::try_into(interp_hdr.e_entry).unwrap();
        if entry > MMArch::USER_END_VADDR {
            return Err(ExecError::BadAddress(Some(entry)));
        }
        // log::debug!("sucessfully load elf interp");
        return Ok((BinaryLoaderResult::new(entry), interp_base));
    }

    /// 加载ELF文件到用户空间
    ///
    /// ## 参数
    ///
    /// - `vaddr`：要加载到的虚拟地址
    /// - `size`：要加载的大小
    /// - `offset_in_file`：在文件内的偏移量
    /// - `file`：待填充映射对应的文件
    fn do_load_file(
        vaddr: VirtAddr,
        size: usize,
        offset_in_file: usize,
        file: &File,
    ) -> Result<(), SystemError> {
        Self::do_load_file_with(vaddr, size, offset_in_file, file, |buf_size| {
            #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
            {
                crate::mm::kernel_buffer::KernelBuffer::new_zeroed_recovering(buf_size)
            }
            #[cfg(not(any(target_arch = "x86_64", target_arch = "riscv64")))]
            {
                let mut buffer = Vec::new();
                buffer
                    .try_reserve_exact(buf_size)
                    .map_err(|_| SystemError::ENOMEM)?;
                buffer.resize(buf_size, 0);
                Ok(buffer)
            }
        })
    }

    fn do_load_file_with<B: core::ops::DerefMut<Target = [u8]>>(
        mut vaddr: VirtAddr,
        size: usize,
        offset_in_file: usize,
        file: &crate::filesystem::vfs::file::File,
        allocate: impl FnOnce(usize) -> Result<B, SystemError>,
    ) -> Result<(), SystemError> {
        if (file.metadata()?.size as usize) < offset_in_file + size {
            return Err(SystemError::ENOEXEC);
        }
        let buf_size = min(size, Self::FILE_READ_BUF_SIZE);
        let mut buf = allocate(buf_size)?;

        let mut remain = size;

        file.lseek(SeekFrom::SeekSet(offset_in_file as i64))?;

        while remain > 0 {
            let read_size = min(remain, buf_size);
            let read_size = file.read(read_size, &mut buf[..read_size])?;
            if read_size == 0 {
                return Err(SystemError::ENOEXEC);
            }
            unsafe {
                copy_to_user_protected(vaddr, &buf[..read_size])
                    .map_err(|_| SystemError::EFAULT)?;
            }

            vaddr += read_size;
            remain -= read_size;
        }
        return Ok(());
    }

    /// 我们需要显式的把数据段之后剩余的内存页都清零。
    fn pad_zero(elf_bss: VirtAddr) -> Result<(), SystemError> {
        let nbyte = Self::elf_page_offset(elf_bss);
        if nbyte > 0 {
            let nbyte = CurrentElfArch::ELF_PAGE_SIZE - nbyte;
            unsafe { clear_user_protected(elf_bss, nbyte).map_err(|_| SystemError::EFAULT) }?;
        }
        return Ok(());
    }

    /// Compute total mapping size for all PT_LOAD segments.
    /// Ref: https://code.dragonos.org.cn/xref/linux-6.1.9/fs/binfmt_elf.c#1158
    ///
    /// Returns 0 if any PT_LOAD segment has `p_vaddr + p_memsz` overflow (malformed ELF).
    fn total_mapping_size(ehdr_table: &SegmentTable<'_, AnyEndian>) -> usize {
        let mut has_load = false;
        let mut min_address = VirtAddr::new(usize::MAX);
        let mut max_address = VirtAddr::new(0usize);
        let loadable_sections = ehdr_table
            .into_iter()
            .filter(|seg| seg.p_type == elf::abi::PT_LOAD);
        for seg_to_load in loadable_sections {
            // On 64-bit platforms, u64 -> usize never overflows
            let vaddr: usize = seg_to_load.p_vaddr.try_into().unwrap();
            // Detect u64 overflow in p_vaddr + p_memsz (malformed ELF)
            let end = match seg_to_load.p_vaddr.checked_add(seg_to_load.p_memsz) {
                Some(v) => v,
                None => return 0, // overflow -> invalid ELF, return 0 so loader rejects it
            };
            min_address = min(min_address, Self::elf_page_start(VirtAddr::new(vaddr)));
            max_address = max(max_address, VirtAddr::new(end.try_into().unwrap()));
            has_load = true;
        }
        let total_size = if has_load {
            max_address - min_address
        } else {
            0
        };
        return total_size;
    }

    /// 创建auxv
    ///
    /// ## 参数
    ///
    /// - `param`：执行参数
    /// - `entrypoint_vaddr`：程序入口地址
    /// - `phdr_vaddr`：程序头表地址
    /// - `elf_header`：ELF文件头
    fn create_auxv(
        &self,
        param: &mut ExecParam,
        entrypoint_vaddr: VirtAddr,
        phdr_vaddr: Option<VirtAddr>,
        interpreter_base: Option<VirtAddr>,
        ehdr: &elf::file::FileHeader<AnyEndian>,
    ) -> Result<(), ExecError> {
        use crate::process::rseq::{ORIG_RSEQ_SIZE, RSEQ_ALIGN};

        let phdr_vaddr = phdr_vaddr.unwrap_or(VirtAddr::new(0));

        let init_info = param.init_info_mut();
        init_info
            .auxv
            .insert(AtType::PhEnt as u8, ehdr.e_phentsize as usize);
        init_info
            .auxv
            .insert(AtType::PageSize as u8, MMArch::PAGE_SIZE);
        init_info.auxv.insert(AtType::Flags as u8, 0);
        init_info.auxv.insert(AtType::Phdr as u8, phdr_vaddr.data());
        init_info
            .auxv
            .insert(AtType::PhNum as u8, ehdr.e_phnum as usize);
        init_info
            .auxv
            .insert(AtType::Entry as u8, entrypoint_vaddr.data());
        init_info.auxv.insert(
            AtType::Base as u8,
            interpreter_base.unwrap_or(VirtAddr::new(0)).data(),
        );
        let cred = crate::process::ProcessManager::current_pcb().cred();
        init_info.auxv.insert(AtType::Uid as u8, cred.uid.data());
        init_info.auxv.insert(AtType::EUid as u8, cred.euid.data());
        init_info.auxv.insert(AtType::Gid as u8, cred.gid.data());
        init_info.auxv.insert(AtType::EGid as u8, cred.egid.data());
        init_info.auxv.insert(AtType::HwCap as u8, 0);
        init_info.auxv.insert(AtType::ClkTck as u8, 100);
        init_info.auxv.insert(AtType::Secure as u8, 0);
        init_info.auxv.insert(AtType::HwCap2 as u8, 0);
        init_info.auxv.insert(AtType::MinSigStackSize as u8, 2048);

        // 添加 rseq 相关的 auxv
        init_info
            .auxv
            .insert(AtType::RseqFeatureSize as u8, ORIG_RSEQ_SIZE as usize);
        init_info
            .auxv
            .insert(AtType::RseqAlign as u8, RSEQ_ALIGN as usize);

        return Ok(());
    }

    /// 解析文件的ehdr
    fn parse_ehdr(data: &[u8]) -> Result<FileHeader<AnyEndian>, elf::ParseError> {
        let ident_buf = data.get_bytes(0..elf::abi::EI_NIDENT)?;
        let ident = elf::file::parse_ident::<AnyEndian>(ident_buf)?;

        let tail_start = elf::abi::EI_NIDENT;
        let tail_end = match ident.1 {
            elf::file::Class::ELF32 => tail_start + elf::file::ELF32_EHDR_TAILSIZE,
            elf::file::Class::ELF64 => tail_start + elf::file::ELF64_EHDR_TAILSIZE,
        };
        let tail_buf = data.get_bytes(tail_start..tail_end)?;

        let ehdr: FileHeader<_> = FileHeader::parse_tail(ident, tail_buf)?;
        return Ok(ehdr);
    }

    /// Linux executable program-header tables fit in one ELF alignment unit
    /// and at most 64 KiB. PN_XNUM is a core-file convention, not exec input.
    fn program_headers_size(ehdr: &FileHeader<AnyEndian>) -> Result<usize, SystemError> {
        let entry = <ProgramHeader as elf::parse::ParseAt>::validate_entsize(
            ehdr.class,
            ehdr.e_phentsize as usize,
        )
        .map_err(|_| SystemError::ENOEXEC)?;
        let size = entry
            .checked_mul(ehdr.e_phnum as usize)
            .ok_or(SystemError::ENOEXEC)?;
        if size == 0 || size > min(65536, CurrentElfArch::ELF_PAGE_SIZE) {
            return Err(SystemError::ENOEXEC);
        }
        let offset = usize::try_from(ehdr.e_phoff).map_err(|_| SystemError::ENOEXEC)?;
        if offset
            .checked_add(size)
            .is_none_or(|end| end > isize::MAX as usize)
        {
            return Err(SystemError::ENOEXEC);
        }
        Ok(size)
    }

    fn zeroed_metadata(size: usize) -> Result<Vec<u8>, SystemError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .map_err(|_| SystemError::ENOMEM)?;
        bytes.resize(size, 0);
        Ok(bytes)
    }

    fn read_program_headers(
        file: &File,
        ehdr: &FileHeader<AnyEndian>,
    ) -> Result<Vec<u8>, SystemError> {
        let size = Self::program_headers_size(ehdr)?;
        let mut bytes = Self::zeroed_metadata(size)?;
        if file.pread(ehdr.e_phoff as usize, size, &mut bytes)? != size {
            return Err(SystemError::ENOEXEC);
        }
        Ok(bytes)
    }

    // 解析 PT_GNU_PROPERTY 类型的段
    // 参照 https://code.dragonos.org.cn/xref/linux-6.1.9/fs/binfmt_elf.c#767
    fn parse_gnu_property() -> Result<(), ExecError> {
        return Ok(());
    }
}

impl BinaryLoader for ElfLoader {
    fn probe(&'static self, param: &ExecParam, buf: &[u8]) -> Result<(), ExecError> {
        // let elf_bytes =
        //     ElfBytes::<AnyEndian>::minimal_parse(buf).map_err(|_| ExecError::NotExecutable)?;

        let ehdr = Self::parse_ehdr(buf).map_err(|_| ExecError::NotExecutable)?;

        #[cfg(target_arch = "x86_64")]
        return self.probe_x86_64(param, &ehdr);

        #[cfg(target_arch = "riscv64")]
        return self.probe_riscv(param, &ehdr);

        #[cfg(target_arch = "loongarch64")]
        return self.probe_loongarch(param, &ehdr);

        #[cfg(not(any(
            target_arch = "x86_64",
            target_arch = "riscv64",
            target_arch = "loongarch64"
        )))]
        compile_error!("BinaryLoader: Unsupported architecture");
    }

    fn load(
        &'static self,
        param: &mut ExecParam,
        head_buf: &[u8],
    ) -> Result<BinaryLoaderResult, ExecError> {
        // 解析elf文件头
        let ehdr = Self::parse_ehdr(head_buf).map_err(|_| ExecError::NotExecutable)?;

        // 参考linux-5.19的load_elf_binary函数
        // https://code.dragonos.org.cn/xref/linux-5.19.10/fs/binfmt_elf.c?r=&mo=22652&fi=824#1034

        let elf_type = ElfType::from(ehdr.e_type);
        // debug!("ehdr = {:?}", ehdr);

        let binding = param.vm().clone();

        // todo: 增加对user stack上的内存是否具有可执行权限的处理（方法：寻找phdr里面的PT_GNU_STACK段）

        // debug!("to parse segments");
        // 加载ELF文件并映射到用户空间
        let prepared =
            PreparedElf::prepare(param.file_ref(), ehdr).map_err(|_| ExecError::NotExecutable)?;
        let ehdr = &prepared.header;
        let phdr_table = prepared.segments();
        let mut _gnu_property_data: Option<ProgramHeader> = None;
        let mut interpreter = None;
        for seg in phdr_table {
            if seg.p_type == PT_GNU_PROPERTY {
                _gnu_property_data = Some(seg);
                continue;
            }
            if seg.p_type != PT_INTERP {
                continue;
            }
            if seg.p_filesz > 4096 || seg.p_filesz < 2 {
                return Err(ExecError::NotExecutable);
            }
            let size = seg.p_filesz as usize;
            let offset = usize::try_from(seg.p_offset).map_err(|_| ExecError::NotExecutable)?;
            if offset
                .checked_add(size)
                .is_none_or(|end| end > isize::MAX as usize)
            {
                return Err(ExecError::NotExecutable);
            }
            let mut buffer = Self::zeroed_metadata(size).map_err(ExecError::SystemError)?;
            if param
                .file_ref()
                .pread(offset, size, &mut buffer)
                .map_err(ExecError::SystemError)?
                != size
            {
                return Err(ExecError::SystemError(SystemError::EIO));
            }
            if buffer[size - 1] != 0 {
                return Err(ExecError::NotExecutable);
            }
            let end = buffer.iter().position(|byte| *byte == 0).unwrap();
            let path =
                core::str::from_utf8(&buffer[..end]).map_err(|_| ExecError::NotExecutable)?;
            let file =
                do_open_execat(AtFlags::AT_FDCWD.bits(), path).map_err(ExecError::SystemError)?;
            interpreter = Some(PreparedInterpreter::prepare(file, param.vm())?);
            // Linux uses the first PT_INTERP; later entries are not reopened.
            break;
        }
        Self::parse_gnu_property()?;

        param.begin_new_exec()?;
        // begin_new_exec() may de-thread, clone/install the files table and
        // drop old table references. None of those operations may run while an
        // address-space write guard is held.
        let mut user_vm = binding.write();

        // todo: 补充逻辑：https://code.dragonos.org.cn/xref/linux-6.6.21/fs/binfmt_elf.c#1007
        param.setup_new_exec();

        // Linux 语义：elf_bss/elf_brk 以“字节端点”记录：
        // - elf_bss: max(p_vaddr + p_filesz)
        // - elf_brk: max(p_vaddr + p_memsz)
        // 之后再做：
        // 1) pad_zero(elf_bss) 清零最后一个文件页的尾部
        // 2) page-align 后，为 [align_up(elf_bss), align_up(elf_brk)) 建立匿名 BSS 映射
        let mut elf_brk = VirtAddr::new(0);
        let mut elf_bss = VirtAddr::new(0);
        let mut start_code: Option<VirtAddr> = None;
        let mut end_code: Option<VirtAddr> = None;
        let mut start_data: Option<VirtAddr> = None;
        let mut end_data: Option<VirtAddr> = None;

        // 加载的时候的偏移量（这个偏移量在加载动态链接段的时候产生）
        let mut load_bias = 0usize;
        let mut bss_prot_flags = ProtFlags::empty();
        // 是否是第一个加载的段
        let mut first_pt_load = true;
        // program header的虚拟地址
        let mut phdr_vaddr: Option<VirtAddr> = None;
        let mut _reloc_func_desc = 0usize;

        let total_size = Self::total_mapping_size(&phdr_table);
        let loadable_sections = phdr_table
            .into_iter()
            .filter(|seg| seg.p_type == elf::abi::PT_LOAD);

        for seg_to_load in loadable_sections {
            // log::debug!("seg_to_load = {:?}", seg_to_load);
            if unlikely(elf_brk > elf_bss) {
                // debug!(
                //     "to set brk, elf_brk = {:?}, elf_bss = {:?}",
                //     elf_brk,
                //     elf_bss
                // );
                // 仅在页粒度上需要扩展 BSS 时才去映射，避免覆盖段的最后一页
                let bss_start_page = Self::elf_page_align_up(elf_bss);
                let brk_end_page = Self::elf_page_align_up(elf_brk);
                if brk_end_page > bss_start_page {
                    drop(user_vm);
                    self.set_elf_brk(
                        &binding,
                        bss_start_page + load_bias,
                        brk_end_page + load_bias,
                        bss_prot_flags,
                    )?;
                    user_vm = binding.write();
                }

                // 处理“BSS 落在最后一个文件页内”的尾部清零（Linux: padzero）
                let nbyte = Self::elf_page_offset(elf_bss);
                if nbyte > 0 {
                    let nbyte = min(CurrentElfArch::ELF_PAGE_SIZE - nbyte, elf_brk - elf_bss);
                    // 重要：clear_user 会触发用户态缺页，缺页路径会获取 AddressSpace 写锁。
                    // 因此必须在释放 user_vm 写锁后再清零，避免死锁。
                    drop(user_vm);
                    unsafe {
                        // 这里清零理论上不应失败：BSS 所属段应为可写；失败时忽略以兼容 Linux 行为
                        clear_user_protected(elf_bss + load_bias, nbyte).ok();
                    }
                    user_vm = binding.write();
                }
            }

            // 生成ProtFlags.
            let elf_prot_flags = Self::make_prot(seg_to_load.p_flags, interpreter.is_some(), false);

            let mut elf_map_flags = MapFlags::MAP_PRIVATE;

            let vaddr = VirtAddr::new(seg_to_load.p_vaddr.try_into().unwrap());

            // Validate segment parameters before loading to prevent malformed ELF
            // from triggering kernel panic. Ref: Linux 6.6.139 fs/binfmt_elf.c:1243-1249
            //
            // Checks:
            // 1. p_vaddr + p_memsz does not overflow u64 (checked_add)
            // 2. p_vaddr is within user address space
            // 3. p_filesz <= p_memsz
            // 4. p_memsz <= USER_END_VADDR (TASK_SIZE)
            // 5. p_vaddr + p_memsz <= USER_END_VADDR (range check without overflow)
            let seg_end = seg_to_load.p_vaddr.checked_add(seg_to_load.p_memsz);
            if seg_end.is_none()
                || !vaddr.check_user()
                || seg_to_load.p_filesz > seg_to_load.p_memsz
                || (seg_to_load.p_memsz as usize) > MMArch::USER_END_VADDR.data()
                || MMArch::USER_END_VADDR.data() - (seg_to_load.p_memsz as usize) < vaddr.data()
            {
                return Err(ExecError::InvalidParemeter);
            }

            #[allow(clippy::if_same_then_else)]
            if !first_pt_load {
                elf_map_flags.insert(MapFlags::MAP_FIXED_NOREPLACE);
            } else if elf_type == ElfType::Executable {
                /*
                 * This logic is run once for the first LOAD Program
                 * Header for ET_EXEC binaries. No special handling
                 * is needed.
                 */
                elf_map_flags.insert(MapFlags::MAP_FIXED_NOREPLACE);
            } else if elf_type == ElfType::DSO {
                // TODO: 支持动态链接
                if interpreter.is_some() {
                    load_bias = CurrentElfArch::ELF_ET_DYN_BASE;
                    if ProcessManager::current_pcb()
                        .flags()
                        .contains(ProcessFlags::RANDOMIZE)
                    {
                        //这里x86下需要一个随机加载的方法，但是很多架构，比如Risc-V都是0，就暂时不写了
                    } else {
                        load_bias = 0;
                    }
                }
                load_bias = Self::elf_page_start(VirtAddr::new(
                    load_bias - TryInto::<usize>::try_into(seg_to_load.p_vaddr).unwrap(),
                ))
                .data();
                if total_size == 0 {
                    return Err(ExecError::InvalidParemeter);
                }
            }

            // 加载这个段到用户空间

            // log::debug!("bias: {load_bias}");
            drop(user_vm);
            let mapped = Self::load_elf_segment(
                &param.file(),
                param.vm(),
                &seg_to_load,
                vaddr + load_bias,
                elf_prot_flags,
                elf_map_flags,
                if first_pt_load { total_size } else { 0 },
            )
            .map_err(|e| match e {
                SystemError::EFAULT => ExecError::BadAddress(None),
                SystemError::ENOMEM => ExecError::OutOfMemory,
                _ => ExecError::Other(format!("load_elf_segment failed: {:?}", e)),
            })?;
            user_vm = binding.write();

            if first_pt_load {
                first_pt_load = false;
                if elf_type == ElfType::DSO {
                    // todo: 在这里增加对load_bias和reloc_func_desc的更新代码
                    load_bias += mapped.data()
                        - Self::elf_page_start(VirtAddr::new(
                            load_bias + TryInto::<usize>::try_into(seg_to_load.p_vaddr).unwrap(),
                        ))
                        .data();
                    _reloc_func_desc = load_bias;
                }
            }

            // debug!("seg_to_load.p_offset={}", seg_to_load.p_offset);
            // debug!("e_phoff={}", ehdr.e_phoff);
            // debug!("seg_to_load.p_filesz={}", seg_to_load.p_filesz);
            // Figure out which segment in the file contains the Program Header Table,
            // and map to the associated virtual address.
            if (seg_to_load.p_offset <= ehdr.e_phoff)
                && (ehdr.e_phoff < (seg_to_load.p_offset + seg_to_load.p_filesz))
            {
                phdr_vaddr = Some(VirtAddr::new(
                    (ehdr.e_phoff - seg_to_load.p_offset + seg_to_load.p_vaddr) as usize,
                ));
            }

            let p_vaddr = VirtAddr::new(seg_to_load.p_vaddr as usize);
            if (seg_to_load.p_flags & elf::abi::PF_X) != 0
                && (start_code.is_none() || start_code.as_ref().unwrap() > &p_vaddr)
            {
                start_code = Some(p_vaddr);
            }

            if start_data.is_none()
                || (start_data.is_some() && start_data.as_ref().unwrap() > &p_vaddr)
            {
                start_data = Some(p_vaddr);
            }

            // Byte endpoint of the file portion (for BSS/padzero semantics)
            // Safety: validation above ensures p_vaddr + p_filesz <= p_vaddr + p_memsz <= USER_END_VADDR,
            // so this addition cannot overflow.
            let seg_file_end = VirtAddr::new((seg_to_load.p_vaddr + seg_to_load.p_filesz) as usize);
            if seg_file_end > elf_bss {
                elf_bss = seg_file_end;
            }

            // 统计 end_code/end_data 使用页对齐后的文件端点（更贴近 /proc/maps 的 VMA 粒度）
            let seg_end_vaddr_f = Self::elf_page_align_up(seg_file_end);

            if ((seg_to_load.p_flags & elf::abi::PF_X) != 0)
                && (end_code.is_none()
                    || (end_code.is_some() && end_code.as_ref().unwrap() < &seg_end_vaddr_f))
            {
                end_code = Some(seg_end_vaddr_f);
            }

            if end_data.is_none()
                || (end_data.is_some() && end_data.as_ref().unwrap() < &seg_end_vaddr_f)
            {
                end_data = Some(seg_end_vaddr_f);
            }

            // Safety: validation above ensures p_vaddr + p_memsz does not overflow u64
            let seg_end_vaddr = VirtAddr::new((seg_to_load.p_vaddr + seg_to_load.p_memsz) as usize);

            if seg_end_vaddr > elf_brk {
                bss_prot_flags = elf_prot_flags;
                elf_brk = seg_end_vaddr;
            }
        }
        // debug!("elf load: phdr_vaddr={phdr_vaddr:?}");
        let program_entrypoint = VirtAddr::new(ehdr.e_entry as usize + load_bias);
        let phdr_vaddr = phdr_vaddr.map(|phdr_vaddr| phdr_vaddr + load_bias);

        elf_bss += load_bias;
        elf_brk += load_bias;
        start_code = start_code.map(|v| v + load_bias);
        end_code = end_code.map(|v| v + load_bias);
        start_data = start_data.map(|v| v + load_bias);
        end_data = end_data.map(|v| v + load_bias);
        let mut interp_load_addr: Option<VirtAddr> = None;
        let mut interpreter_base: Option<VirtAddr> = None;
        // debug!(
        //     "to set brk: elf_bss: {:?}, elf_brk: {:?}, bss_prot_flags: {:?}",
        //     elf_bss,
        //     elf_brk,
        //     bss_prot_flags
        // );
        // 先释放 AddressSpace 写锁，再对最后一个文件页做 padzero（padzero 会写用户地址，可能触发缺页）
        drop(user_vm);
        if likely(elf_bss != elf_brk) && unlikely(Self::pad_zero(elf_bss).is_err()) {
            return Err(ExecError::BadAddress(Some(elf_bss)));
        }

        // 再重新获取写锁，对齐后映射剩余的 BSS 页（避免覆盖最后一页）
        let mut user_vm = binding.write();
        let bss_start_page = Self::elf_page_align_up(elf_bss);
        let brk_end_page = Self::elf_page_align_up(elf_brk);
        if brk_end_page > bss_start_page {
            drop(user_vm);
            self.set_elf_brk(&binding, bss_start_page, brk_end_page, bss_prot_flags)?;
            user_vm = binding.write();
        }
        drop(user_vm);
        if let Some(interpreter) = interpreter {
            // 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/fs/binfmt_elf.c#1249
            let interp = Self::load_elf_interp(&interpreter, param.vm(), load_bias)?;
            let elf_entry = interp.0.entry_point();
            interp_load_addr = Some(elf_entry);
            interpreter_base = Some(interp.1);
            _reloc_func_desc = elf_entry.data();
            //参考 https://code.dragonos.org.cn/xref/linux-6.1.9/fs/binfmt_elf.c#1269
            //TODO allow_write_access(interpreter);
        }
        // debug!("to create auxv");
        let mut user_vm = binding.write();
        self.create_auxv(
            param,
            program_entrypoint,
            phdr_vaddr,
            interpreter_base,
            ehdr,
        )?;

        // debug!("auxv create ok");
        user_vm.start_code = start_code.unwrap_or(VirtAddr::new(0));
        user_vm.end_code = end_code.unwrap_or(VirtAddr::new(0));
        user_vm.start_data = start_data.unwrap_or(VirtAddr::new(0));
        user_vm.end_data = end_data.unwrap_or(VirtAddr::new(0));

        let result = BinaryLoaderResult::new(interp_load_addr.unwrap_or(program_entrypoint));
        drop(user_vm);
        // kdebug!("elf load OK!!!");
        return Ok(result);
    }
}

/// Elf机器架构，对应于e_machine字段。在ABI中，以EM_开头的常量是e_machine字段的值。
#[derive(Debug, Eq, PartialEq)]
pub enum ElfMachine {
    I386,
    AArch32,
    AArch64,
    X86_64,
    RiscV,
    /// 龙芯架构
    LoongArch,
    /// 未知架构
    Unknown,
}

impl From<u16> for ElfMachine {
    fn from(machine: u16) -> Self {
        match machine {
            0x03 => Self::I386,
            0x28 => Self::AArch32,
            0xb7 => Self::AArch64,
            0x3e => Self::X86_64,
            0xf3 => Self::RiscV,
            0x102 => Self::LoongArch,
            // 未知架构
            _ => Self::Unknown,
        }
    }
}

/// Elf文件类型，对应于e_type字段。在ABI中，以ET_开头的常量是e_type字段的值。
#[derive(Debug, Eq, PartialEq)]
pub enum ElfType {
    /// 可重定位文件
    Relocatable,
    /// 可执行文件
    Executable,
    /// 动态链接库
    DSO,
    /// 核心转储文件
    Core,
    /// 未知类型
    Unknown,
}

impl From<u16> for ElfType {
    fn from(elf_type: u16) -> Self {
        match elf_type {
            0x01 => Self::Relocatable,
            0x02 => Self::Executable,
            0x03 => Self::DSO,
            0x04 => Self::Core,
            _ => Self::Unknown,
        }
    }
}

// Simple convenience extension trait to wrap get() with .ok_or(SliceReadError)
trait ReadBytesExt<'data> {
    fn get_bytes(self, range: Range<usize>) -> Result<&'data [u8], elf::ParseError>;
}
impl<'data> ReadBytesExt<'data> for &'data [u8] {
    fn get_bytes(self, range: Range<usize>) -> Result<&'data [u8], elf::ParseError> {
        let start = range.start;
        let end = range.end;
        self.get(range)
            .ok_or(elf::ParseError::SliceReadError((start, end)))
    }
}

#[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
mod scratch_selftest;
#[cfg(all(feature = "mm_selftest", target_arch = "x86_64"))]
pub(crate) use scratch_selftest::run as scratch_birth_selftest;
