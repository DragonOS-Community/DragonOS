// Reflected Castagnoli polynomial (CRC32C), also used by ext4 on disk.
const POLYNOMIAL: u32 = 0x82f6_3b78;

const fn crc32c_tables() -> [[u32; 256]; 8] {
    let mut tables = [[0; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { POLYNOMIAL } else { 0 };
            bit += 1;
        }
        tables[0][i] = crc;
        i += 1;
    }
    let mut slice = 1;
    while slice < 8 {
        i = 0;
        while i < 256 {
            let crc = tables[slice - 1][i];
            tables[slice][i] = (crc >> 8) ^ tables[0][(crc & 0xff) as usize];
            i += 1;
        }
        slice += 1;
    }
    tables
}

// Each successive table advances a byte's contribution by one more zero byte.
// Compile-time initialization keeps this usable without allocation or CPU features.
static CRC32C_TABLES: [[u32; 256]; 8] = crc32c_tables();

/// Update an ext4 CRC32C state. No initial or final complement is applied.
/// Passing the result back as `crc_init` continues a checksum across slices.
pub fn crc32(crc_init: u32, mut data: &[u8]) -> u32 {
    let mut crc = crc_init;
    while data.len() >= 8 {
        // Explicit LE decoding accepts unaligned input on every architecture.
        let first = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) ^ crc;
        crc = CRC32C_TABLES[7][(first & 0xff) as usize]
            ^ CRC32C_TABLES[6][((first >> 8) & 0xff) as usize]
            ^ CRC32C_TABLES[5][((first >> 16) & 0xff) as usize]
            ^ CRC32C_TABLES[4][(first >> 24) as usize]
            ^ CRC32C_TABLES[3][data[4] as usize]
            ^ CRC32C_TABLES[2][data[5] as usize]
            ^ CRC32C_TABLES[1][data[6] as usize]
            ^ CRC32C_TABLES[0][data[7] as usize];
        data = &data[8..];
    }
    for &byte in data {
        crc = CRC32C_TABLES[0][(crc as u8 ^ byte) as usize] ^ (crc >> 8);
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::crc32;

    // Independent bitwise reference, with no table generation or slicing logic.
    fn bitwise(mut crc: u32, data: &[u8]) -> u32 {
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0x82f6_3b78 & 0u32.wrapping_sub(crc & 1));
            }
        }
        crc
    }

    #[test]
    fn known_vector_and_empty_input() {
        assert_eq!(!crc32(!0, b"123456789"), 0xe306_9283);
        for seed in [0, 1, !0, 0x1234_5678] {
            assert_eq!(crc32(seed, &[]), seed);
        }
    }

    #[test]
    fn offsets_lengths_seeds_and_incremental_updates() {
        let mut data = [0u8; 8208];
        let mut state = 0x1234_5678u32;
        for byte in &mut data {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *byte = state as u8;
        }
        for offset in 0..16 {
            for len in (0..=128).chain([255, 256, 257, 4083, 4084, 4085, 4096, 8192]) {
                let input = &data[offset..offset + len];
                for seed in [0, 1, !0, 0x1234_5678, 0xdead_beef] {
                    let expected = bitwise(seed, input);
                    assert_eq!(crc32(seed, input), expected);
                    for cut in [0, len / 3, len / 2, len.saturating_sub(1), len] {
                        assert_eq!(crc32(crc32(seed, &input[..cut]), &input[cut..]), expected);
                    }
                }
            }
        }
    }
}
