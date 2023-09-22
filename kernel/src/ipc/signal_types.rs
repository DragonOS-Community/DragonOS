use core::{
    cell::Cell,
    ffi::{c_int, c_void},
    sync::atomic::AtomicI64,
};

use alloc::{sync::Arc, vec::Vec};

use crate::{
    arch::{
        fpu::FpState,
        interrupt::TrapFrame,
        ipc::signal::{SigCode, SigFlags, SigSet, SignalNumber},
    },
    include::bindings::bindings::siginfo,
    kerror,
    libs::{
        ffi_convert::{FFIBind2Rust, __convert_mut, __convert_ref},
        spinlock::SpinLock,
    },
    process::Pid,
};

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

/// 用户态程序传入的SIG_DFL的值
pub const USER_SIG_DFL: u64 = 0;
/// 用户态程序传入的SIG_IGN的值
pub const USER_SIG_IGN: u64 = 1;
/// 用户态程序传入的SIG_ERR的值
pub const USER_SIG_ERR: u64 = 2;

/// SignalStruct 在 pcb 中加锁
#[derive(Debug)]
pub struct SignalStruct {
    pub cnt: AtomicI64,
    pub handler: Arc<SigHandStruct>,
}

impl Default for SignalStruct {
    fn default() -> Self {
        Self {
            cnt: Default::default(),
            handler: Default::default(),
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub enum SigactionType {
    SaHandler(SaHandlerType),
    SaSigaction(
        Option<
            unsafe extern "C" fn(
                sig: ::core::ffi::c_int,
                sinfo: *mut siginfo,
                arg1: *mut ::core::ffi::c_void,
            ),
        >,
    ),
}

impl SigactionType {
    /// Returns `true` if the sigaction type is [`SaHandler`].
    ///
    /// [`SaHandler`]: SigactionType::SaHandler
    #[must_use]
    pub fn is_sa_handler(&self) -> bool {
        matches!(self, Self::SaHandler(..))
    }
}

#[derive(Debug, Copy, Clone)]
pub enum SaHandlerType {
    SigError,
    SigDefault,
    SigIgnore,
    SigCustomized(__sighandler_t),
}

impl Into<usize> for SaHandlerType {
    fn into(self) -> usize {
        match self {
            Self::SigError => 2 as usize,
            Self::SigIgnore => 1 as usize,
            Self::SigDefault => 0 as usize,
            Self::SigCustomized(handler) => handler as usize,
        }
    }
}

impl SaHandlerType {
    /// Returns `true` if the sa handler type is [`SigCustomized`].
    ///
    /// [`SigCustomized`]: SaHandlerType::SigCustomized
    #[must_use]
    pub fn is_sig_customized(&self) -> bool {
        matches!(self, Self::SigCustomized(..))
    }

    /// Returns `true` if the sa handler type is [`SigDefault`].
    ///
    /// [`SigDefault`]: SaHandlerType::SigDefault
    #[must_use]
    pub fn is_sig_default(&self) -> bool {
        matches!(self, Self::SigDefault)
    }

    /// Returns `true` if the sa handler type is [`SigIgnore`].
    ///
    /// [`SigIgnore`]: SaHandlerType::SigIgnore
    #[must_use]
    pub fn is_sig_ignore(&self) -> bool {
        matches!(self, Self::SigIgnore)
    }

    /// Returns `true` if the sa handler type is [`SigError`].
    ///
    /// [`SigError`]: SaHandlerType::SigError
    #[must_use]
    pub fn is_sig_error(&self) -> bool {
        matches!(self, Self::SigError)
    }
}

/// 信号处理结构体
///
#[derive(Debug, Copy, Clone)]
pub struct Sigaction {
    action: SigactionType,
    flags: SigFlags,
    mask: SigSet, // 为了可扩展性而设置的sa_mask
    /// 信号处理函数执行结束后，将会跳转到这个函数内进行执行，然后执行sigreturn系统调用
    restorer: Option<__sigrestorer_t>,
}

impl Default for Sigaction {
    fn default() -> Self {
        Self {
            action: SigactionType::SaHandler(SaHandlerType::SigDefault),
            flags: Default::default(),
            mask: Default::default(),
            restorer: Default::default(),
        }
    }
}

impl Sigaction {
    pub fn ignore(&self, sig: SignalNumber) -> bool {
        if self.flags.contains(SigFlags::SA_FLAG_IGN) {
            return true;
        }
        //a_flags为SA_FLAG_DFL,但是默认处理函数为忽略的情况的判断
        if self.flags().contains(SigFlags::SA_FLAG_DFL) {
            if let SigactionType::SaHandler(SaHandlerType::SigIgnore) = self.action {
                return true;
            }
        }
        return false;
    }
    pub fn new(
        action: SigactionType,
        flags: SigFlags,
        mask: SigSet,
        restorer: Option<__sigrestorer_t>,
    ) -> Self {
        Self {
            action,
            flags,
            mask,
            restorer,
        }
    }

    pub fn action(&self) -> SigactionType {
        self.action
    }

    pub fn flags(&self) -> SigFlags {
        self.flags
    }

    pub fn restorer(&self) -> Option<u64> {
        self.restorer
    }

    pub fn flags_mut(&mut self) -> &mut SigFlags {
        &mut self.flags
    }

    pub fn set_action(&mut self, action: SigactionType) {
        self.action = action;
    }

    pub fn mask(&self) -> SigSet {
        self.mask
    }

    pub fn mask_mut(&mut self) -> &mut SigSet {
        &mut self.mask
    }

    pub fn set_restorer(&mut self, restorer: Option<__sigrestorer_t>) {
        self.restorer = restorer;
    }
}

/// 用户态传入的sigaction结构体（符合posix规范）
/// 请注意，我们会在sys_sigaction函数里面将其转换成内核使用的sigaction结构体
#[derive(Debug)]
pub struct UserSigaction {
    pub handler: *mut core::ffi::c_void,
    pub sigaction: *mut core::ffi::c_void,
    pub mask: SigSet,
    pub flags: SigFlags,
    pub restorer: *mut core::ffi::c_void,
}

/**
 * siginfo中，根据signal的来源不同，该info中对应了不同的数据./=
 * 请注意，该info最大占用16字节
 */

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct SigInfo {
    sig_no: i32,
    code: i32,
    errno: i32,
    reserved: u32,
    sig_type: SigType,
}

impl SigInfo {
    pub fn sig_no(&self) -> i32 {
        self.sig_no
    }

    pub fn code(&self) -> i32 {
        self.code
    }

    pub fn errno(&self) -> i32 {
        self.errno
    }

    pub fn reserved(&self) -> u32 {
        self.reserved
    }

    pub fn sig_type(&self) -> SigType {
        self.sig_type
    }

    pub fn set_sig_type(&mut self, sig_type: SigType) {
        self.sig_type = sig_type;
    }
}

#[derive(Copy, Clone, Debug)]
pub enum SigType {
    Kill(Pid),
}

impl SigInfo {
    pub fn new(
        sig: SignalNumber,
        sig_errno: i32,
        sig_code: SigCode,
        reserved: u32,
        sig_type: SigType,
    ) -> Self {
        Self {
            sig_no: sig as i32,
            code: sig_code as i32,
            errno: sig_errno,
            reserved,
            sig_type,
        }
    }
}

/// 在获取SigHandStruct的外部就获取到了锁，所以这里是不会有任何竞争的，只是处于内部可变性的需求
/// 才使用了SpinLock，这里并不会带来太多的性能开销
#[derive(Debug)]
pub struct SigHandStruct(pub [Sigaction; MAX_SIG_NUM as usize]);

impl Default for SigHandStruct {
    fn default() -> Self {
        SigHandStruct([Sigaction::default(); MAX_SIG_NUM as usize])
    }
}

#[derive(Debug)]
pub struct SigPending {
    signal: SigSet,
    queue: SigQueue,
}

impl Default for SigPending {
    fn default() -> Self {
        SigPending {
            signal: SigSet::default(),
            queue: SigQueue::default(),
        }
    }
}

impl SigPending {
    pub fn signal(&self) -> SigSet {
        self.signal
    }

    pub fn queue(&self) -> &SigQueue {
        &self.queue
    }

    pub fn queue_mut(&mut self) -> &mut SigQueue {
        &mut self.queue
    }

    pub fn signal_mut(&mut self) -> &mut SigSet {
        &mut self.signal
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SigFrame {
    /// 指向restorer的地址的指针。（该变量必须放在sigframe的第一位，因为这样才能在handler返回的时候，跳转到对应的代码，执行sigreturn)
    pub ret_code_ptr: *mut core::ffi::c_void,
    /// signum
    pub arg0: u64,
    /// siginfo pointer
    pub arg1: usize,
    /// sigcontext pointer
    pub arg2: usize,

    pub handler: *mut c_void,
    pub info: SigInfo,
    pub context: SigContext,
}

#[derive(Debug, Clone, Copy)]
pub struct SigContext {
    /// sigcontext的标志位
    pub sc_flags: u64,
    pub sc_stack: SigStack, // 信号处理程序备用栈信息
    pub frame: TrapFrame,   // 暂存的系统调用/中断返回时，原本要弹出的内核栈帧
    // pub trap_num: u64,    // 用来保存线程结构体中的trap_num字段
    pub oldmask: SigSet, // 暂存的执行信号处理函数之前的，被设置block的信号
    pub cr2: u64,        // 用来保存线程结构体中的cr2字段
    // pub err_code: u64,    // 用来保存线程结构体中的err_code字段
    // todo: 支持x87浮点处理器后，在这里增加浮点处理器的状态结构体指针
    pub reserved_for_x87_state: u64,
    pub reserved: [u64; 8],
}

/// @brief 信号处理备用栈的信息
#[derive(Debug, Clone, Copy)]
pub struct SigStack {
    pub sp: *mut c_void,
    pub flags: u32,
    pub size: u32,
    pub fpstate: FpState,
}

/// @brief 进程接收到的信号的队列
#[derive(Debug, Clone)]
pub struct SigQueue {
    pub q: Vec<SigInfo>,
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
    pub fn find(&self, sig: SignalNumber) -> (Option<&SigInfo>, bool) {
        // 是否存在多个满足条件的siginfo
        let mut still_pending = false;
        let mut info: Option<&SigInfo> = None;

        for x in self.q.iter() {
            if x.sig_no == sig as i32 {
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
    pub fn find_and_delete(&mut self, sig: SignalNumber) -> (Option<SigInfo>, bool) {
        // 是否存在多个满足条件的siginfo
        let mut still_pending = false;
        let mut first = true; // 标记变量，记录当前是否已经筛选出了一个元素

        let filter = |x: &mut SigInfo| {
            if x.sig_no == sig as i32 {
                if !first {
                    // 如果之前已经筛选出了一个元素，则不把当前元素删除
                    still_pending = true;
                    return false;
                } else {
                    // 当前是第一个被筛选出来的元素
                    first = false;
                    return true;
                }
            }
            return false;
        };
        // 从sigqueue中过滤出结果
        let mut filter_result: Vec<SigInfo> = self.q.drain_filter(filter).collect();
        // 筛选出的结果不能大于1个
        assert!(filter_result.len() <= 1);

        return (filter_result.pop(), still_pending);
    }

    /// @brief 从sigqueue中删除mask中被置位的信号。也就是说，比如mask的第1位被置为1,那么就从sigqueue中删除所有signum为2的信号的信息。
    pub fn flush_by_mask(&mut self, mask: &SigSet) {
        // 定义过滤器，从sigqueue中删除mask中被置位的信号
        let filter = |x: &mut SigInfo| {
            if mask.contains(SigSet::from_bits_truncate(x.sig_no as u64)) {
                return true;
            }

            return false;
        };
        let filter_result: Vec<SigInfo> = self.q.drain_filter(filter).collect();
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

/// @brief 将给定的signal_struct解析为Rust的signal.rs中定义的signal_struct的引用
///
/// 这么做的主要原因在于，由于PCB是通过bindgen生成的FFI，因此pcb中的结构体类型都是bindgen自动生成的
impl FFIBind2Rust<crate::include::bindings::bindings::signal_struct> for SignalStruct {
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
impl FFIBind2Rust<crate::include::bindings::bindings::siginfo> for SigInfo {
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
impl FFIBind2Rust<crate::include::bindings::bindings::sigset_t> for SigSet {
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
impl FFIBind2Rust<crate::include::bindings::bindings::sigpending> for SigPending {
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
impl FFIBind2Rust<crate::include::bindings::bindings::sighand_struct> for SigHandStruct {
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
impl FFIBind2Rust<crate::include::bindings::bindings::sigaction> for Sigaction {
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
