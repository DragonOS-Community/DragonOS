use super::BindgenArch;

pub struct RiscV64BindgenArch;
impl BindgenArch for RiscV64BindgenArch {
    fn generate_bindings(&self, builder: bindgen::Builder) -> bindgen::Builder {
        builder
            .clang_arg("-I./src/arch/riscv64/include")
            .clang_arg("--target=riscv64-none-none-elf")
    }
}
