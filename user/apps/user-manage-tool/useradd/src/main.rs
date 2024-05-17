use check::Check;
use error::ErrorHandler;
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

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        ErrorHandler::error_handle(
            "usage: useradd [options] username".to_string(),
            error::ExitStatus::InvalidCmdSyntax,
        );
    }

    // 解析参数
    let cmd = Parser::parse(args);
    // 检查参数有效性
    let userinfo = Check::check(cmd);
    // 执行命令
    Executor::execute(userinfo.clone());
    println!("Add user[{}] successfully!", userinfo.username);

    exit(crate::error::ExitStatus::Success as i32);
}
