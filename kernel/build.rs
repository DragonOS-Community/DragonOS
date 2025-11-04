use std::env;
use std::path::Path;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let path = format!("{}/initram/x86.cpio.xz", manifest_dir);
    if Path::new(&path).exists() {
        println!("cargo:rustc-cfg=has_initram_x86");
    }

    kernel_build::run();
}
