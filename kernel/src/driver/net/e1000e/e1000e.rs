// 参考手册: PCIe* GbE Controllers Open Source Software Developer’s Manual
// Refernce: PCIe* GbE Controllers Open Source Software Developer’s Manual

use super::e1000e_driver::e1000e_driver_init;
use crate::driver::base::device::DeviceId;
use crate::driver::net::dma::{dma_alloc, dma_dealloc};
use crate::driver::net::irq_handle::DefaultNetIrqHandler;
use crate::driver::pci::pci::{
    get_pci_device_structure_mut, PciDeviceStructure, PciDeviceStructureGeneralDevice, PciError,
    PCI_DEVICE_LINKEDLIST,
};
use crate::driver::pci::pci_irq::{IrqCommonMsg, IrqSpecificMsg, PciInterrupt, PciIrqMsg, IRQ};
use crate::exception::IrqNumber;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::intrinsics::unlikely;
use core::mem::size_of;
use core::ptr::NonNull;
use core::slice::{from_raw_parts, from_raw_parts_mut};
use core::sync::atomic::{compiler_fence, Ordering};
use log::{debug, info};

use crate::libs::volatile::{ReadOnly, Volatile, WriteOnly};

const PAGE_SIZE: usize = 4096;
const NETWORK_CLASS: u8 = 0x2;
const ETHERNET_SUBCLASS: u8 = 0x0;
// e1000e系列网卡的device id列表，来源：https://admin.pci-ids.ucw.cz/read/PC/8086
const E1000E_DEVICE_ID: [u16; 14] = [
    0x10d3, // 8574L, qemu default
    0x10cc, // 82567LM-2
    0x10cd, // 82567LF-2
    0x105f, // 82571EB
    0x1060, // 82571EB
    0x107f, // 82572EI
    0x109a, // 82573L
    0x10ea, // 82577LM
    0x10eb, // 82577LC
    0x10ef, // 82578DM
    0x10f0, // 82578DC
    0x1502, // 82579LM
    0x1503, // 82579V
    0x150c, // 82583V
];

// e1000e网卡与BAR有关的常量
// BAR0空间大小(128KB)
const E1000E_BAR_REG_SIZE: u32 = 128 * 1024;
// BAR0空间对齐(64bit)
#[allow(dead_code)]
const E1000E_BAR_REG_ALIGN: u8 = 64;
// 单个寄存器大小(32bit, 4字节)
#[allow(dead_code)]
const E1000E_REG_SIZE: u8 = 4;

// TxBuffer和RxBuffer的大小(DMA页)
const E1000E_DMA_PAGES: usize = 1;

// 中断相关
const E1000E_RECV_VECTOR: IrqNumber = IrqNumber::new(57);

// napi队列中暂时存储的buffer个数
const E1000E_RECV_NAPI: usize = 1024;

// 收/发包的描述符结构 pp.24 Table 3-1
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
// pp.54 Table 3-12
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
#[derive(Copy, Clone)]
// Buffer的Copy只是指针操作，不涉及实际数据的复制，因此要小心使用，确保不同的buffer不会使用同一块内存
pub struct E1000EBuffer {
    buffer: NonNull<u8>,
    paddr: usize,
    // length字段为0则表示这个buffer是一个占位符，不指向实际内存
    // the buffer is empty and no page is allocated if length field is set 0
    length: usize,
}

impl E1000EBuffer {
    pub fn new(length: usize) -> Self {
        assert!(length <= PAGE_SIZE);
        if unlikely(length == 0) {
            // 在某些情况下，我们并不需要实际分配buffer，只需要提供一个占位符即可
            // we dont need to allocate dma pages for buffer in some cases
            E1000EBuffer {
                buffer: NonNull::dangling(),
                paddr: 0,
                length: 0,
            }
        } else {
            let (paddr, vaddr) = dma_alloc(E1000E_DMA_PAGES);
            E1000EBuffer {
                buffer: vaddr,
                paddr,
                length,
            }
        }
    }

    #[allow(dead_code)]
    pub fn as_addr(&self) -> NonNull<u8> {
        assert!(self.length != 0);
        return self.buffer;
    }

    #[allow(dead_code)]
    pub fn as_addr_u64(&self) -> u64 {
        assert!(self.length != 0);
        return self.buffer.as_ptr() as u64;
    }

    pub fn as_paddr(&self) -> usize {
        assert!(self.length != 0);
        return self.paddr;
    }

    #[allow(dead_code)]
    pub fn as_slice(&self) -> &[u8] {
        assert!(self.length != 0);
        return unsafe { from_raw_parts(self.buffer.as_ptr(), self.length) };
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        assert!(self.length != 0);
        return unsafe { from_raw_parts_mut(self.buffer.as_ptr(), self.length) };
    }

    pub fn set_length(&mut self, length: usize) {
        self.length = length;
    }

    pub fn len(&self) -> usize {
        return self.length;
    }
    // 释放buffer内部的dma_pages，需要小心使用
    pub fn free_buffer(self) {
        if self.length != 0 {
            unsafe { dma_dealloc(self.paddr, self.buffer, E1000E_DMA_PAGES) };
        }
    }
}

#[allow(dead_code)]
pub struct E1000EDevice {
    // 设备寄存器
    // device registers
    general_regs: NonNull<GeneralRegs>,
    interrupt_regs: NonNull<InterruptRegs>,
    rctl_regs: NonNull<ReceiveCtrlRegs>,
    receive_regs: NonNull<ReceiveRegs>,
    tctl_regs: NonNull<TransmitCtrlRegs>,
    transimit_regs: NonNull<TransimitRegs>,
    pcie_regs: NonNull<PCIeRegs>,

    // descriptor环形队列，在操作系统与设备之间共享
    // descriptor rings are shared between os and device
    recv_desc_ring: &'static mut [E1000ERecvDesc],
    trans_desc_ring: &'static mut [E1000ETransDesc],
    recv_ring_pa: usize,
    trans_ring_pa: usize,

    // 设备收/发包缓冲区数组
    // buffers of receive/transmit packets
    recv_buffers: Vec<E1000EBuffer>,
    trans_buffers: Vec<E1000EBuffer>,
    mac: [u8; 6],
    first_trans: bool,
    // napi队列，用于存放在中断关闭期间通过轮询收取的buffer
    // the napi queue is designed to save buffer/packet when the interrupt is close
    // NOTE: this feature is not completely implemented and not used in the current version
    napi_buffers: Vec<E1000EBuffer>,
    napi_buffer_head: usize,
    napi_buffer_tail: usize,
    napi_buffer_empty: bool,
}

impl E1000EDevice {
    // 从PCI标准设备进行驱动初始化
    // init the device for PCI standard device struct
    #[allow(unused_assignments)]
    pub fn new(
        device: Arc<PciDeviceStructureGeneralDevice>,
        device_id: Arc<DeviceId>,
    ) -> Result<Self, E1000EPciError> {
        // 从BAR0获取我们需要的寄存器
        // Build registers sturcts from BAR0
        device.bar_ioremap().unwrap()?;
        device.enable_master();
        let bar = device.bar().ok_or(E1000EPciError::BarGetFailed)?.read();
        let bar0 = bar.get_bar(0)?;
        let (address, size) = bar0
            .memory_address_size()
            .ok_or(E1000EPciError::UnexpectedBarType)?;
        if address == 0 {
            return Err(E1000EPciError::BarNotAllocated);
        }
        if size != E1000E_BAR_REG_SIZE {
            return Err(E1000EPciError::UnexpectedBarSize);
        }
        let vaddress = bar0
            .virtual_address()
            .ok_or(E1000EPciError::BarGetVaddrFailed)?
            .data() as u64;

        // 初始化msi中断
        // initialize msi interupt
        let irq_vector = device.irq_vector_mut().unwrap();
        irq_vector.write().push(E1000E_RECV_VECTOR);
        device.irq_init(IRQ::PCI_IRQ_MSI).expect("IRQ Init Failed");
        let msg = PciIrqMsg {
            irq_common_message: IrqCommonMsg::init_from(
                0,
                "E1000E_RECV_IRQ".to_string(),
                &DefaultNetIrqHandler,
                device_id,
            ),
            irq_specific_message: IrqSpecificMsg::msi_default(),
        };
        device.irq_install(msg)?;
        device.irq_enable(true)?;

        let general_regs: NonNull<GeneralRegs> =
            get_register_ptr(vaddress, E1000E_GENERAL_REGS_OFFSET);
        let interrupt_regs: NonNull<InterruptRegs> =
            get_register_ptr(vaddress, E1000E_INTERRRUPT_REGS_OFFSET);
        let rctl_regs: NonNull<ReceiveCtrlRegs> =
            get_register_ptr(vaddress, E1000E_RECEIVE_CTRL_REG_OFFSET);
        let receive_regs: NonNull<ReceiveRegs> =
            get_register_ptr(vaddress, E1000E_RECEIVE_REGS_OFFSET);
        let tctl_regs: NonNull<TransmitCtrlRegs> =
            get_register_ptr(vaddress, E1000E_TRANSMIT_CTRL_REG_OFFSET);
        let transimit_regs: NonNull<TransimitRegs> =
            get_register_ptr(vaddress, E1000E_TRANSMIT_REGS_OFFSET);
        let pcie_regs: NonNull<PCIeRegs> = get_register_ptr(vaddress, E1000E_PCIE_REGS_OFFSET);
        let ra_regs: NonNull<ReceiveAddressRegs> =
            get_register_ptr(vaddress, E1000E_RECEIVE_ADDRESS_REGS_OFFSET);
        // 开始设备初始化 14.3
        // Initialization Sequence
        unsafe {
            let mut ctrl = volread!(general_regs, ctrl);
            // 关闭中断
            // close the interrupt
            volwrite!(interrupt_regs, imc, E1000E_IMC_CLEAR);
            //SW RESET
            volwrite!(general_regs, ctrl, ctrl | E1000E_CTRL_RST);
            compiler_fence(Ordering::AcqRel);
            // PHY RESET
            ctrl = volread!(general_regs, ctrl);
            volwrite!(general_regs, ctrl, ctrl | E1000E_CTRL_PHY_RST);
            volwrite!(general_regs, ctrl, ctrl);
            // 关闭中断
            // close the interrupt
            volwrite!(interrupt_regs, imc, E1000E_IMC_CLEAR);
            let mut gcr = volread!(pcie_regs, gcr);
            gcr |= 1 << 22;
            volwrite!(pcie_regs, gcr, gcr);
            compiler_fence(Ordering::AcqRel);
            // PHY Initialization 14.8.1
            // MAC/PHY Link Setup 14.8.2
            ctrl = volread!(general_regs, ctrl);
            ctrl &= !(E1000E_CTRL_FRCSPD | E1000E_CTRL_FRCDPLX);
            volwrite!(general_regs, ctrl, ctrl | E1000E_CTRL_SLU);
        }
        let status = unsafe { volread!(general_regs, status) };
        debug!("Status: {status:#X}");

        // 读取设备的mac地址
        // Read mac address
        let ral = unsafe { volread!(ra_regs, ral0) };
        let rah = unsafe { volread!(ra_regs, rah0) };
        let mac: [u8; 6] = [
            (ral & 0xFF) as u8,
            ((ral >> 8) & 0xFF) as u8,
            ((ral >> 16) & 0xFF) as u8,
            ((ral >> 24) & 0xFF) as u8,
            (rah & 0xFF) as u8,
            ((rah >> 8) & 0xFF) as u8,
        ];
        // 初始化receive和transimit descriptor环形队列
        // initialize receive and transimit desciptor ring
        let (recv_ring_pa, recv_ring_va) = dma_alloc(E1000E_DMA_PAGES);
        let (trans_ring_pa, trans_ring_va) = dma_alloc(E1000E_DMA_PAGES);
        let recv_ring_length = PAGE_SIZE / size_of::<E1000ERecvDesc>();
        let trans_ring_length = PAGE_SIZE / size_of::<E1000ETransDesc>();

        let recv_desc_ring = unsafe {
            from_raw_parts_mut::<E1000ERecvDesc>(recv_ring_va.as_ptr().cast(), recv_ring_length)
        };
        let trans_desc_ring = unsafe {
            from_raw_parts_mut::<E1000ETransDesc>(trans_ring_va.as_ptr().cast(), trans_ring_length)
        };

        // 初始化receive和transmit packet的缓冲区
        // initialzie receive and transmit buffers
        let mut recv_buffers: Vec<E1000EBuffer> = Vec::with_capacity(recv_ring_length);
        let mut trans_buffers: Vec<E1000EBuffer> = Vec::with_capacity(trans_ring_length);

        // 初始化缓冲区与descriptor，descriptor 中的addr字典应当指向buffer的物理地址
        // Receive buffers of appropriate size should be allocated and pointers to these buffers should be stored in the descriptor ring.
        for ring in recv_desc_ring.iter_mut().take(recv_ring_length) {
            let buffer = E1000EBuffer::new(PAGE_SIZE);
            ring.addr = buffer.as_paddr() as u64;
            ring.status = 0;
            recv_buffers.push(buffer);
        }
        // Same as receive buffers
        for ring in trans_desc_ring.iter_mut().take(recv_ring_length) {
            let buffer = E1000EBuffer::new(PAGE_SIZE);
            ring.addr = buffer.as_paddr() as u64;
            ring.status = 1;
            trans_buffers.push(buffer);
        }

        // Receive Initialization 14.6
        // Initialzie mutlicast table array to 0b
        // 初始化MTA，遍历0x05200-0x053FC中每个寄存器，写入0b，一共128个寄存器
        let mut mta_adress = vaddress + E1000E_MTA_REGS_START_OFFSET;
        while mta_adress != vaddress + E1000E_MTA_REGS_END_OFFSET {
            let mta: NonNull<MTARegs> = get_register_ptr(mta_adress, 0);
            unsafe { volwrite!(mta, mta, 0) };
            mta_adress += 4;
        }
        // 连续的寄存器读-写操作，放在同一个unsafe块中
        unsafe {
            // 设置descriptor环形队列的基地址
            // Program the descriptor base address with the address of the region.
            volwrite!(receive_regs, rdbal0, (recv_ring_pa) as u32);
            volwrite!(receive_regs, rdbah0, (recv_ring_pa >> 32) as u32);
            // 设置descriptor环形队列的长度
            // Set the length register to the size of the descriptor ring.
            volwrite!(receive_regs, rdlen0, PAGE_SIZE as u32);
            // 设置队列的首尾指针
            // Program the head and tail registers
            volwrite!(receive_regs, rdh0, 0);
            volwrite!(receive_regs, rdt0, (recv_ring_length - 1) as u32);
            // 设置控制寄存器的相关功能 14.6.1
            // Set the receive control register
            volwrite!(
                rctl_regs,
                rctl,
                E1000E_RCTL_EN
                    | E1000E_RCTL_BAM
                    | E1000E_RCTL_BSIZE_4K
                    | E1000E_RCTL_BSEX
                    | E1000E_RCTL_SECRC
            );

            // Transmit Initialization 14.7
            // 开启发包descriptor的回写功能
            // Program the TXDCTL register with the desired TX descriptor write-back policy
            volwrite!(
                transimit_regs,
                txdctl,
                E1000E_TXDCTL_WTHRESH | E1000E_TXDCTL_GRAN
            );
            // 设置descriptor环形队列的基地址，长度与首尾指针
            // Program the descriptor base address with the address of the region
            volwrite!(transimit_regs, tdbal0, trans_ring_pa as u32);
            volwrite!(transimit_regs, tdbah0, (trans_ring_pa >> 32) as u32);
            // Set the length register to the size of the descriptor ring.
            volwrite!(transimit_regs, tdlen0, PAGE_SIZE as u32);
            // Program the head and tail registerss
            volwrite!(transimit_regs, tdh0, 0);
            volwrite!(transimit_regs, tdt0, 0);
            // Program the TIPG register
            volwrite!(
                tctl_regs,
                tipg,
                E1000E_TIPG_IPGT | E1000E_TIPG_IPGR1 | E1000E_TIPG_IPGR2
            );
            // Program the TCTL register.
            volwrite!(
                tctl_regs,
                tctl,
                E1000E_TCTL_EN | E1000E_TCTL_PSP | E1000E_TCTL_CT_VAL | E1000E_TCTL_COLD_VAL
            );

            let icr = volread!(interrupt_regs, icr);
            volwrite!(interrupt_regs, icr, icr);
            // 开启收包相关的中断
            // Enable receive interrupts
            let mut ims = volread!(interrupt_regs, ims);
            ims = E1000E_IMS_LSC | E1000E_IMS_RXT0 | E1000E_IMS_RXDMT0 | E1000E_IMS_OTHER;
            volwrite!(interrupt_regs, ims, ims);
        }
        return Ok(E1000EDevice {
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
            napi_buffers: vec![E1000EBuffer::new(0); E1000E_RECV_NAPI],
            napi_buffer_head: 0,
            napi_buffer_tail: 0,
            napi_buffer_empty: true,
        });
    }
    pub fn e1000e_receive(&mut self) -> Option<E1000EBuffer> {
        self.e1000e_intr();
        let mut rdt = unsafe { volread!(self.receive_regs, rdt0) } as usize;
        let index = (rdt + 1) % self.recv_desc_ring.len();
        let desc = &mut self.recv_desc_ring[index];
        if (desc.status & E1000E_RXD_STATUS_DD) == 0 {
            return None;
        }
        let mut buffer = self.recv_buffers[index];
        let new_buffer = E1000EBuffer::new(PAGE_SIZE);
        self.recv_buffers[index] = new_buffer;
        desc.addr = new_buffer.as_paddr() as u64;
        buffer.set_length(desc.len as usize);
        rdt = index;
        unsafe { volwrite!(self.receive_regs, rdt0, rdt as u32) };
        // debug!("e1000e: receive packet");
        return Some(buffer);
    }

    pub fn e1000e_can_transmit(&self) -> bool {
        let tdt = unsafe { volread!(self.transimit_regs, tdt0) } as usize;
        let index = tdt % self.trans_desc_ring.len();
        let desc = &self.trans_desc_ring[index];
        if (desc.status & E1000E_TXD_STATUS_DD) == 0 {
            return false;
        }
        true
    }

    pub fn e1000e_transmit(&mut self, packet: E1000EBuffer) {
        let mut tdt = unsafe { volread!(self.transimit_regs, tdt0) } as usize;
        let index = tdt % self.trans_desc_ring.len();
        let desc = &mut self.trans_desc_ring[index];
        let buffer = self.trans_buffers[index];
        self.trans_buffers[index] = packet;
        // recycle unused transmit buffer
        buffer.free_buffer();
        // Set the transmit descriptor
        desc.addr = packet.as_paddr() as u64;
        desc.len = packet.len() as u16;
        desc.status = 0;
        desc.cmd = E1000E_TXD_CMD_EOP | E1000E_TXD_CMD_RS | E1000E_TXD_CMD_IFCS;
        tdt = (tdt + 1) % self.trans_desc_ring.len();
        unsafe { volwrite!(self.transimit_regs, tdt0, tdt as u32) };
        self.first_trans = false;
    }
    pub fn mac_address(&self) -> [u8; 6] {
        return self.mac;
    }
    // 向ICR寄存器中的某一bit写入1b表示该中断已经被接收，同时会清空该位
    // we need to clear ICR to tell e1000e we have read the interrupt
    pub fn e1000e_intr(&mut self) {
        let icr = unsafe { volread!(self.interrupt_regs, icr) };
        // write 1b to any bit in ICR will clear the bit
        unsafe { volwrite!(self.interrupt_regs, icr, icr) };
    }

    // 切换是否接受分组到达的中断
    // change whether the receive timer interrupt is enabled
    // Note: this method is not completely implemented and not used in the current version
    #[allow(dead_code)]
    pub fn e1000e_intr_set(&mut self, state: bool) {
        let mut ims = unsafe { volread!(self.interrupt_regs, ims) };
        match state {
            true => ims |= E1000E_IMS_RXT0,
            false => ims &= !E1000E_IMS_RXT0,
        }
        unsafe { volwrite!(self.interrupt_regs, ims, ims) };
    }

    // 实现了一部分napi机制的收包函数, 现在还没有投入使用
    // This method is a partial implementation of napi (New API) techniques
    // Note: this method is not completely implemented and not used in the current version
    #[allow(dead_code)]
    pub fn e1000e_receive2(&mut self) -> Option<E1000EBuffer> {
        // 向设备表明我们已经接受到了之前的中断
        // Tell e1000e we have received the interrupt
        self.e1000e_intr();
        // 如果napi队列不存在已经收到的分组...
        // if napi queue is empty...
        if self.napi_buffer_empty {
            // 暂时关闭设备中断
            // close interrupt
            self.e1000e_intr_set(false);
            loop {
                if self.napi_buffer_tail == self.napi_buffer_head && !self.napi_buffer_empty {
                    // napi缓冲队列已满，停止收包
                    // napi queue is full, stop
                    break;
                }
                match self.e1000e_receive() {
                    Some(buffer) => {
                        self.napi_buffers[self.napi_buffer_tail] = buffer;
                        self.napi_buffer_tail = (self.napi_buffer_tail + 1) % E1000E_RECV_NAPI;
                        self.napi_buffer_empty = false;
                    }
                    None => {
                        // 设备队列中没有剩余的已到达的数据包
                        // no packet remains in the device buffer
                        break;
                    }
                };
            }
            // 重新打开设备中断
            // open the interrupt
            self.e1000e_intr_set(true);
        }

        let result = self.napi_buffers[self.napi_buffer_head];
        match result.len() {
            0 => {
                // napi队列和网卡队列中都不存在数据包
                // both napi queue and device buffer is empty, no packet will receive
                return None;
            }
            _ => {
                // 有剩余的已到达的数据包
                // there is packet in napi queue
                self.napi_buffer_head = (self.napi_buffer_head + 1) % E1000E_RECV_NAPI;
                if self.napi_buffer_head == self.napi_buffer_tail {
                    self.napi_buffer_empty = true;
                }
                return Some(result);
            }
        }
    }
}

impl Drop for E1000EDevice {
    fn drop(&mut self) {
        // 释放已分配的所有dma页
        // free all dma pages we have allocated
        debug!("droping...");
        let recv_ring_length = PAGE_SIZE / size_of::<E1000ERecvDesc>();
        let trans_ring_length = PAGE_SIZE / size_of::<E1000ETransDesc>();
        unsafe {
            // 释放所有buffer中的dma页
            // free all dma pages in buffers
            for i in 0..recv_ring_length {
                self.recv_buffers[i].free_buffer();
            }
            for i in 0..trans_ring_length {
                self.trans_buffers[i].free_buffer();
            }
            // 释放descriptor ring
            // free descriptor ring
            dma_dealloc(
                self.recv_ring_pa,
                NonNull::new(self.recv_desc_ring).unwrap().cast(),
                E1000E_DMA_PAGES,
            );
            dma_dealloc(
                self.trans_ring_pa,
                NonNull::new(self.trans_desc_ring).unwrap().cast(),
                E1000E_DMA_PAGES,
            );
        }
    }
}

pub fn e1000e_init() {
    match e1000e_probe() {
        Ok(code) => {
            if code == 1 {
                info!("Successfully init e1000e device!");
            }
        }
        Err(error) => {
            info!("Failed to init e1000e device: {error:?}");
        }
    }
}

pub fn e1000e_probe() -> Result<u64, E1000EPciError> {
    let list = &*PCI_DEVICE_LINKEDLIST;
    let result = get_pci_device_structure_mut(list, NETWORK_CLASS, ETHERNET_SUBCLASS);
    if result.is_empty() {
        return Ok(0);
    }
    let mut initialized = false;
    for device in result {
        let standard_device = device.as_standard_device().unwrap();
        if standard_device.common_header.vendor_id == 0x8086 {
            // intel
            if E1000E_DEVICE_ID.contains(&standard_device.common_header.device_id) {
                debug!(
                    "Detected e1000e PCI device with device id {:#x}",
                    standard_device.common_header.device_id
                );

                // todo: 根据pci的path来生成device id
                let e1000e = E1000EDevice::new(
                    standard_device.clone(),
                    DeviceId::new(
                        None,
                        Some(format!(
                            "e1000e_{}",
                            standard_device.common_header.device_id
                        )),
                    )
                    .unwrap(),
                )?;
                e1000e_driver_init(e1000e);
                initialized = true;
            }
        }
    }

    if initialized {
        Ok(1)
    } else {
        Ok(0)
    }
}

// 用到的e1000e寄存器结构体
// pp.275, Table 13-3
// 设备通用寄存器
#[allow(dead_code)]
struct GeneralRegs {
    ctrl: Volatile<u32>,         //0x00000
    ctrl_alias: Volatile<u32>,   //0x00004
    status: ReadOnly<u32>,       //0x00008
    status_align: ReadOnly<u32>, //0x0000c
    eec: Volatile<u32>,          //0x00010
    eerd: Volatile<u32>,         //0x00014
    ctrl_ext: Volatile<u32>,     //0x00018
    fla: Volatile<u32>,          //0x0001c
    mdic: Volatile<u32>,         //0x00020
}
// 中断控制
#[allow(dead_code)]
struct InterruptRegs {
    icr: Volatile<u32>, //0x000c0 ICR寄存器应当为只读寄存器，但我们需要向其中写入来清除对应位
    itr: Volatile<u32>, //0x000c4
    ics: WriteOnly<u32>, //0x000c8
    ics_align: ReadOnly<u32>, //0x000cc
    ims: Volatile<u32>, //0x000d0
    ims_align: ReadOnly<u32>, //0x000d4
    imc: WriteOnly<u32>, //0x000d8
}
// 收包功能控制
struct ReceiveCtrlRegs {
    rctl: Volatile<u32>, //0x00100
}
// 发包功能控制
#[allow(dead_code)]
struct TransmitCtrlRegs {
    tctl: Volatile<u32>,     //0x00400
    tctl_ext: Volatile<u32>, //0x00404
    unused_1: ReadOnly<u32>, //0x00408
    unused_2: ReadOnly<u32>, //0x0040c
    tipg: Volatile<u32>,     //0x00410
}
// 收包功能相关
#[allow(dead_code)]
struct ReceiveRegs {
    rdbal0: Volatile<u32>,     //0x02800
    rdbah0: Volatile<u32>,     //0x02804
    rdlen0: Volatile<u32>,     //0x02808
    rdl_align: ReadOnly<u32>,  //0x0280c
    rdh0: Volatile<u32>,       //0x02810
    rdh_align: ReadOnly<u32>,  //0x02814
    rdt0: Volatile<u32>,       //0x02818
    rdt_align: ReadOnly<u32>,  //0x281c
    rdtr: Volatile<u32>,       //0x2820
    rdtr_align: ReadOnly<u32>, //0x2824
    rxdctl: Volatile<u32>,     //0x2828
}
// 发包功能相关
#[allow(dead_code)]
struct TransimitRegs {
    tdbal0: Volatile<u32>,      //0x03800
    tdbah0: Volatile<u32>,      //0x03804
    tdlen0: Volatile<u32>,      //0x03808
    tdlen_algin: ReadOnly<u32>, //0x0380c
    tdh0: Volatile<u32>,        //0x03810
    tdh_align: ReadOnly<u32>,   //0x03814
    tdt0: Volatile<u32>,        //0x03818
    tdt_align: ReadOnly<u32>,   //0x0381c
    tidv: Volatile<u32>,        //0x03820
    tidv_align: ReadOnly<u32>,  //0x03824
    txdctl: Volatile<u32>,      //0x03828
    tadv: Volatile<u32>,        //0x0382c
}
// mac地址
struct ReceiveAddressRegs {
    ral0: Volatile<u32>, //0x05400
    rah0: Volatile<u32>, //0x05404
}
// PCIe 通用控制
struct PCIeRegs {
    gcr: Volatile<u32>, //0x05b00
}
#[allow(dead_code)]
struct StatisticsRegs {}

// 0x05200-0x053fc
// 在Receive Initialization 中按照每次一个32bit寄存器的方式来遍历
// Multicast Table Array Registers will be written per 32bit
struct MTARegs {
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
const E1000E_CTRL_FRCSPD: u32 = 1 << 11;
const E1000E_CTRL_FRCDPLX: u32 = 1 << 12;
const E1000E_CTRL_RST: u32 = 1 << 26;
#[allow(dead_code)]
const E1000E_CTRL_RFCE: u32 = 1 << 27;
#[allow(dead_code)]
const E1000E_CTRL_TFCE: u32 = 1 << 28;
const E1000E_CTRL_PHY_RST: u32 = 1 << 31;

// IMS
const E1000E_IMS_LSC: u32 = 1 << 2;
const E1000E_IMS_RXDMT0: u32 = 1 << 4;
#[allow(dead_code)]
const E1000E_IMS_RXO: u32 = 1 << 6;
const E1000E_IMS_RXT0: u32 = 1 << 7;
#[allow(dead_code)]
const E1000E_IMS_RXQ0: u32 = 1 << 20;
const E1000E_IMS_OTHER: u32 = 1 << 24; // qemu use this bit to set msi-x interrupt

// IMC
const E1000E_IMC_CLEAR: u32 = 0xffffffff;

// RCTL
const E1000E_RCTL_EN: u32 = 1 << 1;
const E1000E_RCTL_BAM: u32 = 1 << 15;
const E1000E_RCTL_BSIZE_4K: u32 = 3 << 16;
const E1000E_RCTL_BSEX: u32 = 1 << 25;
const E1000E_RCTL_SECRC: u32 = 1 << 26;

// TCTL
const E1000E_TCTL_EN: u32 = 1 << 1;
const E1000E_TCTL_PSP: u32 = 1 << 3;
const E1000E_TCTL_CT_VAL: u32 = 0x0f << 4; // suggested 16d collision, 手册建议值：16d
const E1000E_TCTL_COLD_VAL: u32 = 0x03f << 12; // suggested 64 byte time for Full-Duplex, 手册建议值：64
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

/// E1000E驱动初始化过程中可能的错误
#[allow(dead_code)]
#[derive(Debug)]
pub enum E1000EPciError {
    // 获取到错误类型的BAR（IO BAR）
    // An IO BAR was provided rather than a memory BAR.
    UnexpectedBarType,
    // 获取的BAR没有被分配到某个地址(address == 0)
    // A BAR which we need was not allocated an address(address == 0).
    BarNotAllocated,
    //获取虚拟地址失败
    BarGetVaddrFailed,
    // 没有对应的BAR或者获取BAR失败
    BarGetFailed,
    // BAR的大小与预期不符(128KB)
    // Size of BAR is not 128KB
    UnexpectedBarSize,
    Pci(PciError),
}

/// PCI error到VirtioPciError的转换，层层上报
impl From<PciError> for E1000EPciError {
    fn from(error: PciError) -> Self {
        Self::Pci(error)
    }
}

/**
 * @brief 获取基地址的某个偏移量的指针，用于在mmio bar中构造寄存器结构体
 * @brief used for build register struct in mmio bar
 * @param vaddr: base address (in virtual memory)
 * @param offset: offset
 */
fn get_register_ptr<T>(vaddr: u64, offset: u64) -> NonNull<T> {
    NonNull::new((vaddr + offset) as *mut T).unwrap()
}
