#![allow(non_camel_case_types)]
// 这是signal暴露给其他模块的公有的接口文件

use core::ffi::c_void;
use core::fmt::Debug;

use alloc::vec::Vec;

use crate::arch::fpu::FpState;
use crate::include::bindings::bindings::NULL;
// todo: 将这里更换为手动编写的ffi绑定
use crate::include::bindings::bindings::atomic_t;
use crate::include::bindings::bindings::pt_regs;
use crate::include::bindings::bindings::spinlock_t;
use crate::kerror;
use crate::libs::ffi_convert::FFIBind2Rust;
use crate::libs::ffi_convert::__convert_mut;
use crate::libs::ffi_convert::__convert_ref;
use crate::libs::refcount::RefCount;

/// 请注意，sigset_t这个bitmap, 第0位表示sig=1的信号。也就是说，SignalNumber-1才是sigset_t中对应的位
pub type sigset_t = u64;
/// 存储信号处理函数的地址(来自用户态)
pub type __signalfn_t = u64;
pub type __sighandler_t = __signalfn_t;
/// 存储信号处理恢复函数的地址(来自用户态)
pub type __sigrestorer_fn_t = u64;
pub type __sigrestorer_t = __sigrestorer_fn_t;

/// 最大的信号数量（改动这个值的时候请同步到signal.h)
pub const MAX_SIG_NUM: i32 = 64;
/// sigset所占用的u64的数量（改动这个值的时候请同步到signal.h)
pub const _NSIG_U64_CNT: i32 = MAX_SIG_NUM / 64;
/// 信号处理的栈的栈指针的最小对齐数量
pub const STACK_ALIGN: u64 = 16;

/// 由于signal_struct总是和sighand_struct一起使用，并且信号处理的过程中必定会对sighand加锁
/// 因此signal_struct不用加锁
/// **请将该结构体与`include/DragonOS/signal.h`中的保持同步**
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct signal_struct {
    pub sig_cnt: atomic_t,
}

impl Default for signal_struct {
    fn default() -> Self {
        Self {
            sig_cnt: Default::default(),
        }
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

impl core::fmt::Debug for sigaction__union_u {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("sigaction__union_u")
    }
}

impl Default for sigaction__union_u {
    fn default() -> Self {
        Self {
            _sa_handler: NULL as u64,
        }
    }
}

// ============ sigaction结构体中的的sa_flags的可选值 begin ===========
pub const SA_FLAG_DFL: u64 = 1u64 << 0; // 当前sigaction表示系统默认的动作
pub const SA_FLAG_IGN: u64 = 1u64 << 1; // 当前sigaction表示忽略信号的动作
pub const SA_FLAG_RESTORER: u64 = 1u64 << 2; // 当前sigaction具有用户指定的restorer
pub const SA_FLAG_IMMUTABLE: u64 = 1u64 << 3; // 当前sigaction不可被更改

/// 所有的sa_flags的mask。（用于去除那些不存在的sa_flags位)
pub const SA_ALL_FLAGS: u64 = SA_FLAG_IGN | SA_FLAG_DFL | SA_FLAG_RESTORER | SA_FLAG_IMMUTABLE;

// ============ sigaction结构体中的的sa_flags的可选值 end ===========

/// 用户态程序传入的SIG_DFL的值
pub const USER_SIG_DFL: u64 = 0;
/// 用户态程序传入的SIG_IGN的值
pub const USER_SIG_IGN: u64 = 1;

/**
 * @brief 信号处理结构体
 */
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct sigaction {
    pub _u: sigaction__union_u,
    pub sa_flags: u64,
    pub sa_mask: sigset_t, // 为了可扩展性而设置的sa_mask
    /// 信号处理函数执行结束后，将会跳转到这个函数内进行执行，然后执行sigreturn系统调用
    pub sa_restorer: __sigrestorer_t,
}

impl Default for sigaction {
    fn default() -> Self {
        Self {
            _u: Default::default(),
            sa_flags: Default::default(),
            sa_mask: Default::default(),
            sa_restorer: Default::default(),
        }
    }
}

impl sigaction {
    /// @brief 判断这个sigaction是否被忽略
    pub fn ignored(&self, _sig: SignalNumber) -> bool {
        if (self.sa_flags & SA_FLAG_IGN) != 0 {
            return true;
        }
        // todo: 增加对sa_flags为SA_FLAG_DFL,但是默认处理函数为忽略的情况的判断

        return false;
    }
}

/// @brief 用户态传入的sigaction结构体（符合posix规范）
/// 请注意，我们会在sys_sigaction函数里面将其转换成内核使用的sigaction结构体
#[repr(C)]
#[derive(Debug)]
pub struct user_sigaction {
    pub sa_handler: *mut core::ffi::c_void,
    pub sa_sigaction: *mut core::ffi::c_void,
    pub sa_mask: sigset_t,
    pub sa_flags: u64,
    pub sa_restorer: *mut core::ffi::c_void,
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

impl siginfo {
    pub fn new(sig: SignalNumber, _si_errno: i32, _si_code: si_code_val) -> Self {
        siginfo {
            _sinfo: __siginfo_union {
                data: __siginfo_union_data {
                    si_signo: sig as i32,
                    si_code: _si_code as i32,
                    si_errno: _si_errno,
                    reserved: 0,
                    _sifields: super::signal_types::__sifields {
                        _kill: super::signal_types::__sifields__kill { _pid: 0 },
                    },
                },
            },
        }
    }
}

impl Debug for siginfo {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        unsafe {
            f.write_fmt(format_args!(
                "si_signo:{}, si_code:{}, si_errno:{}, _pid:{}",
                self._sinfo.data.si_signo,
                self._sinfo.data.si_code,
                self._sinfo.data.si_errno,
                self._sinfo.data._sifields._kill._pid
            ))
        }
    }
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

impl Default for sighand_struct {
    fn default() -> Self {
        Self {
            siglock: Default::default(),
            count: Default::default(),
            action: [Default::default(); MAX_SIG_NUM as usize],
        }
    }
}

/**
 * @brief 正在等待的信号的标志位
 */
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct sigpending {
    pub signal: sigset_t,
    /// 信号队列
    pub queue: *mut SigQueue,
}

/// siginfo中的si_code的可选值
/// 请注意，当这个值小于0时，表示siginfo来自用户态，否则来自内核态
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
    INVALID = 0,
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

impl From<i32> for SignalNumber {
    fn from(value: i32) -> Self {
        if Self::valid_signal_number(value) {
            let ret: SignalNumber = unsafe { core::mem::transmute(value) };
            return ret;
        } else {
            kerror!("Try to convert an invalid number to SignalNumber");
            return SignalNumber::INVALID;
        }
    }
}
impl SignalNumber {
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
    fn convert_mut(
        src: *mut crate::include::bindings::bindings::signal_struct,
    ) -> Option<&'static mut Self> {
        return __convert_mut(src);
    }
    fn convert_ref(
        src: *const crate::include::bindings::bindings::signal_struct,
    ) -> Option<&'static Self> {
        return __convert_ref(src);
    }
}

/// @brief 将给定的siginfo解析为Rust的signal.rs中定义的siginfo的引用
///
/// 这么做的主要原因在于，由于PCB是通过bindgen生成的FFI，因此pcb中的结构体类型都是bindgen自动生成的
impl FFIBind2Rust<crate::include::bindings::bindings::siginfo> for siginfo {
    fn convert_mut(
        src: *mut crate::include::bindings::bindings::siginfo,
    ) -> Option<&'static mut Self> {
        return __convert_mut(src);
    }
    fn convert_ref(
        src: *const crate::include::bindings::bindings::siginfo,
    ) -> Option<&'static Self> {
        return __convert_ref(src);
    }
}

/// @brief 将给定的sigset_t解析为Rust的signal.rs中定义的sigset_t的引用
///
/// 这么做的主要原因在于，由于PCB是通过bindgen生成的FFI，因此pcb中的结构体类型都是bindgen自动生成的
impl FFIBind2Rust<crate::include::bindings::bindings::sigset_t> for sigset_t {
    fn convert_mut(
        src: *mut crate::include::bindings::bindings::sigset_t,
    ) -> Option<&'static mut Self> {
        return __convert_mut(src);
    }
    fn convert_ref(
        src: *const crate::include::bindings::bindings::sigset_t,
    ) -> Option<&'static Self> {
        return __convert_ref(src);
    }
}

/// @brief 将给定的sigpending解析为Rust的signal.rs中定义的sigpending的引用
///
/// 这么做的主要原因在于，由于PCB是通过bindgen生成的FFI，因此pcb中的结构体类型都是bindgen自动生成的
impl FFIBind2Rust<crate::include::bindings::bindings::sigpending> for sigpending {
    fn convert_mut(
        src: *mut crate::include::bindings::bindings::sigpending,
    ) -> Option<&'static mut Self> {
        return __convert_mut(src);
    }
    fn convert_ref(
        src: *const crate::include::bindings::bindings::sigpending,
    ) -> Option<&'static Self> {
        return __convert_ref(src);
    }
}

/// @brief 将给定的来自bindgen的sighand_struct解析为Rust的signal.rs中定义的sighand_struct的引用
///
/// 这么做的主要原因在于，由于PCB是通过bindgen生成的FFI，因此pcb中的结构体类型都是bindgen自动生成的，会导致无法自定义功能的问题。
impl FFIBind2Rust<crate::include::bindings::bindings::sighand_struct> for sighand_struct {
    fn convert_mut(
        src: *mut crate::include::bindings::bindings::sighand_struct,
    ) -> Option<&'static mut Self> {
        return __convert_mut(src);
    }
    fn convert_ref(
        src: *const crate::include::bindings::bindings::sighand_struct,
    ) -> Option<&'static Self> {
        return __convert_ref(src);
    }
}

/// @brief 将给定的来自bindgen的sigaction解析为Rust的signal.rs中定义的sigaction的引用
impl FFIBind2Rust<crate::include::bindings::bindings::sigaction> for sigaction {
    fn convert_mut(
        src: *mut crate::include::bindings::bindings::sigaction,
    ) -> Option<&'static mut Self> {
        return __convert_mut(src);
    }
    fn convert_ref(
        src: *const crate::include::bindings::bindings::sigaction,
    ) -> Option<&'static Self> {
        return __convert_ref(src);
    }
}

/// @brief 进程接收到的信号的队列
pub struct SigQueue {
    pub q: Vec<siginfo>,
}

#[allow(dead_code)]
impl SigQueue {
    /// @brief 初始化一个新的信号队列
    pub fn new(capacity: usize) -> Self {
        SigQueue {
            q: Vec::with_capacity(capacity),
        }
    }

    /// @brief 在信号队列中寻找第一个满足要求的siginfo, 并返回它的引用
    ///
    /// @return (第一个满足要求的siginfo的引用; 是否有多个满足条件的siginfo)
    pub fn find(&self, sig: SignalNumber) -> (Option<&siginfo>, bool) {
        // 是否存在多个满足条件的siginfo
        let mut still_pending = false;
        let mut info: Option<&siginfo> = None;

        for x in self.q.iter() {
            if unsafe { x._sinfo.data.si_signo } == sig as i32 {
                if info.is_some() {
                    still_pending = true;
                    break;
                } else {
                    info = Some(x);
                }
            }
        }
        return (info, still_pending);
    }

    /// @brief 在信号队列中寻找第一个满足要求的siginfo, 并将其从队列中删除，然后返回这个siginfo
    ///
    /// @return (第一个满足要求的siginfo; 从队列中删除前是否有多个满足条件的siginfo)
    pub fn find_and_delete(&mut self, sig: SignalNumber) -> (Option<siginfo>, bool) {
        // 是否存在多个满足条件的siginfo
        let mut still_pending = false;
        let mut first = true; // 标记变量，记录当前是否已经筛选出了一个元素

        let filter = |x: &mut siginfo| {
            if unsafe { x._sinfo.data.si_signo } == sig as i32 {
                if !first {
                    // 如果之前已经筛选出了一个元素，则不把当前元素删除
                    still_pending = true;
                    return false;
                } else {
                    // 当前是第一个被筛选出来的元素
                    first = false;
                    return true;
                }
            } else {
                return false;
            }
        };
        // 从sigqueue中过滤出结果
        let mut filter_result: Vec<siginfo> = self.q.drain_filter(filter).collect();
        // 筛选出的结果不能大于1个
        assert!(filter_result.len() <= 1);

        return (filter_result.pop(), still_pending);
    }

    /// @brief 从sigqueue中删除mask中被置位的信号。也就是说，比如mask的第1位被置为1,那么就从sigqueue中删除所有signum为2的信号的信息。
    pub fn flush_by_mask(&mut self, mask: &sigset_t) {
        // 定义过滤器，从sigqueue中删除mask中被置位的信号
        let filter = |x: &mut siginfo| {
            if sig_is_member(mask, SignalNumber::from(unsafe { x._sinfo.data.si_signo })) {
                true
            } else {
                false
            }
        };
        let filter_result: Vec<siginfo> = self.q.drain_filter(filter).collect();
        // 回收这些siginfo
        for x in filter_result {
            drop(x)
        }
    }

    /// @brief 从C的void*指针转换为static生命周期的可变引用
    pub fn from_c_void(p: *mut c_void) -> &'static mut SigQueue {
        let sq = p as *mut SigQueue;
        let sq = unsafe { sq.as_mut::<'static>() }.unwrap();
        return sq;
    }
}

impl Default for SigQueue {
    fn default() -> Self {
        Self {
            q: Default::default(),
        }
    }
}

/// @brief 清除sigset中，某个信号对应的标志位
#[inline]
pub fn sigset_del(set: &mut sigset_t, sig: SignalNumber) {
    let sig = sig as i32 - 1;
    if _NSIG_U64_CNT == 1 {
        *set &= !(1 << sig);
    } else {
        // 暂时不支持超过64个信号
        panic!("Unsupported signal number: {:?}", sig);
    }
}

/// @brief 将指定的信号在sigset中的对应bit进行置位
#[inline]
pub fn sigset_add(set: &mut sigset_t, sig: SignalNumber) {
    *set |= 1 << ((sig as u32) - 1);
}

/// @brief 将sigset清零
#[inline]
pub fn sigset_clear(set: &mut sigset_t) {
    *set = 0;
}

/// @brief 将mask中置为1的位，在sigset中清零
#[inline]
pub fn sigset_delmask(set: &mut sigset_t, mask: u64) {
    *set &= !mask;
}

/// @brief 判断两个sigset是否相等
#[inline]
pub fn sigset_equal(a: &sigset_t, b: &sigset_t) -> bool {
    if _NSIG_U64_CNT == 1 {
        return *a == *b;
    }
    return false;
}

/// @brief 使用指定的值，初始化sigset（为支持将来超过64个signal留下接口）
#[inline]
pub fn sigset_init(new_set: &mut sigset_t, mask: u64) {
    *new_set = mask;
    match _NSIG_U64_CNT {
        1 => {}
        _ => {
            // 暂时不支持大于64个信号
            todo!();
        }
    };
}

/// @brief 判断指定的信号在sigset中的对应位是否被置位
/// @return true: 给定的信号在sigset中被置位
/// @return false: 给定的信号在sigset中没有被置位
#[inline]
pub fn sig_is_member(set: &sigset_t, _sig: SignalNumber) -> bool {
    return if 1 & (set >> ((_sig as u32) - 1)) != 0 {
        true
    } else {
        false
    };
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct sigframe {
    /// 指向restorer的地址的指针。（该变量必须放在sigframe的第一位，因为这样才能在handler返回的时候，跳转到对应的代码，执行sigreturn)
    pub ret_code_ptr: *mut core::ffi::c_void,
    /// signum
    pub arg0: u64,
    /// siginfo pointer
    pub arg1: usize,
    /// sigcontext pointer
    pub arg2: usize,

    pub handler: *mut c_void,
    pub info: siginfo,
    pub context: sigcontext,
}

#[derive(Debug, Clone, Copy)]
pub struct sigcontext {
    /// sigcontext的标志位
    pub sc_flags: u64,
    pub sc_stack: signal_stack, // 信号处理程序备用栈信息

    pub regs: pt_regs, // 暂存的系统调用/中断返回时，原本要弹出的内核栈帧
    pub trap_num: u64, // 用来保存线程结构体中的trap_num字段
    pub oldmask: u64,  // 暂存的执行信号处理函数之前的，被设置block的信号
    pub cr2: u64,      // 用来保存线程结构体中的cr2字段
    pub err_code: u64, // 用来保存线程结构体中的err_code字段
    // todo: 支持x87浮点处理器后，在这里增加浮点处理器的状态结构体指针
    pub reserved_for_x87_state: u64,
    pub reserved: [u64; 8],
}

/// @brief 信号处理备用栈的信息
#[derive(Debug, Clone, Copy)]
pub struct signal_stack {
    pub sp: *mut c_void,
    pub flags: u32,
    pub size: u32,
    pub fpstate:FpState,
}
