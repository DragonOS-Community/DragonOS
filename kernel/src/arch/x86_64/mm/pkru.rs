const PKRU_AD_BIT: u16 = 0x1;
const PKRU_WD_BIT: u16 = 0x2;
const PKRU_BITS_PER_PKEY: u32 = 2;

pub fn pkru_allows_pkey(pkey: u16, write: bool) -> bool {
    let pkru = read_pkru();

    if !pkru_allows_read(pkru, pkey) {
        return false;
    }
    if write & !pkru_allows_write(pkru, pkey) {
        return false;
    }

    true
}

pub fn pkru_allows_read(pkru: u32, pkey: u16) -> bool {
    let pkru_pkey_bits: u32 = pkey as u32 * PKRU_BITS_PER_PKEY;
    pkru & ((PKRU_AD_BIT as u32) << pkru_pkey_bits) > 0
}

pub fn pkru_allows_write(pkru: u32, pkey: u16) -> bool {
    let pkru_pkey_bits: u32 = pkey as u32 * PKRU_BITS_PER_PKEY;
    pkru & (((PKRU_AD_BIT | PKRU_WD_BIT) as u32) << pkru_pkey_bits) > 0
}

pub fn read_pkru() -> u32 {
    0
}
