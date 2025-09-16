use bitfield_struct::bitfield;
use bitflags::bitflags;

/// When a response is received – either erroneous or valid – the
/// DWC_mobile_storage sets the command_done bit in the RINTSTS register.
/// A short response is copied in Response Register0, while a long response is
/// copied to all four response registers @0x30, 0x34, 0x38, and 0x3C.
/// The Response3 register bit 31 represents the MSB, and the Response0 register
/// bit 0 represents the LSB of a long response.
#[bitfield(u32)]
pub struct CMD {
    #[bits(6)]
    pub cmd_index: usize,
    pub response_expected: bool,
    pub response_length: bool, // false for short (4B), true for long (16B)
    check_response_crc: bool,
    pub data_expected: bool,
    pub read_write: bool,
    pub transfer_mode: bool,
    send_auto_stop: bool,
    wait_prvdata_complete: bool,
    stop_abort_cmd: bool,
    send_initialization: bool,
    #[bits(5)]
    card_number: usize,
    pub update_clock_register_only: bool,
    read_ceata_device: bool,
    ccs_expected: bool,
    enable_boot: bool,
    expect_boot_ack: bool,
    disable_boot: bool,
    boot_mode: bool,
    volt_switch: bool,
    use_hold_reg: bool,
    _reserved: bool,
    start_cmd: bool,
}

impl CMD {
    pub fn offset() -> usize {
        0x2c
    }

    pub fn no_data_cmd(card_number: usize, cmd_index: usize) -> Self {
        CMD::new()
            .with_start_cmd(true)
            .with_use_hold_reg(true) // TODO: check different mode of correct value
            .with_response_expected(true)
            .with_wait_prvdata_complete(true) // Optional
            .with_check_response_crc(true) // Optional
            .with_card_number(card_number)
            .with_cmd_index(cmd_index)
    }

    pub fn no_data_cmd_no_crc(card_number: usize, cmd_index: usize) -> Self {
        CMD::no_data_cmd(card_number, cmd_index).with_check_response_crc(false)
    }

    pub fn data_cmd(card_number: usize, cmd_index: usize) -> Self {
        CMD::new()
            .with_start_cmd(true)
            .with_use_hold_reg(true) // TODO: check different mode of correct value
            .with_data_expected(true)
            .with_response_expected(true)
            .with_wait_prvdata_complete(true) // Optional
            .with_check_response_crc(true) // Optional
            .with_card_number(card_number)
            .with_cmd_index(cmd_index)
    }

    /// Note that even though start_cmd is set for updating clock registers,
    /// the DWC_mobile_storage does not raise a command_done signal upon command
    /// completion.
    pub fn clock_cmd() -> Self {
        CMD::new()
            .with_start_cmd(true)
            .with_wait_prvdata_complete(true)
            .with_update_clock_register_only(true)
    }

    pub fn reset_cmd0(card_number: usize) -> Self {
        CMD::no_data_cmd(card_number, 0).with_send_initialization(true)
    }
    pub fn can_send_cmd(&self) -> bool {
        !self.start_cmd()
    }
}

bitflags! {
    #[allow(non_camel_case_types)]
    pub struct RINSTS_int_status: u16 {
        const Cd = 1 << 0;
        const RE = 1 << 1;
        const CD = 1 << 2;
        const DtO = 1 << 3;
        const TxDR = 1 << 4;
        const RxDR = 1 << 5;
        const RCRC = 1 << 6;
        const DCRC = 1 << 7;
        const RTO = 1 << 8;
        const DRTO = 1 << 9;
        const HTO = 1 << 10;
        const FRUN = 1 << 11;
        const HLE = 1 << 12;
        const SBE = 1 << 13;
        const ACD = 1 << 14;
        const EBE = 1 << 15;
    }
}

/// Writes to bits clear status bit.
/// Value of 1 clears status bit, and value of 0 leaves bit intact.
/// Bits are logged regardless of interrupt mask status.
#[bitfield(u32)]
pub struct RINSTS {
    int_status: u16,
    sdiojinterrupt: u16,
}

impl RINSTS {
    pub fn offset() -> usize {
        0x44
    }
    pub fn command_done(&self) -> bool {
        RINSTS_int_status::from_bits_truncate(self.int_status()).contains(RINSTS_int_status::CD)
    }
    pub fn data_transfer_over(&self) -> bool {
        RINSTS_int_status::from_bits_truncate(self.int_status()).contains(RINSTS_int_status::DtO)
    }
    pub fn receive_data_request(&self) -> bool {
        RINSTS_int_status::from_bits_truncate(self.int_status()).contains(RINSTS_int_status::RxDR)
    }
    pub fn transmit_data_request(&self) -> bool {
        RINSTS_int_status::from_bits_truncate(self.int_status()).contains(RINSTS_int_status::TxDR)
    }
    pub fn no_error(&self) -> bool {
        // Check if response_timeout error, response_CRC error, or response error is
        // set.
        !RINSTS_int_status::from_bits_truncate(self.int_status()).intersects(
            RINSTS_int_status::RTO
                | RINSTS_int_status::DCRC
                | RINSTS_int_status::RE
                | RINSTS_int_status::DRTO
                | RINSTS_int_status::SBE
                | RINSTS_int_status::EBE,
        )
    }
    pub fn command_conflict(&self) -> bool {
        RINSTS_int_status::from_bits_truncate(self.int_status()).contains(RINSTS_int_status::HLE)
    }
    pub fn status(&self) -> RINSTS_int_status {
        RINSTS_int_status::from_bits_truncate(self.int_status())
    }
}

#[bitfield(u32)]
pub struct CMDARG {
    bits: u32,
}

impl CMDARG {
    pub fn empty() -> Self {
        CMDARG::new()
    }
    pub fn offset() -> usize {
        0x28
    }
}

#[bitfield(u128)]
pub struct RESP {
    resp0: u32,
    resp1: u32,
    resp2: u32,
    resp3: u32,
}

impl RESP {
    pub fn offset() -> usize {
        0x30
    }
    pub fn resp(&self, index: usize) -> u32 {
        match index {
            0 => self.resp0(),
            1 => self.resp1(),
            2 => self.resp2(),
            3 => self.resp3(),
            _ => unreachable!(),
        }
    }
    pub fn resps(&self) -> [u32; 4] {
        [self.resp0(), self.resp1(), self.resp2(), self.resp3()]
    }
    pub fn resps_u128(&self) -> u128 {
        let mut result: u128 = self.resp3().into();
        result <<= 32;
        result |= u128::from(self.resp2());
        result <<= 32;
        result |= u128::from(self.resp1());
        result <<= 32;
        result |= u128::from(self.resp0());

        result
    }

    /// Return the OCR Register
    pub fn ocr(&self) -> u32 {
        self.resp0()
    }
}

/// Control Register
#[bitfield(u32)]
pub struct CTRL {
    controller_reset: bool,
    pub fifo_reset: bool,
    pub dma_reset: bool,
    _reserved0: bool,
    int_enable: bool,
    pub dma_enable: bool,
    read_wait: bool,
    send_irq_response: bool,
    abort_read_data: bool,
    send_ccsd: bool,
    send_auto_stop_ccsd: bool,
    ceata_device_interrupt_statue: bool,
    #[bits(4)]
    _reserved1: u8,
    #[bits(4)]
    card_voltage_a: usize,
    #[bits(4)]
    card_voltage_b: usize,
    enable_od_pullup: bool,
    pub use_internal_dmac: bool,
    #[bits(6)]
    _reserved2: u8,
}

impl CTRL {
    pub fn offset() -> usize {
        0x00
    }
}

/// Power Enable Register
#[bitfield(u32)]
pub struct PWREN {
    #[bits(30)]
    pub power_enable: usize,
    #[bits(2)]
    _reserved: usize,
}

impl PWREN {
    pub fn offset() -> usize {
        0x04
    }
}

/// Clock Divider Register
///
/// Clock division is 2* n. For example, value of 0 means
/// divide by 2*0 = 0 (no division, bypass), value of 1 means divide by 2*1 = 2,
/// value of “ff” means divide by 2*255 = 510, and so on.
#[bitfield(u32)]
pub struct CLKDIV {
    pub clk_divider0: u8,
    clk_divider1: u8,
    clk_divider2: u8,
    clk_divider3: u8,
}

impl CLKDIV {
    pub fn offset() -> usize {
        0x08
    }

    pub fn clks(&self) -> [u8; 4] {
        [
            self.clk_divider0(),
            self.clk_divider1(),
            self.clk_divider2(),
            self.clk_divider3(),
        ]
    }
}

/// SD Clock Source Register
///
/// Clock divider source for up to 16 SD cards supported. Each card has two bits
/// assigned to it. For example, bits[1:0] assigned for card-0, which maps and
/// internally routes clock divider[3:0] outputs to cclk_out[15:0] pins,
/// depending on bit value. 00 - Clock divider 0
/// 01 - Clock divider 1
#[bitfield(u32)]
pub struct CLKSRC {
    clk_src: u32,
}

impl CLKSRC {
    pub fn offset() -> usize {
        0x0C
    }
}

#[bitfield(u32)]
pub struct CLKENA {
    pub cclk_enable: u16,
    cclk_low_power: u16,
}

impl CLKENA {
    pub fn offset() -> usize {
        0x10
    }
}

#[bitfield(u32)]
pub struct CTYPE {
    card_width4: u16,
    card_width8: u16,
}

#[derive(Debug)]
pub enum CtypeCardWidth {
    Width8,
    Width4,
    Width1,
}

impl CTYPE {
    pub fn offset() -> usize {
        0x18
    }
    pub fn set_card_width(index: usize, width: CtypeCardWidth) -> CTYPE {
        debug_assert!(index < 16); // Not supported >= 16
        match width {
            CtypeCardWidth::Width1 => CTYPE::new(),
            CtypeCardWidth::Width4 => CTYPE::new().with_card_width4(1 << index),
            CtypeCardWidth::Width8 => CTYPE::new().with_card_width8(1 << index),
        }
    }
    pub fn card_width(&self, index: usize) -> CtypeCardWidth {
        if self.card_width8() & (1 << index) != 0 {
            CtypeCardWidth::Width8
        } else if self.card_width4() & (1 << index) != 0 {
            CtypeCardWidth::Width4
        } else {
            CtypeCardWidth::Width1
        }
    }
}

/// Block Size Register
#[bitfield(u32)]
pub struct BLKSIZ {
    #[bits(16)]
    pub block_size: usize,
    _reserved: u16,
}

impl BLKSIZ {
    pub fn offset() -> usize {
        0x1C
    }
}

/// Byte Count Register
#[bitfield(u32)]
pub struct BYTCNT {
    #[bits(32)]
    pub byte_count: usize,
}

impl BYTCNT {
    pub fn offset() -> usize {
        0x20
    }
}

/// Status Register
#[bitfield(u32)]
pub struct STATUS {
    pub fifo_rx_watermark: bool,
    fifo_tx_watermark: bool,
    fifo_empty: bool,
    pub fifo_full: bool,
    #[bits(4)]
    command_fsm_state: usize,
    data_3_status: bool,
    pub data_busy: bool,
    data_state_mc_busy: bool,
    #[bits(6)]
    pub response_index: usize,
    #[bits(13)]
    pub fifo_count: usize,
    dma_ack: bool,
    dma_req: bool,
}

impl STATUS {
    pub fn offset() -> usize {
        0x48
    }
}

/// Card Detect Register
#[bitfield(u32)]
pub struct CDETECT {
    #[bits(30)]
    pub card_detect_n: usize,
    #[bits(2)]
    _reserved: u8,
}

impl CDETECT {
    pub fn offset() -> usize {
        0x50
    }
}

/// Bus Mode Register
#[bitfield(u32)]
pub struct BMOD {
    pub software_reset: bool,
    fixed_burst: bool,
    #[bits(5)]
    descriptor_skip_length: usize,
    pub idmac_enable: bool,
    #[bits(3)]
    burst_length: usize,
    #[bits(21)]
    _reserved: u32,
}

impl BMOD {
    pub fn offset() -> usize {
        0x80
    }
}

/// Descriptor List Base Address Lower Register
/// 64 bits only
#[bitfield(u32)]
pub struct DBADDRL {
    #[bits(32)]
    pub addr: usize,
}

impl DBADDRL {
    pub fn offset() -> usize {
        0x88
    }
}

/// Descriptor List Base Address Upper Register
/// 64 bits only
#[bitfield(u32)]
pub struct DBADDRU {
    #[bits(32)]
    pub addr: usize,
}

impl DBADDRU {
    pub fn offset() -> usize {
        0x8C
    }
}

/// Internal DMAC Status Register
/// R/W
#[bitfield(u32)]
pub struct IDSTS {
    pub transmit_interrupt: bool,
    pub receive_interrupt: bool,
    pub fatal_bus_error: bool,
    _reserved0: bool,
    descriptor_unavailable: bool,
    card_error_summary: bool,
    #[bits(2)]
    _reserved1: usize,
    normal_interrupt_summary: bool,
    abnormal_interrupt_summary: bool,
    #[bits(3)]
    fatal_bus_error_code: usize,
    #[bits(4)]
    dmac_fsm_state: usize,
    #[bits(15)]
    _reserved2: usize,
}

impl IDSTS {
    pub fn offset() -> usize {
        0x90
    }
}

/// Current Host Descriptor Address Lower Register
/// RO
#[bitfield(u32)]
pub struct DSCADDRL {
    #[bits(32)]
    pub addr: usize,
}

impl DSCADDRL {
    pub fn offset() -> usize {
        0x98
    }
}

/// Current Host Descriptor Address Upper Register
/// RO
#[bitfield(u32)]
pub struct DSCADDRU {
    #[bits(32)]
    pub addr: usize,
}

impl DSCADDRU {
    pub fn offset() -> usize {
        0x9C
    }
}

/// Current Buffer Descriptor Address Upper Register
/// RO
#[bitfield(u32)]
pub struct BUFADDRL {
    #[bits(32)]
    pub addr: usize,
}

impl BUFADDRL {
    pub fn offset() -> usize {
        0xA0
    }
}

/// Current Buffer Descriptor Address Upper Register
/// RO
#[bitfield(u32)]
pub struct BUFADDRU {
    #[bits(32)]
    pub addr: usize,
}

impl BUFADDRU {
    pub fn offset() -> usize {
        0xA0
    }
}

/// SD Card internal CID register
#[bitfield(u128)]
pub struct CID {
    #[bits(1)]
    _reserved0: u32,
    #[bits(7)]
    crc7: u8,
    #[bits(12)]
    pub date: usize, // Manufactured date
    #[bits(4)]
    _reserved1: u32,
    #[bits(32)]
    pub serial: usize, // Serial number
    #[bits(8)]
    pub hwrev: usize, // Hardware revision
    #[bits(40)]
    pub name: usize, // Product name
    #[bits(16)]
    pub oemid: usize, // OEM ID
    #[bits(8)]
    pub manfid: usize, // Manufacturer ID`
}
