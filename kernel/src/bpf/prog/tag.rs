//! Linux-compatible 64-bit BPF program tag for map-free device programs.
//!
//! Linux stores the first eight bytes of SHA-1 over the verified instruction
//! stream, with unstable map-immediate values cleared before hashing.

use rbpf::ebpf;

fn tag_byte(bytes: &[u8], index: usize) -> u8 {
    if index % 8 < 4 {
        return bytes[index];
    }
    let insn = index & !7;
    let is_map_load =
        |at: usize| bytes[at] == ebpf::LD_DW_IMM && matches!(bytes[at + 1] >> 4, 1 | 2);
    if is_map_load(insn)
        || (insn >= 8
            && is_map_load(insn - 8)
            && bytes[insn..insn + 4].iter().all(|byte| *byte == 0))
    {
        0
    } else {
        bytes[index]
    }
}

pub(super) fn tag(bytes: &[u8]) -> [u8; 8] {
    let mut state = [
        0x6745_2301u32,
        0xefcd_ab89,
        0x98ba_dcfe,
        0x1032_5476,
        0xc3d2_e1f0,
    ];
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    let total = (bytes.len() + 9).div_ceil(64) * 64;

    for base in (0..total).step_by(64) {
        let mut words = [0u32; 80];
        for i in 0..64 {
            let index = base + i;
            let byte = if index < bytes.len() {
                tag_byte(bytes, index)
            } else if index == bytes.len() {
                0x80
            } else if index >= total - 8 {
                bit_len.to_be_bytes()[index - (total - 8)]
            } else {
                0
            };
            words[i / 4] |= (byte as u32) << (24 - (i % 4) * 8);
        }
        for i in 16..80 {
            words[i] = (words[i - 3] ^ words[i - 8] ^ words[i - 14] ^ words[i - 16]).rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = state;
        for (i, word) in words.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5a82_7999),
                20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e]) {
            *slot = slot.wrapping_add(value);
        }
    }

    let mut result = [0u8; 8];
    result[..4].copy_from_slice(&state[0].to_be_bytes());
    result[4..].copy_from_slice(&state[1].to_be_bytes());
    result
}

#[cfg(test)]
mod tests {
    use super::tag;

    #[test]
    fn sha1_prefix_vectors() {
        assert_eq!(tag(b""), [0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d]);
        assert_eq!(
            tag(b"abc"),
            [0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a]
        );
    }

    #[test]
    fn map_relocation_does_not_change_tag() {
        let mut first = [0u8; 16];
        first[0] = rbpf::ebpf::LD_DW_IMM;
        first[1] = 1 << 4; // BPF_PSEUDO_MAP_FD
        first[4..8].copy_from_slice(&5u32.to_le_bytes());
        let mut second = first;
        second[4..8].copy_from_slice(&23u32.to_le_bytes());
        second[12..16].copy_from_slice(&41u32.to_le_bytes());
        assert_eq!(tag(&first), tag(&second));
    }
}
