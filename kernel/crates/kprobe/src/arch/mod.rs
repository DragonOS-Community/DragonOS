use alloc::string::String;
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

pub trait ProbeArgs: Send {
    /// 用于使用者转换到特定架构下的TrapFrame
    fn as_any(&self) -> &dyn Any;
    /// 返回导致break异常的地址
    fn break_address(&self) -> usize;
    /// 返回导致单步执行异常的地址
    fn debug_address(&self) -> usize;
}

pub trait KprobeOps: Send {
    /// # 安装kprobe
    ///
    /// 不同的架构下需要保存原指令，然后替换为断点指令
    fn install(self) -> Self;
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
    symbol: String,
    symbol_addr: usize,
    offset: usize,
    pre_handler: ProbeHandler,
    post_handler: ProbeHandler,
    fault_handler: Option<ProbeHandler>,
}

impl KprobeBuilder {
    pub fn new(
        symbol: String,
        symbol_addr: usize,
        offset: usize,
        pre_handler: fn(&dyn ProbeArgs),
        post_handler: fn(&dyn ProbeArgs),
    ) -> Self {
        KprobeBuilder {
            symbol,
            symbol_addr,
            offset,
            pre_handler: ProbeHandler::new(pre_handler),
            post_handler: ProbeHandler::new(post_handler),
            fault_handler: None,
        }
    }

    pub fn fault_handler(mut self, func: fn(&dyn ProbeArgs)) -> Self {
        self.fault_handler = Some(ProbeHandler::new(func));
        self
    }
}

pub struct KprobeBasic {
    symbol: String,
    symbol_addr: usize,
    offset: usize,
    pre_handler: ProbeHandler,
    post_handler: ProbeHandler,
    fault_handler: ProbeHandler,
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

    /// 返回探测点的函数名称
    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    /// 计算探测点的地址
    pub fn kprobe_address(&self) -> usize {
        self.symbol_addr + self.offset
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
            fault_handler,
        }
    }
}
