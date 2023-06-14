use core::{
    cmp::min,
    intrinsics::{likely, unlikely},
};

use elf::{endian::AnyEndian, file::FileHeader, segment::ProgramHeader, ElfBytes};

use crate::{
    arch::{
        libs::user_access::{clear_user, copy_to_user},
        MMArch,
    },
    current_pcb,
    io::SeekFrom,
    kerror,
    mm::{
        allocator::page_frame::{PageFrameCount, VirtPageFrame},
        syscall::{MapFlags, ProtFlags},
        ucontext::InnerAddressSpace,
        MemoryManagementArch, VirtAddr,
    },
    process::{
        abi::AtType,
        exec::{BinaryLoader, BinaryLoaderResult, ExecError, ExecLoadMode, ExecParam},
    },
    syscall::SystemError,
};

use super::rwlock::RwLockWriteGuard;

#[derive(Debug)]
pub struct ElfLoader;

pub const ELF_LOADER: ElfLoader = ElfLoader::new();

impl ElfLoader {
    #[cfg(target_arch = "x86_64")]
    pub const ELF_PAGE_SIZE: usize = MMArch::PAGE_SIZE;

    /// 读取文件的缓冲区大小
    pub const FILE_READ_BUF_SIZE: usize = 512 * 1024;

    pub const fn new() -> Self {
        Self
    }

    #[cfg(target_arch = "x86_64")]
    pub fn probe_x86_64(
        &self,
        param: &ExecParam,
        ehdr: &FileHeader<AnyEndian>,
    ) -> Result<(), ExecError> {
        // 只支持 64 位的 ELF 文件
        if ehdr.class != elf::file::Class::ELF64 {
            return Err(ExecError::WrongArchitecture);
        }

        // 判断架构是否匹配
        if ElfMachine::from(ehdr.e_machine) != ElfMachine::X86_64 {
            return Err(ExecError::WrongArchitecture);
        }

        // 判断是否以可执行文件的形式加载
        if param.load_mode() == ExecLoadMode::Exec {
            // 检查文件类型是否为可执行文件
            if ElfType::from(ehdr.e_type) != ElfType::Executable {
                return Err(ExecError::NotExecutable);
            }
        } else {
            return Err(ExecError::NotSupported);
        }

        return Ok(());
    }

    /// 设置用户堆空间，映射[start, end)区间的虚拟地址，并把brk指针指向end
    ///
    /// ## 参数
    ///
    /// - `user_vm_guard` - 用户虚拟地址空间
    /// - `start` - 本次映射的起始地址
    /// - `end` - 本次映射的结束地址（不包含）
    /// - `prot_flags` - 本次映射的权限
    fn set_brk(
        &self,
        user_vm_guard: &mut RwLockWriteGuard<'_, InnerAddressSpace>,
        start: VirtAddr,
        end: VirtAddr,
        prot_flags: ProtFlags,
    ) -> Result<(), ExecError> {
        let start = self.elf_page_align_up(start);
        let end = self.elf_page_align_up(end);

        if end > start {
            let r = user_vm_guard.map_anonymous(
                start,
                end - start,
                prot_flags,
                MapFlags::MAP_ANONYMOUS,
                false,
            );
            if r.is_err() {
                return Err(ExecError::OutOfMemory);
            }
        }
        user_vm_guard.brk_start = end;
        user_vm_guard.brk = end;
        return Ok(());
    }

    /// 计算addr在ELF PAGE内的偏移
    fn elf_page_offset(&self, addr: VirtAddr) -> usize {
        addr.data() & Self::ELF_PAGE_SIZE
    }

    fn elf_page_start(&self, addr: VirtAddr) -> VirtAddr {
        VirtAddr::new(addr.data() & (!(Self::ELF_PAGE_SIZE - 1)))
    }

    fn elf_page_align_up(&self, addr: VirtAddr) -> VirtAddr {
        VirtAddr::new((addr.data() + Self::ELF_PAGE_SIZE - 1) & (!(Self::ELF_PAGE_SIZE - 1)))
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
        // ref:  https://opengrok.ringotek.cn/xref/linux-5.19.10/fs/binfmt_elf.c?r=&mo=22652&fi=824#572

        return prot;
    }

    /// 加载ELF文件到用户空间
    ///
    /// 参考Linux的elf_map函数
    /// https://opengrok.ringotek.cn/xref/linux-5.19.10/fs/binfmt_elf.c?r=&mo=22652&fi=824#365
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
        // 当前段应该被加载到的内存地址
        let size =
            phent.p_filesz as usize + self.elf_page_offset(VirtAddr::new(phent.p_vaddr as usize));
        let size = self.elf_page_align_up(VirtAddr::new(size)).data();
        // 当前段在文件中的偏移
        let offset =
            phent.p_offset as usize - self.elf_page_offset(VirtAddr::new(phent.p_vaddr as usize));

        addr_to_map = self.elf_page_start(addr_to_map);

        // 如果当前段的大小为0，则直接返回.
        // 段在文件中的大小为0,是合法的，但是段在内存中的大小不能为0
        if size == 0 {
            return Ok((addr_to_map, true));
        }

        let map_err_handler = |err: SystemError| {
            if err == SystemError::EEXIST {
                kerror!(
                    "Pid: {}, elf segment at {:p} overlaps with existing mapping",
                    current_pcb().pid,
                    addr_to_map.as_ptr::<u8>()
                );
            }
            err
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
                .map_anonymous(addr_to_map, total_size, *prot, *map_flags, false)
                .map_err(map_err_handler)?
                .virt_address();
            let to_unmap = map_addr + size;
            let to_unmap_size = total_size - size;
            user_vm_guard.munmap(
                VirtPageFrame::new(to_unmap),
                PageFrameCount::from_bytes(to_unmap_size).unwrap(),
            )?;

            // 加载文件到内存
            self.do_load_file(map_addr, size, offset, param)?;
        } else {
            map_addr = user_vm_guard
                .map_anonymous(addr_to_map, size, *prot, *map_flags, false)?
                .virt_address();

            // 加载文件到内存
            self.do_load_file(map_addr, size, offset, param)?;
        }

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
            let nbyte = Self::ELF_PAGE_SIZE - nbyte;
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
        phdr_vaddr: VirtAddr,
        elf_header: &ElfBytes<AnyEndian>,
    ) -> Result<(), ExecError> {
        let init_info = param.init_info_mut();
        init_info
            .auxv
            .insert(AtType::PhEnt as u8, elf_header.ehdr.e_phentsize as usize);
        init_info
            .auxv
            .insert(AtType::PageSize as u8, MMArch::PAGE_SIZE);
        init_info.auxv.insert(AtType::Phdr as u8, phdr_vaddr.data());
        init_info
            .auxv
            .insert(AtType::PhNum as u8, elf_header.ehdr.e_phnum as usize);
        init_info
            .auxv
            .insert(AtType::Entry as u8, entrypoint_vaddr.data());

        return Ok(());
    }
}

impl BinaryLoader for ElfLoader {
    fn probe(self: &'static Self, param: &ExecParam, buf: &[u8]) -> Result<(), ExecError> {
        let elf_bytes =
            ElfBytes::<AnyEndian>::minimal_parse(buf).map_err(|_| ExecError::NotExecutable)?;

        let ehdr = elf_bytes.ehdr;

        #[cfg(target_arch = "x86_64")]
        return self.probe_x86_64(param, &ehdr);

        #[cfg(not(target_arch = "x86_64"))]
        unimplemented!("Unsupported architecture");
    }

    fn load(
        self: &'static Self,
        param: &mut ExecParam,
        head_buf: &[u8],
    ) -> Result<BinaryLoaderResult, ExecError> {
        // 解析elf文件头
        let elf_bytes =
            ElfBytes::<AnyEndian>::minimal_parse(head_buf).map_err(|_| ExecError::NotExecutable)?;
        // 参考linux-5.19的load_elf_binary函数
        // https://opengrok.ringotek.cn/xref/linux-5.19.10/fs/binfmt_elf.c?r=&mo=22652&fi=824#1034

        let elf_type = ElfType::from(elf_bytes.ehdr.e_type);

        let binding = param.vm().clone();
        let mut user_vm = binding.write();

        // todo: 增加对user stack上的内存是否具有可执行权限的处理（方法：寻找phdr里面的PT_GNU_STACK段）

        // todo: 增加对动态链接的处理

        // 加载ELF文件并映射到用户空间
        let loadable_sections = elf_bytes
            .segments()
            .ok_or(ExecError::ParseError)?
            .iter()
            .filter(|seg| seg.p_type == elf::abi::PT_LOAD);

        let mut elf_brk = VirtAddr::new(0);
        let mut elf_bss = VirtAddr::new(0);
        let mut start_code: Option<VirtAddr> = None;
        let mut end_code: Option<VirtAddr> = None;
        let mut start_data: Option<VirtAddr> = None;
        let mut end_data: Option<VirtAddr> = None;

        // 加载的时候的偏移量（这个偏移量在加载动态链接段的时候产生，由于还没有动态链接，因此暂时不可变。）
        // 请不要删除load_bias! 以免到时候写动态链接的时候忘记了。
        let load_bias = 0usize;
        let mut bss_prot_flags = ProtFlags::empty();
        // 是否是第一个加载的段
        let mut first_pt_load = true;
        // program header的虚拟地址
        let mut phdr_vaddr: Option<VirtAddr> = None;
        for seg_to_load in loadable_sections {
            if unlikely(elf_brk > elf_bss) {
                self.set_brk(
                    &mut user_vm,
                    elf_bss + load_bias,
                    elf_brk + load_bias,
                    bss_prot_flags,
                )?;
                let nbyte = self.elf_page_offset(elf_bss);
                if nbyte > 0 {
                    let nbyte = min(Self::ELF_PAGE_SIZE - nbyte, elf_brk - elf_bss);
                    unsafe {
                        // This bss-zeroing can fail if the ELF file specifies odd protections.
                        // So we don't check the return value.
                        clear_user(elf_bss + load_bias, nbyte).ok();
                    }
                }
            }

            // 生成ProtFlags.
            // TODO: 当有了动态链接之后，需要根据情况设置这里的has_interpreter
            let elf_prot_flags = self.make_prot(seg_to_load.p_flags, false, false);

            let mut elf_map_flags = MapFlags::MAP_PRIVATE;

            let vaddr = VirtAddr::new(seg_to_load.p_vaddr as usize);

            if !first_pt_load {
                elf_map_flags.insert(MapFlags::MAP_FIXED);
            } else if elf_type == ElfType::Executable {
                /*
                 * This logic is run once for the first LOAD Program
                 * Header for ET_EXEC binaries. No special handling
                 * is needed.
                 */
                elf_map_flags.insert(MapFlags::MAP_FIXED_NOREPLACE);
            } else if elf_type == ElfType::DSO {
                // TODO: 支持动态链接
                unimplemented!("DragonOS currently does not support dynamic linking!");
            }

            // 加载这个段到用户空间
            // todo: 引入动态链接后，这里的total_size要按照实际的填写，而不一定是0

            let e = self
                .load_elf_segment(
                    &mut user_vm,
                    param,
                    &seg_to_load,
                    vaddr + load_bias,
                    &elf_prot_flags,
                    &elf_map_flags,
                    0,
                )
                .map_err(|_| ExecError::InvalidParemeter)?;

            // 如果地址不对，那么就报错
            if !e.1 {
                return Err(ExecError::BadAddress(e.0));
            }

            if first_pt_load {
                first_pt_load = false;
                if elf_type == ElfType::DSO {
                    // todo: 在这里增加对load_bias和reloc_func_desc的更新代码
                    todo!()
                }
            }

            // Figure out which segment in the file contains the Program Header Table,
            // and map to the associated virtual address.
            if (seg_to_load.p_offset < elf_bytes.ehdr.e_phoff)
                && (elf_bytes.ehdr.e_phoff < (seg_to_load.p_offset + seg_to_load.p_filesz))
            {
                phdr_vaddr = Some(VirtAddr::new(
                    (elf_bytes.ehdr.e_phoff - seg_to_load.p_offset + seg_to_load.p_vaddr) as usize,
                ));
            }

            let p_vaddr = VirtAddr::new(seg_to_load.p_vaddr as usize);
            if (seg_to_load.p_flags & elf::abi::PF_X) != 0 {
                if start_code.is_none() || start_code.as_ref().unwrap() > &p_vaddr {
                    start_code = Some(p_vaddr);
                }
            }

            if start_data.is_none()
                || (start_data.is_some() && start_data.as_ref().unwrap() > &p_vaddr)
            {
                start_data = Some(p_vaddr);
            }

            // 如果程序段要加载的目标地址不在用户空间内，或者是其他不合法的情况，那么就报错
            if !p_vaddr.check_user()
                || seg_to_load.p_filesz > seg_to_load.p_memsz
                || seg_to_load.p_memsz > MMArch::USER_END_VADDR.data() as u64
            {
                return Err(ExecError::InvalidParemeter);
            }

            drop(p_vaddr);

            // end vaddr of this segment(code+data+bss)
            let seg_end_vaddr_f =
                VirtAddr::new((seg_to_load.p_vaddr + seg_to_load.p_filesz) as usize);
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

            drop(seg_end_vaddr_f);

            let seg_end_vaddr = VirtAddr::new((seg_to_load.p_vaddr + seg_to_load.p_memsz) as usize);

            if seg_end_vaddr > elf_brk {
                bss_prot_flags = elf_prot_flags;
                elf_brk = seg_end_vaddr;
            }
        }

        let program_entrypoint = VirtAddr::new(elf_bytes.ehdr.e_entry as usize + load_bias);
        if phdr_vaddr.is_none() {
            return Err(ExecError::InvalidParemeter);
        }

        let phdr_vaddr: VirtAddr = phdr_vaddr.unwrap() + load_bias;
        elf_bss += load_bias;
        elf_brk += load_bias;
        start_code = start_code.map(|v| v + load_bias);
        end_code = end_code.map(|v| v + load_bias);
        start_data = start_data.map(|v| v + load_bias);
        end_data = end_data.map(|v| v + load_bias);

        self.set_brk(&mut user_vm, elf_bss, elf_brk, bss_prot_flags)?;

        if likely(elf_bss != elf_brk) && unlikely(self.pad_zero(elf_bss).is_ok()) {
            return Err(ExecError::BadAddress(elf_bss));
        }
        // todo: 动态链接：增加加载interpreter的代码

        self.create_auxv(param, program_entrypoint, phdr_vaddr, &elf_bytes)?;

        user_vm.start_code = start_code.unwrap_or(VirtAddr::new(0));
        user_vm.end_code = end_code.unwrap_or(VirtAddr::new(0));
        user_vm.start_data = start_data.unwrap_or(VirtAddr::new(0));
        user_vm.end_data = end_data.unwrap_or(VirtAddr::new(0));

        let result = BinaryLoaderResult::new(program_entrypoint);
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
