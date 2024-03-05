use bitfield_struct::bitfield;

#[allow(dead_code)]
#[derive(Debug)]
pub enum X86MsiData {
    Normal(X86MsiDataNormal),
    Dmar(X86MsiDataDmar),
}

#[bitfield(u32)]
pub struct X86MsiDataNormal {
    #[bits(8)]
    pub vector: u8,
    #[bits(3)]
    pub delivery_mode: u8,
    #[bits(1)]
    pub dest_mode_logical: bool,
    #[bits(2)]
    reserved: u8,
    #[bits(1)]
    pub active_low: bool,
    #[bits(1)]
    pub is_level_triggered: bool,
    #[bits(16)]
    reserved2: u16,
}

#[derive(Debug)]
pub struct X86MsiDataDmar {
    pub dmar_subhandle: u32,
}

impl X86MsiDataDmar {
    #[allow(dead_code)]
    pub const fn new(dmar_subhandle: u32) -> Self {
        X86MsiDataDmar { dmar_subhandle }
    }
}

pub const X86_MSI_BASE_ADDRESS_LOW: u32 = 0xfee00000 >> 20;

#[allow(dead_code)]
#[derive(Debug)]
pub enum X86MsiAddrLo {
    Normal(X86MsiAddrLoNormal),
    Dmar(X86MsiAddrLoDmar),
}

#[bitfield(u32)]
pub struct X86MsiAddrLoNormal {
    #[bits(2)]
    reserved_0: u32,
    #[bits(1)]
    pub dest_mode_logical: bool,
    #[bits(1)]
    pub redirecti_hint: bool,
    #[bits(1)]
    reserved_1: bool,
    #[bits(7)]
    pub virt_destid_8_14: u32,
    #[bits(8)]
    pub destid_0_7: u32,
    #[bits(12)]
    pub base_address: u32,
}

#[bitfield(u32)]
pub struct X86MsiAddrLoDmar {
    #[bits(2)]
    reserved_0: u32,
    #[bits(1)]
    pub index_15: bool,
    #[bits(1)]
    pub subhandle_valid: bool,
    #[bits(1)]
    pub format: bool,
    #[bits(15)]
    pub index_0_14: u32,
    #[bits(12)]
    pub base_address: u32,
}

#[bitfield(u32)]
pub struct X86MsiAddrHi {
    #[bits(8)]
    reserved: u32,
    #[bits(24)]
    pub destid_8_31: u32,
}
