use std::process::exit;

use check::Check;
use error::ErrorHandler;
use executor::Executor;
use libc::geteuid;
use parser::Parser;

mod check;
mod error;
mod executor;
mod parser;

fn main() {
    // 判断是否具有root用户权限
    if unsafe { geteuid() } != 0 {
        ErrorHandler::error_handle(
            "permission denied (are you root?)".to_string(),
            error::ExitStatus::PermissionDenied,
        );
    }

    let args = std::env::args().collect::<Vec<_>>();
    if args.len() < 2 {
        ErrorHandler::error_handle(
            "usage: groupdel groupname".to_string(),
            error::ExitStatus::InvalidCmdSyntax,
        );
    }

    let cmd = Parser::parse(args);
    let info = Check::check(cmd);
    Executor::execute(info.clone());

    println!("group: [{}] deleted successfully!", info.groupname);
    exit(crate::error::ExitStatus::Success as i32);
}
