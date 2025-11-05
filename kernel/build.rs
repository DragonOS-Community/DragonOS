use std::env;
use std::path::Path;

fn main() {
    if let Ok(initram_path) = env::var("INITRAM_PATH") {
        if Path::new(&initram_path).exists() {
            println!("cargo:rustc-cfg=has_initram");
            // 将路径传递给编译时常量
            println!("cargo:rustc-env=INITRAM_PATH={}", initram_path);
            println!("cargo:rerun-if-env-changed=INITRAM_PATH");
        }
    }

    kernel_build::run();
}
