use crate::sched::SchedArch;

/// 发起调度
#[no_mangle]
pub extern "C" fn sched() {
    unimplemented!("RiscV64::sched")
}

pub struct RiscV64SchedArch;

impl SchedArch for RiscV64SchedArch {
    fn enable_sched_local() {
        todo!()
    }

    fn disable_sched_local() {
        todo!()
    }

    fn initial_setup_sched_local() {
        todo!()
    }
}
