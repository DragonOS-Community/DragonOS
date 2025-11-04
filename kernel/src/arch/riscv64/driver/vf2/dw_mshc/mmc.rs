// Driver for Synopsys DesignWare Mobile Storage Host Controller

use alloc::{
    string::String,
    string::ToString,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    any::Any,
    cell::UnsafeCell,
    fmt::Debug,
    mem::{self, size_of},
};

use crate::arch::MMArch;
use crate::driver::base::block::block_device::{BlockDevice, BlockId, GeneralBlockRange};
use crate::driver::base::block::disk_info::Partition;
use crate::driver::base::block::manager::BlockDevMeta;
use crate::driver::base::class::Class;
use crate::driver::base::device::{
    bus::Bus, device_number::Major, driver::Driver, DevName, Device, DeviceCommonData, DeviceType,
    IdTable,
};
use crate::driver::base::kobject::{
    KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState,
};
use crate::driver::base::kset::KSet;
use crate::driver::block::cache::BLOCK_SIZE;
use crate::filesystem::devfs::LockedDevFSInode;
use crate::filesystem::{
    devfs::{DevFS, DeviceINode},
    kernfs::KernFSInode,
    mbr::MbrDiskPartionTable,
    vfs::{syscall::ModeType, IndexNode, Metadata},
};
use crate::libs::{
    rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
    spinlock::{SpinLock, SpinLockGuard},
};
use crate::mm::allocator::page_frame::PhysPageFrame;
use crate::mm::mmio_buddy::{mmio_pool, MMIOSpaceGuard};
use crate::mm::{MemoryManagementArch, PhysAddr};
use byte_slice_cast::*;
use log::{debug, info, warn};
use system_error::SystemError;

use super::registers::{
    CtypeCardWidth, BLKSIZ, BMOD, BYTCNT, CDETECT, CID, CLKDIV, CLKENA, CMD, CMDARG, CTRL, CTYPE,
    DBADDRL, DBADDRU, IDSTS, PWREN, RESP, RINSTS, STATUS,
};

macro_rules! wait_for {
    ($cond:expr) => {{
        let mut timeout = 10000000;
        while !$cond && timeout > 0 {
            core::hint::spin_loop();
            timeout -= 1;
        }
    }};
}

#[derive(Debug)]
pub struct MMC {
    blkdev_meta: BlockDevMeta,
    inner: SpinLock<InnerMMC>,
    locked_kobj_state: LockedKObjectState,
    fifo_offset: UnsafeCell<usize>,
    _frames: UnsafeCell<Vec<PhysPageFrame>>,
    self_ref: Weak<Self>,
    mmio_virt_base: usize,
    _mmio_guard: MMIOSpaceGuard,

    fs: RwLock<Weak<DevFS>>,
    metadata: Metadata,
}

struct InnerMMC {
    _name: Option<String>,
    _device_common: DeviceCommonData,
    kobject_common: KObjectCommonData,
}

impl Debug for InnerMMC {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InnerMMC").finish()
    }
}

unsafe impl Send for MMC {}
unsafe impl Sync for MMC {}

#[allow(dead_code)]
impl MMC {
    pub fn new(base_address: usize, size: usize, _interrupt_number: usize) -> Arc<Self> {
        let page_offset = base_address % MMArch::PAGE_SIZE;
        let paddr = base_address - page_offset;

        let mmio_guard = mmio_pool().create_mmio(size).unwrap();
        unsafe { mmio_guard.map_phys(PhysAddr::new(paddr), size) }.unwrap();

        let vaddr = mmio_guard.vaddr() + page_offset;

        let dev = Arc::new_cyclic(|self_ref| Self {
            blkdev_meta: BlockDevMeta::new(
                DevName::new("sdio".to_string(), 0),
                Major::MMC_BLK_MAJOR,
            ),
            self_ref: self_ref.clone(),
            locked_kobj_state: LockedKObjectState::default(),
            inner: SpinLock::new(InnerMMC {
                _name: None,
                _device_common: DeviceCommonData::default(),
                kobject_common: KObjectCommonData::default(),
            }),
            fs: RwLock::new(Weak::default()),
            metadata: Metadata::new(
                crate::filesystem::vfs::FileType::BlockDevice,
                ModeType::from_bits_truncate(0o755),
            ),
            fifo_offset: UnsafeCell::new(0x600),
            _frames: UnsafeCell::new(Vec::new()),
            mmio_virt_base: vaddr.data(),
            _mmio_guard: mmio_guard,
        });

        dev
    }

    fn inner(&self) -> SpinLockGuard<InnerMMC> {
        self.inner.lock()
    }

    pub fn card_init(&self) {
        info!("====================== SDIO Init START ========================");

        info!("Card detect: {:b}", self.card_detect());
        info!("Power enable: {:b}", self.power_enable().power_enable());
        info!("Clock enable: {:b}", self.clock_enable().cclk_enable());
        info!("Card 0 width: {:?}", self.card_width(0));
        info!("Control register: {:?}", self.control_reg());
        info!("DMA enabled: {}", self.dma_enabled());
        info!(
            "Descriptor base address: {:x}",
            self.descriptor_base_address()
        );

        let card_idx = 0;
        // 0xAA is check pattern, see https://elixir.bootlin.com/linux/v6.4-rc7/source/drivers/mmc/core/sd_ops.c#L162
        const TEST_PATTERN: u32 = 0xAA;

        // Read clock divider
        info!("Read clock divider");
        let base = self.virt_base_address() as *mut CLKDIV;
        let clkdiv = unsafe { base.byte_add(CLKDIV::offset()).read_volatile() };
        info!("Clock divider: {:?}", clkdiv.clks());

        self.reset_clock();
        self.reset_fifo();
        self.set_controller_bus_width(card_idx, CtypeCardWidth::Width1);
        self.set_dma(false); // Disable DMA
        info!("Control register: {:?}", self.control_reg());

        let cmd = CMD::reset_cmd0(0);
        self.send_cmd(cmd, CMDARG::empty(), None, false);

        // SDIO Check
        // info!("SDIO Check");
        // // CMD5
        // let cmd = CMD::no_data_cmd(card_idx, 5);
        // let cmdarg = CMDARG::empty();
        // if self.send_cmd(cmd, cmdarg).is_none() {
        //     info!("No response from card, not SDIO");
        // }

        // Voltage check and SDHC 2.0 check
        info!("Voltage Check");
        // CMD8
        let cmd = CMD::no_data_cmd(card_idx, 8);
        let cmdarg = CMDARG::from((1 << 8) | TEST_PATTERN);
        let resp = self
            .send_cmd(cmd, cmdarg, None, false)
            .expect("Error sending command");
        if (resp.resp(0) & TEST_PATTERN) == 0 {
            warn!("Card {} unusable", card_idx);
        }

        // If card responses, consider it SD

        // Send ACMD41 to power up
        loop {
            // Send CMD55 before ACMD
            let cmd = CMD::no_data_cmd(card_idx, 55);
            let cmdarg = CMDARG::empty();
            self.send_cmd(cmd, cmdarg, None, false);
            let cmd = CMD::no_data_cmd_no_crc(card_idx, 41); // CRC is all 1 bit by design
            let cmdarg = CMDARG::from((1 << 30) | (1 << 24) | 0xFF8000);
            if let Some(resp) = self.send_cmd(cmd, cmdarg, None, false) {
                if resp.ocr() & (1 << 31) != 0 {
                    info!("Card {} powered up", card_idx);
                    if resp.ocr() & (1 << 30) != 0 {
                        info!("Card {} is high capacity", card_idx);
                    }
                    break;
                }
            }
            for _ in 0..100000 {
                core::hint::spin_loop();
            }
        }

        // CMD2, get CID
        let cmd = CMD::no_data_cmd_no_crc(card_idx, 2).with_response_length(true); // R2 response, no CRC
        if let Some(resp) = self.send_cmd(cmd, CMDARG::empty(), None, false) {
            let cid = CID::from(resp.resps_u128());
            info!("CID: {:x?}", cid);
            info!(
                "Card Name: {}",
                core::str::from_utf8(cid.name().to_be_bytes().as_byte_slice()).unwrap()
            );
        }

        // CMD3, get RCA
        let cmd = CMD::no_data_cmd(card_idx, 3);
        let resp = self
            .send_cmd(cmd, CMDARG::empty(), None, false)
            .expect("Error executing CMD3");
        let rca = resp.resp(0) >> 16; // RCA[31:16]
        info!("Card status: {:x?}", resp.resp(0) & 0xFFFF);

        // CMD9, get CSD
        let cmd = CMD::no_data_cmd_no_crc(card_idx, 9).with_response_length(true); // R2 response, no CRC
        let cmdarg = CMDARG::from(rca << 16);
        self.send_cmd(cmd, cmdarg, None, false);

        // CMD7 select card
        let cmd = CMD::no_data_cmd(card_idx, 7);
        let cmdarg = CMDARG::from(rca << 16);
        let _resp = self
            .send_cmd(cmd, cmdarg, None, false)
            .expect("Error executing CMD7");

        info!("Current FIFO count: {}", self.fifo_filled_cnt());

        // ACMD51 get bus width
        // Send CMD55 before ACMD
        let cmd = CMD::no_data_cmd(card_idx, 55);
        let cmdarg = CMDARG::from(rca << 16);
        self.send_cmd(cmd, cmdarg, None, false); // RCA is required
        self.set_size(8, 8); // Set transfer size
        let cmd = CMD::data_cmd(card_idx, 51);
        let mut buffer: [usize; 64] = [0; 64]; // 512B
        self.send_cmd(cmd, CMDARG::empty(), Some(&mut buffer), true);
        info!("Current FIFO count: {}", self.fifo_filled_cnt());
        let resp = u64::from_be(self.read_fifo::<u64>());
        info!("Bus width supported: {:b}", (resp >> 48) & 0xF);

        // CMD16 set block length
        // let cmd = CMD::no_data_cmd(card_idx, 16);
        // let cmdarg = CMDARG::from(512);
        // self.send_cmd(cmd, cmdarg);

        info!("Current FIFO count: {}", self.fifo_filled_cnt());

        // Read one block
        self.set_size(512, 512);
        let cmd = CMD::data_cmd(card_idx, 17);
        let cmdarg = CMDARG::empty();
        let _resp = self
            .send_cmd(cmd, cmdarg, Some(&mut buffer), true)
            .expect("Error sending command");

        info!("Current FIFO count: {}", self.fifo_filled_cnt());

        let cmdarg = CMDARG::from(153);
        let _resp = self
            .send_cmd(cmd, cmdarg, Some(&mut buffer), true)
            .expect("Error sending command");
        debug!("Magic: 0x{:x}", buffer[0]);
        info!("Current FIFO count: {}", self.fifo_filled_cnt());

        // Try DMA

        /*
        // Allocate a page for descriptor table
        let (paddr, count) = unsafe { allocate_page_frames(PageFrameCount::ONE).unwrap() };
        let descriptor_page_paddr: PhysAddr = paddr;
        unsafe { &mut *self.frames.get() }.push(frame);
        let descriptor_page_vaddr = descriptor_page_paddr.to_vaddr().bits();
        const DESCRIPTOR_CNT: usize = 2;
        let mut buffer_page_paddr: [usize; DESCRIPTOR_CNT] = [0; DESCRIPTOR_CNT];
        for i in 0..DESCRIPTOR_CNT {
            let frame = unsafe { allocate_page_frames(PageFrameCount::ONE) };
            buffer_page_paddr[i] = frame.ppn.to_paddr().bits();
            unsafe { &mut *self.frames.get() }.push(frame);
        }
        let _descriptor_table = unsafe {
            core::slice::from_raw_parts_mut(
                descriptor_page_vaddr as *mut Descriptor,
                DESCRIPTOR_CNT,
            )
        };
         */

        // Build chain descriptor
        // for idx in 0..descriptor_cnt {
        //     descriptor_table[idx] = Descriptor::new(
        //         512,
        //         buffer_page_paddr[idx],
        //         descriptor_page_paddr + (idx + 1) % descriptor_cnt * 16, // 16B for
        // each     );
        // }
        // // Set descriptor base address
        // self.set_descript_base_address(descriptor_page_paddr);

        // // Enable DMA
        // self.set_dma(true);

        // // Read one block
        // let buffer = unsafe {
        //     core::slice::from_raw_parts_mut(
        //         kernel_phys_to_virt(buffer_page_paddr[0]) as *mut usize,
        //         64,
        //     )
        // };
        // debug!("Magic before: 0x{:x}", buffer[0]);
        // let cmdarg = CMDARG::from(0x200);
        // let resp = self.send_cmd(cmd, cmdarg, None).expect("Error sending command");

        // debug!("Magic after: 0x{:x}", buffer[0]);

        info!("Control register: {:?}", self.control_reg());
        let base = self.virt_base_address() as *mut u32;
        let rinsts: RINSTS = unsafe { base.byte_add(RINSTS::offset()).read_volatile() }.into();
        // Clear interrupt by writing 1
        unsafe {
            // Just clear all for now
            base.byte_add(RINSTS::offset())
                .write_volatile(rinsts.into());
        }

        // read write block test
        // log::info!("read test");
        // let block_id = 16_000_000;
        // let mut origin_buf: Vec<usize> = vec![0x0; 64];
        // debug!("reading block {}", block_id);
        // // Read one block
        // self.set_size(512, 512);
        // let cmd = CMD::data_cmd(0, 17); // TODO: card number hard coded to 0
        // let cmdarg = CMDARG::from(block_id as u32);
        // self.send_cmd(cmd, cmdarg, Some(&mut origin_buf), true)
        //     .expect("Error sending command");
        //
        // log::info!("write test");
        // let mut buf: Vec<usize> = vec![0xff; 64];
        // buf[4] = 0x88;
        // buf[30] = 0x99;
        // buf[60] = 0x99;
        // debug!("writing block {}", block_id);
        // self.set_size(512, 512);
        // // CMD24 single block write
        // let cmd = CMD::write_data_cmd(0, 24); // TODO: card number hard coded to 0
        // let cmdarg = CMDARG::from(block_id as u32);
        // self.send_cmd(cmd, cmdarg, Some(&mut buf), false)
        //     .expect("Error sending command");
        //
        // log::info!("read test");
        // let mut read_buf: Vec<usize> = vec![0x0; 64];
        // debug!("reading block {}", block_id);
        // // Read one block
        // self.set_size(512, 512);
        // let cmd = CMD::data_cmd(0, 17); // TODO: card number hard coded to 0
        // let cmdarg = CMDARG::from(block_id as u32);
        // self.send_cmd(cmd, cmdarg, Some(&mut read_buf), true)
        //     .expect("Error sending command");
        //
        // log::info!("write test");
        // debug!("writing block {}", block_id);
        // self.set_size(512, 512);
        // // CMD24 single block write
        // let cmd = CMD::write_data_cmd(0, 24); // TODO: card number hard coded to 0
        // let cmdarg = CMDARG::from(block_id as u32);
        // self.send_cmd(cmd, cmdarg, Some(&mut origin_buf), false)
        //     .expect("Error sending command");
        //
        // debug_assert_eq!(buf, read_buf);

        info!("INT Status register: {:?}", rinsts);
        info!("======================= SDIO Init END ========================");
    }

    /// Internal function to send a command to the card
    fn send_cmd(
        &self,
        cmd: CMD,
        arg: CMDARG,
        buffer: Option<&mut [usize]>,
        is_read: bool,
    ) -> Option<RESP> {
        let base = self.virt_base_address() as *mut u32;

        // Sanity check
        if cmd.data_expected() && !self.dma_enabled() {
            debug_assert!(buffer.is_some());
            // 在生产环境中添加错误处理
            if buffer.is_none() {
                warn!("send_cmd: data is expected, but buffer is None!");
                return None;
            }
        }

        let mut buffer_offset = 0;

        // Wait for can send cmd
        wait_for!({
            let cmd: CMD = unsafe { base.byte_add(CMD::offset()).read_volatile() }.into();
            cmd.can_send_cmd()
        });
        // Wait for card not busy if data is required
        if cmd.data_expected() {
            wait_for!({
                let status: STATUS =
                    unsafe { base.byte_add(STATUS::offset()).read_volatile() }.into();
                !status.data_busy()
            })
        }
        unsafe {
            // Set CMARG
            base.byte_add(CMDARG::offset()).write_volatile(arg.into());
            // Send CMD
            base.byte_add(CMD::offset()).write_volatile(cmd.into());
        }

        // Wait for cmd accepted
        wait_for!({
            let cmd: CMD = unsafe { base.byte_add(CMD::offset()).read_volatile() }.into();
            cmd.can_send_cmd()
        });

        // Wait for command done if response expected
        if cmd.response_expected() {
            wait_for!({
                let rinsts: RINSTS =
                    unsafe { base.byte_add(RINSTS::offset()).read_volatile() }.into();
                rinsts.command_done()
            });
        }

        // Wait for data transfer complete if data expected
        if cmd.data_expected() {
            if let Some(buffer) = buffer {
                assert!(buffer_offset == 0);
                if is_read {
                    wait_for!({
                        let rinsts: RINSTS =
                            unsafe { base.byte_add(RINSTS::offset()).read_volatile() }.into();
                        if rinsts.receive_data_request() && !self.dma_enabled() {
                            while self.fifo_filled_cnt() >= 2 {
                                if buffer_offset >= 64 {
                                    break;
                                }
                                buffer[buffer_offset] = self.read_fifo::<usize>();
                                buffer_offset += 1;
                            }
                        }
                        rinsts.data_transfer_over() || !rinsts.no_error()
                    });
                } else {
                    wait_for!({
                        let rinsts: RINSTS =
                            unsafe { base.byte_add(RINSTS::offset()).read_volatile() }.into();
                        if rinsts.transmit_data_request() && !self.dma_enabled() {
                            // Hard coded FIFO depth
                            while self.fifo_filled_cnt() < 120 {
                                if buffer_offset >= 64 {
                                    break;
                                }
                                self.write_fifo::<usize>(buffer[buffer_offset]);
                                buffer_offset += 1;
                            }
                        }
                        rinsts.data_transfer_over() || !rinsts.no_error()
                    });
                }
                //debug!("transmit {:?} bytes", (buffer_offset) * 8);
                //debug!("Current oFIFO count: {}", self.fifo_filled_cnt());
                self.reset_fifo_offset();
            }
        }

        // Check for error
        let rinsts: RINSTS = unsafe { base.byte_add(RINSTS::offset()).read_volatile() }.into();
        // Clear interrupt by writing 1
        unsafe {
            // Just clear all for now
            base.byte_add(RINSTS::offset())
                .write_volatile(rinsts.into());
        }

        // Check response
        let base = self.virt_base_address() as *mut RESP;
        let resp = unsafe { base.byte_add(RESP::offset()).read_volatile() };
        if rinsts.no_error() && !rinsts.command_conflict() {
            // No return for clock command
            if cmd.update_clock_register_only() {
                info!("Clock cmd done");
                return None;
            }
            //debug!(
            //    "CMD{} done: {:?}, dma: {:?}",
            //    cmd.cmd_index(),
            //    rinsts.status(),
            //    self.dma_enabled()
            //);
            Some(resp)
        } else {
            warn!("CMD{} error: {:?}", cmd.cmd_index(), rinsts.status());
            warn!("Dumping response");
            warn!("Response: {:x?}", resp);
            warn!("dma: {:?}", self.dma_enabled());
            None
        }
    }

    /// Read data from FIFO
    fn read_fifo<T>(&self) -> T {
        let base = self.virt_base_address() as *mut T;
        let result = unsafe { base.byte_add(*self.fifo_offset.get()).read_volatile() };
        unsafe { *self.fifo_offset.get() += size_of::<T>() };
        result
    }
    /// Write data to FIFO
    fn write_fifo<T>(&self, value: T) {
        let base = self.virt_base_address() as *mut T;
        unsafe {
            base.byte_add(*self.fifo_offset.get()).write_volatile(value);
            *self.fifo_offset.get() += size_of::<T>()
        };
    }
    /// Reset FIFO offset
    fn reset_fifo_offset(&self) {
        // Hard coded
        // From Synopsys documentation
        unsafe { *self.fifo_offset.get() = 0x600 };
    }

    /// Reset FIFO
    fn reset_fifo(&self) {
        let base = self.virt_base_address() as *mut CTRL;
        let ctrl = self.control_reg().with_fifo_reset(true);
        unsafe { base.byte_add(*self.fifo_offset.get()).write_volatile(ctrl) }
    }

    /// Set transaction size
    ///
    /// block_size: size of transfer
    /// byte_cnt: number of bytes to transfer
    fn set_size(&self, block_size: usize, byte_cnt: usize) {
        let blksiz = BLKSIZ::new().with_block_size(block_size);
        let bytcnt = BYTCNT::new().with_byte_count(byte_cnt);
        let base = self.virt_base_address() as *mut BLKSIZ;
        unsafe { base.byte_add(BLKSIZ::offset()).write_volatile(blksiz) };
        let base = self.virt_base_address() as *mut BYTCNT;
        unsafe { base.byte_add(BYTCNT::offset()).write_volatile(bytcnt) };
    }

    fn set_controller_bus_width(&self, card_index: usize, width: CtypeCardWidth) {
        let ctype = CTYPE::set_card_width(card_index, width);
        let base = self.virt_base_address() as *mut CTYPE;
        unsafe { base.byte_add(CTYPE::offset()).write_volatile(ctype) }
    }

    fn last_response_command_index(&self) -> usize {
        let base = self.virt_base_address() as *mut STATUS;
        let status = unsafe { base.byte_add(STATUS::offset()).read_volatile() };
        status.response_index()
    }

    fn fifo_filled_cnt(&self) -> usize {
        self.status().fifo_count()
    }

    fn status(&self) -> STATUS {
        let base = self.virt_base_address() as *mut STATUS;

        unsafe { base.byte_add(STATUS::offset()).read_volatile() }
    }

    fn card_detect(&self) -> usize {
        let base = self.virt_base_address() as *mut CDETECT;
        let cdetect = unsafe { base.byte_add(CDETECT::offset()).read_volatile() };
        !cdetect.card_detect_n() & 0xFFFF
    }

    fn power_enable(&self) -> PWREN {
        let base = self.virt_base_address() as *mut PWREN;

        unsafe { base.byte_add(PWREN::offset()).read_volatile() }
    }

    fn clock_enable(&self) -> CLKENA {
        let base = self.virt_base_address() as *mut CLKENA;

        unsafe { base.byte_add(CLKENA::offset()).read_volatile() }
    }

    fn set_dma(&self, enable: bool) {
        let base = self.virt_base_address() as *mut BMOD;
        let bmod = unsafe { base.byte_add(BMOD::offset()).read_volatile() };
        let bmod = bmod.with_idmac_enable(enable).with_software_reset(true);
        unsafe { base.byte_add(BMOD::offset()).write_volatile(bmod) };

        // Also reset the dma controller
        let base = self.virt_base_address() as *mut CTRL;
        let ctrl = unsafe { base.byte_add(CTRL::offset()).read_volatile() };
        let ctrl = ctrl.with_dma_reset(true).with_use_internal_dmac(enable);
        unsafe { base.byte_add(CTRL::offset()).write_volatile(ctrl) };
    }

    fn dma_enabled(&self) -> bool {
        let base = self.virt_base_address() as *mut BMOD;
        let bmod = unsafe { base.byte_add(BMOD::offset()).read_volatile() };
        bmod.idmac_enable()
    }

    fn dma_status(&self) -> IDSTS {
        let base = self.virt_base_address() as *mut IDSTS;

        unsafe { base.byte_add(IDSTS::offset()).read_volatile() }
    }

    fn card_width(&self, index: usize) -> CtypeCardWidth {
        let base = self.virt_base_address() as *mut CTYPE;
        let ctype = unsafe { base.byte_add(CTYPE::offset()).read_volatile() };
        ctype.card_width(index)
    }

    fn control_reg(&self) -> CTRL {
        let base = self.virt_base_address() as *mut CTRL;

        unsafe { base.byte_add(CTRL::offset()).read_volatile() }
    }

    fn descriptor_base_address(&self) -> usize {
        let base = self.virt_base_address() as *mut DBADDRL;
        let dbaddrl = unsafe { base.byte_add(DBADDRL::offset()).read_volatile() };
        let base = self.virt_base_address() as *mut DBADDRU;
        let dbaddru = unsafe { base.byte_add(DBADDRU::offset()).read_volatile() };
        dbaddru.addr() << 32 | dbaddrl.addr()
    }

    fn set_descript_base_address(&self, addr: usize) {
        let base = self.virt_base_address() as *mut u32;
        unsafe { base.byte_add(DBADDRL::offset()).write_volatile(addr as u32) };
        unsafe {
            base.byte_add(DBADDRU::offset())
                .write_volatile((addr >> 32) as u32)
        };
    }

    fn reset_clock(&self) {
        // Disable clock
        info!("Disable clock");
        let base = self.virt_base_address() as *mut CLKENA;
        let clkena = CLKENA::new().with_cclk_enable(0);
        unsafe { base.byte_add(CLKENA::offset()).write_volatile(clkena) };
        let cmd = CMD::clock_cmd();
        self.send_cmd(cmd, CMDARG::empty(), None, false);

        // Set clock divider
        info!("Set clock divider");
        let base = self.virt_base_address() as *mut CLKDIV;
        let clkdiv = CLKDIV::new().with_clk_divider0(4); // Magic, supposedly set to 400KHz
        unsafe { base.byte_add(CLKDIV::offset()).write_volatile(clkdiv) };

        // Re enable clock
        info!("Renable clock");
        let base = self.virt_base_address() as *mut CLKENA;
        let clkena = CLKENA::new().with_cclk_enable(1);
        unsafe { base.byte_add(CLKENA::offset()).write_volatile(clkena) };

        let cmd = CMD::clock_cmd();
        self.send_cmd(cmd, CMDARG::empty(), None, false);
    }

    fn virt_base_address(&self) -> usize {
        self.mmio_virt_base
    }

    fn read_block(&self, block_id: usize, buf: &mut [u8]) {
        assert!(buf.len() == BLOCK_SIZE);

        let buf_trans: &mut [usize] = unsafe {
            let len = buf.len() / mem::size_of::<usize>();
            core::slice::from_raw_parts_mut(buf.as_ptr() as *mut usize, len)
        };
        //debug!("reading block {}", block_id);
        // Read one block
        self.set_size(512, 512);
        let cmd = CMD::data_cmd(0, 17); // TODO: card number hard coded to 0
        let cmdarg = CMDARG::from(block_id as u32);
        self.send_cmd(cmd, cmdarg, Some(buf_trans), true)
            .expect("Error sending command");
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) {
        assert!(buf.len() == BLOCK_SIZE);

        let mut temp_buf = [0usize; BLOCK_SIZE / core::mem::size_of::<usize>()];

        unsafe {
            core::ptr::copy_nonoverlapping(
                buf.as_ptr() as *const usize,
                temp_buf.as_mut_ptr(),
                temp_buf.len(),
            );
        }

        //debug!("writing block {}", block_id);
        self.set_size(512, 512);
        // CMD24 single block write
        let cmd = CMD::data_cmd(0, 24).with_read_write(true); // TODO: card number hard coded to 0

        let cmdarg = CMDARG::from(block_id as u32);
        self.send_cmd(cmd, cmdarg, Some(&mut temp_buf), false)
            .expect("Error sending command");
    }
}

impl Device for MMC {
    fn dev_type(&self) -> DeviceType {
        DeviceType::Block
    }

    fn id_table(&self) -> IdTable {
        IdTable::new("sdio_vf2".to_string(), None)
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        None
    }

    fn set_bus(&self, _bus: Option<Weak<dyn Bus>>) {}

    fn class(&self) -> Option<Arc<dyn Class>> {
        None
    }

    fn set_class(&self, _class: Option<Weak<dyn Class>>) {}

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        None
    }

    fn set_driver(&self, _driver: Option<Weak<dyn Driver>>) {}

    fn is_dead(&self) -> bool {
        false
    }

    fn can_match(&self) -> bool {
        false
    }

    fn set_can_match(&self, _can_match: bool) {}

    fn state_synced(&self) -> bool {
        true
    }

    fn dev_parent(&self) -> Option<Weak<dyn Device>> {
        None
    }

    fn set_dev_parent(&self, _parent: Option<Weak<dyn Device>>) {}
}

impl BlockDevice for MMC {
    fn dev_name(&self) -> &DevName {
        &self.blkdev_meta.devname
    }

    fn blkdev_meta(&self) -> &BlockDevMeta {
        &self.blkdev_meta
    }

    fn disk_range(&self) -> GeneralBlockRange {
        // TODO: 实现自动读，下面的数字为fdisk -l DragonOS/bin/disk....img的结果
        let blocks = 2097151;
        GeneralBlockRange::new(0, blocks).unwrap()
    }

    fn read_at_sync(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        if count == 0 {
            return Ok(0);
        }

        let block_size = self.block_size();
        let required_size = count * block_size;
        if buf.len() < required_size {
            return Err(SystemError::EIO);
        }

        for i in 0..count {
            let current_block = lba_id_start + i;
            let start = i * block_size;
            let end = start + block_size;
            let block_buf = &mut buf[start..end];

            self.read_block(current_block, block_buf);
        }

        Ok(count * block_size)
    }

    fn write_at_sync(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        if count == 0 {
            return Ok(0);
        }

        let block_size = self.block_size();
        let required_size = count * block_size;
        if buf.len() < required_size {
            return Err(SystemError::EIO);
        }

        for i in 0..count {
            let current_block = lba_id_start + i;
            let start = i * block_size;
            let end = start + block_size;
            let block_buf = &buf[start..end];

            self.write_block(current_block, block_buf);
        }

        Ok(count * block_size)
    }

    fn sync(&self) -> Result<(), SystemError> {
        Ok(())
    }

    fn blk_size_log2(&self) -> u8 {
        9
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn device(&self) -> Arc<dyn Device> {
        self.self_ref.upgrade().unwrap()
    }

    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    fn partitions(&self) -> Vec<Arc<Partition>> {
        let device = self.self_ref.upgrade().unwrap() as Arc<dyn BlockDevice>;
        let mbr_table = MbrDiskPartionTable::from_disk(device.clone())
            .expect("Failed to get MBR partition table");
        mbr_table.partitions(Arc::downgrade(&device))
    }
}

impl KObject for MMC {
    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner().kobject_common.kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner().kobject_common.kern_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner().kobject_common.parent.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner().kobject_common.parent = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner().kobject_common.kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner().kobject_common.kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.inner().kobject_common.kobj_type
    }

    fn name(&self) -> String {
        "sdio_vf2".to_string()
    }

    fn set_name(&self, _name: String) {
        // do nothing
    }

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        self.locked_kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        self.locked_kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.locked_kobj_state.write() = state;
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner().kobject_common.kobj_type = ktype;
    }
}

impl IndexNode for MMC {
    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        todo!()
    }
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }
    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }
    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, system_error::SystemError> {
        Err(SystemError::ENOSYS)
    }
    fn metadata(&self) -> Result<crate::filesystem::vfs::Metadata, SystemError> {
        Ok(self.metadata.clone())
    }
}

impl DeviceINode for MMC {
    fn set_fs(&self, fs: alloc::sync::Weak<crate::filesystem::devfs::DevFS>) {
        *self.fs.write() = fs;
    }

    fn set_parent(&self, _parent: Weak<LockedDevFSInode>) {
        panic!("DeviceINode for MMC is not supportted!")
    }
}
