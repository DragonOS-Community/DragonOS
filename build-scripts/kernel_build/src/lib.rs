#[macro_use]
extern crate lazy_static;
extern crate cc;

mod cfiles;
mod constant;
mod kconfig;
mod utils;
mod version_gen;

/// 运行构建
pub fn run() {
    println!("cargo:rustc-link-search=src");

    crate::cfiles::CFilesBuilder::build();
    crate::kconfig::KConfigBuilder::build();
    crate::version_gen::generate_version();
}
