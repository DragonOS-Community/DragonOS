use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
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
    probe_point: Option<Arc<KprobePoint>>,
}

impl KprobeBuilder {
    pub fn new(
        symbol: Option<String>,
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
            probe_point: None,
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
            fault_handler,
        }
    }
}

/// 管理所有的kprobe探测点
#[derive(Debug, Default)]
pub struct KprobeManager {
    break_list: BTreeMap<usize, Vec<Arc<Kprobe>>>,
    debug_list: BTreeMap<usize, Vec<Arc<Kprobe>>>,
}

impl KprobeManager {
    pub const fn new() -> Self {
        KprobeManager {
            break_list: BTreeMap::new(),
            debug_list: BTreeMap::new(),
        }
    }
    /// # 插入一个kprobe
    ///
    /// ## 参数
    /// - `kprobe`: kprobe的实例
    pub fn insert_kprobe(&mut self, kprobe: Arc<Kprobe>) {
        let probe_point = kprobe.probe_point();
        self.insert_break_point(probe_point.break_address(), kprobe.clone());
        self.insert_debug_point(probe_point.debug_address(), kprobe);
    }

    /// # 向break_list中插入一个kprobe
    ///
    /// ## 参数
    /// - `address`: kprobe的地址, 由`KprobePoint::break_address()`或者`KprobeBuilder::probe_addr()`返回
    /// - `kprobe`: kprobe的实例
    fn insert_break_point(&mut self, address: usize, kprobe: Arc<Kprobe>) {
        let list = self.break_list.entry(address).or_default();
        list.push(kprobe);
    }

    /// # 向debug_list中插入一个kprobe
    ///
    /// ## 参数
    /// - `address`: kprobe的单步执行地址，由`KprobePoint::debug_address()`返回
    /// - `kprobe`: kprobe的实例
    fn insert_debug_point(&mut self, address: usize, kprobe: Arc<Kprobe>) {
        let list = self.debug_list.entry(address).or_default();
        list.push(kprobe);
    }

    pub fn get_break_list(&self, address: usize) -> Option<&Vec<Arc<Kprobe>>> {
        self.break_list.get(&address)
    }

    pub fn get_debug_list(&self, address: usize) -> Option<&Vec<Arc<Kprobe>>> {
        self.debug_list.get(&address)
    }

    /// # 返回一个地址上注册的kprobe数量
    ///
    /// ## 参数
    /// - `address`: kprobe的地址, 由`KprobePoint::break_address()`或者`KprobeBuilder::probe_addr()`返回
    pub fn kprobe_num(&self, address: usize) -> usize {
        self.break_list_len(address)
    }

    #[inline]
    fn break_list_len(&self, address: usize) -> usize {
        self.break_list
            .get(&address)
            .map(|list| list.len())
            .unwrap_or(0)
    }
    #[inline]
    fn debug_list_len(&self, address: usize) -> usize {
        self.debug_list
            .get(&address)
            .map(|list| list.len())
            .unwrap_or(0)
    }

    /// # 移除一个kprobe
    ///
    /// ## 参数
    /// - `kprobe`: kprobe的实例
    pub fn remove_kprobe(&mut self, kprobe: &Arc<Kprobe>) {
        let probe_point = kprobe.probe_point();
        self.remove_one_break(probe_point.break_address(), kprobe);
        self.remove_one_debug(probe_point.debug_address(), kprobe);
    }

    /// # 从break_list中移除一个kprobe
    ///
    /// 如果没有其他kprobe注册在这个地址上，则删除列表
    ///
    /// ## 参数
    /// - `address`: kprobe的地址, 由`KprobePoint::break_address()`或者`KprobeBuilder::probe_addr()`返回
    /// - `kprobe`: kprobe的实例
    fn remove_one_break(&mut self, address: usize, kprobe: &Arc<Kprobe>) {
        if let Some(list) = self.break_list.get_mut(&address) {
            list.retain(|x| !Arc::ptr_eq(x, kprobe));
        }
        if self.break_list_len(address) == 0 {
            self.break_list.remove(&address);
        }
    }

    /// # 从debug_list中移除一个kprobe
    ///
    /// 如果没有其他kprobe注册在这个地址上，则删除列表
    ///
    /// ## 参数
    /// - `address`: kprobe的单步执行地址，由`KprobePoint::debug_address()`返回
    /// - `kprobe`: kprobe的实例
    fn remove_one_debug(&mut self, address: usize, kprobe: &Arc<Kprobe>) {
        if let Some(list) = self.debug_list.get_mut(&address) {
            list.retain(|x| !Arc::ptr_eq(x, kprobe));
        }
        if self.debug_list_len(address) == 0 {
            self.debug_list.remove(&address);
        }
    }
}
