use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser, Clone)]
pub struct CommandLineArgs {
    #[arg(short, long)]
    /// The kernel ELF file to load.
    kernel: PathBuf,

    /// The kernel memory file to load.
    /// If not specified, the default value is /dev/shm/dragonos-qemu-shm.ram
    #[arg(long, value_parser=kmem_file_parser)]
    kmem: Option<String>,
}

/// 用于解析kmem参数的函数
fn kmem_file_parser(s: &str) -> Result<Option<String>, String> {
    if s.len() == 0 {
        return Ok(Some("/dev/shm/dragonos-qemu-shm.ram".to_string()));
    } else {
        return Ok(Some(s.to_string()));
    }
}
