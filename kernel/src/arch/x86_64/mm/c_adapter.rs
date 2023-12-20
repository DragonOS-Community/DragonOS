use super::LowAddressRemapping;

#[no_mangle]
unsafe extern "C" fn rs_unmap_at_low_addr() -> usize {
    LowAddressRemapping::unmap_at_low_address(true);
    return 0;
}
