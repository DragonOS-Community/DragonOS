use std::format;
use args::Args;
use clap::Parser;

mod ping;
mod config;
mod error;
mod args;
fn main(){
    let args = Args::parse();
    println!("args{:?}", args.destination.raw);
    match ping::Ping::new(args.as_config()) {
        Ok(pinger) => {
            pinger.run().unwrap_or_else(|e| 
            {
                exit(format!("Error on run ping: {}", e));
            })
        },
        Err(e) => {
            exit(format!("Error on init: {}", e))
        }
    }
}

fn exit(msg: String) {
    eprintln!("{}", msg);
    std::process::exit(1);
}
