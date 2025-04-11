use super::BindgenArch;

pub struct LoongArch64BindgenArch;
impl BindgenArch for LoongArch64BindgenArch {
    fn generate_bindings(&self, builder: bindgen::Builder) -> bindgen::Builder {
        builder
            .clang_arg("-I./src/arch/loongarch64/include")
            .clang_arg("--target=x86_64-none-none") // 由于clang不支持loongarch64，所以使用x86_64作为目标，按理来说问题不大
    }
}
