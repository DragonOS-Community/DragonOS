#![allow(non_camel_case_types)]
// 这是signal暴露给其他模块的公有的接口文件

// todo: 将这里更换为手动编写的ffi绑定
use crate::include::bindings::bindings::atomic_t;
use crate::include::bindings::bindings::spinlock_t;
use crate::libs::ffi_convert::FFIBind2Rust;
use crate::libs::ffi_convert::__convert_mut;
use crate::libs::ffi_convert::__convert_ref;
use crate::libs::refcount::RefCount;

/// 请注意，sigset_t这个bitmap, 第0位表示sig=1的信号。也就是说，SignalNumber-1才是sigset_t中对应的位
pub type sigset_t = u64;
pub type __signalfn_t = ::core::option::Option<unsafe extern "C" fn(arg1: ::core::ffi::c_int)>;
pub type __sighandler_t = __signalfn_t;

// 最大的信号数量（改动这个值的时候请同步到signal.h)
pub const MAX_SIG_NUM: i32 = 64;

/// 由于signal_struct总是和sighand_struct一起使用，并且信号处理的过程中必定会对sighand加锁
/// 因此signal_struct不用加锁
/// **请将该结构体与`include/DragonOS/signal.h`中的保持同步**
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct signal_struct {
    pub sig_cnt: atomic_t,
}

impl Default for signal_struct{
    fn default() -> Self {
        Self { sig_cnt: Default::default() }
    }
}

/**
 * sigaction中的信号处理函数结构体
 * 分为两种处理函数
 */
#[repr(C)]
#[derive(Copy, Clone)]
pub union sigaction__union_u {
    pub _sa_handler: __sighandler_t, // 传统处理函数
    pub _sa_sigaction: ::core::option::Option<
        unsafe extern "C" fn(
            sig: ::core::ffi::c_int,
            sinfo: *mut siginfo,
            arg1: *mut ::core::ffi::c_void,
        ),
    >,
}

impl core::fmt::Debug for sigaction__union_u{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("sigaction__union_u")
    }
}


impl Default for sigaction__union_u {
    fn default() -> Self {
        Self{_sa_handler:None}
    }
}

/**
 * @brief 信号处理结构体
 */
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct sigaction {
    pub _u: sigaction__union_u,
    pub sa_flags: u64,
    pub sa_mask: sigset_t,
    pub sa_restorer: ::core::option::Option<unsafe extern "C" fn()>, // 暂时未实现该函数
}

impl Default for sigaction{
    fn default() -> Self {
        Self { _u: Default::default(), sa_flags: Default::default(), sa_mask: Default::default(), sa_restorer: Default::default() }
    }
}



/**
 * 信号消息的结构体，作为参数传入sigaction结构体中指向的处理函数
 */
#[repr(C)]
#[derive(Copy, Clone)]
pub struct siginfo {
    pub _sinfo: __siginfo_union,
}
#[repr(C)]
#[derive(Copy, Clone)]
pub union __siginfo_union {
    pub data: __siginfo_union_data,
    pub padding: [u64; 4usize],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct __siginfo_union_data {
    pub si_signo: i32,
    pub si_code: i32,
    pub si_errno: i32,
    pub reserved: u32,
    pub _sifields: __sifields,
}

/**
 * siginfo中，根据signal的来源不同，该union中对应了不同的数据./=
 * 请注意，该union最大占用16字节
 */
#[repr(C)]
#[derive(Copy, Clone)]
pub union __sifields {
    pub _kill: __sifields__kill,
}

/**
 * 来自kill命令的signal
 */
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct __sifields__kill {
    pub _pid: i64, /* 发起kill的进程的pid */
}

/**
 * @brief 信号处理结构体，位于pcb之中
 */
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct sighand_struct {
    pub siglock: spinlock_t,
    pub count: RefCount,
    pub action: [sigaction; MAX_SIG_NUM as usize],
}

impl Default for sighand_struct{
    fn default() -> Self {
        Self { siglock: Default::default(), count: Default::default(), action: [Default::default();MAX_SIG_NUM as usize] }
    }
}

/**
 * @brief 正在等待的信号的标志位
 */
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct sigpending {
    pub signal: sigset_t,
}

#[allow(dead_code)]
#[repr(i32)]
pub enum si_code_val {
    /// sent by kill, sigsend, raise
    SI_USER = 0,
    /// sent by kernel from somewhere
    SI_KERNEL = 0x80,
    /// 通过sigqueue发送
    SI_QUEUE = -1,
    /// 定时器过期时发送
    SI_TIMER = -2,
    /// 当实时消息队列的状态发生改变时发送
    SI_MESGQ = -3,
    /// 当异步IO完成时发送
    SI_ASYNCIO = -4,
    /// sent by queued SIGIO
    SI_SIGIO = -5,
}

impl si_code_val {
    /// 为si_code_val这个枚举类型实现从i32转换到枚举类型的转换函数
    #[allow(dead_code)]
    pub fn from_i32(x: i32) -> si_code_val {
        match x {
            0 => Self::SI_USER,
            0x80 => Self::SI_KERNEL,
            -1 => Self::SI_QUEUE,
            -2 => Self::SI_TIMER,
            -3 => Self::SI_MESGQ,
            -4 => Self::SI_ASYNCIO,
            -5 => Self::SI_SIGIO,
            _ => panic!("si code not valid"),
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
#[repr(i32)]
pub enum SignalNumber {
    SIGHUP = 1,
    SIGINT,
    SIGQUIT,
    SIGILL,
    SIGTRAP,
    /// SIGABRT和SIGIOT共用这个号码
    SIGABRT_OR_IOT,
    SIGBUS,
    SIGFPE,
    SIGKILL,
    SIGUSR1,

    SIGSEGV = 11,
    SIGUSR2,
    SIGPIPE,
    SIGALRM,
    SIGTERM,
    SIGSTKFLT,
    SIGCHLD,
    SIGCONT,
    SIGSTOP,
    SIGTSTP,

    SIGTTIN = 21,
    SIGTTOU,
    SIGURG,
    SIGXCPU,
    SIGXFSZ,
    SIGVTALRM,
    SIGPROF,
    SIGWINCH,
    /// SIGIO和SIGPOLL共用这个号码
    SIGIO_OR_POLL,
    SIGPWR,

    SIGSYS = 31,
}

/// 为SignalNumber实现判断相等的trait
impl PartialEq for SignalNumber {
    fn eq(&self, other: &SignalNumber) -> bool {
        *self as i32 == *other as i32
    }
}

impl SignalNumber {
    /// @brief 从i32转换为SignalNumber枚举类型，如果传入的x不符合要求，则返回None
    #[allow(dead_code)]
    pub fn from_i32(x: i32) -> Option<SignalNumber> {
        if Self::valid_signal_number(x) {
            let ret: SignalNumber = unsafe { core::mem::transmute(x) };
            return Some(ret);
        }

        return None;
    }

    /// 判断一个数字是否为可用的信号
    fn valid_signal_number(x: i32) -> bool {
        if x > 0 && x < MAX_SIG_NUM {
            return true;
        } else {
            return false;
        }
    }
}

#[allow(dead_code)]
pub const SIGRTMIN: i32 = 32;
#[allow(dead_code)]
pub const SIGRTMAX: i32 = MAX_SIG_NUM;

/// @brief 将给定的signal_struct解析为Rust的signal.rs中定义的signal_struct的引用
///
/// 这么做的主要原因在于，由于PCB是通过bindgen生成的FFI，因此pcb中的结构体类型都是bindgen自动生成的
impl FFIBind2Rust<crate::include::bindings::bindings::signal_struct> for signal_struct {
    fn convert_mut<'a>(
        src: *mut crate::include::bindings::bindings::signal_struct,
    ) -> Option<&'a mut Self> {
        return __convert_mut(src);
    }
    fn convert_ref<'a>(
        src: *const crate::include::bindings::bindings::signal_struct,
    ) -> Option<&'a Self> {
        return __convert_ref(src);
    }
}

/// @brief 将给定的siginfo解析为Rust的signal.rs中定义的siginfo的引用
///
/// 这么做的主要原因在于，由于PCB是通过bindgen生成的FFI，因此pcb中的结构体类型都是bindgen自动生成的
impl FFIBind2Rust<crate::include::bindings::bindings::siginfo> for siginfo {
    fn convert_mut<'a>(
        src: *mut crate::include::bindings::bindings::siginfo,
    ) -> Option<&'a mut Self> {
        return __convert_mut(src);
    }
    fn convert_ref<'a>(
        src: *const crate::include::bindings::bindings::siginfo,
    ) -> Option<&'a Self> {
        return __convert_ref(src)
    }
}


/// @brief 将给定的sigset_t解析为Rust的signal.rs中定义的sigset_t的引用
///
/// 这么做的主要原因在于，由于PCB是通过bindgen生成的FFI，因此pcb中的结构体类型都是bindgen自动生成的
impl FFIBind2Rust<crate::include::bindings::bindings::sigset_t> for sigset_t {
    fn convert_mut<'a>(
        src: *mut crate::include::bindings::bindings::sigset_t,
    ) -> Option<&'a mut Self> {
        return __convert_mut(src);
    }
    fn convert_ref<'a>(
        src: *const crate::include::bindings::bindings::sigset_t,
    ) -> Option<&'a Self> {
        return __convert_ref(src)
    }
}

/// @brief 将给定的sigpending解析为Rust的signal.rs中定义的sigpending的引用
///
/// 这么做的主要原因在于，由于PCB是通过bindgen生成的FFI，因此pcb中的结构体类型都是bindgen自动生成的
impl FFIBind2Rust<crate::include::bindings::bindings::sigpending> for sigpending {
    fn convert_mut<'a>(
        src: *mut crate::include::bindings::bindings::sigpending,
    ) -> Option<&'a mut Self> {
        return __convert_mut(src);
    }
    fn convert_ref<'a>(
        src: *const crate::include::bindings::bindings::sigpending,
    ) -> Option<&'a Self> {
        return __convert_ref(src)
    }
}

/// @brief 将给定的来自bindgen的sighand_struct解析为Rust的signal.rs中定义的sighand_struct的引用
///
/// 这么做的主要原因在于，由于PCB是通过bindgen生成的FFI，因此pcb中的结构体类型都是bindgen自动生成的，会导致无法自定义功能的问题。
impl FFIBind2Rust<crate::include::bindings::bindings::sighand_struct> for sighand_struct{
    fn convert_mut<'a>(
        src: *mut crate::include::bindings::bindings::sighand_struct,
    ) -> Option<&'a mut Self> {
        return __convert_mut(src);
    }
    fn convert_ref<'a>(
        src: *const crate::include::bindings::bindings::sighand_struct,
    ) -> Option<&'a Self> {
        return __convert_ref(src)
    }
}

/// @brief 将给定的来自bindgen的sigaction解析为Rust的signal.rs中定义的sigaction的引用
impl FFIBind2Rust<crate::include::bindings::bindings::sigaction> for sigaction{
    fn convert_mut<'a>(
        src: *mut crate::include::bindings::bindings::sigaction,
    ) -> Option<&'a mut Self> {
        return __convert_mut(src);
    }
    fn convert_ref<'a>(
        src: *const crate::include::bindings::bindings::sigaction,
    ) -> Option<&'a Self> {
        return __convert_ref(src)
    }
}