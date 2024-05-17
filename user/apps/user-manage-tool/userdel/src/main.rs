use check::Check;
use error::{ErrorHandler, ExitStatus};
use executor::Executor;
use libc::geteuid;
use parser::Parser;
use std::process::exit;

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
            "usage: userdel [options] username".to_string(),
            ExitStatus::InvalidCmdSyntax,
        );
    }

    let cmd = Parser::parse(args);
    let info = Check::check(cmd);
    Executor::execute(info.clone());
    println!("Delete user[{}] successfully!", info.username);
    exit(crate::error::ExitStatus::Success as i32);
}
