use crate::virt::vm::kvm_host::vcpu::VirtCpu;

use super::kvm_host::gfn_to_gpa;

pub fn kvm_mtrr_check_gfn_range_consistency(_vcpu: &mut VirtCpu, gfn: u64, page_num: u64) -> bool {
    // let mtrr_state = &vcpu.arch.mtrr_state;
    // let mut iter = MtrrIter {
    //     mem_type: -1,
    //     mtrr_disabled: false,
    //     partial_map: false,
    // };
    let _start = gfn_to_gpa(gfn);
    let _end = gfn_to_gpa(gfn + page_num);

    // mtrr_for_each_mem_type(&mut iter, mtrr_state, start, end, |iter| {
    //     if iter.mem_type == -1 {
    //         iter.mem_type = iter.mem_type;
    //     } else if iter.mem_type != iter.mem_type {
    //         return false;
    //     }
    // });

    // if iter.mtrr_disabled {
    //     return true;
    // }

    // if !iter.partial_map {
    //     return true;
    // }

    // if iter.mem_type == -1 {
    //     return true;
    // }

    // iter.mem_type == mtrr_default_type(mtrr_state)
    true
}
