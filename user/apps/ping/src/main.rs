use args::Args;
use clap::Parser;
use std::format;

mod args;
mod config;
mod error;
mod ping;
fn main() {
    let args = Args::parse();
    match ping::Ping::new(args.as_config()) {
        Ok(pinger) => pinger.run().unwrap_or_else(|e| {
            exit(format!("Error on run ping: {}", e));
        }),
        Err(e) => exit(format!("Error on init: {}", e)),
    }
}

fn exit(msg: String) {
    eprintln!("{}", msg);
    std::process::exit(1);
}
