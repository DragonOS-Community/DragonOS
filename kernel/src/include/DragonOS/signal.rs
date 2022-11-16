#![allow(non_camel_case_types)]
// 这是signal暴露给其他模块的公有的接口文件

// todo: 将这里更换为手动编写的ffi绑定
use crate::include::bindings::bindings::atomic_t;
use crate::include::bindings::bindings::refcount_t;
use crate::include::bindings::bindings::spinlock_t;
use crate::include::bindings::bindings::wait_queue_head_t;

pub type sigset_t = u64;
pub type __signalfn_t = ::core::option::Option<unsafe extern "C" fn(arg1: ::core::ffi::c_int)>;
pub type __sighandler_t = __signalfn_t;

/// 由于signal_struct总是和sighand_struct一起使用，并且信号处理的过程中必定会对sighand加锁
/// 因此signal_struct不用加锁
/// **请将该结构体与`include/DragonOS/signal.h`中的保持同步**
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct signal_struct {
    pub sig_cnt: atomic_t,
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

/**
 * @brief 信号处理结构体
 */
#[repr(C)]
#[derive(Copy, Clone)]
pub struct sigaction {
    pub _u: sigaction__union_u,
    pub sa_flags: u64,
    pub sa_mask: sigset_t,
    pub sa_restorer: ::core::option::Option<unsafe extern "C" fn()>, // 暂时未实现该函数
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
    pub code: i32,
    pub si_errno: i32,
    pub _sifields: __sifields,
}

/**
 * siginfo中，根据signal的来源不同，该union中对应了不同的数据
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
#[derive(Copy, Clone)]
pub struct sighand_struct {
    pub siglock: spinlock_t,
    pub count: refcount_t,
    pub signal_fd_wqh: wait_queue_head_t,
    pub action: [sigaction; 64usize],
}

/**
 * @brief 正在等待的信号的标志位
 */
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct sigpending {
    pub signal: sigset_t,
}
