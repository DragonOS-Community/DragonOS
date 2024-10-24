use crate::arch::interrupt::TrapFrame;
use crate::debug::kprobe::{register_kprobe, unregister_kprobe, KprobeInfo};
use alloc::string::ToString;
use kprobe::ProbeArgs;
use log::info;

#[inline(never)]
fn detect_func(x: usize, y: usize) -> usize {
    let hart = 0;
    info!("detect_func: hart_id: {}, x: {}, y:{}", hart, x, y);
    hart
}

fn pre_handler(regs: &dyn ProbeArgs) {
    let pt_regs = regs.as_any().downcast_ref::<TrapFrame>().unwrap();
    info!(
        "call pre_handler, the sp is {:#x}",
        pt_regs as *const _ as usize
    );
}

fn post_handler(regs: &dyn ProbeArgs) {
    let pt_regs = regs.as_any().downcast_ref::<TrapFrame>().unwrap();
    info!(
        "call post_handler, the sp is {:#x}",
        pt_regs as *const _ as usize
    );
}

fn fault_handler(regs: &dyn ProbeArgs) {
    let pt_regs = regs.as_any().downcast_ref::<TrapFrame>().unwrap();
    info!(
        "call fault_handler, the sp is {:#x}",
        pt_regs as *const _ as usize
    );
}

pub fn kprobe_test() {
    info!("kprobe test for [detect_func]: {:#x}", detect_func as usize);
    let kprobe_info = KprobeInfo {
        pre_handler,
        post_handler,
        fault_handler: Some(fault_handler),
        event_callback: None,
        symbol: None,
        addr: Some(detect_func as usize),
        offset: 0,
        enable: true,
    };
    let kprobe = register_kprobe(kprobe_info).unwrap();

    let new_pre_handler = |regs: &dyn ProbeArgs| {
        let pt_regs = regs.as_any().downcast_ref::<TrapFrame>().unwrap();
        info!(
            "call new pre_handler, the sp is {:#x}",
            pt_regs as *const _ as usize
        );
    };

    let kprobe_info = KprobeInfo {
        pre_handler: new_pre_handler,
        post_handler,
        fault_handler: Some(fault_handler),
        event_callback: None,
        symbol: Some("dragonos_kernel::debug::kprobe::test::detect_func".to_string()),
        addr: None,
        offset: 0,
        enable: true,
    };
    let kprobe2 = register_kprobe(kprobe_info).unwrap();
    info!(
        "install 2 kprobes at [detect_func]: {:#x}",
        detect_func as usize
    );
    detect_func(1, 2);
    unregister_kprobe(kprobe);
    unregister_kprobe(kprobe2);
    info!(
        "uninstall 2 kprobes at [detect_func]: {:#x}",
        detect_func as usize
    );
    detect_func(1, 2);
    info!("kprobe test end");
}
