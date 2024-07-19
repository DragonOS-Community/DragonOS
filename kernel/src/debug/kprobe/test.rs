use crate::arch::interrupt::TrapFrame;
use crate::debug::kprobe::{register_kprobe, unregister_kprobe, KprobeInfo};
use kprobe::ProbeArgs;

#[inline(never)]
#[no_mangle]
pub fn detect_func(x: usize, y: usize) -> usize {
    let hart = 0;
    println!("detect_func: hart_id: {}, x: {}, y:{}", hart, x, y);
    hart
}

pub fn kprobe_test() {
    let pre_handler = |regs: &dyn ProbeArgs| {
        let pt_regs = regs.as_any().downcast_ref::<TrapFrame>().unwrap();
        println!(
            "call pre_handler, the sp is {:#x}",
            pt_regs as *const _ as usize
        );
    };
    let post_handler = |regs: &dyn ProbeArgs| {
        let pt_regs = regs.as_any().downcast_ref::<TrapFrame>().unwrap();
        println!(
            "call post_handler, the sp is {:#x}",
            pt_regs as *const _ as usize
        );
    };
    let fault_handler = |regs: &dyn ProbeArgs| {
        let pt_regs = regs.as_any().downcast_ref::<TrapFrame>().unwrap();
        println!(
            "call fault_handler, the sp is {:#x}",
            pt_regs as *const _ as usize
        );
    };
    println!("kprobe test for [detect_func]: {:#x}", detect_func as usize);
    let kprobe_info = KprobeInfo {
        pre_handler,
        post_handler,
        fault_handler,
        symbol: "detect_func",
        offset: 0,
    };
    let kprobe = register_kprobe(kprobe_info).unwrap();
    println!(
        "install kprobe at [detect_func]: {:#x}",
        detect_func as usize
    );
    detect_func(1, 2);
    unregister_kprobe(kprobe).unwrap();
    detect_func(1, 2);
}
