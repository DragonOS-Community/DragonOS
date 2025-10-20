// build.rs
use std::path::Path;

fn main() {
    let archs = ["x86_64", "riscv64", "loongarch64"];
    let mut has_initramfs = false;

    for arch in archs {
        let path = format!("initram/{}.cpio.xz", arch);
        if Path::new(&path).exists() {
            has_initramfs = true;
            println!("cargo:rerun-if-changed={}", path);
        }
    }

    if has_initramfs {
        println!("cargo:rustc-cfg=has_initramfs");
    }
}
