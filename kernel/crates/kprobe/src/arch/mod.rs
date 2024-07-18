use alloc::{boxed::Box, string::String};
use core::{any::Any, fmt::Debug};

#[cfg(target_arch = "riscv64")]
mod rv64;
#[cfg(target_arch = "x86_64")]
mod x86;

#[cfg(target_arch = "loongarch64")]
mod loongarch64;

#[cfg(target_arch = "loongarch64")]
pub use loongarch64::*;
#[cfg(target_arch = "riscv64")]
pub use rv64::*;
#[cfg(target_arch = "x86_64")]
pub use x86::*;

pub trait ProbeArgs: Send {
    fn as_any(&self) -> &dyn Any;
    fn break_address(&self) -> usize;
    fn debug_address(&self) -> usize;
}

pub trait KprobeOps: Send {
    /// Install the kprobe
    fn install(self) -> Self;
    /// The next instruction address
    fn return_address(&self) -> usize;
    /// The location of the instruction that needs to be single-stepped
    fn single_step_address(&self) -> usize;
    /// The instruction address that triggered the exception after single-step execution
    fn debug_address(&self) -> usize;
}

pub struct ProbeHandler {
    func: Box<fn(&dyn ProbeArgs)>,
}

impl ProbeHandler {
    pub fn new(func: fn(&dyn ProbeArgs)) -> Self {
        ProbeHandler {
            func: Box::new(func),
        }
    }
    pub fn call(&self, trap_frame: &dyn ProbeArgs) {
        (self.func)(trap_frame);
    }
}

pub struct KprobeBuilder {
    symbol: Option<String>,
    symbol_addr: Option<usize>,
    offset: Option<usize>,
    pre_handler: Option<ProbeHandler>,
    post_handler: Option<ProbeHandler>,
    fault_handler: Option<ProbeHandler>,
}

impl Default for KprobeBuilder {
    fn default() -> Self {
        Self::new()
    }
}
impl KprobeBuilder {
    pub fn new() -> Self {
        KprobeBuilder {
            symbol: None,
            symbol_addr: None,
            offset: None,
            pre_handler: None,
            post_handler: None,
            fault_handler: None,
        }
    }

    pub fn symbol(mut self, symbol: String) -> Self {
        self.symbol = Some(symbol);
        self
    }

    pub fn symbol_addr(mut self, symbol_addr: usize) -> Self {
        self.symbol_addr = Some(symbol_addr);
        self
    }

    pub fn offset(mut self, offset: usize) -> Self {
        self.offset = Some(offset);
        self
    }

    pub fn pre_handler(mut self, func: fn(&dyn ProbeArgs)) -> Self {
        self.pre_handler = Some(ProbeHandler::new(func));
        self
    }

    pub fn post_handler(mut self, func: fn(&dyn ProbeArgs)) -> Self {
        self.post_handler = Some(ProbeHandler::new(func));
        self
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

    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    pub fn kprobe_address(&self) -> usize {
        self.symbol_addr + self.offset
    }
}

impl From<KprobeBuilder> for KprobeBasic {
    fn from(value: KprobeBuilder) -> Self {
        let fault_handler = value
            .fault_handler
            .unwrap_or_else(|| ProbeHandler::new(|_| {}));
        KprobeBasic {
            symbol: value.symbol.unwrap(),
            symbol_addr: value.symbol_addr.unwrap(),
            offset: value.offset.unwrap(),
            pre_handler: value.pre_handler.unwrap(),
            post_handler: value.post_handler.unwrap(),
            fault_handler,
        }
    }
}
