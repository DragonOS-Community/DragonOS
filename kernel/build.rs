extern crate bindgen;

extern crate cbindgen;

use ::std::env;

use std::path::PathBuf;

fn main() {
    // Tell cargo to look for shared libraries in the specified directory
    println!("cargo:rustc-link-search=src");
    println!("cargo:rerun-if-changed=src/include/bindings/wrapper.h");

    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_path = PathBuf::from(String::from("src/include/bindings/"));

    // The bindgen::Builder is the main entry point
    // to bindgen, and lets you build up options for
    // the resulting bindings.
    {
        let bindings = bindgen::Builder::default()
            .clang_arg("-I./src")
            // The input header we would like to generate
            // bindings for.
            .header("src/include/bindings/wrapper.h")
            .clang_arg("--target=x86_64-none-none")
            .clang_arg("-v")
            // 使用core，并将c语言的类型改为core::ffi，而不是使用std库。
            .use_core()
            .ctypes_prefix("::core::ffi")
            .generate_inline_functions(true)
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

    cbindgen::generate(crate_dir)
        .unwrap()
        .write_to_file(out_path.join("bindings.h"));
}
