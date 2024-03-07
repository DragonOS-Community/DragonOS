use core::{
    ffi::{c_int, c_void},
    sync::atomic::compiler_fence,
};

use num::ToPrimitive;
use system_error::SystemError;

use crate::{
    arch::{
        ipc::signal::{SigCode, SigFlags, SigSet, Signal},
        MMArch,
    },
    filesystem::vfs::{
        file::{File, FileMode},
        FilePrivateData,
    },
    ipc::shm::ShmInfo,
    kerror, kwarn,
    libs::align::page_align_up,
    mm::{
        allocator::page_frame::{FrameAllocator, PageFrameCount},
        page::PageFlushAll,
        syscall::{MapFlags, ProtFlags},
        ucontext::AddressSpace,
        MemoryManagementArch, VirtAddr,
    },
    process::{Pid, ProcessManager},
    syscall::{
        user_access::{UserBufferReader, UserBufferWriter},
        Syscall,
    },
};

use super::{
    pipe::{LockedPipeInode, PipeFsPrivateData},
    shm::{
        IpcPerm, ShmCtlCmd, ShmFlags, ShmId, ShmIdDs, ShmKey, ShmMetaInfo, IPC_PRIVATE, SHM_MANAGER,
    },
    signal_types::{
        SaHandlerType, SigInfo, SigType, Sigaction, SigactionType, UserSigaction, USER_SIG_DFL,
        USER_SIG_ERR, USER_SIG_IGN,
    },
};

impl Syscall {
    /// # 创建带参数的匿名管道
    ///
    /// ## 参数
    ///
    /// - `fd`: 用于返回文件描述符的数组
    /// - `flags`:设置管道的参数
    pub fn pipe2(fd: *mut i32, flags: FileMode) -> Result<usize, SystemError> {
        if !flags
            .difference(FileMode::O_CLOEXEC | FileMode::O_NONBLOCK | FileMode::O_DIRECT)
            .is_empty()
        {
            return Err(SystemError::EINVAL);
        }

        let mut user_buffer = UserBufferWriter::new(fd, core::mem::size_of::<[c_int; 2]>(), true)?;
        let fd = user_buffer.buffer::<i32>(0)?;
        let pipe_ptr = LockedPipeInode::new();

        let mut read_file = File::new(
            pipe_ptr.clone(),
            FileMode::O_RDONLY | (flags & FileMode::O_NONBLOCK),
        )?;
        read_file.private_data =
            FilePrivateData::Pipefs(PipeFsPrivateData::new(FileMode::O_RDONLY));

        let mut write_file = File::new(
            pipe_ptr.clone(),
            FileMode::O_WRONLY | (flags & (FileMode::O_NONBLOCK | FileMode::O_DIRECT)),
        )?;
        write_file.private_data = FilePrivateData::Pipefs(PipeFsPrivateData::new(
            FileMode::O_WRONLY | (flags & (FileMode::O_NONBLOCK | FileMode::O_DIRECT)),
        ));

        if flags.contains(FileMode::O_CLOEXEC) {
            read_file.set_close_on_exec(true);
            write_file.set_close_on_exec(true);
        }
        let fd_table_ptr = ProcessManager::current_pcb().fd_table();
        let mut fd_table_guard = fd_table_ptr.write();
        let read_fd = fd_table_guard.alloc_fd(read_file, None)?;
        let write_fd = fd_table_guard.alloc_fd(write_file, None)?;

        drop(fd_table_guard);

        fd[0] = read_fd;
        fd[1] = write_fd;
        Ok(0)
    }

    pub fn kill(pid: Pid, sig: c_int) -> Result<usize, SystemError> {
        let sig = Signal::from(sig);
        if sig == Signal::INVALID {
            // 传入的signal数值不合法
            kwarn!("Not a valid signal number");
            return Err(SystemError::EINVAL);
        }

        // 初始化signal info
        let mut info = SigInfo::new(sig, 0, SigCode::User, SigType::Kill(pid));

        compiler_fence(core::sync::atomic::Ordering::SeqCst);

        let retval = sig
            .send_signal_info(Some(&mut info), pid)
            .map(|x| x as usize);

        compiler_fence(core::sync::atomic::Ordering::SeqCst);

        return retval;
    }

    /// 通用信号注册函数
    ///
    /// ## 参数
    ///
    /// - `sig` 信号的值
    /// - `act` 用户空间传入的 Sigaction 指针
    /// - `old_act` 用户空间传入的用来保存旧 Sigaction 的指针
    /// - `from_user` 用来标识这个函数调用是否来自用户空间
    ///
    /// @return int 错误码
    #[no_mangle]
    pub fn sigaction(
        sig: c_int,
        new_act: usize,
        old_act: usize,
        from_user: bool,
    ) -> Result<usize, SystemError> {
        // 请注意：用户态传进来的user_sigaction结构体类型，请注意，这个结构体与内核实际的不一样
        let act: *mut UserSigaction = new_act as *mut UserSigaction;
        let old_act = old_act as *mut UserSigaction;
        let mut new_ka: Sigaction = Default::default();
        let mut old_sigaction: Sigaction = Default::default();
        // 如果传入的，新的sigaction不为空
        if !act.is_null() {
            // 如果参数的范围不在用户空间，则返回错误
            let r = UserBufferWriter::new(act, core::mem::size_of::<Sigaction>(), from_user);
            if r.is_err() {
                return Err(SystemError::EFAULT);
            }
            let mask: SigSet = unsafe { (*act).mask };
            let input_sighandler = unsafe { (*act).handler as u64 };
            match input_sighandler {
                USER_SIG_DFL => {
                    new_ka = Sigaction::DEFAULT_SIGACTION.clone();
                    *new_ka.flags_mut() = unsafe { (*act).flags };
                    new_ka.set_restorer(None);
                }

                USER_SIG_IGN => {
                    new_ka = Sigaction::DEFAULT_SIGACTION_IGNORE.clone();
                    *new_ka.flags_mut() = unsafe { (*act).flags };

                    new_ka.set_restorer(None);
                }
                _ => {
                    // 从用户空间获得sigaction结构体
                    // TODO mask是default还是用户空间传入
                    new_ka = Sigaction::new(
                        SigactionType::SaHandler(SaHandlerType::SigCustomized(unsafe {
                            VirtAddr::new((*act).handler as usize)
                        })),
                        unsafe { (*act).flags },
                        SigSet::default(),
                        unsafe { Some(VirtAddr::new((*act).restorer as usize)) },
                    );
                }
            }

            // TODO 如果为空，赋默认值？
            // kdebug!("new_ka={:?}", new_ka);
            // 如果用户手动给了sa_restorer，那么就置位SA_FLAG_RESTORER，否则报错。（用户必须手动指定restorer）
            if new_ka.restorer().is_some() {
                new_ka.flags_mut().insert(SigFlags::SA_RESTORER);
            } else if new_ka.action().is_customized() {
                kerror!(
                "pid:{:?}: in sys_sigaction: User must manually sprcify a sa_restorer for signal {}.",
                ProcessManager::current_pcb().pid(),
                sig
            );
                return Err(SystemError::EINVAL);
            }
            *new_ka.mask_mut() = mask;
        }

        let sig = Signal::from(sig as i32);
        // 如果给出的信号值不合法
        if sig == Signal::INVALID {
            return Err(SystemError::EINVAL);
        }

        let retval = super::signal::do_sigaction(
            sig,
            if act.is_null() {
                None
            } else {
                Some(&mut new_ka)
            },
            if old_act.is_null() {
                None
            } else {
                Some(&mut old_sigaction)
            },
        );

        //
        if (retval == Ok(())) && (!old_act.is_null()) {
            let r =
                UserBufferWriter::new(old_act, core::mem::size_of::<UserSigaction>(), from_user);
            if r.is_err() {
                return Err(SystemError::EFAULT);
            }

            let sigaction_handler: VirtAddr;
            sigaction_handler = match old_sigaction.action() {
                SigactionType::SaHandler(handler) => {
                    if let SaHandlerType::SigCustomized(hand) = handler {
                        hand
                    } else if handler.is_sig_ignore() {
                        VirtAddr::new(USER_SIG_IGN as usize)
                    } else if handler.is_sig_error() {
                        VirtAddr::new(USER_SIG_ERR as usize)
                    } else {
                        VirtAddr::new(USER_SIG_DFL as usize)
                    }
                }
                SigactionType::SaSigaction(_) => {
                    kerror!("unsupported type: SaSigaction");
                    VirtAddr::new(USER_SIG_DFL as usize)
                }
            };

            unsafe {
                (*old_act).handler = sigaction_handler.data() as *mut c_void;
                (*old_act).flags = old_sigaction.flags();
                (*old_act).mask = old_sigaction.mask();
                if old_sigaction.restorer().is_some() {
                    (*old_act).restorer = old_sigaction.restorer().unwrap().data() as *mut c_void;
                }
            }
        }
        return retval.map(|_| 0);
    }

    /// # SYS_SHMGET系统调用函数，用于获取共享内存
    ///
    /// ## 参数
    ///
    /// - `key`: 共享内存键值
    /// - `size`: 共享内存大小(bytes)
    /// - `shm_flags`: 共享内存标志
    ///
    /// ## 返回值
    ///
    /// 成功：共享内存id
    /// 失败：错误码
    pub fn shmget(key: ShmKey, size: usize, shm_flags: ShmFlags) -> Result<usize, SystemError> {
        // 暂不支持巨页映射
        if shm_flags.contains(ShmFlags::SHM_HUGETLB) {
            kerror!("shmget: not support huge page");
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }

        let mut shm_manager_guard = SHM_MANAGER.lock();
        match key {
            // 创建共享内存
            IPC_PRIVATE => shm_manager_guard.add(key, size, shm_flags),
            _ => {
                // 查找key是否存在
                let shm_id = shm_manager_guard.find_key(key);
                if shm_id.is_none() {
                    // key不存在且shm_flags不包含IPC_CREAT创建IPC对象标志，则返回错误码
                    if !shm_flags.contains(ShmFlags::IPC_CREAT) {
                        return Err(SystemError::ENOENT);
                    }

                    // 存在创建IPC对象标志
                    return shm_manager_guard.add(key, size, shm_flags);
                } else {
                    // key值存在，说明有对应共享内存，返回该共享内存id
                    if shm_flags.contains(ShmFlags::IPC_CREAT | ShmFlags::IPC_EXCL) {
                        return Err(SystemError::EEXIST);
                    }

                    return Ok(shm_id.unwrap().data());
                }
            }
        }
    }

    /// # SYS_SHMAT系统调用函数，用于连接共享内存段
    ///
    /// ## 参数
    ///
    /// - `shm_id`: 共享内存id
    /// - `start_vaddr`: 连接共享内存的进程虚拟内存区域起始地址
    /// - `shm_flags`: 共享内存标志
    ///
    /// ## 返回值
    ///
    /// 成功：映射到共享内存的虚拟内存区域起始地址
    /// 失败：错误码
    pub fn shmat(
        shm_id: ShmId,
        start_vaddr: VirtAddr,
        shm_flags: ShmFlags,
    ) -> Result<usize, SystemError> {
        let shm_manager_guard = SHM_MANAGER.lock();
        let shm_kernel = shm_manager_guard.get(shm_id).ok_or(SystemError::EINVAL)?;
        let shm_kernel_guard = shm_kernel.lock();

        // start_vaddr如果是0，则由内核指定虚拟内存区域
        let len = shm_kernel_guard.size();
        let start_vaddr = match start_vaddr.data() {
            0 => {
                let start_vaddr = VirtAddr::new(0);
                let prot_flags: ProtFlags = shm_flags.into();
                let prot_flags = prot_flags.bits().to_usize().unwrap();
                let map_flags = MapFlags::MAP_ANONYMOUS | MapFlags::MAP_SHARED;
                let map_flags = map_flags.bits().to_usize().unwrap();
                let vaddr = Self::mmap(start_vaddr, len, prot_flags, map_flags, 0, 0)?;
                VirtAddr::new(vaddr)
            }
            _ => start_vaddr,
        };

        // 判断用户虚拟内存区域是否有效
        let _ = UserBufferReader::new(
            start_vaddr.data() as *const u8,
            shm_kernel_guard.size(),
            true,
        )?;

        // 取消原虚拟内存区域映射，重新映射到共享内存
        let current_address_space = AddressSpace::current()?;
        let vma = current_address_space.read().mappings.contains(start_vaddr);
        if vma.is_none() {
            return Err(SystemError::EINVAL);
        }
        let vma = vma.unwrap();

        if vma.lock().region().size() != page_align_up(shm_kernel_guard.size()) {
            return Err(SystemError::EINVAL);
        }

        let entry = current_address_space
            .read()
            .user_mapper
            .utable
            .translate(vma.lock().region().start());
        if entry.is_none() {
            return Err(SystemError::EINVAL);
        }

        let page_flags = entry.unwrap().1;
        let mut paddr = shm_kernel_guard.paddr();
        drop(shm_kernel_guard);
        drop(shm_manager_guard);

        let mut write_guard = current_address_space.write();
        let flusher: PageFlushAll<MMArch> = PageFlushAll::new();
        vma.unmap(&mut write_guard.user_mapper.utable, flusher);

        // 映射到共享内存
        for page in vma.lock().region().pages() {
            let r = unsafe {
                write_guard
                    .user_mapper
                    .utable
                    .map_phys(page.virt_address(), paddr, page_flags)
            };
            if let Some(r) = r {
                r.flush();
            }

            paddr += MMArch::PAGE_SIZE;
        }
        vma.update_mapped(true);
        SHM_MANAGER.lock().add_nattch(shm_id);

        return Ok(start_vaddr.data());
    }

    /// # SYS_SHMDT系统调用函数，用于取消对共享内存的连接
    ///
    /// ## 参数
    ///
    /// - `start_vaddr`: 映射到共享内存的虚拟内存区域的起始地址
    ///
    /// ## 返回值
    ///
    /// 成功：0
    /// 失败：错误码
    pub fn shmdt(start_vaddr: VirtAddr) -> Result<usize, SystemError> {
        let current_address_space = AddressSpace::current()?;
        let vma = current_address_space
            .read()
            .mappings
            .contains(start_vaddr)
            .ok_or(SystemError::EINVAL)?;

        let len = vma.lock().region().size();

        return Self::munmap(start_vaddr, len);
    }

    /// # SYS_SHMCTL系统调用函数，用于控制共享内存段
    ///
    /// ## 参数
    ///
    /// - `shm_id`: 共享内存id
    /// - `cmd`: 操作码
    /// - `buf_vaddr`: 用户传入的结构体地址
    /// - `from_user`: buf_vaddr是否来自用户地址空间
    ///
    /// ## 返回值
    ///
    /// 成功：0
    /// 失败：错误码
    pub fn shmctl(
        shm_id: ShmId,
        cmd: ShmCtlCmd,
        buf_vaddr: usize,
        from_user: bool,
    ) -> Result<usize, SystemError> {
        let mut shm_manager_guard = SHM_MANAGER.lock();
        let shm_kernel = shm_manager_guard.get(shm_id).ok_or(SystemError::EINVAL)?;

        match cmd {
            // 查看ShmMetaData
            ShmCtlCmd::IpcInfo => {
                let shminfo = shm_manager_guard.metadata();
                let mut user_buffer_writer = UserBufferWriter::new(
                    buf_vaddr as *mut u8,
                    core::mem::size_of::<ShmMetaInfo>(),
                    from_user,
                )?;
                user_buffer_writer.copy_one_to_user(&shminfo, 0)?;

                return Ok(0);
            }
            // 查看ShmInfo
            ShmCtlCmd::ShmInfo => {
                let used_ids = shm_manager_guard.used_ids().to_i32().unwrap();
                let shm_tot = shm_manager_guard.shm_tot();

                let shm_info = ShmInfo::new(used_ids, shm_tot, 0, 0, 0, 0);
                let mut user_buffer_writer = UserBufferWriter::new(
                    buf_vaddr as *mut u8,
                    core::mem::size_of::<ShmInfo>(),
                    from_user,
                )?;
                user_buffer_writer.copy_one_to_user(&shm_info, 0)?;

                return Ok(0);
            }
            // 查看ShmMetaData
            ShmCtlCmd::ShmStat | ShmCtlCmd::ShmtStatAny | ShmCtlCmd::IpcStat => {
                let shm_kernel_guard = shm_kernel.lock();
                let key = shm_kernel_guard.key().data().to_i32().unwrap();
                let mode = shm_kernel_guard.mode().bits();

                let shm_perm = IpcPerm::new(key, 0, 0, 0, 0, mode);
                let shm_segsz = shm_kernel_guard.size();
                let shm_atime = shm_kernel_guard.atim();
                let shm_dtime = shm_kernel_guard.dtim();
                let shm_ctime = shm_kernel_guard.ctim();
                let shm_cpid = shm_kernel_guard.cprid().data().to_u32().unwrap();
                let shm_lpid = shm_kernel_guard.lprid().data().to_u32().unwrap();
                let shm_nattch = shm_kernel_guard.nattch();

                let shm_id_ds = ShmIdDs::new(
                    shm_perm, shm_segsz, shm_atime, shm_dtime, shm_ctime, shm_cpid, shm_lpid,
                    shm_nattch,
                );

                let mut user_buffer_writer = UserBufferWriter::new(
                    buf_vaddr as *mut u8,
                    core::mem::size_of::<ShmIdDs>(),
                    from_user,
                )?;
                user_buffer_writer.copy_one_to_user(&shm_id_ds, 0)?;

                let r: usize = if cmd == ShmCtlCmd::IpcStat {
                    0
                } else {
                    shm_kernel_guard.id().data()
                };

                return Ok(r);
            }
            // 设置KernIpcPerm选项
            ShmCtlCmd::IpcSet => {
                let mut shm_kernel_guard = shm_kernel.lock();
                let user_buffer_reader = UserBufferReader::new(
                    buf_vaddr as *const u8,
                    core::mem::size_of::<ShmIdDs>(),
                    from_user,
                )?;
                let mut shm_id_ds = ShmIdDs::default();
                user_buffer_reader.copy_one_from_user(&mut shm_id_ds, 0)?;

                shm_kernel_guard.copy_from_ipc_perm(&shm_id_ds);

                shm_kernel_guard.debug();

                return Ok(0);
            }
            // 链接数为0时删除共享内存段，否则设置SHM_DEST
            ShmCtlCmd::IpcRmid => {
                let mut shm_kernel_guard = shm_kernel.lock();
                if shm_kernel_guard.nattch() > 0 {
                    shm_kernel_guard.set_dest();
                } else {
                    let current_address_space = AddressSpace::current()?;
                    let page_count =
                        PageFrameCount::from_bytes(page_align_up(shm_kernel_guard.size())).unwrap();
                    unsafe {
                        current_address_space
                            .write()
                            .user_mapper
                            .utable
                            .allocator_mut()
                            .free(shm_kernel_guard.paddr(), page_count)
                    };

                    let id = shm_kernel_guard.id();
                    let key = shm_kernel_guard.key();
                    drop(shm_kernel_guard);

                    shm_manager_guard.free(id, key);
                }

                return Ok(0);
            }
            // 不允许共享内存段被置换出物理内存
            ShmCtlCmd::ShmLock => {
                let mut shm_kernel_guard = shm_kernel.lock();
                shm_kernel_guard.lock();

                return Ok(0);
            }
            // 允许共享内存段被置换出物理内存
            ShmCtlCmd::ShmUnlock => {
                let mut shm_kernel_guard = shm_kernel.lock();
                shm_kernel_guard.unlock();

                return Ok(0);
            }
            ShmCtlCmd::Default => Err(SystemError::EINVAL),
        }
    }
}
