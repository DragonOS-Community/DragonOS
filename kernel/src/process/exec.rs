use alloc::{collections::BTreeMap, string::String, sync::Arc, vec::Vec};

use crate::{
    filesystem::vfs::{
        file::{File, FileMode},
        IndexNode, ROOT_INODE,
    },
    libs::{elf::ELF_LOADER, rwlock::RwLock},
    mm::{
        ucontext::{AddressSpace, InnerAddressSpace},
        VirtAddr,
    },
    syscall::SystemError,
};

/// 系统支持的所有二进制文件加载器的列表
const BINARY_LOADERS: [&'static dyn BinaryLoader; 1] = [&ELF_LOADER];

pub trait BinaryLoader: 'static {
    /// 检查二进制文件是否为当前加载器支持的格式
    fn probe(self: &'static Self, param: &ExecParam, buf: &[u8]) -> Result<(), ExecError>;

    fn load(
        self: &'static Self,
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

#[derive(Debug)]
pub enum ExecError {
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
    BadAddress(VirtAddr),
}

impl Into<SystemError> for ExecError {
    fn into(self) -> SystemError {
        match self {
            ExecError::NotExecutable => SystemError::ENOEXEC,
            ExecError::WrongArchitecture => SystemError::EOPNOTSUPP_OR_ENOTSUP,
            ExecError::PermissionDenied => SystemError::EACCES,
            ExecError::NotSupported => SystemError::EOPNOTSUPP_OR_ENOTSUP,
            ExecError::ParseError => SystemError::ENOEXEC,
            ExecError::OutOfMemory => SystemError::ENOMEM,
            ExecError::InvalidParemeter => SystemError::EINVAL,
            ExecError::BadAddress(addr) => SystemError::EFAULT,
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
pub struct ExecParam<'a> {
    file_path: &'a str,
    file: Option<File>,
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

impl<'a> ExecParam<'a> {
    pub fn new(file_path: &'a str, vm: Arc<AddressSpace>, flags: ExecParamFlags) -> Self {
        Self {
            file_path,
            file: None,
            vm,
            flags,
            init_info: ProcInitInfo::new(),
        }
    }

    pub fn file_path(&self) -> &'a str {
        self.file_path
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
        self.file.as_mut().unwrap()
    }
}

/// ## 加载二进制文件
pub fn load_binary_file(param: &mut ExecParam) -> Result<(), SystemError> {
    let inode = ROOT_INODE().lookup(param.file_path)?;

    let file = File::new(inode, FileMode::O_RDONLY)?;
    param.file = Some(file);
    let mut head_buf = [0u8; 256];
    let _bytes = param.file_mut().read(256, &mut head_buf)?;

    let mut loader = None;
    for bl in BINARY_LOADERS.iter() {
        if bl.probe(param, &head_buf).is_ok() {
            loader = Some(bl);
            break;
        }
    }

    if loader.is_none() {
        return Err(SystemError::ENOEXEC);
    }

    let loader: &&dyn BinaryLoader = loader.unwrap();
    assert!(param.vm().is_current());
    loader.load(param, &head_buf).map_err(|e| e.into())?;

    return Ok(());
}

#[derive(Debug)]
pub struct ProcInitInfo {
    pub args: Vec<String>,
    pub envs: Vec<String>,
    pub auxv: BTreeMap<u8, usize>,
}

impl ProcInitInfo {
    pub fn new() -> Self {
        Self {
            args: Vec::new(),
            envs: Vec::new(),
            auxv: BTreeMap::new(),
        }
    }
}
