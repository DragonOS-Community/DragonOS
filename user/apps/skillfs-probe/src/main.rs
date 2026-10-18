//! Minimal SkillFS control-socket test client.
//!
//! Sends one newline-delimited `skill.resolveLiveSource` request and prints
//! the raw response line. Intended for end-to-end verification inside the
//! guest; not a general JSON-RPC client.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: skillfs-probe <socket> <canonicalSkillDir>");
        std::process::exit(2);
    }
    let socket_path = &args[1];
    let canonical_dir = &args[2];

    let mut stream = match UnixStream::connect(socket_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("connect {} failed: {}", socket_path, e);
            std::process::exit(1);
        }
    };

    // The escape-free path is intentional: this probe is fed literal
    // canonical directories, and keeping the request hand-rolled avoids any
    // dependency in the guest image.
    let request = format!(
        "{{\"schemaVersion\":\"1\",\"method\":\"skill.resolveLiveSource\",\"canonicalSkillDir\":\"{}\"}}\n",
        canonical_dir
    );
    if let Err(e) = stream
        .write_all(request.as_bytes())
        .and_then(|_| stream.flush())
    {
        eprintln!("write failed: {}", e);
        std::process::exit(1);
    }

    let mut line = String::new();
    let mut reader = BufReader::new(&stream);
    match reader.read_line(&mut line) {
        Ok(0) => eprintln!("server closed connection without a response"),
        Ok(_) => {
            print!("{}", line);
            let _ = std::io::stdout().flush();
        }
        Err(e) => eprintln!("read failed: {}", e),
    }
}
