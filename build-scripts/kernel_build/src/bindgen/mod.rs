use std::{path::PathBuf, str::FromStr};

use crate::{bindgen::arch::current_bindgenarch, utils::cargo_handler::CargoHandler};

mod arch;

/// 生成 C->Rust bindings
pub fn generate_bindings() {
    let wrapper_h = PathBuf::from_str("src/include/bindings/wrapper.h")
        .expect("Failed to parse 'wrapper.h' path");
    CargoHandler::emit_rerun_if_files_changed(&[wrapper_h.clone()]);

    let out_path = PathBuf::from(String::from("src/include/bindings/"));

    // The bindgen::Builder is the main entry point
    // to bindgen, and lets you build up options for
    // the resulting bindings.

    let builder = bindgen::Builder::default()
        .clang_arg("-I./src")
        .clang_arg("-I./src/include")
        // The input header we would like to generate
        // bindings for.
        .header(wrapper_h.to_str().unwrap())
        .blocklist_file("src/include/bindings/bindings.h")
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
        .parse_callbacks(Box::new(bindgen::CargoCallbacks));

    // 处理架构相关的绑定
    let builder = current_bindgenarch().generate_bindings(builder);

    // Finish the builder and generate the bindings.
    let bindings = builder
        .generate()
        // Unwrap the Result and panic on failure.
        .expect("Unable to generate bindings");

    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}
