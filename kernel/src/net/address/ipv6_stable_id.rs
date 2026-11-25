//! IPv6 stable interface identifiers compatible with Linux addrconf.

use crate::libs::sha1;

pub(super) fn interface_id(
    prefix: &[u8; 8],
    secret: &[u8; 16],
    permanent_mac: &[u8; 6],
    mut dad_count: u8,
    retries: i32,
) -> Option<[u8; 8]> {
    let mut block = [0; 64];
    block[..16].copy_from_slice(secret);
    block[16..24].copy_from_slice(prefix);
    // Linux allocates MAX_ADDR_LEN (32 bytes) for the permanent address.
    block[24..30].copy_from_slice(permanent_mac);
    loop {
        block[56] = dad_count;
        let mut digest = sha1::INITIAL_STATE;
        sha1::compress(&mut digest, &block);
        let mut id = [0; 8];
        id[..4].copy_from_slice(&digest[0].to_ne_bytes());
        id[4..].copy_from_slice(&digest[1].to_ne_bytes());
        if !reserved(u64::from_be_bytes(id)) {
            return Some(id);
        }
        dad_count = dad_count.checked_add(1)?;
        if i32::from(dad_count) > retries {
            return None;
        }
    }
}

fn reserved(id: u64) -> bool {
    id == 0
        || id & 0xffff_ffff_fe00_0000 == 0x0200_5eff_fe00_0000
        || id & 0xffff_ffff_ffff_ff80 == 0xfdff_ffff_ffff_ff80
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_veth_vector() {
        let prefix = [0xfe, 0x80, 0, 0, 0, 0, 0, 0];
        let mut secret = [0; 16];
        secret[0] = 0xfd;
        secret[15] = 6;
        assert_eq!(
            interface_id(&prefix, &secret, &[0; 6], 0, 3),
            Some(
                [0xeda4_9d88u32.to_ne_bytes(), 0x7f6f_5ae9u32.to_ne_bytes(),]
                    .concat()
                    .try_into()
                    .unwrap()
            )
        );
    }
}
