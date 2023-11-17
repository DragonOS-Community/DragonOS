#[macro_use]
extern crate lazy_static;
extern crate cc;

mod bindgen;
mod cfiles;
mod constant;
mod kconfig;
mod utils;

/// 运行构建
pub fn run() {
    println!("cargo:rustc-link-search=src");

    crate::bindgen::generate_bindings();
    crate::cfiles::CFilesBuilder::build();
    crate::kconfig::KConfigBuilder::build();
}
