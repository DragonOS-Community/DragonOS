use std::{env, process::exit};

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

    let args = env::args().collect::<Vec<String>>();
    if args.len() < 2 {
        ErrorHandler::error_handle(
            "usage: usermod [options] username".to_string(),
            error::ExitStatus::InvalidCmdSyntax,
        );
    }

    let cmd = Parser::parse(args);
    if !cmd.options.is_empty() {
        let info = Check::check(cmd);
        Executor::execute(info.clone());
        println!("Modify user[{}] successfully!", info.username);
    }
    exit(crate::error::ExitStatus::Success as i32);
}
