use crate::driver::pci::pci::SegmentGroupNumber;
use crate::include::bindings::bindings::acpi_system_description_table_header_t;
use core::ptr::{slice_from_raw_parts_mut, NonNull};
// MCFG表中的Segement配置部分，开始位置为44+16*n
#[repr(C, packed)]
pub struct Segement_Configuration_Space {
    pub base_address: u64,
    pub segement_group_number: SegmentGroupNumber,
    pub bus_begin: u8,
    pub bus_end: u8,
    pub reverse: u32,
}

/// @brief 获取Segement_Configuration_Space的数量并返回对应数量的Segement_Configuration_Space的切片指针
/// @param head acpi_system_description_table_header_t的指针
/// @return NonNull<[Segement_Configuration_Space]>
pub fn mcfg_find_segment(
    head: NonNull<acpi_system_description_table_header_t>,
) -> NonNull<[Segement_Configuration_Space]> {
    let table_length = unsafe { (*head.as_ptr()).Length };
    let number_of_segments = ((table_length - 44) / 16) as u16;
    NonNull::new(slice_from_raw_parts_mut(
        (head.as_ptr() as usize + 44) as *mut _,
        number_of_segments as usize,
    ))
    .unwrap()
}
