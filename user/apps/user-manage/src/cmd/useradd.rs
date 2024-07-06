use crate::{
    check::check::UAddCheck,
    error::error::{ErrorHandler, ExitStatus},
    executor::executor::UAddExecutor,
    parser::parser::UserParser,
};
use libc::geteuid;
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

    if unsafe { geteuid() } != 0 {
        ErrorHandler::error_handle(
            "permission denied (are you root?)".to_string(),
            ExitStatus::PermissionDenied,
        )
    }

    if args.len() < 2 {
        ErrorHandler::error_handle(
            format!("usage: {} [options] username", args[0]),
            ExitStatus::InvalidCmdSyntax,
        );
    }

    let cmd = UserParser::parse(args);
    let info = UAddCheck::check(cmd);
    let username = info.username.clone();
    UAddExecutor::execute(info);
    println!("Add user[{}] successfully!", username);

    exit(ExitStatus::Success as i32);
}
