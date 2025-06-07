use aya::programs::TracePoint;
#[rustfmt::skip]
use log::{debug, warn};
use tokio::{signal, task::yield_now, time};
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // env_logger::init();

    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .format_timestamp(None)
        .init();
    // Bump the memlock rlimit. This is needed for older kernels that don't use the
    // new memcg based accounting, see https://lwn.net/Articles/837122/
    let rlim = libc::rlimit {
        rlim_cur: libc::RLIM_INFINITY,
        rlim_max: libc::RLIM_INFINITY,
    };
    let ret = unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &rlim) };
    if ret != 0 {
        debug!("remove limit on locked memory failed, ret is: {ret}");
    }

    // This will include your eBPF object file as raw bytes at compile-time and load it at
    // runtime. This approach is recommended for most real-world use cases. If you would
    // like to specify the eBPF program at runtime rather than at compile-time, you can
    // reach for `Bpf::load_file` instead.
    let mut ebpf = aya::Ebpf::load(aya::include_bytes_aligned!(concat!(
        env!("OUT_DIR"),
        "/mytrace"
    )))?;
    if let Err(e) = aya_log::EbpfLogger::init(&mut ebpf) {
        // This can happen if you remove all log statements from your eBPF program.
        warn!("failed to initialize eBPF logger: {e}");
    }
    let program: &mut TracePoint = ebpf.program_mut("mytrace").unwrap().try_into()?;
    program.load()?;
    program.attach("syscalls", "sys_enter_openat")?;

    tokio::spawn(async move {
        let mut now = time::Instant::now();
        loop {
            let new_now = time::Instant::now();
            let duration = new_now.duration_since(now);
            if duration.as_secs() >= 2 {
                // open a file to trigger the tracepoint
                log::info!("Triggering tracepoint by opening /bin");
                let bin = std::fs::File::open("/bin").unwrap();
                drop(bin);
                now = new_now;
            }
            yield_now().await;
        }
    });

    let ctrl_c = signal::ctrl_c();
    println!("Waiting for Ctrl-C...");
    ctrl_c.await?;
    println!("Exiting...");

    Ok(())
}
