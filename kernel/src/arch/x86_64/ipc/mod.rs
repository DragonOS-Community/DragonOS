pub mod signal;

use super::interrupt::TrapFrame;

use crate::{
    arch::CurrentIrqArch, exception::InterruptArch, ipc::signal_types::SignalNumber,
    process::ProcessManager,
};
