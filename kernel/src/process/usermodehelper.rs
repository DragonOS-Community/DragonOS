//! Start a trusted userspace helper without inheriting the requesting task's
//! credentials, mount namespace, root directory, or file descriptors.
//!
//! The supervisor is a child of kthreadd. It creates the userspace task as
//! its own child and waits for it, so a successful exec cannot escape the
//! existing wait/reap path merely by dropping the KTHREAD flag.

use alloc::{boxed::Box, ffi::CString, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, ipc::signal::Signal},
    filesystem::{
        fs::FsStruct,
        vfs::fdtable::{FdTableState, FileDescriptorTable},
    },
    ipc::signal_types::Sigaction,
    libs::spinlock::SpinLock,
    process::{
        cred::Cred,
        execve::do_execve,
        fork::CloneFlags,
        kthread::{KernelThreadClosure, KernelThreadCreateInfo, KernelThreadMechanism},
        namespace::mnt::root_mnt_namespace,
        ProcessFlags, ProcessManager,
    },
    sched::completion::Completion,
};

struct HelperResult {
    done: Completion,
    status: SpinLock<Option<Result<i32, SystemError>>>,
}

impl HelperResult {
    fn new() -> Self {
        Self {
            done: Completion::new(),
            status: SpinLock::new(None),
        }
    }

    fn complete(&self, result: Result<i32, SystemError>) {
        *self.status.lock() = Some(result);
        self.done.complete_all();
    }
}

/// A running helper. Dropping the handle does not terminate it: the supervisor
/// retains the completion state and always waits for its child.
pub struct UserModeHelper {
    result: Arc<HelperResult>,
}

type HelperExit = Box<dyn FnOnce(&Result<i32, SystemError>) + Send>;

impl UserModeHelper {
    pub fn start(
        path: String,
        argv: Vec<CString>,
        envp: Vec<CString>,
    ) -> Result<Self, SystemError> {
        Self::start_with_context(path, argv, envp, None, None)
    }

    /// Start a helper with credentials prepared by a trusted kernel caller.
    /// The optional completion runs in the supervisor after the child has
    /// been reaped, even if its exec fails.  The requester need not wait for
    /// that point to learn that a request-key construction was instantiated.
    pub(crate) fn start_with_context(
        path: String,
        argv: Vec<CString>,
        envp: Vec<CString>,
        cred: Option<Arc<Cred>>,
        on_exit: Option<HelperExit>,
    ) -> Result<Self, SystemError> {
        // This architecture still has no kthread bootstrap or user switch.
        // Refuse the request instead of reaching either platform todo!().
        if cfg!(target_arch = "loongarch64") {
            return Err(SystemError::ENOSYS);
        }
        if path.is_empty() || argv.is_empty() {
            return Err(SystemError::EINVAL);
        }

        let result = Arc::try_new(HelperResult::new()).map_err(|_| SystemError::ENOMEM)?;
        let worker_result = result.clone();
        let args = SpinLock::new(Some((path, argv, envp, cred, on_exit)));
        let closure = KernelThreadClosure::EmptyClosure((
            Box::new(move || {
                let (path, argv, envp, cred, on_exit) =
                    args.lock().take().expect("helper closure ran twice");
                let status = run_supervisor(path, argv, envp, cred);
                if let Some(callback) = on_exit {
                    callback(&status);
                }
                worker_result.complete(status);
                0
            }),
            (),
        ));
        let supervisor = KernelThreadMechanism::create(closure, "usermode-helper".into())
            .ok_or(SystemError::ENOMEM)?;
        // create() publishes a stopped kthread; it cannot have exited yet.
        ProcessManager::wakeup(&supervisor)?;
        Ok(Self { result })
    }

    /// Wait for the helper process's raw wait(2) status. Fatal signals may
    /// interrupt the requester, but the supervisor continues reaping.
    pub fn wait(&self) -> Result<i32, SystemError> {
        self.result.done.wait_for_completion_killable()?;
        self.result
            .status
            .lock()
            .as_ref()
            .cloned()
            .ok_or(SystemError::EIO)?
    }
}

fn run_supervisor(
    path: String,
    argv: Vec<CString>,
    envp: Vec<CString>,
    cred: Option<Arc<Cred>>,
) -> Result<i32, SystemError> {
    let parent = ProcessManager::current_pcb();
    // A real SIGCHLD disposition (without SA_NOCLDWAIT) is required for
    // kernel_wait4() to own and reap the helper's zombie.
    parent
        .sighand()
        .set_handler(Signal::SIGCHLD, Sigaction::default());

    let exec_error = Arc::try_new(SpinLock::new(None)).map_err(|_| SystemError::ENOMEM)?;
    let child_error = exec_error.clone();
    let closure = KernelThreadClosure::UserMode(Box::new(move || {
        run_child_prepare(path, argv, envp, cred, child_error)
    }));
    let info = KernelThreadCreateInfo::new(closure, "usermode-helper-exec".into());
    // Calling create() here would reparent the child to kthreadd. The direct
    // creation keeps this supervisor as the wait(2) parent.
    KernelThreadMechanism::__inner_create(&info, CloneFlags::CLONE_VM)?;
    let child = info.poll_result().ok_or(SystemError::ENOMEM)?;
    let child_pid = child.raw_pid().data() as i32;
    let wake_result = ProcessManager::wakeup(&child);
    drop(child);

    // A child can exit after publication but before this wake (for example,
    // following a fatal signal). Even then, its parent must consume the
    // zombie before reporting the wake error.
    let (_, status) = crate::process::exit::kernel_wait4_uninterruptible(child_pid)?;
    wake_result?;
    if let Some(error) = exec_error.lock().take() {
        return Err(error);
    }
    Ok(status)
}

fn run_child_prepare(
    path: String,
    argv: Vec<CString>,
    envp: Vec<CString>,
    cred: Option<Arc<Cred>>,
    exec_error: Arc<SpinLock<Option<SystemError>>>,
) -> Result<TrapFrame, SystemError> {
    let child = ProcessManager::current_pcb();
    if let Some(cred) = cred {
        child.install_cred(cred);
    }
    let fd_table = match Arc::try_new(FileDescriptorTable::new(FdTableState::new())) {
        Ok(table) => table,
        Err(_) => {
            *exec_error.lock() = Some(SystemError::ENOMEM);
            return Err(SystemError::ENOMEM);
        }
    };
    let old_table = child.basic_mut().set_fd_table(Some(fd_table));
    drop(old_table);

    // The child has a private fs_struct because it was cloned without
    // CLONE_FS. Never resolve /sbin/request-key through the caller's chroot.
    let root = root_mnt_namespace().root_inode();
    let fs: Arc<FsStruct> = child.fs_struct();
    fs.set_root(root.clone());
    fs.set_pwd(root);
    drop(fs);

    let mut frame = TrapFrame::new();
    child.flags().remove(ProcessFlags::KTHREAD);
    drop(child);
    match do_execve(&path, argv, envp, &mut frame) {
        Ok(()) => Ok(frame),
        Err(error) => {
            // The kthread bootstrap still owns the closure's return path.
            ProcessManager::current_pcb()
                .flags()
                .insert(ProcessFlags::KTHREAD);
            *exec_error.lock() = Some(error.clone());
            Err(error)
        }
    }
}

/// A bounded in-guest check of the real exec, error, and wait/reap paths.
/// Invoked only by the root-readable debugfs selftest file.
pub(crate) fn run_debug_selftest() -> Result<String, SystemError> {
    let callback_seen = Arc::new(AtomicBool::new(false));
    let callback_flag = callback_seen.clone();
    let ok = UserModeHelper::start_with_context(
        "/bin/busybox".into(),
        vec![
            CString::new("busybox").unwrap(),
            CString::new("true").unwrap(),
        ],
        Vec::new(),
        None,
        Some(Box::new(move |result| {
            if matches!(result, Ok(0)) {
                callback_flag.store(true, Ordering::Release);
            }
        })),
    )?
    .wait()?;
    if ok != 0 || !callback_seen.load(Ordering::Acquire) {
        return Err(SystemError::EIO);
    }

    let failed_exec = UserModeHelper::start(
        "/no/such/usermode-helper".into(),
        vec![CString::new("missing").unwrap()],
        Vec::new(),
    )?
    .wait();
    if !matches!(failed_exec, Err(SystemError::ENOENT)) {
        return Err(SystemError::EIO);
    }
    Ok("exec=ok\nmissing_exec=enoent\nwait_reap=ok\n".into())
}
