use core::{
    str,
    sync::atomic::{fence, Ordering},
};

use alloc::{ffi::CString, vec::Vec};

use crate::libs::spinlock::SpinLock;

use super::boot_params;

#[::linkme::distributed_slice]
pub static KCMDLINE_PARAM_EARLY_KV: [KernelCmdlineParameter] = [..];

#[::linkme::distributed_slice]
pub static KCMDLINE_PARAM_KV: [KernelCmdlineParameter] = [..];

#[::linkme::distributed_slice]
pub static KCMDLINE_PARAM_ARG: [KernelCmdlineParameter] = [..];

static KERNEL_CMDLINE_PARAM_MANAGER: KernelCmdlineManager = KernelCmdlineManager::new();

#[inline(always)]
pub fn kenrel_cmdline_param_manager() -> &'static KernelCmdlineManager {
    &KERNEL_CMDLINE_PARAM_MANAGER
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KCmdlineParamType {
    /// bool类型参数
    Arg,
    /// key-value类型参数
    KV,
    /// 内存管理初始化之前的KV参数
    EarlyKV,
}

pub struct KernelCmdlineParamBuilder {
    name: &'static str,
    ty: KCmdlineParamType,
    default_str: &'static str,
    default_bool: bool,
    inv: bool,
}

#[allow(dead_code)]
impl KernelCmdlineParamBuilder {
    pub const fn new(name: &'static str, ty: KCmdlineParamType) -> Self {
        Self {
            name,
            ty,
            default_str: "",
            default_bool: false,
            inv: false,
        }
    }

    pub const fn default_str(mut self, default_str: &'static str) -> Self {
        self.default_str = default_str;
        self
    }

    pub const fn default_bool(mut self, default_bool: bool) -> Self {
        self.default_bool = default_bool;
        self
    }

    pub const fn inv(mut self, inv: bool) -> Self {
        self.inv = inv;
        self
    }

    pub const fn build_early_kv(self) -> Option<KernelCmdlineEarlyKV> {
        if matches!(self.ty, KCmdlineParamType::EarlyKV) {
            Some(KernelCmdlineEarlyKV {
                name: self.name,
                value: [0; KernelCmdlineEarlyKV::VALUE_MAX_LEN],
                index: 0,
                initialized: false,
                default: self.default_str,
            })
        } else {
            None
        }
    }

    pub const fn build(self) -> Option<KernelCmdlineParameter> {
        match self.ty {
            KCmdlineParamType::Arg => Some(KernelCmdlineParameter::Arg(KernelCmdlineArg {
                name: self.name,
                value: self.default_bool,
                initialized: false,
                inv: self.inv,
                default: self.default_bool,
            })),
            KCmdlineParamType::KV => Some(KernelCmdlineParameter::KV(KernelCmdlineKV {
                name: self.name,
                value: None,
                initialized: false,
                default: self.default_str,
            })),
            _ => None,
        }
    }
}

#[allow(dead_code)]
pub enum KernelCmdlineParameter {
    Arg(KernelCmdlineArg),
    KV(KernelCmdlineKV),
    EarlyKV(&'static KernelCmdlineEarlyKV),
}

#[allow(dead_code)]
impl KernelCmdlineParameter {
    pub fn name(&self) -> &str {
        match self {
            KernelCmdlineParameter::Arg(v) => v.name,
            KernelCmdlineParameter::KV(v) => v.name,
            KernelCmdlineParameter::EarlyKV(v) => v.name,
        }
    }

    /// 获取bool类型参数的值
    pub fn value_bool(&self) -> Option<bool> {
        match self {
            KernelCmdlineParameter::Arg(v) => Some(v.value()),
            _ => None,
        }
    }

    /// 获取key-value类型参数的值
    pub fn value_str(&self) -> Option<&str> {
        match self {
            KernelCmdlineParameter::Arg(_) => None,
            KernelCmdlineParameter::KV(v) => v
                .value
                .as_ref()
                .and_then(|v| str::from_utf8(v.as_bytes()).ok()),
            KernelCmdlineParameter::EarlyKV(v) => v.value_str(),
        }
    }

    pub fn is_arg(&self) -> bool {
        matches!(self, KernelCmdlineParameter::Arg(_))
    }

    pub fn is_kv(&self) -> bool {
        matches!(self, KernelCmdlineParameter::KV(_))
    }

    pub fn is_early_kv(&self) -> bool {
        matches!(self, KernelCmdlineParameter::EarlyKV(_))
    }

    /// 强行获取可变引用
    ///
    /// # Safety
    ///
    /// 只能在内核初始化阶段pid0使用！
    #[allow(clippy::mut_from_ref)]
    unsafe fn force_mut(&self) -> &mut Self {
        let p = self as *const Self as *mut Self;
        p.as_mut().unwrap()
    }
}

#[derive(Debug)]
pub struct KernelCmdlineArg {
    name: &'static str,
    value: bool,
    initialized: bool,
    /// 是否反转
    inv: bool,
    default: bool,
}

impl KernelCmdlineArg {
    pub fn value(&self) -> bool {
        volatile_read!(self.value)
    }
}

pub struct KernelCmdlineKV {
    name: &'static str,
    value: Option<CString>,
    initialized: bool,
    default: &'static str,
}

/// 在内存管理初始化之前的KV参数
pub struct KernelCmdlineEarlyKV {
    name: &'static str,
    value: [u8; Self::VALUE_MAX_LEN],
    index: usize,
    initialized: bool,
    default: &'static str,
}

#[allow(dead_code)]
impl KernelCmdlineEarlyKV {
    pub const VALUE_MAX_LEN: usize = 256;

    pub fn value(&self) -> &[u8] {
        &self.value[..self.index]
    }

    pub fn value_str(&self) -> Option<&str> {
        core::str::from_utf8(&self.value[..self.index]).ok()
    }

    /// 强行获取可变引用
    ///
    /// # Safety
    ///
    /// 只能在内核初始化阶段pid0使用！
    #[allow(clippy::mut_from_ref)]
    unsafe fn force_mut(&self) -> &mut Self {
        let p = self as *const Self as *mut Self;
        p.as_mut().unwrap()
    }
}

pub struct KernelCmdlineManager {
    inner: SpinLock<InnerKernelCmdlineManager>,
}

pub(super) struct InnerKernelCmdlineManager {
    /// init进程的路径
    init_path: Option<CString>,
    init_args: Vec<CString>,
    init_envs: Vec<CString>,
}

impl KernelCmdlineManager {
    const fn new() -> Self {
        Self {
            inner: SpinLock::new(InnerKernelCmdlineManager {
                init_path: None,
                init_args: Vec::new(),
                init_envs: Vec::new(),
            }),
        }
    }

    pub(super) fn init_proc_path(&self) -> Option<CString> {
        self.inner.lock().init_path.clone()
    }

    pub(super) fn init_proc_args(&self) -> Vec<CString> {
        self.inner.lock().init_args.clone()
    }

    pub(super) fn init_proc_envs(&self) -> Vec<CString> {
        self.inner.lock().init_envs.clone()
    }

    /// 在内存管理初始化之前设置部分参数
    pub fn early_init(&self) {
        let boot_params = boot_params().read();

        for argument in self.split_args(boot_params.boot_cmdline_str()) {
            let (node, option, value) = match self.split_arg(argument) {
                Some(v) => v,
                None => continue,
            };
            // 查找参数
            if let Some(param) = self.find_param(node, option, KCmdlineParamType::EarlyKV) {
                let param = unsafe { param.force_mut() };
                match param {
                    KernelCmdlineParameter::EarlyKV(p) => {
                        let p = unsafe { p.force_mut() };
                        if let Some(value) = value {
                            let value = value.as_bytes();
                            let len = value.len().min(KernelCmdlineEarlyKV::VALUE_MAX_LEN);
                            p.value[..len].copy_from_slice(&value[..len]);
                            p.index = len;
                        }
                        p.initialized = true;
                    }
                    _ => unreachable!(),
                }
                fence(Ordering::SeqCst);
            }
        }

        // 初始化默认值
        KCMDLINE_PARAM_EARLY_KV.iter().for_each(|x| {
            let x = unsafe { x.force_mut() };
            if let KernelCmdlineParameter::EarlyKV(v) = x {
                if !v.initialized {
                    let v = unsafe { v.force_mut() };
                    let len = v.default.len().min(KernelCmdlineEarlyKV::VALUE_MAX_LEN);
                    v.value[..len].copy_from_slice(v.default.as_bytes());
                    v.index = len;
                    v.initialized = true;
                }
            }
        });
    }

    /// 在内存管理初始化之后设置命令行参数
    pub fn init(&self) {
        let mut inner = self.inner.lock();
        let boot_params = boot_params().read();
        // `--`以后的参数都是init进程的参数
        let mut kernel_cmdline_end = false;
        for argument in self.split_args(boot_params.boot_cmdline_str()) {
            if kernel_cmdline_end {
                if inner.init_path.is_none() {
                    panic!("cmdline: init proc path is not set while init proc args are set");
                }
                if !argument.is_empty() {
                    inner.init_args.push(CString::new(argument).unwrap());
                }
                continue;
            }

            if argument == "--" {
                kernel_cmdline_end = true;
                continue;
            }

            log::debug!("cmdline: argument: {:?} ", argument);
            let (node, option, value) = match self.split_arg(argument) {
                Some(v) => v,
                None => continue,
            };
            if option == "init" && value.is_some() {
                if inner.init_path.is_some() {
                    panic!("cmdline: init proc path is set twice");
                }
                inner.init_path = Some(CString::new(value.unwrap()).unwrap());
                continue;
            }
            // log::debug!(
            //     "cmdline: node: {:?}, option: {:?}, value: {:?}",
            //     node,
            //     option,
            //     value
            // );
            if let Some(param) = self.find_param(node, option, KCmdlineParamType::KV) {
                let param = unsafe { param.force_mut() };
                match param {
                    KernelCmdlineParameter::KV(p) => {
                        if p.value.is_some() {
                            log::warn!("cmdline: parameter {} is set twice", p.name);
                            continue;
                        }
                        p.value = Some(CString::new(value.unwrap()).unwrap());
                        p.initialized = true;
                    }
                    _ => unreachable!(),
                }
                fence(Ordering::SeqCst);
            } else if let Some(param) = self.find_param(node, option, KCmdlineParamType::Arg) {
                let param = unsafe { param.force_mut() };
                match param {
                    KernelCmdlineParameter::Arg(p) => {
                        if p.initialized {
                            log::warn!("cmdline: parameter {} is set twice", p.name);
                            continue;
                        }
                        p.value = !p.inv;
                        p.initialized = true;
                    }
                    _ => unreachable!(),
                }
                fence(Ordering::SeqCst);
            } else if node.is_none() {
                if let Some(val) = value {
                    inner
                        .init_envs
                        .push(CString::new(format!("{}={}", option, val)).unwrap());
                } else if !option.is_empty() {
                    inner.init_args.push(CString::new(option).unwrap());
                }
            }
        }
        fence(Ordering::SeqCst);
        // 初始化默认值
        self.default_initialize();
        fence(Ordering::SeqCst);
    }

    fn default_initialize(&self) {
        KCMDLINE_PARAM_ARG.iter().for_each(|x| {
            let x = unsafe { x.force_mut() };
            if let KernelCmdlineParameter::Arg(v) = x {
                if !v.initialized {
                    v.value = v.default;
                    v.initialized = true;
                }
            }
            fence(Ordering::SeqCst);
        });

        KCMDLINE_PARAM_KV.iter().for_each(|x| {
            let x = unsafe { x.force_mut() };
            if let KernelCmdlineParameter::KV(v) = x {
                if !v.initialized {
                    v.value = Some(CString::new(v.default).unwrap());
                    v.initialized = true;
                }
            }
            fence(Ordering::SeqCst);
        });
    }

    fn find_param(
        &self,
        node: Option<&str>,
        option: &str,
        param_typ: KCmdlineParamType,
    ) -> Option<&KernelCmdlineParameter> {
        let list = match param_typ {
            KCmdlineParamType::Arg => &KCMDLINE_PARAM_ARG,
            KCmdlineParamType::KV => &KCMDLINE_PARAM_KV,
            KCmdlineParamType::EarlyKV => &KCMDLINE_PARAM_EARLY_KV,
        };

        list.iter().find(|x| {
            let name = x.name();
            if let Some(node) = node {
                // 加1是因为有一个点号
                name.len() == (node.len() + option.len() + 1)
                    && name.starts_with(node)
                    && name[node.len() + 1..].starts_with(option)
            } else {
                name == option
            }
        })
    }

    fn split_arg<'a>(&self, arg: &'a str) -> Option<(Option<&'a str>, &'a str, Option<&'a str>)> {
        let mut iter = arg.splitn(2, '=');
        let key = iter.next().unwrap();
        let value = iter.next();
        let value = value.map(|v| v.trim());
        if value.is_some() && iter.next().is_some() {
            log::warn!("cmdline: invalid argument: {}", arg);
            return None;
        }

        let mut iter = key.splitn(2, '.');
        let v1 = iter.next().map(|v| v.trim());
        let v2 = iter.next().map(|v| v.trim());
        let v3 = iter.next().map(|v| v.trim());
        let v = [v1, v2, v3];

        let mut key_split_len = 0;
        v.iter().for_each(|x| {
            if x.is_some() {
                key_split_len += 1
            }
        });

        let (node, option) = match key_split_len {
            1 => (None, v[0].unwrap()),
            2 => (Some(v[0].unwrap()), v[1].unwrap()),
            _ => {
                log::warn!("cmdline: invalid argument: {}", arg);
                return None;
            }
        };

        Some((node, option, value))
    }

    fn split_args<'a>(&self, cmdline: &'a str) -> impl Iterator<Item = &'a str> {
        // 是否在引号内
        let mut in_quote = false;
        cmdline.split(move |c: char| {
            if c == '"' {
                in_quote = !in_quote;
            }
            !in_quote && c.is_whitespace()
        })
    }
}
