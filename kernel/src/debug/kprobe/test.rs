use crate::arch::interrupt::TrapFrame;
use crate::debug::kprobe::{register_kprobe, unregister_kprobe, KprobeInfo};
use kprobe::ProbeArgs;

#[inline(never)]
fn detect_func(x: usize, y: usize) -> usize {
    let hart = 0;
    println!("detect_func: hart_id: {}, x: {}, y:{}", hart, x, y);
    hart
}

fn pre_handler(regs: &dyn ProbeArgs) {
    let pt_regs = regs.as_any().downcast_ref::<TrapFrame>().unwrap();
    println!(
        "call pre_handler, the sp is {:#x}",
        pt_regs as *const _ as usize
    );
}

fn post_handler(regs: &dyn ProbeArgs) {
    let pt_regs = regs.as_any().downcast_ref::<TrapFrame>().unwrap();
    println!(
        "call post_handler, the sp is {:#x}",
        pt_regs as *const _ as usize
    );
}

fn fault_handler(regs: &dyn ProbeArgs) {
    let pt_regs = regs.as_any().downcast_ref::<TrapFrame>().unwrap();
    println!(
        "call fault_handler, the sp is {:#x}",
        pt_regs as *const _ as usize
    );
}

pub fn kprobe_test() {
    println!("kprobe test for [detect_func]: {:#x}", detect_func as usize);
    let kprobe_info = KprobeInfo {
        pre_handler,
        post_handler,
        fault_handler: Some(fault_handler),
        symbol: None,
        addr: Some(detect_func as usize),
        offset: 0,
    };
    let kprobe = register_kprobe(kprobe_info).unwrap();

    let new_pre_handler = |regs: &dyn ProbeArgs| {
        let pt_regs = regs.as_any().downcast_ref::<TrapFrame>().unwrap();
        println!(
            "call new pre_handler, the sp is {:#x}",
            pt_regs as *const _ as usize
        );
    };

    let kprobe_info = KprobeInfo {
        pre_handler: new_pre_handler,
        post_handler,
        fault_handler: Some(fault_handler),
        symbol: Some("dragonos_kernel::debug::kprobe::test::detect_func"),
        addr: None,
        offset: 0,
    };
    let kprobe2 = register_kprobe(kprobe_info).unwrap();
    println!(
        "install 2 kprobes at [detect_func]: {:#x}",
        detect_func as usize
    );
    detect_func(1, 2);
    unregister_kprobe(kprobe).unwrap();
    unregister_kprobe(kprobe2).unwrap();
    println!(
        "uninstall 2 kprobes at [detect_func]: {:#x}",
        detect_func as usize
    );
    detect_func(1, 2);
    println!("kprobe test end");
}
