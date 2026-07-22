use core::{fmt::Debug, ptr::null, sync::atomic::Ordering};

use alloc::{collections::BTreeMap, ffi::CString, string::String, sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{
    arch::ipc::signal::Signal,
    driver::base::block::SeekFrom,
    filesystem::vfs::{fcntl::AtFlags, file::File, open::do_open_execat},
    ipc::sighand::GroupExecCancelResult,
    libs::elf::ELF_LOADER,
    mm::{
        ucontext::{AddressSpace, UserStack},
        VirtAddr,
    },
};

use super::{
    namespace::nsproxy::exec_task_namespaces,
    pid::PidType,
    shebang::{ShebangLoader, SHEBANG_LOADER, SHEBANG_MAX_RECURSION_DEPTH},
    ProcessControlBlock, ProcessFlags, ProcessManager, PTRACE_RELATION_LOCK,
};

/// List of all binary format loaders supported by the system.
const BINARY_LOADERS: [&'static dyn BinaryLoader; 1] = [&ELF_LOADER];

pub trait BinaryLoader: 'static + Debug {
    /// Checks whether the binary file is in a format supported by this loader.
    fn probe(&'static self, param: &ExecParam, buf: &[u8]) -> Result<(), ExecError>;

    fn load(
        &'static self,
        param: &mut ExecParam,
        head_buf: &[u8],
    ) -> Result<BinaryLoaderResult, ExecError>;
}

/// Result of loading a binary file.
#[derive(Debug)]
pub struct BinaryLoaderResult {
    /// Program entry point address.
    entry_point: VirtAddr,
}

impl BinaryLoaderResult {
    pub fn new(entry_point: VirtAddr) -> Self {
        Self { entry_point }
    }

    pub fn entry_point(&self) -> VirtAddr {
        self.entry_point
    }
}

/// The complete result of loading a binary file.
///
/// Used to distinguish between a normal load and the need for re-execution
/// (the shebang scenario).
#[derive(Debug)]
pub enum LoadBinaryResult {
    /// Normal load completed; returns the entry point.
    Loaded(BinaryLoaderResult),
    /// Re-execution of an interpreter is needed (shebang scenario).
    NeedReexec {
        /// Start information for the next exec round.
        next: ExecStartInfo,
        /// New argv (interpreter path + [optional arg] + script path + original
        /// args).
        new_argv: Vec<CString>,
    },
}

bitflags! {
    pub struct ExecInterpFlags: u32 {
        const PATH_INACCESSIBLE = 1 << 0;
    }
}

#[derive(Debug, Clone)]
pub struct ExecStartInfo {
    file: Arc<File>,
    filename: String,
    execfn: String,
    interp_flags: ExecInterpFlags,
}

impl ExecStartInfo {
    pub fn new(
        file: Arc<File>,
        filename: String,
        execfn: String,
        interp_flags: ExecInterpFlags,
    ) -> Self {
        Self {
            file,
            filename,
            execfn,
            interp_flags,
        }
    }

    pub fn file(&self) -> Arc<File> {
        self.file.clone()
    }

    pub fn filename(&self) -> &str {
        &self.filename
    }

    pub fn execfn(&self) -> &str {
        &self.execfn
    }

    pub fn interp_flags(&self) -> ExecInterpFlags {
        self.interp_flags
    }
}

/// Execution context used to track recursive execution state.
#[derive(Debug, Clone)]
pub struct ExecContext {
    /// Current recursion depth.
    pub recursion_depth: usize,
}

impl Default for ExecContext {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecContext {
    pub fn new() -> Self {
        Self { recursion_depth: 0 }
    }

    /// Checks whether the maximum recursion depth has been exceeded.
    pub fn check_recursion_limit(&self) -> Result<(), SystemError> {
        if self.recursion_depth >= SHEBANG_MAX_RECURSION_DEPTH {
            return Err(SystemError::ELOOP);
        }
        Ok(())
    }

    pub fn increment_depth(mut self) -> Self {
        self.recursion_depth += 1;
        self
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub enum ExecError {
    SystemError(SystemError),
    /// Binary file is not executable.
    NotExecutable,
    /// Binary file is for a different architecture.
    WrongArchitecture,
    /// Insufficient access permissions.
    PermissionDenied,
    /// Unsupported operation.
    NotSupported,
    /// Error while parsing the file itself (e.g. some fields are invalid).
    ParseError,
    /// Out of memory.
    OutOfMemory,
    /// Invalid parameter.
    InvalidParemeter,
    /// Invalid address.
    BadAddress(Option<VirtAddr>),
    Other(String),
}

impl From<ExecError> for SystemError {
    fn from(val: ExecError) -> Self {
        match val {
            ExecError::SystemError(e) => e,
            ExecError::NotExecutable => SystemError::ENOEXEC,
            ExecError::WrongArchitecture => SystemError::ENOEXEC,
            ExecError::PermissionDenied => SystemError::EACCES,
            ExecError::NotSupported => SystemError::ENOSYS,
            ExecError::ParseError => SystemError::ENOEXEC,
            ExecError::OutOfMemory => SystemError::ENOMEM,
            ExecError::InvalidParemeter => SystemError::EINVAL,
            ExecError::BadAddress(_addr) => SystemError::EFAULT,
            ExecError::Other(_msg) => SystemError::ENOEXEC,
        }
    }
}

bitflags! {
    pub struct ExecParamFlags: u32 {
        // Whether to load as an executable file.
        const EXEC = 1 << 0;
    }
}

#[derive(Debug)]
pub struct ExecParam {
    file: Arc<File>,
    vm: Arc<AddressSpace>,
    /// Flags.
    flags: ExecParamFlags,
    filename: CString,
    execfn: CString,
    interp_flags: ExecInterpFlags,
    /// Information used to initialize the process. Filled jointly by the binary
    /// loader and the exec mechanism.
    init_info: ProcInitInfo,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ExecLoadMode {
    /// Load as an executable file.
    Exec,
    /// Load as a dynamic shared object.
    DSO,
}

#[allow(dead_code)]
impl ExecParam {
    /// Create an `ExecParam` using an already-opened file.
    ///
    /// ## Parameters
    /// - `file`: The executable file opened via `do_open_execat`.
    /// - `vm`: The address space.
    /// - `flags`: Execution flags.
    pub fn new(
        file: Arc<File>,
        vm: Arc<AddressSpace>,
        flags: ExecParamFlags,
        filename: CString,
        execfn: CString,
        interp_flags: ExecInterpFlags,
    ) -> Self {
        let mut init_info = ProcInitInfo::new(execfn.to_string_lossy().as_ref());
        init_info.execfn = Some(execfn.clone());
        Self {
            file,
            vm,
            flags,
            filename,
            execfn,
            interp_flags,
            init_info,
        }
    }

    pub fn vm(&self) -> &Arc<AddressSpace> {
        &self.vm
    }

    pub fn flags(&self) -> &ExecParamFlags {
        &self.flags
    }

    pub fn init_info(&self) -> &ProcInitInfo {
        &self.init_info
    }

    pub fn init_info_mut(&mut self) -> &mut ProcInitInfo {
        &mut self.init_info
    }

    /// Returns the load mode.
    pub fn load_mode(&self) -> ExecLoadMode {
        if self.flags.contains(ExecParamFlags::EXEC) {
            ExecLoadMode::Exec
        } else {
            ExecLoadMode::DSO
        }
    }

    pub fn file_ref(&self) -> &File {
        &self.file
    }

    /// Returns an `Arc` reference to the `File`.
    pub fn file(&self) -> Arc<File> {
        self.file.clone()
    }

    pub fn filename(&self) -> &CString {
        &self.filename
    }

    pub fn execfn(&self) -> &CString {
        &self.execfn
    }

    pub fn interp_flags(&self) -> ExecInterpFlags {
        self.interp_flags
    }

    /// Consume the `ExecParam` and take ownership of the `File` (used for adding
    /// the file to the file descriptor table).
    ///
    /// Panics if the `Arc` has multiple references.
    pub fn into_file(self) -> File {
        Arc::try_unwrap(self.file).expect("Cannot unwrap Arc<File>: multiple references exist")
    }

    /// Calling this is the point of no return. None of the failures will be
    /// seen by userspace since either the process is already taking a fatal
    /// signal.
    ///
    /// https://code.dragonos.org.cn/xref/linux-6.6.21/fs/exec.c#1246
    pub fn begin_new_exec(&mut self) -> Result<(), ExecError> {
        let me = ProcessManager::current_pcb();
        // TODO: Implement the remaining Linux logic.
        de_thread(&me).map_err(ExecError::SystemError)?;

        me.flags().remove(ProcessFlags::FORKNOEXEC);

        exec_task_namespaces().map_err(ExecError::SystemError)?;
        Ok(())
    }

    /// https://code.dragonos.org.cn/xref/linux-6.6.21/fs/exec.c?fi=setup_new_exec#1443
    pub fn setup_new_exec(&mut self) {
        // todo!("setup_new_exec logic");
    }
}

/// https://code.dragonos.org.cn/xref/linux-6.6.21/fs/exec.c#1044
fn de_thread(pcb: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
    let current = ProcessManager::current_pcb();
    if !Arc::ptr_eq(&current, pcb) {
        return Err(SystemError::EINVAL);
    }

    let sighand = current.sighand();
    let leader = {
        let ti = current.threads_read_irqsave();
        ti.group_leader().unwrap_or_else(|| current.clone())
    };
    let old_leader = (!Arc::ptr_eq(&leader, &current)).then_some(leader.clone());

    // Collection and per-task token assignment share SigHand::inner with the
    // unhash-tail completion path. An already exiting/ptraced zombie sibling
    // remains pending until it has stopped touching identity-bearing lists.
    let (kill_list, invalid_old_leader) =
        sighand.start_group_exec_transaction(&current, old_leader.as_ref(), |generation| {
            let invalid_old_leader = old_leader
                .as_ref()
                .map(|old| old.is_dead())
                .unwrap_or(false);
            let mut kill_list = Vec::new();
            let mut pending = 0;

            if let Some(old) = old_leader.as_ref() {
                if !old.flags().contains(ProcessFlags::EXITING)
                    && !old.is_exited()
                    && !old.is_zombie()
                    && !old.is_dead()
                {
                    kill_list.push(old.clone());
                }
            }

            let ti = leader.threads_read_irqsave();
            for weak in &ti.group_tasks {
                let Some(task) = weak.upgrade() else {
                    continue;
                };
                if Arc::ptr_eq(&task, &current)
                    || old_leader
                        .as_ref()
                        .map(|old| Arc::ptr_eq(old, &task))
                        .unwrap_or(false)
                {
                    continue;
                }
                if !task.identity_unhash_complete() {
                    task.assign_group_exec_generation(generation);
                    pending += 1;
                }
                if !task.flags().contains(ProcessFlags::EXITING)
                    && !task.is_exited()
                    && !task.is_zombie()
                    && !task.is_dead()
                {
                    kill_list.push(task);
                }
            }
            ((kill_list, invalid_old_leader), pending)
        })?;

    if invalid_old_leader {
        match sighand.try_cancel_group_exec(&current) {
            GroupExecCancelResult::Canceled => {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            GroupExecCancelResult::Committed => {
                panic!("invalid old leader reached committed group-exec state");
            }
            GroupExecCancelResult::NotOwner => {
                panic!("group-exec ownership disappeared during cleanup");
            }
        }
    }

    for task in kill_list {
        let _ = Signal::SIGKILL.send_signal_info_to_pcb(None, task, PidType::PID);
    }

    let pending_wait = sighand.wait_group_exec_event_killable(
        || Signal::fatal_signal_pending(&current) || sighand.group_exec_pending_complete(&current),
        None::<fn()>,
    );
    if pending_wait.is_err() || Signal::fatal_signal_pending(&current) {
        match sighand.try_cancel_group_exec(&current) {
            GroupExecCancelResult::Canceled | GroupExecCancelResult::NotOwner => {
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            GroupExecCancelResult::Committed => {
                sighand
                    .wait_group_exec_event_uninterruptible(
                        || sighand.group_exec_handoff_ready(&current),
                        None::<fn()>,
                    )
                    .expect("uninterruptible group-exec wait failed");
            }
        }
    }

    if let Some(leader) = old_leader.as_ref() {
        if !sighand.group_exec_handoff_ready(&current) {
            let leader_wait = sighand.wait_group_exec_event_killable(
                || {
                    Signal::fatal_signal_pending(&current)
                        || sighand.group_exec_handoff_ready(&current)
                },
                None::<fn()>,
            );
            if leader_wait.is_err() || Signal::fatal_signal_pending(&current) {
                match sighand.try_cancel_group_exec(&current) {
                    GroupExecCancelResult::Canceled | GroupExecCancelResult::NotOwner => {
                        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                    }
                    GroupExecCancelResult::Committed => {
                        sighand
                            .wait_group_exec_event_uninterruptible(
                                || sighand.group_exec_handoff_ready(&current),
                                None::<fn()>,
                            )
                            .expect("uninterruptible group-exec wait failed");
                    }
                }
            }
        }

        assert!(
            sighand.group_exec_handoff_ready(&current),
            "committed group-exec handoff is not ready"
        );
        assert!(leader.is_zombie(), "old exec leader must be zombie");

        ProcessManager::exchange_tid_and_raw_pids(&current, leader);

        current
            .exit_signal
            .store(Signal::SIGCHLD as i32, Ordering::SeqCst);
        leader.exit_signal.store(-1, Ordering::SeqCst);

        // Promote the current thread to thread-group leader and clear
        // group_tasks (no other threads remain).
        {
            let mut cur_ti = current.threads_write_irqsave();
            cur_ti.group_leader = Arc::downgrade(&current);
            cur_ti.group_tasks.clear();
        }
        {
            let mut leader_ti = leader.threads_write_irqsave();
            leader_ti.group_leader = Arc::downgrade(&current);
            leader_ti.group_tasks.clear();
        }

        // Transfer the old leader's children list to the new leader.
        let moved_children = {
            let leader_pid_ns = leader.active_pid_ns();
            let (first, second) =
                if (Arc::as_ptr(&current) as usize) <= (Arc::as_ptr(leader) as usize) {
                    (current.clone(), leader.clone())
                } else {
                    (leader.clone(), current.clone())
                };

            let mut first_children = first.children.write_irqsave();
            let mut second_children = second.children.write_irqsave();

            if Arc::ptr_eq(leader, &first) {
                let moved = core::mem::take(&mut *first_children);
                second_children.extend(moved.iter().copied());
                moved
            } else {
                let moved = core::mem::take(&mut *second_children);
                first_children.extend(moved.iter().copied());
                moved
            }
            .into_iter()
            .filter_map(|pid| ProcessManager::find_task_by_pid_ns(pid, &leader_pid_ns))
            .collect::<Vec<_>>()
        };
        for child in moved_children {
            ProcessControlBlock::reparent_child_links_from_thread_group(
                &child,
                current.tgid,
                &current,
            );
        }

        // Inherit parent process relationships from the old leader.
        {
            let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
            let leader_parent = leader.real_parent_pcb.read_irqsave().clone();
            *current.parent_pcb.write_irqsave() = leader_parent.clone();
            *current.real_parent_pcb.write_irqsave() = leader_parent.clone();
            *current.wait_parent_pcb.write_irqsave() = leader_parent.clone();
            *current.fork_parent_pcb.write_irqsave() = leader_parent;
        }

        // log::info!("de_thread: reparented current to old leader's parent");

        // The old leader should be reaped by the exec thread to prevent the
        // parent from reaping it prematurely before/after the swap.
        let mut release_leader = false;
        if leader.is_zombie() {
            if leader.try_mark_dead_from_zombie() {
                release_leader = true;
            }
        } else if leader.is_dead() {
            release_leader = true;
        }

        if release_leader {
            // Remove the old PCB from ALL_PROCESS before generic release can
            // return its swapped-out TID to the PID allocator. Otherwise a
            // concurrent fork can reuse that number and a late release would
            // remove the newly published child under the same map key.
            unsafe { ProcessManager::release(leader.raw_pid()) };

            // Every DragonOS thread owns TGID/PGID/SID task links. The
            // generic release path sees the migrated old leader as a
            // non-leader and only detaches PID, so remove its remaining links
            // explicitly. `leader` remains alive through this Arc.
            leader.detach_exec_leader_non_pid_links();
        }
    } else {
        current
            .exit_signal
            .store(Signal::SIGCHLD as i32, Ordering::SeqCst);
    }

    assert!(
        sighand.finish_group_exec_owned(&current),
        "group-exec owner could not finish a completed transaction"
    );
    Ok(())
}

/// ## Load a binary file.
///
/// ## Parameters
/// - `param`: Execution parameters.
/// - `ctx`: Execution context used to track recursion depth.
///
/// ## Returns
/// - `LoadBinaryResult::Loaded`: Normal load completed.
/// - `LoadBinaryResult::NeedReexec`: Re-execution of an interpreter is needed
///   (shebang scenario).
pub fn load_binary_file_with_context(
    param: &mut ExecParam,
    ctx: &ExecContext,
) -> Result<LoadBinaryResult, SystemError> {
    // Check the recursion depth.
    ctx.check_recursion_limit()?;

    // Read the file header to determine the file type.
    let mut head_buf = [0u8; 512];
    param.file_ref().lseek(SeekFrom::SeekSet(0))?;
    let _bytes = param.file_ref().read(512, &mut head_buf)?;

    // First, check if this is a shebang script.
    if SHEBANG_LOADER.probe(param, &head_buf).is_ok() {
        // Parse the shebang line.
        let shebang_info =
            ShebangLoader::parse_shebang_line(&head_buf).map_err(|_| SystemError::ENOEXEC)?;

        if param
            .interp_flags()
            .contains(ExecInterpFlags::PATH_INACCESSIBLE)
        {
            return Err(SystemError::ENOENT);
        }

        // Open the interpreter file through the normal open path.
        let interpreter_file =
            do_open_execat(AtFlags::AT_FDCWD.bits(), &shebang_info.interpreter_path).inspect_err(
                |_e| {
                    // log::warn!(
                    //     "Shebang interpreter not found: {}, error: {:?}",
                    //     shebang_info.interpreter_path,
                    //     e
                    // );
                },
            )?;

        // Get the script path.
        let script_path = param.filename().to_string_lossy().into_owned();

        // Build the new argv.
        // Linux semantics: [interpreter, optional_arg, script_path, original_args[1:]...]
        let mut new_argv = Vec::new();

        // argv[0] = interpreter path
        new_argv.push(
            CString::new(shebang_info.interpreter_path.clone()).map_err(|_| SystemError::EINVAL)?,
        );

        // argv[1] = optional argument (if present)
        if let Some(ref arg) = shebang_info.interpreter_arg {
            new_argv.push(CString::new(arg.clone()).map_err(|_| SystemError::EINVAL)?);
        }

        // argv[N] = script path
        new_argv.push(CString::new(script_path).map_err(|_| SystemError::EINVAL)?);

        // Append the original arguments (skip argv[0], already replaced by the
        // script path).
        let original_args = &param.init_info().args;
        if original_args.len() > 1 {
            new_argv.extend(original_args[1..].iter().cloned());
        }

        return Ok(LoadBinaryResult::NeedReexec {
            next: ExecStartInfo::new(
                interpreter_file,
                shebang_info.interpreter_path.clone(),
                param.execfn().to_string_lossy().into_owned(),
                ExecInterpFlags::empty(),
            ),
            new_argv,
        });
    }

    // Then try other loaders (ELF, etc.)
    let mut loader = None;
    for bl in BINARY_LOADERS.iter() {
        let probe_result = bl.probe(param, &head_buf);
        if probe_result.is_ok() {
            loader = Some(bl);
            break;
        }
    }

    if loader.is_none() {
        return Err(SystemError::ENOEXEC);
    }

    let loader: &&dyn BinaryLoader = loader.unwrap();
    assert!(param.vm().is_current());

    let result: BinaryLoaderResult = loader.load(param, &head_buf).map_err(SystemError::from)?;

    Ok(LoadBinaryResult::Loaded(result))
}

/// Program initialization information. These values are pushed onto the user
/// stack.
#[derive(Debug)]
pub struct ProcInitInfo {
    pub proc_name: CString,
    pub args: Vec<CString>,
    pub envs: Vec<CString>,
    pub auxv: BTreeMap<u8, usize>,
    pub execfn: Option<CString>,
    pub rand_num: [u8; 16],
}

impl ProcInitInfo {
    pub fn new(proc_name: &str) -> Self {
        Self {
            proc_name: CString::new(proc_name).unwrap_or(CString::new("").unwrap()),
            args: Vec::new(),
            envs: Vec::new(),
            auxv: BTreeMap::new(),
            execfn: None,
            rand_num: [0u8; 16],
        }
    }

    /// Push the program initialization information onto the user stack.
    /// This function pushes arguments, environment variables, auxv, etc. onto
    /// the user stack.
    ///
    /// ## Returns
    ///
    /// A tuple where the first element is the final user stack pointer and the
    /// second element is the starting address of the `envp` pointer array.
    pub unsafe fn push_at(
        &mut self,
        ustack: &mut UserStack,
    ) -> Result<(VirtAddr, VirtAddr), SystemError> {
        // First, push the program name onto the stack.
        self.push_str(ustack, &self.proc_name)?;

        // Then push the environment variables onto the stack.
        let envps = self
            .envs
            .iter()
            .map(|s| {
                self.push_str(ustack, s).expect("push_str failed");
                ustack.sp()
            })
            .collect::<Vec<_>>();

        // Then push the arguments onto the stack.
        let argps = self
            .args
            .iter()
            .map(|s| {
                self.push_str(ustack, s).expect("push_str failed");
                ustack.sp()
            })
            .collect::<Vec<_>>();

        // Push the random number and store its pointer in auxv.
        self.push_slice(ustack, &[self.rand_num])?;
        self.auxv
            .insert(super::abi::AtType::Random as u8, ustack.sp().data());

        if let Some(execfn) = self.execfn.as_ref() {
            self.push_str(ustack, execfn)?;
            self.auxv
                .insert(super::abi::AtType::ExecFn as u8, ustack.sp().data());
        }

        // Align the stack to 16 bytes.
        // The remaining content to push is all `usize` width:
        // - auxv terminator: 2 words
        // - auxv entries: 2 words / entry
        // - envp NULL: 1 word
        // - envp pointer array: envps.len() words
        // - argv NULL: 1 word
        // - argv pointer array: argps.len() words
        // - argc: 1 word
        let length_to_push =
            self.remaining_stack_words(envps.len(), argps.len()) * core::mem::size_of::<usize>();
        self.push_slice(
            ustack,
            &vec![0u8; (ustack.sp().data() - length_to_push) & 0xF],
        )?;

        // Push auxv.
        self.push_slice(ustack, &[null::<u8>(), null::<u8>()])?;
        for (&k, &v) in self.auxv.iter() {
            self.push_slice(ustack, &[k as usize, v])?;
        }

        // Push the environment variable pointers onto the stack.
        self.push_slice(ustack, &[null::<u8>()])?;
        self.push_slice(ustack, envps.as_slice())?;

        // Push the argument pointers onto the stack.
        self.push_slice(ustack, &[null::<u8>()])?;
        self.push_slice(ustack, argps.as_slice())?;
        let argv_ptr = ustack.sp();

        // Push argc onto the stack.
        self.push_slice(ustack, &[self.args.len()])?;

        return Ok((ustack.sp(), argv_ptr));
    }

    fn remaining_stack_words(&self, envc: usize, argc: usize) -> usize {
        let aux_words = 2 + self.auxv.len() * 2;
        let env_words = 1 + envc;
        let argv_words = 1 + argc;
        let argc_words = 1;
        aux_words + env_words + argv_words + argc_words
    }

    fn push_slice<T: Copy>(&self, ustack: &mut UserStack, slice: &[T]) -> Result<(), SystemError> {
        let mut sp = ustack.sp();
        sp -= core::mem::size_of_val(slice);
        sp -= sp.data() % core::mem::align_of::<T>();

        unsafe { core::slice::from_raw_parts_mut(sp.data() as *mut T, slice.len()) }
            .copy_from_slice(slice);
        unsafe {
            ustack.set_sp(sp);
        }

        return Ok(());
    }

    fn push_str(&self, ustack: &mut UserStack, s: &CString) -> Result<(), SystemError> {
        let bytes = s.as_bytes_with_nul();
        self.push_slice(ustack, bytes)?;
        return Ok(());
    }
}
