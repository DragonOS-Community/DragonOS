use crate::utils::cargo_handler::{CargoHandler, TargetArch};

use self::x86_64::X86_64BindgenArch;

pub mod riscv64;
pub mod x86_64;

pub(super) trait BindgenArch {
    fn generate_bindings(&self, builder: bindgen::Builder) -> bindgen::Builder;
}

/// 获取当前的bindgen架构;
pub(super) fn current_bindgenarch() -> &'static dyn BindgenArch {
    let arch = CargoHandler::target_arch();
    match arch {
        TargetArch::X86_64 => &X86_64BindgenArch,
        TargetArch::Riscv64 => &riscv64::RiscV64BindgenArch,
        _ => panic!("Unsupported arch: {:?}", arch),
    }
}
