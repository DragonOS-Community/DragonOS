use crate::{
    check::check::PasswdCheck, error::error::ExitStatus, executor::executor::PasswdExecutor,
    parser::parser::PasswdParser,
};
use std::process::exit;

#[path = "../check/mod.rs"]
mod check;
#[path = "../error/mod.rs"]
mod error;
#[path = "../executor/mod.rs"]
mod executor;
#[path = "../parser/mod.rs"]
mod parser;

#[allow(dead_code)]
fn main() {
    let args = std::env::args().collect::<Vec<_>>();

    let cmd = PasswdParser::parse(args);
    let info = PasswdCheck::check(cmd);
    PasswdExecutor::execute(info);

    exit(ExitStatus::Success as i32);
}
