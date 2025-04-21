use core::{
    ffi::{c_int, c_void},
    sync::atomic::compiler_fence,
};

use log::{error, warn};
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
    ipc::shm::{shm_manager_lock, IPC_PRIVATE},
    libs::{align::page_align_up, spinlock::SpinLock},
    mm::{
        allocator::page_frame::{PageFrameCount, PhysPageFrame, VirtPageFrame},
        page::{page_manager_lock_irqsave, EntryFlags, PageFlushAll},
        syscall::ProtFlags,
        ucontext::{AddressSpace, VMA},
        VirtAddr, VmFlags,
    },
    process::{Pid, ProcessManager},
    syscall::{
        user_access::{UserBufferReader, UserBufferWriter},
        Syscall,
    },
};

use super::{
    pipe::{LockedPipeInode, PipeFsPrivateData},
    shm::{ShmCtlCmd, ShmFlags, ShmId, ShmKey},
    signal::{set_sigprocmask, SigHow},
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
        read_file.private_data = SpinLock::new(FilePrivateData::Pipefs(PipeFsPrivateData::new(
            FileMode::O_RDONLY,
        )));

        let mut write_file = File::new(
            pipe_ptr.clone(),
            FileMode::O_WRONLY | (flags & (FileMode::O_NONBLOCK | FileMode::O_DIRECT)),
        )?;
        write_file.private_data = SpinLock::new(FilePrivateData::Pipefs(PipeFsPrivateData::new(
            FileMode::O_WRONLY | (flags & (FileMode::O_NONBLOCK | FileMode::O_DIRECT)),
        )));

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
            warn!("Not a valid signal number");
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
                    new_ka = Sigaction::DEFAULT_SIGACTION;
                    *new_ka.flags_mut() = unsafe { (*act).flags };
                    new_ka.set_restorer(None);
                }

                USER_SIG_IGN => {
                    new_ka = Sigaction::DEFAULT_SIGACTION_IGNORE;
                    *new_ka.flags_mut() = unsafe { (*act).flags };

                    new_ka.set_restorer(None);
                }
                _ => {
                    // 从用户空间获得sigaction结构体
                    // TODO mask是default还是用户空间传入
                    new_ka = Sigaction::new(
                        SigactionType::SaHandler(SaHandlerType::Customized(unsafe {
                            VirtAddr::new((*act).handler as usize)
                        })),
                        unsafe { (*act).flags },
                        SigSet::default(),
                        unsafe { Some(VirtAddr::new((*act).restorer as usize)) },
                    );
                }
            }

            // TODO 如果为空，赋默认值？
            // debug!("new_ka={:?}", new_ka);
            // 如果用户手动给了sa_restorer，那么就置位SA_FLAG_RESTORER，否则报错。（用户必须手动指定restorer）
            if new_ka.restorer().is_some() {
                new_ka.flags_mut().insert(SigFlags::SA_RESTORER);
            } else if new_ka.action().is_customized() {
                error!(
                "pid:{:?}: in sys_sigaction: User must manually sprcify a sa_restorer for signal {}.",
                ProcessManager::current_pcb().pid(),
                sig
            );
                return Err(SystemError::EINVAL);
            }
            *new_ka.mask_mut() = mask;
        }

        let sig = Signal::from(sig);
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

            let sigaction_handler = match old_sigaction.action() {
                SigactionType::SaHandler(handler) => {
                    if let SaHandlerType::Customized(hand) = handler {
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
                    error!("unsupported type: SaSigaction");
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
    /// - `shmflg`: 共享内存标志
    ///
    /// ## 返回值
    ///
    /// 成功：共享内存id
    /// 失败：错误码
    pub fn shmget(key: ShmKey, size: usize, shmflg: ShmFlags) -> Result<usize, SystemError> {
        // 暂不支持巨页
        if shmflg.contains(ShmFlags::SHM_HUGETLB) {
            error!("shmget: not support huge page");
            return Err(SystemError::ENOSYS);
        }

        let mut shm_manager_guard = shm_manager_lock();
        match key {
            // 创建共享内存段
            IPC_PRIVATE => shm_manager_guard.add(key, size, shmflg),
            _ => {
                // 查找key对应的共享内存段是否存在
                let id = shm_manager_guard.contains_key(&key);
                if let Some(id) = id {
                    // 不能重复创建
                    if shmflg.contains(ShmFlags::IPC_CREAT | ShmFlags::IPC_EXCL) {
                        return Err(SystemError::EEXIST);
                    }

                    // key值存在，说明有对应共享内存，返回该共享内存id
                    return Ok(id.data());
                } else {
                    // key不存在且shm_flags不包含IPC_CREAT创建IPC对象标志，则返回错误码
                    if !shmflg.contains(ShmFlags::IPC_CREAT) {
                        return Err(SystemError::ENOENT);
                    }

                    // 存在创建IPC对象标志
                    return shm_manager_guard.add(key, size, shmflg);
                }
            }
        }
    }

    /// # SYS_SHMAT系统调用函数，用于连接共享内存段
    ///
    /// ## 参数
    ///
    /// - `id`: 共享内存id
    /// - `vaddr`: 连接共享内存的进程虚拟内存区域起始地址
    /// - `shmflg`: 共享内存标志
    ///
    /// ## 返回值
    ///
    /// 成功：映射到共享内存的虚拟内存区域起始地址
    /// 失败：错误码
    pub fn shmat(id: ShmId, vaddr: VirtAddr, shmflg: ShmFlags) -> Result<usize, SystemError> {
        let mut shm_manager_guard = shm_manager_lock();
        let current_address_space = AddressSpace::current()?;
        let mut address_write_guard = current_address_space.write();

        let kernel_shm = shm_manager_guard.get_mut(&id).ok_or(SystemError::EINVAL)?;
        let size = page_align_up(kernel_shm.size());
        let mut phys = PhysPageFrame::new(kernel_shm.start_paddr());
        let count = PageFrameCount::from_bytes(size).unwrap();
        let r = match vaddr.data() {
            // 找到空闲区域并映射到共享内存
            0 => {
                // 找到空闲区域
                let region = address_write_guard
                    .mappings
                    .find_free(vaddr, size)
                    .ok_or(SystemError::EINVAL)?;
                let vm_flags = VmFlags::from(shmflg);
                let destination = VirtPageFrame::new(region.start());
                let page_flags: EntryFlags<MMArch> =
                    EntryFlags::from_prot_flags(ProtFlags::from(vm_flags), true);
                let flusher: PageFlushAll<MMArch> = PageFlushAll::new();

                // 将共享内存映射到对应虚拟区域
                let vma = VMA::physmap(
                    phys,
                    destination,
                    count,
                    vm_flags,
                    page_flags,
                    &mut address_write_guard.user_mapper.utable,
                    flusher,
                )?;

                // 将VMA加入到当前进程的VMA列表中
                address_write_guard.mappings.insert_vma(vma);

                region.start().data()
            }
            // 指定虚拟地址
            _ => {
                // 获取对应vma
                let vma = address_write_guard
                    .mappings
                    .contains(vaddr)
                    .ok_or(SystemError::EINVAL)?;
                if vma.lock_irqsave().region().start() != vaddr {
                    return Err(SystemError::EINVAL);
                }

                // 验证用户虚拟内存区域是否有效
                let _ = UserBufferReader::new(vaddr.data() as *const u8, size, true)?;

                // 必须在取消映射前获取到EntryFlags
                let page_flags = address_write_guard
                    .user_mapper
                    .utable
                    .translate(vaddr)
                    .ok_or(SystemError::EINVAL)?
                    .1;

                // 取消原映射
                let flusher: PageFlushAll<MMArch> = PageFlushAll::new();
                vma.unmap(&mut address_write_guard.user_mapper.utable, flusher);

                // 将该虚拟内存区域映射到共享内存区域
                let mut page_manager_guard = page_manager_lock_irqsave();
                let mut virt = VirtPageFrame::new(vaddr);
                for _ in 0..count.data() {
                    let r = unsafe {
                        address_write_guard.user_mapper.utable.map_phys(
                            virt.virt_address(),
                            phys.phys_address(),
                            page_flags,
                        )
                    }
                    .expect("Failed to map zero, may be OOM error");
                    r.flush();

                    // 将vma加入到对应Page的anon_vma
                    page_manager_guard
                        .get_unwrap(&phys.phys_address())
                        .write_irqsave()
                        .insert_vma(vma.clone());

                    phys = phys.next();
                    virt = virt.next();
                }

                // 更新vma的映射状态
                vma.lock_irqsave().set_mapped(true);

                vaddr.data()
            }
        };

        // 更新最后一次连接时间
        kernel_shm.update_atim();

        // 映射计数增加
        kernel_shm.increase_count();

        Ok(r)
    }

    /// # SYS_SHMDT系统调用函数，用于取消对共享内存的连接
    ///
    /// ## 参数
    ///
    /// - `vaddr`:  需要取消映射的虚拟内存区域起始地址
    ///
    /// ## 返回值
    ///
    /// 成功：0
    /// 失败：错误码
    pub fn shmdt(vaddr: VirtAddr) -> Result<usize, SystemError> {
        let current_address_space = AddressSpace::current()?;
        let mut address_write_guard = current_address_space.write();

        // 获取vma
        let vma = address_write_guard
            .mappings
            .contains(vaddr)
            .ok_or(SystemError::EINVAL)?;

        // 判断vaddr是否为起始地址
        if vma.lock_irqsave().region().start() != vaddr {
            return Err(SystemError::EINVAL);
        }

        // 取消映射
        let flusher: PageFlushAll<MMArch> = PageFlushAll::new();
        vma.unmap(&mut address_write_guard.user_mapper.utable, flusher);

        return Ok(0);
    }

    /// # SYS_SHMCTL系统调用函数，用于管理共享内存段
    ///
    /// ## 参数
    ///
    /// - `id`: 共享内存id
    /// - `cmd`: 操作码
    /// - `user_buf`: 用户缓冲区
    /// - `from_user`: buf_vaddr是否来自用户地址空间
    ///
    /// ## 返回值
    ///
    /// 成功：0
    /// 失败：错误码
    pub fn shmctl(
        id: ShmId,
        cmd: ShmCtlCmd,
        user_buf: *const u8,
        from_user: bool,
    ) -> Result<usize, SystemError> {
        let mut shm_manager_guard = shm_manager_lock();

        match cmd {
            // 查看共享内存元信息
            ShmCtlCmd::IpcInfo => shm_manager_guard.ipc_info(user_buf, from_user),
            // 查看共享内存使用信息
            ShmCtlCmd::ShmInfo => shm_manager_guard.shm_info(user_buf, from_user),
            // 查看id对应的共享内存信息
            ShmCtlCmd::ShmStat | ShmCtlCmd::ShmtStatAny | ShmCtlCmd::IpcStat => {
                shm_manager_guard.shm_stat(id, cmd, user_buf, from_user)
            }
            // 设置KernIpcPerm
            ShmCtlCmd::IpcSet => shm_manager_guard.ipc_set(id, user_buf, from_user),
            // 将共享内存段设置为可回收状态
            ShmCtlCmd::IpcRmid => shm_manager_guard.ipc_rmid(id),
            // 锁住共享内存段，不允许内存置换
            ShmCtlCmd::ShmLock => shm_manager_guard.shm_lock(id),
            // 解锁共享内存段，允许内存置换
            ShmCtlCmd::ShmUnlock => shm_manager_guard.shm_unlock(id),
            // 无效操作码
            ShmCtlCmd::Default => Err(SystemError::EINVAL),
        }
    }

    /// # SYS_SIGPROCMASK系统调用函数，用于设置或查询当前进程的信号屏蔽字
    ///
    /// ## 参数
    ///
    /// - `how`: 指示如何修改信号屏蔽字
    /// - `nset`: 新的信号屏蔽字
    /// - `oset`: 旧的信号屏蔽字的指针，由于可以是NULL，所以用Option包装
    /// - `sigsetsize`: 信号集的大小
    ///
    /// ## 返回值
    ///
    /// 成功：0
    /// 失败：错误码
    ///
    /// ## 说明
    /// 根据 https://man7.org/linux/man-pages/man2/sigprocmask.2.html ，传进来的oldset和newset都是指针类型，这里选择传入usize然后转换为u64的指针类型
    pub fn rt_sigprocmask(
        how: i32,
        newset: usize,
        oldset: usize,
        sigsetsize: usize,
    ) -> Result<usize, SystemError> {
        // 对应oset传进来一个NULL的情况
        let oset = if oldset == 0 { None } else { Some(oldset) };
        let nset = if newset == 0 { None } else { Some(newset) };

        if sigsetsize != size_of::<SigSet>() {
            return Err(SystemError::EFAULT);
        }

        let sighow = SigHow::try_from(how)?;

        let mut new_set = SigSet::default();
        if let Some(nset) = nset {
            let reader = UserBufferReader::new(
                VirtAddr::new(nset).as_ptr::<u64>(),
                core::mem::size_of::<u64>(),
                true,
            )?;

            let nset = reader.read_one_from_user::<u64>(0)?;
            new_set = SigSet::from_bits_truncate(*nset);
            // debug!("Get Newset: {}", &new_set.bits());
            let to_remove: SigSet =
                <Signal as Into<SigSet>>::into(Signal::SIGKILL) | Signal::SIGSTOP.into();
            new_set.remove(to_remove);
        }

        let oldset_to_return = set_sigprocmask(sighow, new_set)?;
        if let Some(oldset) = oset {
            // debug!("Get Oldset to return: {}", &oldset_to_return.bits());
            let mut writer = UserBufferWriter::new(
                VirtAddr::new(oldset).as_ptr::<u64>(),
                core::mem::size_of::<u64>(),
                true,
            )?;
            writer.copy_one_to_user::<u64>(&oldset_to_return.bits(), 0)?;
        }

        Ok(0)
    }

    pub fn restart_syscall() -> Result<usize, SystemError> {
        let restart_block = ProcessManager::current_pcb().restart_block().take();
        if let Some(mut restart_block) = restart_block {
            return restart_block.restart_fn.call(&mut restart_block.data);
        } else {
            // 不应该走到这里，因此kill掉当前进程及同组的进程
            let pid = Pid::new(0);
            let sig = Signal::SIGKILL;
            let mut info = SigInfo::new(sig, 0, SigCode::Kernel, SigType::Kill(pid));

            sig.send_signal_info(Some(&mut info), pid)
                .expect("Failed to kill ");
            return Ok(0);
        }
    }

    #[inline(never)]
    pub fn rt_sigpending(user_sigset_ptr: usize, sigsetsize: usize) -> Result<usize, SystemError> {
        if sigsetsize != size_of::<SigSet>() {
            return Err(SystemError::EINVAL);
        }

        let mut user_buffer_writer =
            UserBufferWriter::new(user_sigset_ptr as *mut SigSet, size_of::<SigSet>(), true)?;

        let pcb = ProcessManager::current_pcb();
        let siginfo_guard = pcb.sig_info_irqsave();
        let pending_set = siginfo_guard.sig_pending().signal();
        let shared_pending_set = siginfo_guard.sig_shared_pending().signal();
        let blocked_set = *siginfo_guard.sig_blocked();
        drop(siginfo_guard);

        let mut result = pending_set.union(shared_pending_set);
        result = result.difference(blocked_set);

        user_buffer_writer.copy_one_to_user(&result, 0)?;

        Ok(0)
    }
}
