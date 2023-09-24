use core::ffi::c_void;

use alloc::{string::String, vec::Vec,sync::Arc};

use super::{fork::CloneFlags, Pid, ProcessManager, ProcessState};
use crate::{
    arch::{interrupt::TrapFrame, sched::sched, CurrentIrqArch},
    exception::InterruptArch,
    filesystem::vfs::MAX_PATHLEN,
    process::{ProcessControlBlock,TaskGroup,PROCESS_GROUP_MANAGER},
    syscall::{
        user_access::{
            check_and_clone_cstr, check_and_clone_cstr_array, UserBufferReader, UserBufferWriter,
        },
        Syscall, SystemError,
    },
};

impl Syscall {
    pub fn fork(frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let r = ProcessManager::fork(frame, CloneFlags::empty()).map(|pid| pid.into());
        return r;
    }

    pub fn vfork(frame: &mut TrapFrame) -> Result<usize, SystemError> {
        ProcessManager::fork(
            frame,
            CloneFlags::CLONE_VM | CloneFlags::CLONE_FS | CloneFlags::CLONE_SIGNAL,
        )
        .map(|pid| pid.into())
    }

    pub fn execve(
        path: *const u8,
        argv: *const *const u8,
        envp: *const *const u8,
        frame: &mut TrapFrame,
    ) -> Result<(), SystemError> {
        // kdebug!(
        //     "execve path: {:?}, argv: {:?}, envp: {:?}\n",
        //     path,
        //     argv,
        //     envp
        // );
        if path.is_null() {
            return Err(SystemError::EINVAL);
        }

        let x = || {
            let path: String = check_and_clone_cstr(path, Some(MAX_PATHLEN))?;
            let argv: Vec<String> = check_and_clone_cstr_array(argv)?;
            let envp: Vec<String> = check_and_clone_cstr_array(envp)?;
            Ok((path, argv, envp))
        };
        let r: Result<(String, Vec<String>, Vec<String>), SystemError> = x();
        if let Err(e) = r {
            panic!("Failed to execve: {:?}", e);
        }
        let (path, argv, envp) = r.unwrap();
        ProcessManager::current_pcb()
            .basic_mut()
            .set_name(ProcessControlBlock::generate_name(&path, &argv));

        return Self::do_execve(path, argv, envp, frame);
    }

    pub fn wait4(
        pid: i64,
        wstatus: *mut i32,
        options: i32,
        rusage: *mut c_void,
    ) -> Result<usize, SystemError> {
        let mut _rusage_buf =
            UserBufferReader::new::<c_void>(rusage, core::mem::size_of::<c_void>(), true)?;

        let mut wstatus_buf =
            UserBufferWriter::new::<i32>(wstatus, core::mem::size_of::<i32>(), true)?;

        // 暂时不支持options选项
        if options != 0 {
            return Err(SystemError::EINVAL);
        }

        let cur_pcb = ProcessManager::current_pcb();
        let rd_childen = cur_pcb.children.read();

        if pid > 0 {
            let pid = Pid(pid as usize);
            let child_pcb = rd_childen.get(&pid).ok_or(SystemError::ECHILD)?.clone();
            drop(rd_childen);

            // 获取退出码
            if let ProcessState::Exited(status) = child_pcb.sched_info().state() {
                if !wstatus.is_null() {
                    wstatus_buf.copy_one_to_user(&status, 0)?;
                }
                return Ok(pid.into());
            }
            // 等待指定进程
            child_pcb.wait_queue.sleep();
        } else if pid < -1 {
            // TODO 判断是否pgid == -pid（等待指定组任意进程）
            // 暂时不支持
            return Err(SystemError::EINVAL);
        } else if pid == 0 {
            // TODO 判断是否pgid == current_pgid（等待当前组任意进程）
            // 暂时不支持
            return Err(SystemError::EINVAL);
        } else {
            // 等待任意子进程(这两)
            let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
            for (pid, pcb) in rd_childen.iter() {
                if pcb.sched_info().state().is_exited() {
                    if !wstatus.is_null() {
                        wstatus_buf.copy_one_to_user(&0, 0)?;
                    }
                    return Ok(pid.clone().into());
                } else {
                    unsafe { pcb.wait_queue.sleep_without_schedule() };
                }
            }
            drop(irq_guard);
            sched();
        }

        return Ok(0);
    }

    /// # 退出进程
    ///
    /// ## 参数
    ///
    /// - status: 退出状态
    pub fn exit(status: usize) -> ! {
        ProcessManager::exit(status);
    }

    /// @brief 获取当前进程的pid
    pub fn getpid() -> Result<Pid, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        return Ok(current_pcb.pid());
    }

    /// @brief 获取指定进程的pgid
    ///
    /// @param pid 指定一个进程号
    ///
    /// @return 成功，指定进程的进程组id
    /// @return 错误，不存在该进程
    pub fn getpgid(mut pid: Pid) -> Result<Pid, SystemError> {
        if pid == Pid(0) {
            let current_pcb = ProcessManager::current_pcb();
            pid = current_pcb.pid();
        }
        let target_proc = ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;
        return Ok(target_proc.basic().pgid());
    }
    ////! todo 错误处理    
    ///!   init_group_se()
    pub fn setpgid(pid:Pid,pgid:Pid) -> Result<(),SystemError>{
        if pid == Pid(0) {
            let current_pcb = ProcessManager::current_pcb();
            pid = current_pcb.basic().pid();
        }
        let target_proc = ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;
        if pgid == 0 {
            let pgid = pid;
            PROCESS_GROUP_MANAGER.add_process(pid, pid);
            let ptg:Arc<TaskGroup> = PROCESS_GROUP_MANAGER.find(pid);
            
            let ntg=TaskGroup::new(ptg);
            // 将当前TaskGroup加入父进程组的子进程哈希表中
            
            if let Some(ppcb_arc) = ntg.parent_tg.read().upgrade() {
            let mut children = ppcb_arc.children.write();
                children.insert(pgid, ntg.clone());
            } else {
                panic!("parent tg is None");
            }
            target_proc.basic().set_pgid(pgid);
            target_proc.basic().set_tg(Some(ntg));
            
            TaskGroup::add_tg(pid, ntg);
        }else{
            let old_pgid = target_proc.basic().pgid();
            let ornewtg:bool=PROCESS_GROUP_MANAGER.set_pgid_by_pid(pid, pgid, old_pgid);
            if ornewtg ==true {
                let ptg:Arc<TaskGroup> = PROCESS_GROUP_MANAGER.find(old_pgid);
                let ntg = TaskGroup::new(ptg);
                if let Some(ppcb_arc) = ntg.parent_tg.read().upgrade() {
                    let mut children = ppcb_arc.children.write();
                        children.insert(pgid, ntg.clone());
                    } else {
                        panic!("parent tg is None");
                    }
                TaskGroup::add_tg(pgid, ntg);
                target_proc.basic().set_pgid(pgid);
                target_proc.basic().set_tg(Some(ntg));
            } else {
                target_proc.basic().set_pgid(pgid);
                let ntg:Arc<TaskGroup> = PROCESS_GROUP_MANAGER.find(pgid);
                target_proc.basic().set_tg(Some(ntg));
            }
        }
        Ok(())
    }
    
    /// @brief 获取当前进程的父进程id

    /// 若为initproc则ppid设置为0   
    pub fn getppid() -> Result<Pid, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        return Ok(current_pcb.basic().ppid());
    }
}
