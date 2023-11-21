use super::BindgenArch;

pub struct X86_64BindgenArch;

impl BindgenArch for X86_64BindgenArch {
    fn generate_bindings(&self, builder: bindgen::Builder) -> bindgen::Builder {
        builder
            .clang_arg("-I./src/arch/x86_64/include")
            .clang_arg("--target=x86_64-none-none")
    }
}
