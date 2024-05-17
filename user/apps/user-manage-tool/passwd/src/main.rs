use check::Check;
use executor::Executor;
use parser::Parser;
use std::process::exit;

mod check;
mod error;
mod executor;
mod parser;

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let cmd = Parser::parse(args);
    let info = Check::check(cmd);
    Executor::execute(info);

    exit(crate::error::ExitStatus::Success as i32);
}
