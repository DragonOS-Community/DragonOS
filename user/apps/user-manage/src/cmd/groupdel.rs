use crate::{
    check::check::GDelCheck,
    error::error::{ErrorHandler, ExitStatus},
    executor::executor::GDelExecutor,
    parser::parser::GroupParser,
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
            format!("usage: {} [options] groupname", args[0]),
            ExitStatus::InvalidCmdSyntax,
        );
    }

    let cmd = GroupParser::parse(args);
    let info = GDelCheck::check(cmd);
    let groupname = info.groupname.clone();
    GDelExecutor::execute(info);

    println!("Delete group [{}]  successfully!", groupname);

    exit(ExitStatus::Success as i32);
}
