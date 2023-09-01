use core::mem::size_of;
use core::ptr::NonNull;
use core::sync::atomic::{compiler_fence, Ordering};
use core::slice::{from_raw_parts_mut, from_raw_parts};
use alloc::vec::Vec;

use crate::driver::pci::pci::{
    PciDeviceStructure, PciDeviceStructureGeneralDevice, PciError,PCI_DEVICE_LINKEDLIST, get_pci_device_structure_mut,
};
use crate::libs::volatile::{Volatile, VolatileReadable, VolatileWritable, ReadOnly, WriteOnly};
use crate::{kdebug, kerror, kwarn, kinfo};
use crate::driver::net::dma::{dma_alloc, dma_dealloc};
use super::e1000e_driver::e1000e_driver_init;

const PAGE_SIZE: usize = 4096;
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
struct E1000ETransDesc {
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

pub struct E1000EDevice{
    // 设备寄存器
    general_regs: NonNull<GeneralRegs>,
    interrupt_regs: NonNull<InterruptRegs>,
    rctl_regs: NonNull<ReceiveCtrlRegs>,
    receive_regs: NonNull<ReceiveRegs>,
    tctl_regs: NonNull<TransmitCtrlRegs>,
    transimit_regs: NonNull<TransimitRegs>,
    pcie_regs: NonNull<PCIeRegs>,

    // descriptor环形队列，在操作系统与设备之间共享
    recv_desc_ring: &'static mut [E1000ERecvDesc],
    trans_desc_ring: &'static mut [E1000ETransDesc],
    recv_ring_pa: usize,
    trans_ring_pa: usize,

    // 设备收/发包缓冲区指针数组
    recv_buffers: Vec<NonNull<u8>>,
    trans_buffers: Vec<NonNull<u8>>,

    mac: [u8; 6],
    first_trans: bool,
}


impl E1000EDevice{
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
        
        // 打算用个函数包装一下
        let general_regs: NonNull<GeneralRegs> = get_register_ptr(vaddress, E1000E_GENERAL_REGS_OFFSET);
        let interrupt_regs: NonNull<InterruptRegs> = get_register_ptr(vaddress, E1000E_INTERRRUPT_REGS_OFFSET);
        let rctl_regs: NonNull<ReceiveCtrlRegs> = get_register_ptr(vaddress, E1000E_RECEIVE_CTRL_REG_OFFSET);
        let receive_regs: NonNull<ReceiveRegs> = get_register_ptr(vaddress, E1000E_RECEIVE_REGS_OFFSET);
        let tctl_regs: NonNull<TransmitCtrlRegs> = get_register_ptr(vaddress, E1000E_TRANSMIT_CTRL_REG_OFFSET);
        let transimit_regs: NonNull<TransimitRegs> = get_register_ptr(vaddress, E1000E_TRANSMIT_REGS_OFFSET);
        let pcie_regs: NonNull<PCIeRegs> = get_register_ptr(vaddress, E1000E_PCIE_REGS_OFFSET);
        let ra_regs: NonNull<ReceiveAddressRegs> = get_register_ptr(vaddress, E1000E_RECEIVE_ADDRESS_REGS_OFFSET);
        unsafe{

            let status = volread!(general_regs, status);
            let ctrl = volread!(general_regs, ctrl);
            kdebug!("STATUS:{status}, CTRL:{ctrl}");

            // 关闭中断
            volwrite!(interrupt_regs, imc, 0xffffffff);

            //SW RESET
            let ctrl_val = ctrl | E1000E_CTRL_RST;
            volwrite!(general_regs, ctrl, ctrl_val);
            compiler_fence(Ordering::AcqRel);

            // 关闭中断
            volwrite!(interrupt_regs, imc, 0xffffffff);

            let mut gcr = volread!(pcie_regs, gcr);
            gcr = gcr | (1 << 22);
            volwrite!(pcie_regs, gcr, gcr);

            compiler_fence(Ordering::AcqRel);

            // PHY Initialization
            // MAC/PHY Link Setup
            let mut ctrl = volread!(general_regs, ctrl);
            ctrl = ctrl | E1000E_CTRL_SLU;
            volwrite!(general_regs, ctrl, ctrl);

            let status = volread!(general_regs, status);
            kdebug!("Status: {status:#X}");

            // 读取设备的mac地址
            let ral = volread!(ra_regs, ral0);
            let rah = volread!(ra_regs, rah0);
            let mut mac: [u8; 6] = [0x00; 6];
            for i in 0..4{
                mac[i] = ((ral & (0xff << (i * 8))) >> (i * 8)) as u8;
            }
            for i in 0..2{
                mac[i + 4] = ((rah & (0xff << (i * 8))) >> (i * 8)) as u8
            }
            // 初始化receive和transimit descriptor环形队列
            let (recv_ring_pa, recv_ring_va) = dma_alloc(1);
            let (trans_ring_pa, trans_ring_va) = dma_alloc(1);
            let recv_ring_length = PAGE_SIZE / size_of::<E1000ERecvDesc>();
            let trans_ring_length = PAGE_SIZE / size_of::<E1000ETransDesc>();

            let recv_desc_ring = from_raw_parts_mut::<E1000ERecvDesc
            >(recv_ring_va.as_ptr().cast(), recv_ring_length);
            let trans_desc_ring = from_raw_parts_mut::<E1000ETransDesc>(trans_ring_va.as_ptr().cast(), trans_ring_length);

            // 初始化receive和transmit packet的缓冲区，元素表示packet的虚拟地址，为了确保内存一致性，所有的buffer都在驱动初始化程序中分配dma内存页
            let mut recv_buffers: Vec<NonNull<u8>> = Vec::with_capacity(recv_ring_length);
            let mut trans_buffers: Vec<NonNull<u8>> = Vec::with_capacity(trans_ring_length); 

            // Receive Initialization
            // 初始化MTA，遍历0x05200-0x053FC中每个寄存器，写入0，一共128个寄存器
            let mut mta_adress = vaddress + E1000E_MTA_REGS_START_OFFSET;
            while mta_adress != vaddress + E1000E_MTA_REGS_END_OFFSET{
                let mta: NonNull<MTARegs> = get_register_ptr(mta_adress, 0);
                volwrite!(mta, mta, 0);
                mta_adress = mta_adress + 4;
            }
            // Receive buffers of appropriate size should be allocated and pointers to these buffers should be stored in the descriptor ring.
            for i in 0..recv_ring_length{
                let (buffer_pa, buffer_va) = dma_alloc(1);
                recv_desc_ring[i].addr = buffer_pa as u64;
                recv_desc_ring[i].status = 0;
                recv_buffers.push(buffer_va);
            }
            // Program the descriptor base address with the address of the region.
            volwrite!(receive_regs, rdbal0, (recv_ring_pa) as u32);
            volwrite!(receive_regs, rdbah0, (recv_ring_pa >> 32) as u32);
            // Set the length register to the size of the descriptor ring.
            volwrite!(receive_regs, rdlen0, PAGE_SIZE as u32);

            // Program the head and tail registers
            volwrite!(receive_regs, rdh0, 0);
            volwrite!(receive_regs, rdt0, (recv_ring_length - 1) as u32);

            // Set the receive control register
            volwrite!(rctl_regs, rctl, E1000E_RCTL_EN | E1000E_RCTL_BAM | E1000E_RCTL_BSIZE_4K | E1000E_RCTL_BSEX | E1000E_RCTL_SECRC);

            // Enable receive interrupts
            let mut ims = volread!(interrupt_regs, ims);
            ims = ims | E1000E_IMS_LSC | E1000E_IMS_RXO | E1000E_IMS_RXT0 | E1000E_IMS_RXDMT0;
            volwrite!(interrupt_regs, ims, ims);

            // Transmit Initialization
            // 
            for i in 0..trans_ring_length{
                let (buffer_pa, buffer_va) = dma_alloc(1);
                trans_desc_ring[i].addr = buffer_pa as u64;
                //trans_desc_ring[i].status = 0;
                trans_buffers.push(buffer_va);
            }
            // Program the TXDCTL register with the desired TX descriptor write-back policy
            volwrite!(transimit_regs, txdctl, E1000E_TXDCTL_WTHRESH | E1000E_TXDCTL_GRAN);
            // Program the descriptor base address with the address of the region
            volwrite!(transimit_regs, tdbal0, trans_ring_pa as u32);
            volwrite!(transimit_regs, tdbah0, (trans_ring_pa >> 32) as u32);
            // Set the length register to the size of the descriptor ring.
            volwrite!(transimit_regs, tdlen0, PAGE_SIZE as u32);
            // Program the head and tail registerss
            volwrite!(transimit_regs, tdh0, 0);
            volwrite!(transimit_regs, tdt0, 0);
            // Program the TIPG register
            volwrite!(tctl_regs, tipg, E1000E_TIPG_IPGT | E1000E_TIPG_IPGR1 | E1000E_TIPG_IPGR2);
            // Program the TCTL register.
            volwrite!(tctl_regs, tctl, E1000E_TCTL_EN | E1000E_TCTL_PSP | E1000E_TCTL_CT_VAL | E1000E_TCTL_COLD_VAL);
            return Ok(E1000EDevice{
                general_regs,
                interrupt_regs,
                rctl_regs,
                receive_regs,
                tctl_regs,
                transimit_regs,
                pcie_regs,
                recv_desc_ring,
                trans_desc_ring,
                recv_ring_pa,
                trans_ring_pa,
                recv_buffers,
                trans_buffers,
                mac,
                first_trans: true,
            });
        }
        // 开始网卡初始化流程

    }
    pub fn e1000e_receive(&mut self) -> Option<Vec<u8>>{
        unsafe{
            let mut rdt = volread!(self.receive_regs, rdt0) as usize;
            let index = (rdt + 1) % self.recv_desc_ring.len();
            let desc = &mut self.recv_desc_ring[index];
            if (desc.status & E1000E_RXD_STATUS_DD) == 0 {
                return None;
            }
            let buffer = from_raw_parts(self.recv_buffers[index].as_ptr() as *const u8, desc.len as usize);
            rdt = index;
            volwrite!(self.receive_regs, rdt0, rdt as u32);
            kdebug!("e1000e: receive packet");
            return Some(buffer.to_vec());
        }
    }

    pub fn e1000e_can_transmit(&self) -> bool{
        unsafe{
            let tdt = volread!(self.transimit_regs, tdt0) as usize;
            //kdebug!("e1000e: tdt:{tdt}");
            let index = tdt % self.trans_desc_ring.len();
            let desc = &self.trans_desc_ring[index];
            if (desc.status & E1000E_TXD_STATUS_DD) == 0 {
                // 不知道为什么，e1000e设备没有在descriptor中回写status，所以这个函数不会返回false
                kdebug!("dd!!");
            }
            true
        }
    }

    pub fn e1000e_transmit(&mut self, packet: &[u8]){
        unsafe{
            let mut tdt = volread!(self.transimit_regs, tdt0) as usize;
            let index = tdt % self.trans_desc_ring.len();
            let desc = &mut self.trans_desc_ring[index];
            // Copy data from packet to transmit buffer
            kdebug!("addr:{:#x}", self.trans_buffers[index].as_ptr() as u64);
            let buffer = from_raw_parts_mut(self.trans_buffers[index].as_ptr(), packet.len());
            buffer.copy_from_slice(packet);
            // Set the transmit descriptor
            desc.len = packet.len() as u16;
            //desc.status = 0;
            desc.cmd = E1000E_TXD_CMD_EOP | E1000E_TXD_CMD_RS | E1000E_TXD_CMD_IFCS;
            tdt = (tdt + 1) % self.trans_desc_ring.len();
            volwrite!(self.transimit_regs, tdt0, tdt as u32);
            self.first_trans = false;
        }   
    }
    pub fn mac_address(&self) -> [u8; 6]{
        return self.mac;
    }
}

impl Drop for E1000EDevice{
    fn drop(&mut self) {
        // 释放已分配的所有dma buffer
        kdebug!("droping...");
        let recv_ring_length = PAGE_SIZE / size_of::<E1000ERecvDesc>();
        let trans_ring_length = PAGE_SIZE / size_of::<E1000ETransDesc>();
        unsafe{
            for i in 0..recv_ring_length{
                dma_dealloc(self.recv_desc_ring[i].addr as usize, self.recv_buffers[i], 1);
            }
            for i in 0..trans_ring_length{
                dma_dealloc(self.trans_desc_ring[i].addr as usize, self.trans_buffers[i], 1);
            }
            // 释放descriptor ring
            dma_dealloc(self.recv_ring_pa, NonNull::new(self.recv_desc_ring).unwrap().cast(), 1);
            dma_dealloc(self.trans_ring_pa, NonNull::new(self.trans_desc_ring).unwrap().cast(), 1);
        }

        
    }
}

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
            let e1000e = E1000EDevice::new(standard_device)?;
            e1000e_driver_init(e1000e);
            // loop{
            //     match e1000e.e1000e_receive(){
            //         Some(pkt) =>{
            //             if (e1000e.e1000e_can_transmit() == true){
            //                 kdebug!("can trans");
            //             }
            //             e1000e.e1000e_transmit(&pkt);
            //             kdebug!("receive");
            //         }
            //         None => {
            //             //kdebug!("nothing");
            //         }
            //     }
            // }
        }
    }

    return Ok(1);
}



// 用到的e1000e寄存器的偏移量
// Table 13-3
// 状态与中断控制
struct GeneralRegs{
    ctrl: Volatile<u32>, //0x00000
    ctrl_alias: Volatile<u32>, //0x00004
    status: ReadOnly<u32>, //0x00008
    status_align: ReadOnly<u32>, //0x0000c
    eec: Volatile<u32>, //0x00010
    eerd: Volatile<u32>, //0x00014
    ctrl_ext: Volatile<u32>, //0x00018
    fla: Volatile<u32>, //0x0001c
    mdic: Volatile<u32>, //0x00020
}

struct InterruptRegs{
    icr: ReadOnly<u32>, //0x000c0
    itr: Volatile<u32>, //0x000c4
    ics: WriteOnly<u32>, //0x000c8
    ics_align: ReadOnly<u32>, //0x000cc
    ims: Volatile<u32>, //0x000d0
    ims_align: ReadOnly<u32>, //0x000d4
    imc: WriteOnly<u32>, //0x000d8
}

struct ReceiveCtrlRegs{
    rctl: Volatile<u32>, //0x00100
}

struct TransmitCtrlRegs{
    tctl: Volatile<u32>, //0x00400
    tctl_ext: Volatile<u32>, //0x00404
    unused_1: ReadOnly<u32>, //0x00408
    unused_2: ReadOnly<u32>, //0x0040c
    tipg: Volatile<u32>, //0x00410
}
struct ReceiveRegs{
    rdbal0: Volatile<u32>, //0x02800
    rdbah0: Volatile<u32>, //0x02804
    rdlen0: Volatile<u32>, //0x02808
    rdl_align: ReadOnly<u32>, //0x0280c
    rdh0: Volatile<u32>, //0x02810
    rdh_align: ReadOnly<u32>, //0x02814
    rdt0: Volatile<u32>, //0x02818
    rdt_align: ReadOnly<u32>, //0x281c
    rdtr: Volatile<u32>, //0x2820
    rdtr_align: ReadOnly<u32>, //0x2824
    rxdctl: Volatile<u32>, //0x2828
 }

struct TransimitRegs{
    tdbal0: Volatile<u32>, //0x03800
    tdbah0: Volatile<u32>, //0x03804
    tdlen0: Volatile<u32>, //0x03808
    tdlen_algin: ReadOnly<u32>, //0x0380c
    tdh0: Volatile<u32>, //0x03810
    tdh_align: ReadOnly<u32>, //0x03814
    tdt0: Volatile<u32>, //0x03818
    tdt_align: ReadOnly<u32>, //0x0381c
    tidv: Volatile<u32>, //0x03820
    tidv_align: ReadOnly<u32>, //0x03824
    txdctl: Volatile<u32>, //0x03828
    tadv: Volatile<u32>, //0x0382c
}

struct ReceiveAddressRegs{
    ral0: Volatile<u32>, //0x05400
    rah0: Volatile<u32>, //0x05404
}
struct PCIeRegs{
    gcr: Volatile<u32>, //0x05b00
}
struct StatisticsRegs{

}

// 0x05200-0x053fc
// 在Receive Initialization 中按照每次一个32bit寄存器的方式来遍历
struct MTARegs{
    mta: Volatile<u32>,
}

const E1000E_GENERAL_REGS_OFFSET: u64 = 0x00000;
const E1000E_INTERRRUPT_REGS_OFFSET: u64 = 0x000c0;
const E1000E_RECEIVE_CTRL_REG_OFFSET: u64 = 0x00100;
const E1000E_RECEIVE_REGS_OFFSET: u64 = 0x02800;
const E1000E_TRANSMIT_CTRL_REG_OFFSET: u64 = 0x00400;
const E1000E_TRANSMIT_REGS_OFFSET: u64 = 0x03800;
const E1000E_RECEIVE_ADDRESS_REGS_OFFSET: u64 = 0x05400;
const E1000E_PCIE_REGS_OFFSET: u64 = 0x05b00;
const E1000E_MTA_REGS_START_OFFSET: u64 = 0x05200;
const E1000E_MTA_REGS_END_OFFSET: u64 = 0x053fc;
// 寄存器的特定位
//CTRL
const E1000E_CTRL_SLU: u32 = 1 << 6;
const E1000E_CTRL_RST: u32 = 1 << 26; 
const E1000E_CTRL_RFCE: u32 = 1 << 27;
const E1000E_CTRL_TFCE: u32 = 1 << 28;

// IMS
const E1000E_IMS_LSC: u32 = 1 << 2;
const E1000E_IMS_RXDMT0: u32 = 1 << 4;
const E1000E_IMS_RXO: u32 = 1 << 6;
const E1000E_IMS_RXT0: u32 = 1 << 7;
const E1000E_IMS_RXQ0: u32 = 1 << 20;

// MDIC
const E1000E_MDIC_OP_W: u32 = 1 << 26;
const E1000E_MDIC_OP_R: u32 = 1 << 27;
const E1000E_MDIC_READY: u32 = 1 << 28;
const E1000E_MDIC_INTERRUPT: u32 = 1 << 29;
const E1000E_MDIC_ERROR: u32 = 1 << 30;

// RCTL
const E1000E_RCTL_EN: u32 = 1 << 1;
const E1000E_RCTL_BAM: u32 = 1 << 15;
const E1000E_RCTL_BSIZE_4K: u32 = 3 << 16;
const E1000E_RCTL_BSEX: u32 = 1 << 25;
const E1000E_RCTL_SECRC: u32 = 1 << 26;

// TCTL
const E1000E_TCTL_EN: u32 = 1 << 1;
const E1000E_TCTL_PSP: u32 = 1 << 3;
const E1000E_TCTL_CT_VAL: u32 = 0x0f << 4; // suggested 16d collision
const E1000E_TCTL_COLD_VAL: u32 = 0x03f << 12; // suggested 64 byte time for Full-Duplex
// TXDCTL
const E1000E_TXDCTL_WTHRESH: u32 = 1 << 16;
const E1000E_TXDCTL_GRAN: u32 = 1 << 24;
// TIPG
const E1000E_TIPG_IPGT: u32 = 8;
const E1000E_TIPG_IPGR1: u32 = 2 << 10;
const E1000E_TIPG_IPGR2: u32 = 10 << 20;

// RxDescriptorStatus
const E1000E_RXD_STATUS_DD: u16 = 1 << 0;

// TxDescriptorStatus
const E1000E_TXD_STATUS_DD: u8 = 1 << 0;
const E1000E_TXD_CMD_EOP: u8 = 1 << 0;
const E1000E_TXD_CMD_IFCS: u8 = 1 << 1;
const E1000E_TXD_CMD_RS: u8 = 1 << 3;


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

// 用于读写PHY内部寄存器MDI方法，因为驱动还不支持手动设置参数，暂时不需要使用
enum MDIError{
    // MDIC bit 30 is set to 1b by hardware when it fails to complete an MDI read 
    MDIReadFailed,
    // MDIC bit 28 is set to 1b by hardware at the end of the MDI transaction
    MDINotReady,
    // PHY register address is too large
    MDIRegAddrOutOfRange,
    // Write data is too large
    MDIWriteDataOutOfRange,
}

fn get_register_ptr<T>(vaddr: u64, offset: u64) -> NonNull<T>{
    NonNull::new((vaddr + offset) as *mut T).unwrap()
}

unsafe fn mdi_write(regs: NonNull<GeneralRegs>, reg_addr: u32, data: u32) -> Result<(), MDIError>{
    let mut mdic = volread!(regs, mdic);
    if (data >> 16) != 0 {
        return Err(MDIError::MDIWriteDataOutOfRange);
    }
    if (reg_addr >> 5) != 0{
        return Err(MDIError::MDIRegAddrOutOfRange);
    }
    if (mdic & E1000E_MDIC_READY) == 0{
        return Err(MDIError::MDINotReady);
    }
    // clear the ready bit
    mdic = mdic & (!E1000E_MDIC_READY);
    // clear the error bit
    mdic = mdic & (!E1000E_MDIC_ERROR);
    // clear the data and regaddr field
    mdic = (mdic >> 21) << 21;
    // set the regaddr field
    mdic = mdic | (reg_addr << 16);
    // set the data field
    mdic = mdic | data;
    // set the op field
    mdic = mdic | E1000E_MDIC_OP_W;

    volwrite!(regs, mdic, mdic);
    // wait 64 microseconds for the command to complete
    return Ok(());
}

unsafe fn mdi_read(regs: NonNull<GeneralRegs>, reg_addr: u32) -> Result<u16, MDIError>{
    let mut mdic = volread!(regs, mdic);
    if (mdic & E1000E_MDIC_READY) == 0{
        return Err(MDIError::MDINotReady);
    }
    // clear the ready bit
    mdic = mdic & (!E1000E_MDIC_READY);
    // clear the error bit
    mdic = mdic & (!E1000E_MDIC_ERROR);
    // clear the data and regaddr field
    mdic = (mdic >> 21) << 21;
    // set the regaddr field
    mdic = mdic | (reg_addr << 16);
    // set the op field
    mdic = mdic | E1000E_MDIC_OP_R;

    volwrite!(regs, mdic, mdic);
    // wait 64 microseconds for the command to complete
    mdic = volread!(regs, mdic);
    if (mdic & E1000E_MDIC_ERROR) == 0{
        return Err(MDIError::MDIReadFailed);
    }
    let data = (mdic & 0xffff) as u16;
    return Ok(data)
}