use aya::maps::HashMap;
use aya::programs::KProbe;
use aya::{include_bytes_aligned, Ebpf};
use aya_log::EbpfLogger;
use log::{info, warn};
use std::error::Error;
use tokio::task::yield_now;
use tokio::{signal, time};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    env_logger::builder()
        .filter_level(log::LevelFilter::Warn)
        .format_timestamp(None)
        .init();

    let mut bpf = Ebpf::load(include_bytes_aligned!(
        "../syscall_ebpf/target/bpfel-unknown-none/release/syscall_ebpf"
    ))?;

    // create a async task to read the log
    if let Err(e) = EbpfLogger::init(&mut bpf) {
        // This can happen if you remove all log statements from your eBPF program.
        warn!("failed to initialize eBPF logger: {}", e);
    }

    let program: &mut KProbe = bpf.program_mut("syscall_ebpf").unwrap().try_into()?;
    program.load()?;
    program.attach("dragonos_kernel::syscall::Syscall::handle", 0)?;

    info!("attacch the kprobe to dragonos_kernel::syscall::Syscall::handle");

    // print the value of the blocklist per 5 seconds
    tokio::spawn(async move {
        let blocklist: HashMap<_, u32, u32> =
            HashMap::try_from(bpf.map("SYSCALL_LIST").unwrap()).unwrap();
        let mut now = time::Instant::now();
        loop {
            let new_now = time::Instant::now();
            let duration = new_now.duration_since(now);
            if duration.as_secs() >= 5 {
                println!("------------SYSCALL_LIST----------------");
                let iter = blocklist.iter();
                for item in iter {
                    if let Ok((key, value)) = item {
                        println!("syscall: {:?}, count: {:?}", key, value);
                    }
                }
                println!("----------------------------------------");
                now = new_now;
            }
            yield_now().await;
        }
    });

    info!("Waiting for Ctrl-C...");
    signal::ctrl_c().await?;
    info!("Exiting...");
    Ok(())
}
