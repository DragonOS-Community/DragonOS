use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use core::{any::Any, fmt::Debug};

#[cfg(target_arch = "loongarch64")]
mod loongarch64;
#[cfg(target_arch = "riscv64")]
mod rv64;
#[cfg(target_arch = "x86_64")]
mod x86;

#[cfg(target_arch = "loongarch64")]
pub use loongarch64::*;
#[cfg(target_arch = "riscv64")]
pub use rv64::*;
#[cfg(target_arch = "x86_64")]
pub use x86::*;

#[cfg(target_arch = "x86_64")]
pub type KprobePoint = X86KprobePoint;
#[cfg(target_arch = "riscv64")]
pub type KprobePoint = Rv64KprobePoint;
#[cfg(target_arch = "loongarch64")]
pub type KprobePoint = LA64KprobePoint;

pub trait ProbeArgs: Send {
    /// 用于使用者转换到特定架构下的TrapFrame
    fn as_any(&self) -> &dyn Any;
    /// 返回导致break异常的地址
    fn break_address(&self) -> usize;
    /// 返回导致单步执行异常的地址
    fn debug_address(&self) -> usize;
}

pub trait KprobeOps: Send {
    /// # 返回探测点的下一条指令地址
    ///
    /// 执行流需要回到正常的路径中，在执行完探测点的指令后，需要返回到下一条指令
    fn return_address(&self) -> usize;
    /// # 返回单步执行的指令地址
    ///
    /// 通常探测点的处的原指令被保存在一个数组当中。根据架构的不同, 在保存的指令后面，可能会填充必要的指令。
    /// 例如x86架构下支持单步执行的特性， 而其它架构下通常没有，因此我们使用break异常来进行模拟，所以会填充
    /// 一条断点指令。
    fn single_step_address(&self) -> usize;
    /// # 返回单步执行指令触发异常的地址
    ///
    /// 其值等于`single_step_address`的值加上探测点指令的长度
    fn debug_address(&self) -> usize;
    /// # 返回设置break断点的地址
    ///
    /// 其值与探测点地址相等
    fn break_address(&self) -> usize;
}

struct ProbeHandler {
    func: fn(&dyn ProbeArgs),
}

impl ProbeHandler {
    pub fn new(func: fn(&dyn ProbeArgs)) -> Self {
        ProbeHandler { func }
    }
    /// 调用探测点处理函数
    pub fn call(&self, trap_frame: &dyn ProbeArgs) {
        (self.func)(trap_frame);
    }
}

pub struct KprobeBuilder {
    symbol: Option<String>,
    symbol_addr: usize,
    offset: usize,
    pre_handler: ProbeHandler,
    post_handler: ProbeHandler,
    fault_handler: Option<ProbeHandler>,
    event_callback: Option<Box<dyn CallBackFunc>>,
    probe_point: Option<Arc<KprobePoint>>,
    enable: bool,
}

pub trait EventCallback: Send {
    fn call(&self, trap_frame: &dyn ProbeArgs);
}

impl KprobeBuilder {
    pub fn new(
        symbol: Option<String>,
        symbol_addr: usize,
        offset: usize,
        pre_handler: fn(&dyn ProbeArgs),
        post_handler: fn(&dyn ProbeArgs),
        enable: bool,
    ) -> Self {
        KprobeBuilder {
            symbol,
            symbol_addr,
            offset,
            pre_handler: ProbeHandler::new(pre_handler),
            post_handler: ProbeHandler::new(post_handler),
            event_callback: None,
            fault_handler: None,
            probe_point: None,
            enable,
        }
    }

    pub fn with_fault_handler(mut self, func: fn(&dyn ProbeArgs)) -> Self {
        self.fault_handler = Some(ProbeHandler::new(func));
        self
    }

    pub fn with_probe_point(mut self, point: Arc<KprobePoint>) -> Self {
        self.probe_point = Some(point);
        self
    }

    pub fn with_event_callback(mut self, event_callback: Box<dyn CallBackFunc>) -> Self {
        self.event_callback = Some(event_callback);
        self
    }

    /// 获取探测点的地址
    ///
    /// 探测点的地址 == break指令的地址
    pub fn probe_addr(&self) -> usize {
        self.symbol_addr + self.offset
    }
}

pub struct KprobeBasic {
    symbol: Option<String>,
    symbol_addr: usize,
    offset: usize,
    pre_handler: ProbeHandler,
    post_handler: ProbeHandler,
    fault_handler: ProbeHandler,
    event_callback: Option<Box<dyn CallBackFunc>>,
    enable: bool,
}

pub trait CallBackFunc: Send + Sync {
    fn call(&self, trap_frame: &dyn ProbeArgs);
}

impl Debug for KprobeBasic {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Kprobe")
            .field("symbol", &self.symbol)
            .field("symbol_addr", &self.symbol_addr)
            .field("offset", &self.offset)
            .finish()
    }
}

impl KprobeBasic {
    pub fn call_pre_handler(&self, trap_frame: &dyn ProbeArgs) {
        self.pre_handler.call(trap_frame);
    }

    pub fn call_post_handler(&self, trap_frame: &dyn ProbeArgs) {
        self.post_handler.call(trap_frame);
    }

    pub fn call_fault_handler(&self, trap_frame: &dyn ProbeArgs) {
        self.fault_handler.call(trap_frame);
    }

    pub fn call_event_callback(&self, trap_frame: &dyn ProbeArgs) {
        if let Some(ref call_back) = self.event_callback {
            call_back.call(trap_frame);
        }
    }

    pub fn update_event_callback(&mut self, callback: Box<dyn CallBackFunc>) {
        self.event_callback = Some(callback);
    }

    pub fn disable(&mut self) {
        self.enable = false;
    }

    pub fn enable(&mut self) {
        self.enable = true;
    }

    pub fn is_enabled(&self) -> bool {
        self.enable
    }
    /// 返回探测点的函数名称
    pub fn symbol(&self) -> Option<&str> {
        self.symbol.as_deref()
    }
}

impl From<KprobeBuilder> for KprobeBasic {
    fn from(value: KprobeBuilder) -> Self {
        let fault_handler = value.fault_handler.unwrap_or(ProbeHandler::new(|_| {}));
        KprobeBasic {
            symbol: value.symbol,
            symbol_addr: value.symbol_addr,
            offset: value.offset,
            pre_handler: value.pre_handler,
            post_handler: value.post_handler,
            event_callback: value.event_callback,
            fault_handler,
            enable: value.enable,
        }
    }
}
