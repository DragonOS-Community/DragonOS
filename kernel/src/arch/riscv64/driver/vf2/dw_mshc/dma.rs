use bitfield_struct::bitfield;

#[bitfield(u32)]
pub struct DES0 {
    _reserved0: bool,
    disable_interrupt_on_completion: bool,
    last_descriptor: bool,
    first_descriptor: bool,
    second_address_chained: bool,
    end_of_ring: bool,
    #[bits(24)]
    _reserved1: usize,
    card_error_summary: bool,

    // Is owned by the card.
    own: bool,
}
#[bitfield(u32)]
pub struct DES1 {
    #[bits(13)]
    buffer_1_size: usize,
    #[bits(13)]
    buffer_2_size: usize,
    #[bits(6)]
    _reserved0: usize,
}
#[bitfield(u32)]
pub struct DES2 {
    #[bits(32)]
    pub buffer_addr1: usize,
}
#[bitfield(u32)]
pub struct DES3 {
    #[bits(32)]
    pub buffer_addr2: usize,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct Descriptor {
    pub des0: DES0,
    pub des1: DES1,
    pub des2: DES2,
    pub des3: DES3,
}

#[allow(dead_code)]
impl Descriptor {
    pub fn new(size: usize, buffer_paddr: usize, next_paddr: usize) -> Self {
        Descriptor {
            des0: DES0::new().with_second_address_chained(true).with_own(true),
            des1: DES1::new().with_buffer_1_size(size),
            des2: DES2::new().with_buffer_addr1(buffer_paddr),
            des3: DES3::new().with_buffer_addr2(next_paddr),
        }
    }
    pub fn own_by_card(&self) -> bool {
        self.des0.own()
    }
    pub fn set_own_by_card(&mut self) {
        self.des0.set_own(true);
    }
}
