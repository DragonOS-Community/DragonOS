use core::mem::{size_of, align_of};
use core::num::{NonZeroU16, NonZeroU32};
use core::ptr::{NonNull, self};
use core::slice;

use smoltcp::phy;
use virtio_drivers::device;
use x86::current::registers;

use crate::driver::pci::pci::{
    BusDeviceFunction, PciDeviceStructure, PciDeviceStructureGeneralDevice, PciError,
    PciStandardDeviceBar, PCI_CAP_ID_VNDR, PciDeviceLinkedList, PCI_DEVICE_LINKEDLIST, get_pci_device_structure_mut,
};
use crate::libs::volatile::{Volatile, VolatileReadable, VolatileWritable, ReadOnly};
use crate::time::{sleep, TimeSpec};
use crate::{kdebug, kerror, kwarn, kinfo};

use super::dma::{dma_alloc, dma_dealloc};

const NETWORK_CLASS: u8 = 0x2;
const ETHERNET_SUBCLASS: u8 = 0x0;
// e1000e系列网卡的device id列表，来源：https://admin.pci-ids.ucw.cz/read/PC/8086
const E1000E_DEVICE_ID: [u16; 3] = [0x10d3, 0x10cc, 0x10cd];

// e1000e网卡与BAR有关的常量
// 寄存器BAR索引(BAR0)
const E1000E_BAR_REG_INDEX: u8 = 0;
// BAR0空间大小(128KB)
const E1000E_BAR_REG_SIZE: u32 = 128 * 1024;
// BAR0空间对齐(64bit)
const E1000E_BAR_REG_ALIGN: u8 = 64;
// 单个寄存器大小(32bit, 4字节)
const E1000E_REG_SIZE: u8 = 4;

// TxBuffer和RxBuffer的大小(DMA页)
const E1000E_DMA_PAGES: usize = 1;

#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct E1000ESendDesc {
    addr: u64,
    len: u16,
    cso: u8,
    cmd: u8,
    status: u8,
    css: u8,
    special: u8,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct E1000ERecvDesc {
    addr: u64,
    len: u16,
    chksum: u16,
    status: u16,
    error: u8,
    special: u8,
}

struct E1000EDriver{
    registers: &'static mut [Volatile<u32>],

}

struct Status{
    status:ReadOnly<u32>,
}

impl E1000EDriver{
    // 从PCI标准设备进行驱动初始化
    pub fn new(device: &mut PciDeviceStructureGeneralDevice) -> Result<Self, E1000EPciError> {
        kdebug!("Initializiing...");
        device.bar_ioremap().unwrap()?;
        device.enable_master();
        let bar = device.bar().ok_or(E1000EPciError::BarGetFailed)?;

        // 初始化和后续操作需要的寄存器都在BAR0中
        // 从BAR0构造我们需要的寄存器切片
        let bar0 = bar.get_bar(0)?;
        let (address, size) = bar0.memory_address_size().ok_or(E1000EPciError::UnexpectedBarType)?;
        if address == 0{
            return Err(E1000EPciError::BarNotAllocated);
        }
        if size != E1000E_BAR_REG_SIZE{
            return Err(E1000EPciError::UnexpectedBarSize);
        }
        let vaddress = bar0.virtual_address().ok_or(E1000EPciError::BarGetVaddrFailed)?;

        kdebug!("BAR0 address {address:#X}, size {size}, vaddr: {vaddress:#X}");
        
        let v = NonNull::new(vaddress as *mut Volatile<u32>).unwrap();
        let device = NonNull::new((vaddress + 0x8) as *mut Status).unwrap();
        let m = unsafe {
            volread!(device, status)
        };
        kdebug!("STATUS:{m}");
        let registers = get_bar_slice(v, E1000E_BAR_REG_SIZE / E1000E_REG_SIZE as u32);
        unsafe{
            let ctrl = vaddress as *const u32;
            let ims = (vaddress + (E1000E_IMS * 4) as u64) as *mut u32;
            let mut t = volatile_read!(*ctrl);
            kdebug!("CTRL:{}", t);
            let mut i = volatile_read!(*ims);
            kdebug!("IMS:{}", i);
            volatile_write!(*ims, i | 1);
            i = volatile_read!(*ims);
            kdebug!("IMS:{}", i);
            let new_val = t | E1000E_CTRL_RST | 1 << 5; //CTRL_ASDE
            kdebug!("new val:{new_val}");
            let ctrl_m = vaddress as *mut u32;
            volatile_write!(*ctrl_m, new_val);
            t = volatile_read!(*ctrl);
            kdebug!("CTRL:{t}");
            i = volatile_read!(*ims);
            kdebug!("IMS:{}", i);
        }
        // 开始网卡初始化流程
        return Ok(E1000EDriver { registers });
    }
    fn e1000e_init(&mut self) -> (){
        unsafe{
            let status = &self.registers[E1000E_STATUS] as *const Volatile<u32>;
            let mut k = volread(&self.registers[E1000E_STATUS]);
            let vaddr = status as usize;
            kdebug!("STATUS REG BEFORE SETUP:{k}, vaddr: {vaddr:#X}");
            volset(&mut self.registers[E1000E_CTRL], 1<<27, true);
            let t = volread(&self.registers[E1000E_CTRL]);
            kdebug!("CTRL:{t}");
            // let t = volread(&self.registers[E1000E_RDT0]);
            // kdebug!("RDT0:{t}");
            // volwrite(&mut self.registers[E1000E_RDT0], 1);
            // let t = volread(&self.registers[E1000E_RDT0]);
            // kdebug!("RDT0:{t}");
            // Initialization Sequence 4.6
            // For 82574L Only
            // Disable Interrupts
            // Set 1b in IMC register to clear interupt
            // volwrite(&mut self.registers[E1000E_IMC], 0xffffffff);
            // Issue a global reset by setting bit 0 of the CTRL register
            volset(&mut self.registers[E1000E_CTRL], E1000E_CTRL_RST, true);
            let time = TimeSpec{tv_sec:0, tv_nsec:3000};
            match sleep::nanosleep(time){
                Ok(timeSpec)=>{
                    kdebug!("sleep remain: {timeSpec:?}");
                }
                Err(errorCode)=>{
                    kwarn!("sleep error{errorCode:?}");
                }
            }
            let t = volread(&self.registers[E1000E_CTRL]);
            kdebug!("CTRL:{t}");
            // Set 1b in IMC register to clear interupt
            //volwrite(&mut self.registers[E1000E_IMC], 0xffffffff);
            // General Configuration
            //volset(&mut self.registers[E1000E_GCR], 1 << 22, true);
            // PHY and link setup
            k = volread(&self.registers[E1000E_STATUS]);
            kdebug!("STATUS REG BEFORE PHY SETUP:{k}");
            
            // Initialization of Statistics

            // Apply for DMA pages
            let (phys_addr, virt_addr) = dma_alloc(E1000E_DMA_PAGES);
            // Receive Initialization

            // Transmit Initialization

            // Enable Interrupts
        }
    }
}

// impl phy::Device for E1000EDriver{
//     // Token
//     // type RxToken<'a> = E1000ERecvDesc where Self:'a;
//     fn receive(&mut self, timestamp: smoltcp::time::Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        
//     }

//     fn transmit(&mut self, timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        
//     }

//     fn capabilities(&self) -> phy::DeviceCapabilities {
//         // 需要修改
//         let mut caps = phy::DeviceCapabilities::default();
//         // 网卡的最大传输单元. 请与IP层的MTU进行区分。这个值应当是网卡的最大传输单元，而不是IP层的MTU。
//         caps.max_transmission_unit = 2000;
//         /*
//            Maximum burst size, in terms of MTU.
//            The network device is unable to send or receive bursts large than the value returned by this function.
//            If None, there is no fixed limit on burst size, e.g. if network buffers are dynamically allocated.
//         */
//         caps.max_burst_size = Some(1);
//         return caps;
//     }
// }

#[no_mangle]
pub extern "C" fn rs_e1000e_init(){
    e1000e_init();
}

pub fn e1000e_init() -> (){
    match e1000e_probe(){
        Ok(code) => kinfo!("Successfully init!"),
        Err(error) => kinfo!("Error occurred!"),
    }
}

pub fn e1000e_probe() -> Result<u64, E1000EPciError>{
    kdebug!("start probe e1000e device...");
    let mut list = PCI_DEVICE_LINKEDLIST.write();
    let result = get_pci_device_structure_mut(&mut list, NETWORK_CLASS, ETHERNET_SUBCLASS);
    if result.is_empty(){
        return Ok(0);
    }
    kdebug!("Successfully get list");
    for device in result{
        let standard_device = device.as_standard_device_mut().unwrap();
        let header = &standard_device.common_header;
        if header.vendor_id == 0x8086{
            // if header.device_id == 0x108b || header.device_id == 0x108c || header.device_id == 0x109A{
            kdebug!("Detected e1000e PCI device with device id {}", header.device_id);
            // }
            let mut e1000e = E1000EDriver::new(standard_device)?;
            // e1000e.e1000e_init();
        }
    }
    return Ok(1);
}

fn _e1000e_init(device: &mut PciDeviceStructureGeneralDevice) -> Result<(), PciError>{
    let header = &device.common_header;
    let bar = &device.bar().unwrap();
    for i in 0..5{
        let bar0 = bar.get_bar(i).unwrap();
        kinfo!("{bar0}");
    }
    // let registers_address_size = bar0.memory_address_size().unwrap();
    // let (bar_address, bar_size) = registers_address_size;
    // kinfo!("bar_address: {bar_address}");
    // kinfo!("bar_size: {bar_size}");
    // 寄存器切片
    Ok(())
}

fn get_bar_slice<T>(addr: NonNull<T>, len: u32) -> &'static mut [T]{
    unsafe{
        slice::from_raw_parts_mut(addr.as_ptr(), len as usize)
    }
}

// 用到的e1000e寄存器的偏移量
// Table 13-3
// 状态与中断控制
const E1000E_CTRL: usize = 0x00000 / 4;
const E1000E_STATUS: usize = 0x00008 / 4;
const E1000E_ICR: usize = 0x000C0 / 4;
const E1000E_ICS: usize = 0x000C8 / 4;
const E1000E_IMS: usize = 0x000D0 / 4;
const E1000E_IMC: usize = 0x000D8 / 4;
// receive buffer
const E1000E_RCTL: usize = 0x00100 / 4;
const E1000E_RDBAL0: usize = 0x00110 / 4;
const E1000E_RDBAH0: usize = 0x00114 / 4;
const E1000E_RDH0: usize = 0x00120 / 4;
const E1000E_RDT0: usize = 0x00128 / 4; 

const E1000E_GCR: usize = 0x05B00 / 4;
// 寄存器的特定位
const E1000E_CTRL_RST: u32 = 1 << 26; 


// E1000E驱动初始化过程中可能的错误
pub enum E1000EPciError{
        /// An IO BAR was provided rather than a memory BAR.
        UnexpectedBarType,
        /// A BAR which we need was not allocated an address.
        BarNotAllocated,
        ///获取虚拟地址失败
        BarGetVaddrFailed,
        // 没有对应的BAR或者获取BAR失败
        BarGetFailed,
        // BAR的大小与预期不符(128KB)
        UnexpectedBarSize,
        Pci(PciError),
}

/// PCI error到VirtioPciError的转换，层层上报
impl From<PciError> for E1000EPciError {
    fn from(error: PciError) -> Self {
        Self::Pci(error)
    }
}

unsafe fn volwrite<T:Copy>(addr: &mut Volatile<T>, value: T){
    let addr = addr as *mut Volatile<T>;
    addr.vwrite(value);
}

unsafe fn volread<T:Copy>(addr: &Volatile<T>) -> T {
    let addr = addr as *const Volatile<T>;
    return addr.vread();
}

unsafe fn volset(addr: &mut Volatile<u32>, bit: u32, flag: bool){
    let value = volread(addr);
    match flag{
        true => {
            volwrite(addr, value | bit);
        }
        false => {
            volwrite(addr, value & (!bit));
        }
    }
}