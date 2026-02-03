use core::{fmt::Debug, ptr::null, sync::atomic::Ordering};

use alloc::{collections::BTreeMap, ffi::CString, string::String, sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::process::Signal;

use crate::{
    driver::base::block::SeekFrom,
    filesystem::vfs::{fcntl::AtFlags, file::File, open::do_open_execat},
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
    ProcessControlBlock, ProcessFlags, ProcessManager,
};

/// 系统支持的所有二进制文件加载器的列表
const BINARY_LOADERS: [&'static dyn BinaryLoader; 1] = [&ELF_LOADER];

pub trait BinaryLoader: 'static + Debug {
    /// 检查二进制文件是否为当前加载器支持的格式
    fn probe(&'static self, param: &ExecParam, buf: &[u8]) -> Result<(), ExecError>;

    fn load(
        &'static self,
        param: &mut ExecParam,
        head_buf: &[u8],
    ) -> Result<BinaryLoaderResult, ExecError>;
}

/// 二进制文件加载结果
#[derive(Debug)]
pub struct BinaryLoaderResult {
    /// 程序入口地址
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

/// 二进制文件加载的完整结果
///
/// 用于区分正常加载和需要重新执行(shebang场景)的情况
#[derive(Debug)]
pub enum LoadBinaryResult {
    /// 正常加载完成，返回入口点
    Loaded(BinaryLoaderResult),
    /// 需要重新执行解释器 (shebang场景)
    NeedReexec {
        /// 解释器文件（通过do_open_execat打开）
        interpreter_file: Arc<File>,
        /// 新的argv (解释器路径 + [可选参数] + 脚本路径 + 原始参数)
        new_argv: Vec<CString>,
    },
}

/// 执行上下文，用于跟踪递归执行状态
#[derive(Debug, Clone)]
pub struct ExecContext {
    /// 当前递归深度
    pub recursion_depth: usize,
    /// 原始脚本路径 (用于argv)
    pub original_path: Option<String>,
}

impl Default for ExecContext {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecContext {
    pub fn new() -> Self {
        Self {
            recursion_depth: 0,
            original_path: None,
        }
    }

    /// 检查是否超过最大递归深度
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
    /// 二进制文件不可执行
    NotExecutable,
    /// 二进制文件不是当前架构的
    WrongArchitecture,
    /// 访问权限不足
    PermissionDenied,
    /// 不支持的操作
    NotSupported,
    /// 解析文件本身的时候出现错误（比如一些字段本身不合法）
    ParseError,
    /// 内存不足
    OutOfMemory,
    /// 参数错误
    InvalidParemeter,
    /// 无效的地址
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
        // 是否以可执行文件的形式加载
        const EXEC = 1 << 0;
    }
}

#[derive(Debug)]
pub struct ExecParam {
    file: Arc<File>,
    vm: Arc<AddressSpace>,
    /// 一些标志位
    flags: ExecParamFlags,
    /// 用来初始化进程的一些信息。这些信息由二进制加载器和exec机制来共同填充
    init_info: ProcInitInfo,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ExecLoadMode {
    /// 以可执行文件的形式加载
    Exec,
    /// 以动态链接库的形式加载
    DSO,
}

#[allow(dead_code)]
impl ExecParam {
    /// 使用已打开的文件创建ExecParam
    ///
    /// ## 参数
    /// - `file`: 通过do_open_execat打开的可执行文件
    /// - `vm`: 地址空间
    /// - `flags`: 执行标志
    pub fn new(file: Arc<File>, vm: Arc<AddressSpace>, flags: ExecParamFlags) -> Self {
        Self {
            file,
            vm,
            flags,
            init_info: ProcInitInfo::new(ProcessManager::current_pcb().basic().name()),
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

    /// 获取加载模式
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

    /// 获取File的Arc引用
    pub fn file(&self) -> Arc<File> {
        self.file.clone()
    }

    /// 消费ExecParam并获取File的所有权（用于将文件加入文件描述符表）
    ///
    /// 如果Arc有多个引用，会panic
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
        // todo: 补充linux的逻辑
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
    // 与 group-exit/并发 exec 互斥
    sighand.start_group_exec(&current)?;

    let result = (|| {
        // log::info!(
        //     "de_thread: start pid={:?} tgid={:?}",
        //     current.raw_pid(),
        //     current.raw_tgid()
        // );

        if current.threads_read_irqsave().thread_group_empty() {
            // log::info!(
            //     "de_thread: single-thread fast path pid={:?}",
            //     current.raw_pid()
            // );
            current.exit_signal.store(Signal::SIGCHLD, Ordering::SeqCst);
            return Ok(());
        }

        let leader = {
            let ti = current.threads_read_irqsave();
            ti.group_leader().unwrap_or_else(|| current.clone())
        };

        // log::info!(
        //     "de_thread: leader pid={:?}, current_is_leader={}",
        //     leader.raw_pid(),
        //     Arc::ptr_eq(&leader, &current)
        // );

        let mut kill_list: Vec<Arc<ProcessControlBlock>> = Vec::new();
        let mut leader_in_kill_list = false;
        if !Arc::ptr_eq(&leader, &current)
            && !leader.flags().contains(ProcessFlags::EXITING)
            && !leader.is_exited()
            && !leader.is_zombie()
            && !leader.is_dead()
        {
            kill_list.push(leader.clone());
            leader_in_kill_list = true;
        }
        {
            let ti = leader.threads_read_irqsave();
            for weak in &ti.group_tasks {
                if let Some(task) = weak.upgrade() {
                    if Arc::ptr_eq(&task, &current) {
                        continue;
                    }
                    if task.flags().contains(ProcessFlags::EXITING)
                        || task.is_exited()
                        || task.is_zombie()
                        || task.is_dead()
                    {
                        continue;
                    }
                    kill_list.push(task);
                }
            }
        }

        let mut notify_count = kill_list.len() as isize;
        // 非 leader exec：notify_count 不包含 leader 本身
        if !Arc::ptr_eq(&leader, &current) && leader_in_kill_list {
            notify_count -= 1;
        }
        sighand.set_group_exec_notify_count(notify_count);

        for task in kill_list {
            let _ = Signal::SIGKILL.send_signal_info_to_pcb(None, task, PidType::PID);
        }

        // 先等待除 leader 外的线程退出（notify_count == 0）
        let wait_res = sighand.wait_group_exec_event_killable(
            || {
                if Signal::fatal_signal_pending(&current) {
                    return true;
                }
                sighand.group_exec_notify_count() == 0
            },
            None::<fn()>,
        );
        if wait_res.is_err() || Signal::fatal_signal_pending(&current) {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        // 非 leader exec：等待 leader 进入 zombie 后再交换 tid/raw_pid
        if !Arc::ptr_eq(&leader, &current) {
            // 标记等待 leader 退出（notify_count < 0），由 exit_notify 唤醒
            if !leader.is_zombie() && !leader.is_dead() {
                sighand.set_group_exec_notify_count(-1);
                let wait_res = sighand.wait_group_exec_event_killable(
                    || {
                        if Signal::fatal_signal_pending(&current) {
                            return true;
                        }
                        leader.is_zombie() || leader.is_dead()
                    },
                    None::<fn()>,
                );
                if wait_res.is_err() || Signal::fatal_signal_pending(&current) {
                    return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                }
            }

            ProcessManager::exchange_tid_and_raw_pids(&current, &leader)?;

            current.exit_signal.store(Signal::SIGCHLD, Ordering::SeqCst);
            leader.exit_signal.store(Signal::INVALID, Ordering::SeqCst);

            // 将当前线程提升为线程组 leader，并清空 group_tasks（已无其他线程）
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

            // 将旧 leader 的 children 列表转移给新 leader
            {
                let (first, second) =
                    if (Arc::as_ptr(&current) as usize) <= (Arc::as_ptr(&leader) as usize) {
                        (current.clone(), leader.clone())
                    } else {
                        (leader.clone(), current.clone())
                    };

                let mut first_children = first.children.write_irqsave();
                let mut second_children = second.children.write_irqsave();

                if Arc::ptr_eq(&leader, &first) {
                    let moved = core::mem::take(&mut *first_children);
                    second_children.extend(moved);
                } else {
                    let moved = core::mem::take(&mut *second_children);
                    first_children.extend(moved);
                }
            }

            // 继承 old leader 的父进程关系
            let leader_parent = leader.real_parent_pcb.read().clone();
            *current.parent_pcb.write() = leader_parent.clone();
            *current.real_parent_pcb.write() = leader_parent;
            *current.fork_parent_pcb.write() = current.self_ref.clone();

            // log::info!("de_thread: reparented current to old leader's parent");

            // 补做旧 leader 在退出阶段延迟的 PID/TGID/PGID/SID unhash
            leader.finish_deferred_unhash_for_exec();

            // 旧 leader 应由 exec 线程回收，避免父进程在交换前/后提前回收
            if leader.is_zombie() {
                if leader.try_mark_dead_from_zombie() {
                    unsafe { ProcessManager::release(leader.raw_pid()) };
                }
            } else if leader.is_dead() {
                unsafe { ProcessManager::release(leader.raw_pid()) };
            }
        } else {
            current.exit_signal.store(Signal::SIGCHLD, Ordering::SeqCst);
        }

        // log::info!("de_thread: done pid={:?}", current.raw_pid());
        Ok(())
    })();

    sighand.finish_group_exec();
    result
}

/// ## 加载二进制文件
///
///
/// ## 参数
/// - `param`: 执行参数
/// - `ctx`: 执行上下文，用于跟踪递归深度
///
/// ## 返回值
/// - `LoadBinaryResult::Loaded`: 正常加载完成
/// - `LoadBinaryResult::NeedReexec`: 需要递归执行解释器（shebang场景）
pub fn load_binary_file_with_context(
    param: &mut ExecParam,
    ctx: &ExecContext,
) -> Result<LoadBinaryResult, SystemError> {
    // 检查递归深度
    ctx.check_recursion_limit()?;

    // 读取文件头部，用于判断文件类型
    let mut head_buf = [0u8; 512];
    param.file_ref().lseek(SeekFrom::SeekSet(0))?;
    let _bytes = param.file_ref().read(512, &mut head_buf)?;

    // 首先检查是否为shebang脚本
    if SHEBANG_LOADER.probe(param, &head_buf).is_ok() {
        // 解析shebang行
        let shebang_info =
            ShebangLoader::parse_shebang_line(&head_buf).map_err(|_| SystemError::ENOEXEC)?;

        // 通过正常的open流程打开解释器文件
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

        // 获取脚本路径
        let script_path = param
            .file_ref()
            .inode()
            .absolute_path()
            .unwrap_or_else(|_| ctx.original_path.clone().unwrap_or_default());

        // 构建新的argv
        // Linux语义: [interpreter, optional_arg, script_path, original_args[1:]...]
        let mut new_argv = Vec::new();

        // argv[0] = 解释器路径
        new_argv.push(
            CString::new(shebang_info.interpreter_path.clone()).map_err(|_| SystemError::EINVAL)?,
        );

        // argv[1] = 可选参数 (如果存在)
        if let Some(ref arg) = shebang_info.interpreter_arg {
            new_argv.push(CString::new(arg.clone()).map_err(|_| SystemError::EINVAL)?);
        }

        // argv[N] = 脚本路径
        new_argv.push(CString::new(script_path).map_err(|_| SystemError::EINVAL)?);

        // 追加原始参数 (跳过argv[0]，因为已经用脚本路径替换)
        let original_args = &param.init_info().args;
        if original_args.len() > 1 {
            new_argv.extend(original_args[1..].iter().cloned());
        }

        return Ok(LoadBinaryResult::NeedReexec {
            interpreter_file,
            new_argv,
        });
    }

    // 然后尝试其他加载器 (ELF等)
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

/// 程序初始化信息，这些信息会被压入用户栈中
#[derive(Debug)]
pub struct ProcInitInfo {
    pub proc_name: CString,
    pub args: Vec<CString>,
    pub envs: Vec<CString>,
    pub auxv: BTreeMap<u8, usize>,
    pub rand_num: [u8; 16],
}

impl ProcInitInfo {
    pub fn new(proc_name: &str) -> Self {
        Self {
            proc_name: CString::new(proc_name).unwrap_or(CString::new("").unwrap()),
            args: Vec::new(),
            envs: Vec::new(),
            auxv: BTreeMap::new(),
            rand_num: [0u8; 16],
        }
    }

    /// 把程序初始化信息压入用户栈中
    /// 这个函数会把参数、环境变量、auxv等信息压入用户栈中
    ///
    /// ## 返回值
    ///
    /// 返回值是一个元组，第一个元素是最终的用户栈顶地址，第二个元素是环境变量pointer数组的起始地址     
    pub unsafe fn push_at(
        &mut self,
        ustack: &mut UserStack,
    ) -> Result<(VirtAddr, VirtAddr), SystemError> {
        // 先把程序的名称压入栈中
        self.push_str(ustack, &self.proc_name)?;

        // 然后把环境变量压入栈中
        let envps = self
            .envs
            .iter()
            .map(|s| {
                self.push_str(ustack, s).expect("push_str failed");
                ustack.sp()
            })
            .collect::<Vec<_>>();

        // 然后把参数压入栈中
        let argps = self
            .args
            .iter()
            .map(|s| {
                self.push_str(ustack, s).expect("push_str failed");
                ustack.sp()
            })
            .collect::<Vec<_>>();

        // 压入随机数，把指针放入auxv
        self.push_slice(ustack, &[self.rand_num])?;
        self.auxv
            .insert(super::abi::AtType::Random as u8, ustack.sp().data());

        // 实现栈的16字节对齐
        // 用当前栈顶地址减去后续要压栈的长度，得到的压栈后的栈顶地址与0xF按位与操作得到对齐要填充的字节数
        let length_to_push = (self.auxv.len() + envps.len() + 1 + argps.len() + 1 + 1)
            * core::mem::align_of::<usize>();
        self.push_slice(
            ustack,
            &vec![0u8; (ustack.sp().data() - length_to_push) & 0xF],
        )?;

        // 压入auxv
        self.push_slice(ustack, &[null::<u8>(), null::<u8>()])?;
        for (&k, &v) in self.auxv.iter() {
            self.push_slice(ustack, &[k as usize, v])?;
        }

        // 把环境变量指针压入栈中
        self.push_slice(ustack, &[null::<u8>()])?;
        self.push_slice(ustack, envps.as_slice())?;

        // 把参数指针压入栈中
        self.push_slice(ustack, &[null::<u8>()])?;
        self.push_slice(ustack, argps.as_slice())?;
        let argv_ptr = ustack.sp();

        // 把argc压入栈中
        self.push_slice(ustack, &[self.args.len()])?;

        return Ok((ustack.sp(), argv_ptr));
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
