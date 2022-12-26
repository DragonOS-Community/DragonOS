use crate::{include::bindings::bindings::process_control_block, process::process::process_cpu, arch::asm::current::current_pcb};

/// @brief 获取指定的cpu上正在执行的进程的pcb
#[inline]
pub fn cpu_executing(cpu_id:u32) -> *const process_control_block{
    // todo: 引入per_cpu之后，该函数真正执行“返回指定的cpu上正在执行的pcb”的功能

    if cpu_id == process_cpu(current_pcb()){
        return current_pcb();
    }else {
        todo!()
    }
}