use crate::{
    check::check::GModCheck,
    error::error::{ErrorHandler, ExitStatus},
    executor::executor::GModExecutor,
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
    if !cmd.options.is_empty() {
        let info = GModCheck::check(cmd);
        let groupname = info.groupname.clone();
        GModExecutor::execute(info);
        println!("Modify group [{}]  successfully!", groupname);
    }

    exit(ExitStatus::Success as i32);
}
