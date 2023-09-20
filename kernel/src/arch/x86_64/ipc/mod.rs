pub mod signal;

use super::interrupt::TrapFrame;

use crate::{arch::CurrentIrqArch, exception::InterruptArch, process::ProcessManager, ipc::signal_types::SignalNumber};

#[no_mangle]
pub unsafe extern "C" fn do_signal(frame: &mut TrapFrame) {
    // 检查sigpending是否为0
    if ProcessManager::current_pcb()
        .sig_info()
        .sig_pedding()
        .signal()
        == 0
        || !frame.from_user()
    {
        // 若没有正在等待处理的信号，或者将要返回到的是内核态，则启用中断，然后返回
        CurrentIrqArch::interrupt_enable();
        return;
    }

    // 做完上面的检查后，开中断
    CurrentIrqArch::interrupt_enable();

    let oldset = ProcessManager::current_pcb().sig_blocked;
    loop {
        let (sig_number, info, ka) = get_signal_to_deliver(regs.clone());
        // 所有的信号都处理完了
        if sig_number == SignalNumber::INVALID {
            return;
        }
        kdebug!(
            "To handle signal [{}] for pid:{}",
            sig_number as i32,
            current_pcb().pid
        );
        let res = handle_signal(sig_number, ka.unwrap(), &info.unwrap(), &oldset, regs);
        if res.is_err() {
            kerror!(
                "Error occurred when handling signal: {}, pid={}, errcode={:?}",
                sig_number as i32,
                ProcessManager::current_pcb().pid(),
                res.unwrap_err()
            );
        }
    }
    return;
}
