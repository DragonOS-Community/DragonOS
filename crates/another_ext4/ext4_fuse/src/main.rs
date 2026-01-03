#![feature(trait_upcasting)]

mod block_dev;
mod common;
mod fuse_fs;

use block_dev::BlockMem;
use clap::Parser;
use fuse_fs::StateExt4FuseFs;
use fuser::MountOption;
use log::LevelFilter;
use simple_logger::SimpleLogger;
use std::sync::{Arc, OnceLock};

#[derive(Parser, Debug)]
#[command(about = "Another ext4 FUSE Tool")]
struct Args {
    /// Fs mount point
    #[arg(short, long)]
    mountpoint: String,
    /// Load initial image
    #[arg(short, long)]
    image: Option<String>,
    /// Fs total block number, ignored when [image] is set
    #[arg(short, long, default_value_t = 8192)]
    block: u64,
    /// Save image on exit
    #[arg(short, long)]
    output: Option<String>,
    /// Log level
    #[arg(short, long, default_value_t = String::from("info"))]
    log: String,
}

fn parse_log_level(level_str: &str) -> LevelFilter {
    match level_str.to_lowercase().as_str() {
        "off" => LevelFilter::Off,
        "error" => LevelFilter::Error,
        "warn" => LevelFilter::Warn,
        "info" => LevelFilter::Info,
        "debug" => LevelFilter::Debug,
        "trace" => LevelFilter::Trace,
        _ => LevelFilter::Off,
    }
}

/// Global exit flag
static EXIT_FLAG: OnceLock<bool> = OnceLock::new();

fn main() {
    let args = Args::parse();

    // Initialize logger
    println!("Log level {}", args.log);
    SimpleLogger::new().init().unwrap();
    log::set_max_level(parse_log_level(&args.log));

    // Initialize block device and filesystem
    let block_mem = if let Some(image) = &args.image {
        println!("Load image {}", image);
        Arc::new(BlockMem::load(&image))
    } else {
        println!("Create disk image with {} blocks", args.block);
        let block_mem = Arc::new(BlockMem::new(args.block));
        block_mem.mkfs();
        block_mem
    };
    // Create filesystem and init if image is newly created
    let fs = StateExt4FuseFs::new(block_mem.clone(), args.image.is_none());

    // Mount fs and enter session loop
    println!("Mount ext4fs to {}", args.mountpoint);
    let options = Vec::<MountOption>::new();
    let session =
        fuser::spawn_mount2(fs, &args.mountpoint, &options).expect("Failed to start FUSE session");
    
    // Set EXIT_FLAG when Ctrl+C is received
    let _ = ctrlc::set_handler(|| {
        EXIT_FLAG.get_or_init(|| true);
    });
    // Loop until EXIT_FLAG is set
    loop {
        if EXIT_FLAG.get().is_some() {
            println!("Received Ctrl+C, exiting...");
            if let Some(output) = &args.output {
                println!("Save image {}", output);
                block_mem.save(output);
            }
            break;
        }
    }
    session.join();
}
