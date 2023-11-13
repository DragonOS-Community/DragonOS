use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser, Clone)]
pub struct CommandLineArgs {
    #[arg(short, long)]
    /// The kernel ELF file to load.
    pub kernel: PathBuf,

    /// The kernel memory file to load.
    #[arg(long, value_parser=kmem_file_parser, default_value = "/dev/shm/dragonos-qemu-shm.ram")]
    pub kmem: String,

    /// If set, the monitor will start the TUI.
    #[arg(long, default_value = "false")]
    pub tui: bool,

    /// The directory to store the log files.
    #[arg(long, default_value = "logs")]
    pub log_dir: PathBuf,
}

/// 用于解析kmem参数的函数
fn kmem_file_parser(s: &str) -> Result<String, String> {
    log::warn!("kmem_file_parser: {}", s);
    if s.len() == 0 {
        return Ok("/dev/shm/dragonos-qemu-shm.ram".to_string());
    } else {
        return Ok(s.to_string());
    }
}
