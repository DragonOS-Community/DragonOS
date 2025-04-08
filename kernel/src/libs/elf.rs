use core::{
    cmp::{max, min},
    fmt::Debug,
    intrinsics::{likely, unlikely},
    ops::Range,
};

use alloc::vec::Vec;
use elf::{
    abi::{PT_GNU_PROPERTY, PT_INTERP},
    endian::AnyEndian,
    file::FileHeader,
    segment::ProgramHeader,
};
use log::error;
use system_error::SystemError;

use crate::{
    arch::{CurrentElfArch, MMArch},
    driver::base::block::SeekFrom,
    filesystem::vfs::file::File,
    libs::align::page_align_up,
    mm::{
        allocator::page_frame::{PageFrameCount, VirtPageFrame},
        syscall::{MapFlags, ProtFlags},
        ucontext::InnerAddressSpace,
        MemoryManagementArch, VirtAddr,
    },
    process::{
        abi::AtType,
        exec::{BinaryLoader, BinaryLoaderResult, ExecError, ExecLoadMode, ExecParam},
        ProcessFlags, ProcessManager,
    },
    syscall::user_access::{clear_user, copy_to_user},
};

use super::rwlock::RwLockWriteGuard;

// 存放跟架构相关的Elf属性，
pub trait ElfArch: Clone + Copy + Debug {
    const ELF_ET_DYN_BASE: usize;
    const ELF_PAGE_SIZE: usize;
}

#[derive(Debug)]
pub struct ElfLoader;

pub const ELF_LOADER: ElfLoader = ElfLoader::new();

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
    /// - `prot_flags` - 本次映射的权限
    fn set_elf_brk(
        &self,
        user_vm_guard: &mut RwLockWriteGuard<'_, InnerAddressSpace>,
        start: VirtAddr,
        end: VirtAddr,
        prot_flags: ProtFlags,
    ) -> Result<(), ExecError> {
        let start = self.elf_page_start(start);
        let end = self.elf_page_align_up(end);
        // debug!("set_elf_brk: start={:?}, end={:?}", start, end);
        if end > start {
            let r = user_vm_guard.map_anonymous(
                start,
                end - start,
                prot_flags,
                MapFlags::MAP_ANONYMOUS | MapFlags::MAP_FIXED_NOREPLACE,
                false,
                true,
            );
            // debug!("set_elf_brk: map_anonymous: r={:?}", r);
            if r.is_err() {
                error!("set_elf_brk: map_anonymous failed, err={:?}", r);
                return Err(ExecError::OutOfMemory);
            }
        }
        user_vm_guard.elf_brk_start = end;
        user_vm_guard.elf_brk = end;
        return Ok(());
    }

    /// 计算addr在ELF PAGE内的偏移
    fn elf_page_offset(&self, addr: VirtAddr) -> usize {
        addr.data() & (CurrentElfArch::ELF_PAGE_SIZE - 1)
    }

    fn elf_page_start(&self, addr: VirtAddr) -> VirtAddr {
        VirtAddr::new(addr.data() & (!(CurrentElfArch::ELF_PAGE_SIZE - 1)))
    }

    fn elf_page_align_up(&self, addr: VirtAddr) -> VirtAddr {
        VirtAddr::new(
            (addr.data() + CurrentElfArch::ELF_PAGE_SIZE - 1)
                & (!(CurrentElfArch::ELF_PAGE_SIZE - 1)),
        )
    }

    /// 根据ELF的p_flags生成对应的ProtFlags
    fn make_prot(&self, p_flags: u32, _has_interpreter: bool, _is_interpreter: bool) -> ProtFlags {
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

    /// 加载ELF文件到用户空间
    ///
    /// 参考Linux的elf_map函数
    /// https://code.dragonos.org.cn/xref/linux-5.19.10/fs/binfmt_elf.c?r=&mo=22652&fi=824#365
    /// ## 参数
    ///
    /// - `user_vm_guard`：用户空间地址空间
    /// - `param`：执行参数
    /// - `phent`：ELF文件的ProgramHeader
    /// - `addr_to_map`：当前段应该被加载到的内存地址
    /// - `prot`：保护标志
    /// - `map_flags`：映射标志
    /// - `total_size`：ELF文件的总大小
    ///
    /// ## 返回值
    ///
    /// - `Ok((VirtAddr, bool))`：如果成功加载，则bool值为true，否则为false. VirtAddr为加载的地址
    #[allow(clippy::too_many_arguments)]
    fn load_elf_segment(
        &self,
        user_vm_guard: &mut RwLockWriteGuard<'_, InnerAddressSpace>,
        param: &mut ExecParam,
        phent: &ProgramHeader,
        mut addr_to_map: VirtAddr,
        prot: &ProtFlags,
        map_flags: &MapFlags,
        total_size: usize,
    ) -> Result<(VirtAddr, bool), SystemError> {
        // debug!("load_elf_segment: addr_to_map={:?}", addr_to_map);

        // 映射位置的偏移量（页内偏移）
        let beginning_page_offset = self.elf_page_offset(addr_to_map);
        addr_to_map = self.elf_page_start(addr_to_map);
        // 计算要映射的内存的大小
        let map_size = phent.p_filesz as usize + beginning_page_offset;
        let map_size = self.elf_page_align_up(VirtAddr::new(map_size)).data();
        // 当前段在文件中的大小
        let seg_in_file_size = phent.p_filesz as usize;
        // 当前段在文件中的偏移量
        let file_offset = phent.p_offset as usize;

        // 如果当前段的大小为0，则直接返回.
        // 段在文件中的大小为0,是合法的，但是段在内存中的大小不能为0
        if map_size == 0 {
            return Ok((addr_to_map, true));
        }

        let map_err_handler = |err: SystemError| {
            if err == SystemError::EEXIST {
                error!(
                    "Pid: {:?}, elf segment at {:p} overlaps with existing mapping",
                    ProcessManager::current_pcb().pid(),
                    addr_to_map.as_ptr::<u8>()
                );
            }
            err
        };
        // 由于后面需要把ELF文件的内容加载到内存，因此暂时把当前段的权限设置为可写
        let tmp_prot = if !prot.contains(ProtFlags::PROT_WRITE) {
            *prot | ProtFlags::PROT_WRITE
        } else {
            *prot
        };

        // 映射到的虚拟地址。请注意，这个虚拟地址是user_vm_guard这个地址空间的虚拟地址。不一定是当前进程地址空间的
        let map_addr: VirtAddr;

        // total_size is the size of the ELF (interpreter) image.
        // The _first_ mmap needs to know the full size, otherwise
        // randomization might put this image into an overlapping
        // position with the ELF binary image. (since size < total_size)
        // So we first map the 'big' image - and unmap the remainder at
        // the end. (which unmap is needed for ELF images with holes.)
        if total_size != 0 {
            let total_size = self.elf_page_align_up(VirtAddr::new(total_size)).data();

            map_addr = user_vm_guard
                .map_anonymous(addr_to_map, total_size, tmp_prot, *map_flags, false, true)
                .map_err(map_err_handler)?
                .virt_address();

            let to_unmap = map_addr + map_size;
            let to_unmap_size = total_size - map_size;

            user_vm_guard.munmap(
                VirtPageFrame::new(to_unmap),
                PageFrameCount::from_bytes(to_unmap_size).unwrap(),
            )?;

            // 加载文件到内存
            self.do_load_file(
                map_addr + beginning_page_offset,
                seg_in_file_size,
                file_offset,
                param,
            )?;
            if tmp_prot != *prot {
                user_vm_guard.mprotect(
                    VirtPageFrame::new(map_addr),
                    PageFrameCount::from_bytes(page_align_up(map_size)).unwrap(),
                    *prot,
                )?;
            }
        } else {
            // debug!("total size = 0");

            map_addr = user_vm_guard
                .map_anonymous(addr_to_map, map_size, tmp_prot, *map_flags, false, true)?
                .virt_address();
            // debug!(
            //     "map ok: addr_to_map={:?}, map_addr={map_addr:?},beginning_page_offset={beginning_page_offset:?}",
            //     addr_to_map
            // );

            // 加载文件到内存
            self.do_load_file(
                map_addr + beginning_page_offset,
                seg_in_file_size,
                file_offset,
                param,
            )?;

            if tmp_prot != *prot {
                user_vm_guard.mprotect(
                    VirtPageFrame::new(map_addr),
                    PageFrameCount::from_bytes(page_align_up(map_size)).unwrap(),
                    *prot,
                )?;
            }
        }
        // debug!("load_elf_segment OK: map_addr={:?}", map_addr);
        return Ok((map_addr, true));
    }

    /// 加载ELF文件到用户空间
    ///
    /// ## 参数
    ///
    /// - `vaddr`：要加载到的虚拟地址
    /// - `size`：要加载的大小
    /// - `offset_in_file`：在文件内的偏移量
    /// - `param`：执行参数
    fn do_load_file(
        &self,
        mut vaddr: VirtAddr,
        size: usize,
        offset_in_file: usize,
        param: &mut ExecParam,
    ) -> Result<(), SystemError> {
        let file = param.file_mut();
        if (file.metadata()?.size as usize) < offset_in_file + size {
            return Err(SystemError::ENOEXEC);
        }
        let buf_size = min(size, Self::FILE_READ_BUF_SIZE);
        let mut buf = vec![0u8; buf_size];

        let mut remain = size;

        file.lseek(SeekFrom::SeekSet(offset_in_file as i64))?;

        while remain > 0 {
            let read_size = min(remain, buf_size);
            file.read(read_size, &mut buf[..read_size])?;
            // debug!("copy_to_user: vaddr={:?}, read_size = {read_size}", vaddr);
            unsafe {
                copy_to_user(vaddr, &buf[..read_size]).map_err(|_| SystemError::EFAULT)?;
            }

            vaddr += read_size;
            remain -= read_size;
        }
        return Ok(());
    }

    /// 我们需要显式的把数据段之后剩余的内存页都清零。
    fn pad_zero(&self, elf_bss: VirtAddr) -> Result<(), SystemError> {
        let nbyte = self.elf_page_offset(elf_bss);
        if nbyte > 0 {
            let nbyte = CurrentElfArch::ELF_PAGE_SIZE - nbyte;
            unsafe { clear_user(elf_bss, nbyte).map_err(|_| SystemError::EFAULT) }?;
        }
        return Ok(());
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
        ehdr: &elf::file::FileHeader<AnyEndian>,
    ) -> Result<(), ExecError> {
        let phdr_vaddr = phdr_vaddr.unwrap_or(VirtAddr::new(0));

        let init_info = param.init_info_mut();
        init_info
            .auxv
            .insert(AtType::PhEnt as u8, ehdr.e_phentsize as usize);
        init_info
            .auxv
            .insert(AtType::PageSize as u8, MMArch::PAGE_SIZE);
        init_info.auxv.insert(AtType::Phdr as u8, phdr_vaddr.data());
        init_info
            .auxv
            .insert(AtType::PhNum as u8, ehdr.e_phnum as usize);
        init_info
            .auxv
            .insert(AtType::Entry as u8, entrypoint_vaddr.data());

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

    /// 解析文件的program header table
    ///
    /// ## 参数
    ///
    /// - `param`：执行参数
    /// - `ehdr`：文件头
    /// - `data_buf`：用于缓存SegmentTable的Vec。
    ///     这是因为SegmentTable的生命周期与data_buf一致。初始化这个Vec的大小为0即可。
    ///
    /// ## 说明
    ///
    /// 这个函数由elf库的`elf::elf_bytes::find_phdrs`修改而来。
    fn parse_segments<'a>(
        param: &mut ExecParam,
        ehdr: &FileHeader<AnyEndian>,
        data_buf: &'a mut Vec<u8>,
    ) -> Result<Option<elf::segment::SegmentTable<'a, AnyEndian>>, elf::ParseError> {
        // It's Ok to have no program headers
        if ehdr.e_phoff == 0 {
            return Ok(None);
        }
        let file = param.file_mut();
        // If the number of segments is greater than or equal to PN_XNUM (0xffff),
        // e_phnum is set to PN_XNUM, and the actual number of program header table
        // entries is contained in the sh_info field of the section header at index 0.
        let mut phnum = ehdr.e_phnum as usize;
        if phnum == elf::abi::PN_XNUM as usize {
            let shoff: usize = ehdr.e_shoff.try_into()?;

            // 从磁盘读取shdr的前2个entry
            file.lseek(SeekFrom::SeekSet(shoff as i64))
                .map_err(|_| elf::ParseError::BadOffset(shoff as u64))?;
            let shdr_buf_size = ehdr.e_shentsize * 2;
            let mut shdr_buf = vec![0u8; shdr_buf_size as usize];
            file.read(shdr_buf_size as usize, &mut shdr_buf)
                .map_err(|_| elf::ParseError::BadOffset(shoff as u64))?;

            let mut offset = 0;
            let shdr0 = <elf::section::SectionHeader as elf::parse::ParseAt>::parse_at(
                ehdr.endianness,
                ehdr.class,
                &mut offset,
                &shdr_buf,
            )?;
            phnum = shdr0.sh_info.try_into()?;
        }

        // Validate phentsize before trying to read the table so that we can error early for corrupted files
        let entsize = <ProgramHeader as elf::parse::ParseAt>::validate_entsize(
            ehdr.class,
            ehdr.e_phentsize as usize,
        )?;
        let phoff: usize = ehdr.e_phoff.try_into()?;
        let size = entsize
            .checked_mul(phnum)
            .ok_or(elf::ParseError::IntegerOverflow)?;
        phoff
            .checked_add(size)
            .ok_or(elf::ParseError::IntegerOverflow)?;

        // 读取program header table

        file.lseek(SeekFrom::SeekSet(phoff as i64))
            .map_err(|_| elf::ParseError::BadOffset(phoff as u64))?;
        data_buf.clear();
        data_buf.resize(size, 0);

        file.read(size, data_buf)
            .expect("read program header table failed");
        let buf = data_buf.get_bytes(0..size)?;

        return Ok(Some(elf::segment::SegmentTable::new(
            ehdr.endianness,
            ehdr.class,
            buf,
        )));
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
        let mut user_vm = binding.write();

        // todo: 增加对user stack上的内存是否具有可执行权限的处理（方法：寻找phdr里面的PT_GNU_STACK段）

        // debug!("to parse segments");
        // 加载ELF文件并映射到用户空间
        let mut phdr_buf = Vec::new();
        let phdr_table = Self::parse_segments(param, &ehdr, &mut phdr_buf)
            .map_err(|_| ExecError::ParseError)?
            .ok_or(ExecError::ParseError)?;
        let mut _gnu_property_data: Option<ProgramHeader> = None;
        let interpreter: Option<File> = None;
        for seg in phdr_table {
            if seg.p_type == PT_GNU_PROPERTY {
                _gnu_property_data = Some(seg);
                continue;
            }
            if seg.p_type != PT_INTERP {
                continue;
            }

            // 接下来处理这个 .interpreter 段以及动态链接器
            // 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/fs/binfmt_elf.c#881

            if seg.p_filesz > 4096 || seg.p_filesz < 2 {
                return Err(ExecError::NotExecutable);
            }
            let mut buffer = vec![0; seg.p_filesz.try_into().unwrap()];
            let r = param
                .file_mut()
                .pread(
                    seg.p_offset.try_into().unwrap(),
                    seg.p_filesz.try_into().unwrap(),
                    buffer.as_mut_slice(),
                )
                .map_err(|e| {
                    log::error!("Failed to load interpreter :{:?}", e);
                    return ExecError::NotSupported;
                })?;
            if r != seg.p_filesz.try_into().unwrap() {
                log::error!("Failed to load interpreter ");
                return Err(ExecError::NotSupported);
            }
            let _interpreter_path = core::str::from_utf8(
                &buffer[0..TryInto::<usize>::try_into(seg.p_filesz).unwrap() - 1], //
            )
            .map_err(|e| {
                ExecError::Other(format!(
                    "Failed to parse the path of dynamic linker with error {}",
                    e
                ))
            })?;

            //TODO 加入对动态链接器的加载，参照 https://code.dragonos.org.cn/xref/linux-6.1.9/fs/binfmt_elf.c#890
        }
        if interpreter.is_some() {
            /* Some simple consistency checks for the interpreter */
            // 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/fs/binfmt_elf.c#950
        }
        Self::parse_gnu_property()?;

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
        // 参考https://code.dragonos.org.cn/xref/linux-6.1.9/fs/binfmt_elf.c#1158，获取要加载的total_size
        let mut has_load = false;
        let mut min_address = VirtAddr::new(usize::MAX);
        let mut max_address = VirtAddr::new(0usize);
        let loadable_sections = phdr_table
            .into_iter()
            .filter(|seg| seg.p_type == elf::abi::PT_LOAD);

        for seg_to_load in loadable_sections {
            min_address = min(
                min_address,
                self.elf_page_start(VirtAddr::new(seg_to_load.p_vaddr.try_into().unwrap())),
            );
            max_address = max(
                max_address,
                VirtAddr::new(
                    (seg_to_load.p_vaddr + seg_to_load.p_memsz)
                        .try_into()
                        .unwrap(),
                ),
            );
            has_load = true;
        }
        let total_size = if has_load {
            max_address - min_address
        } else {
            0
        };
        let loadable_sections = phdr_table
            .into_iter()
            .filter(|seg| seg.p_type == elf::abi::PT_LOAD);
        for seg_to_load in loadable_sections {
            // debug!("seg_to_load = {:?}", seg_to_load);
            if unlikely(elf_brk > elf_bss) {
                // debug!(
                //     "to set brk, elf_brk = {:?}, elf_bss = {:?}",
                //     elf_brk,
                //     elf_bss
                // );
                self.set_elf_brk(
                    &mut user_vm,
                    elf_bss + load_bias,
                    elf_brk + load_bias,
                    bss_prot_flags,
                )?;
                let nbyte = self.elf_page_offset(elf_bss);
                if nbyte > 0 {
                    let nbyte = min(CurrentElfArch::ELF_PAGE_SIZE - nbyte, elf_brk - elf_bss);
                    unsafe {
                        // This bss-zeroing can fail if the ELF file specifies odd protections.
                        // So we don't check the return value.
                        clear_user(elf_bss + load_bias, nbyte).ok();
                    }
                }
            }

            // 生成ProtFlags.
            let elf_prot_flags = self.make_prot(seg_to_load.p_flags, interpreter.is_some(), false);

            let mut elf_map_flags = MapFlags::MAP_PRIVATE;

            let vaddr = VirtAddr::new(seg_to_load.p_vaddr.try_into().unwrap());

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
                load_bias = self
                    .elf_page_start(VirtAddr::new(
                        load_bias - TryInto::<usize>::try_into(seg_to_load.p_vaddr).unwrap(),
                    ))
                    .data();
                if total_size == 0 {
                    return Err(ExecError::InvalidParemeter);
                }
            }

            // 加载这个段到用户空间
            // debug!("to load elf segment");
            let e = self
                .load_elf_segment(
                    &mut user_vm,
                    param,
                    &seg_to_load,
                    vaddr + load_bias,
                    &elf_prot_flags,
                    &elf_map_flags,
                    total_size,
                )
                .map_err(|e| {
                    error!("load_elf_segment failed: {:?}", e);
                    match e {
                        SystemError::EFAULT => ExecError::BadAddress(None),
                        SystemError::ENOMEM => ExecError::OutOfMemory,
                        _ => ExecError::Other(format!("load_elf_segment failed: {:?}", e)),
                    }
                })?;

            // 如果地址不对，那么就报错
            if !e.1 {
                return Err(ExecError::BadAddress(Some(e.0)));
            }

            if first_pt_load {
                first_pt_load = false;
                if elf_type == ElfType::DSO {
                    // todo: 在这里增加对load_bias和reloc_func_desc的更新代码
                    load_bias += e.0.data()
                        - self
                            .elf_page_start(VirtAddr::new(
                                load_bias
                                    + TryInto::<usize>::try_into(seg_to_load.p_vaddr).unwrap(),
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

            // 如果程序段要加载的目标地址不在用户空间内，或者是其他不合法的情况，那么就报错
            if !p_vaddr.check_user()
                || seg_to_load.p_filesz > seg_to_load.p_memsz
                || self.elf_page_align_up(p_vaddr + seg_to_load.p_memsz as usize)
                    >= MMArch::USER_END_VADDR
            {
                // debug!("ERR:     p_vaddr={p_vaddr:?}");
                return Err(ExecError::InvalidParemeter);
            }

            // end vaddr of this segment(code+data+bss)
            let seg_end_vaddr_f = self.elf_page_align_up(VirtAddr::new(
                (seg_to_load.p_vaddr + seg_to_load.p_filesz) as usize,
            ));

            if seg_end_vaddr_f > elf_bss {
                elf_bss = seg_end_vaddr_f;
            }

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

        // debug!(
        //     "to set brk: elf_bss: {:?}, elf_brk: {:?}, bss_prot_flags: {:?}",
        //     elf_bss,
        //     elf_brk,
        //     bss_prot_flags
        // );
        self.set_elf_brk(&mut user_vm, elf_bss, elf_brk, bss_prot_flags)?;

        if likely(elf_bss != elf_brk) && unlikely(self.pad_zero(elf_bss).is_err()) {
            // debug!("elf_bss = {elf_bss:?}, elf_brk = {elf_brk:?}");
            return Err(ExecError::BadAddress(Some(elf_bss)));
        }
        if interpreter.is_some() {
            // TODO 添加对动态加载器的处理
            // 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/fs/binfmt_elf.c#1249
        }
        // debug!("to create auxv");

        self.create_auxv(param, program_entrypoint, phdr_vaddr, &ehdr)?;

        // debug!("auxv create ok");
        user_vm.start_code = start_code.unwrap_or(VirtAddr::new(0));
        user_vm.end_code = end_code.unwrap_or(VirtAddr::new(0));
        user_vm.start_data = start_data.unwrap_or(VirtAddr::new(0));
        user_vm.end_data = end_data.unwrap_or(VirtAddr::new(0));

        let result = BinaryLoaderResult::new(program_entrypoint);
        // debug!("elf load OK!!!");
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
