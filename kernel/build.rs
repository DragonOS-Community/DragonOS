extern crate bindgen;
extern crate cc;
// use ::std::env;

use std::path::PathBuf;

use cc::Build;

fn main() {
    // Tell cargo to look for shared libraries in the specified directory
    println!("cargo:rustc-link-search=src");
    println!("cargo:rerun-if-changed=src/include/bindings/wrapper.h");

    generate_bindings();
    CFilesBuilder::build();
}

fn generate_bindings() {
    // let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_path = PathBuf::from(String::from("src/include/bindings/"));

    // The bindgen::Builder is the main entry point
    // to bindgen, and lets you build up options for
    // the resulting bindings.
    {
        let bindings = bindgen::Builder::default()
            .clang_arg("-I./src")
            .clang_arg("-I./src/include")
            .clang_arg("-I./src/arch/x86_64/include") // todo: 当引入多种架构之后，需要修改这里，对于不同的架构编译时，include不同的路径
            // The input header we would like to generate
            // bindings for.
            .header("src/include/bindings/wrapper.h")
            .blocklist_file("src/include/bindings/bindings.h")
            .clang_arg("--target=x86_64-none-none")
            .clang_arg("-v")
            // 使用core，并将c语言的类型改为core::ffi，而不是使用std库。
            .use_core()
            .ctypes_prefix("::core::ffi")
            .generate_inline_functions(true)
            .raw_line("#![allow(dead_code)]")
            .raw_line("#![allow(non_upper_case_globals)]")
            .raw_line("#![allow(non_camel_case_types)]")
            // Tell cargo to invalidate the built crate whenever any of the
            // included header files changed.
            .parse_callbacks(Box::new(bindgen::CargoCallbacks))
            // Finish the builder and generate the bindings.
            .generate()
            // Unwrap the Result and panic on failure.
            .expect("Unable to generate bindings");

        bindings
            .write_to_file(out_path.join("bindings.rs"))
            .expect("Couldn't write bindings!");
    }
}

/// 构建项目的c文件
struct CFilesBuilder;

impl CFilesBuilder {
    fn build() {
        let mut c = cc::Build::new();
        Self::setup_global_flags(&mut c);
        Self::setup_defines(&mut c);
        Self::setup_global_include_dir(&mut c);
        Self::setup_files(&mut c);
        c.compile("dragonos_kernel_cfiles");
    }

    fn setup_global_flags(c: &mut Build) {
        c.flag("-mcmodel=large")
            .flag("-fno-builtin")
            .flag("-nostdlib")
            .flag("-fno-stack-protector")
            .flag("-fno-pie")
            .flag("-Wno-expansion-to-defined")
            .flag("-Wno-unused-parameter")
            .flag("-m64")
            .flag("-O1");
    }

    fn setup_defines(c: &mut Build) {
        if let Ok(k) = std::env::var("EMULATOR") {
            c.define("EMULATOR", Some(k.as_str()));
        } else {
            c.define("EMULATOR", "__NO_EMULATION__");
        }

        {
            #[cfg(target_arch = "x86_64")]
            c.define("__x86_64__", None);
        }

        c.define("PIC", "_INTR_APIC_");
    }

    fn setup_global_include_dir(c: &mut Build) {
        c.include("src/include");
        c.include("src");
        c.include(".");

        #[cfg(target_arch = "x86_64")]
        c.include("src/arch/x86_64/include");
    }

    /// 设置需要编译的文件
    fn setup_files(c: &mut Build) {
        c.file("src/arch/x86_64/driver/hpet.c");
    }
}
