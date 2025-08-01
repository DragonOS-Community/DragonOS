use core::{fmt::Debug, ptr::null, sync::atomic::Ordering};

use alloc::{collections::BTreeMap, ffi::CString, string::String, sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::process::Signal;

use crate::{
    driver::base::block::SeekFrom,
    filesystem::vfs::{
        file::{File, FileMode},
        IndexNode,
    },
    libs::elf::ELF_LOADER,
    mm::{
        ucontext::{AddressSpace, UserStack},
        VirtAddr,
    },
};

use super::{
    namespace::nsproxy::exec_task_namespaces, ProcessControlBlock, ProcessFlags, ProcessManager,
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
    file: File,
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
    pub fn new(
        file_inode: Arc<dyn IndexNode>,
        vm: Arc<AddressSpace>,
        flags: ExecParamFlags,
    ) -> Result<Self, SystemError> {
        // 读取文件头部，用于判断文件类型
        let file = File::new(file_inode, FileMode::O_RDONLY)?;

        Ok(Self {
            file,
            vm,
            flags,
            init_info: ProcInitInfo::new(ProcessManager::current_pcb().basic().name()),
        })
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

    pub fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }

    /// 获取File的所有权，用于将动态链接器加入文件描述符表中
    pub fn file(self) -> File {
        self.file
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
    ProcessManager::current_pcb()
        .sig_struct_irqsave()
        .reset_sighandlers();
    // todo: 该函数未正确实现
    let tg_empty = pcb.threads_read_irqsave().thread_group_empty();
    if tg_empty {
        pcb.exit_signal.store(Signal::SIGCHLD, Ordering::SeqCst);
        return Ok(());
    }
    log::warn!("de_thread: todo impl thread group logic");

    return Ok(());
}

/// ## 加载二进制文件
pub fn load_binary_file(param: &mut ExecParam) -> Result<BinaryLoaderResult, SystemError> {
    // 读取文件头部，用于判断文件类型
    let mut head_buf = [0u8; 512];
    param.file_mut().lseek(SeekFrom::SeekSet(0))?;
    let _bytes = param.file_mut().read(512, &mut head_buf)?;
    // debug!("load_binary_file: read {} bytes", _bytes);

    let mut loader = None;
    for bl in BINARY_LOADERS.iter() {
        let probe_result = bl.probe(param, &head_buf);
        if probe_result.is_ok() {
            loader = Some(bl);
            break;
        }
    }
    // debug!("load_binary_file: loader: {:?}", loader);
    if loader.is_none() {
        return Err(SystemError::ENOEXEC);
    }

    let loader: &&dyn BinaryLoader = loader.unwrap();
    assert!(param.vm().is_current());
    // debug!("load_binary_file: to load with param: {:?}", param);

    let result: BinaryLoaderResult = loader
        .load(param, &head_buf)
        .unwrap_or_else(|e| panic!("load_binary_file failed: error: {e:?}, param: {param:?}"));

    // debug!("load_binary_file: load success: {result:?}");
    return Ok(result);
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
