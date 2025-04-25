use std::process;

fn main() {
    print_process_info();
}
fn print_process_info() {
    let pid = process::id();
    let pgid = unsafe { libc::getpgid(0) };
    let sid = unsafe { libc::getsid(0) };

    println!("PID: {}", pid);
    match pgid {
        -1 => eprintln!("Failed to get PGID"),
        pgid => println!("PGID: {}", pgid),
    }
    match sid {
        -1 => eprintln!("Failed to get SID"),
        sid => println!("SID: {}", sid),
    }

    println!("Sleeping for 10 seconds");
    std::thread::sleep(std::time::Duration::from_secs(10));
    println!("Woke up after 10 seconds");
}
