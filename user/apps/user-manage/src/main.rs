use libc::geteuid;
use std::process::exit;
use user_manage_tool::{
    check::*,
    error::{self, ErrorHandler},
    executor::*,
    parser::*,
};

fn main() {
    let args = std::env::args().collect::<Vec<_>>();

    match args[0].as_str() {
        "useradd" => useradd(args),
        "userdel" => userdel(args),
        "usermod" => usermod(args),
        "passwd" => passwd(args),
        "groupadd" | "/bin/groupadd" => groupadd(args),
        "groupdel" => groupdel(args),
        "groupmod" => groupmod(args),
        _ => ErrorHandler::error_handle(
            "command not found".to_string(),
            error::ExitStatus::InvalidCmdSyntax,
        ),
    }

    exit(crate::error::ExitStatus::Success as i32);
}

fn check_root() {
    if unsafe { geteuid() } != 0 {
        ErrorHandler::error_handle(
            "permission denied (are you root?)".to_string(),
            error::ExitStatus::PermissionDenied,
        )
    }
}

fn useradd(args: Vec<String>) {
    check_root();

    if args.len() < 2 {
        ErrorHandler::error_handle(
            "usage: useradd [options] username".to_string(),
            error::ExitStatus::InvalidCmdSyntax,
        );
    }

    let cmd = UserParser::parse(args);
    let info = UAddCheck::check(cmd);
    let username = info.username.clone();
    UAddExecutor::execute(info);
    println!("Add user[{}] successfully!", username);
}

fn userdel(args: Vec<String>) {
    check_root();

    if args.len() < 2 {
        ErrorHandler::error_handle(
            "usage: userdel [options] username".to_string(),
            crate::error::ExitStatus::InvalidCmdSyntax,
        );
    }

    let cmd = UserParser::parse(args);
    let info = UDelCheck::check(cmd);
    let username = info.username.clone();
    UDelExecutor::execute(info);
    println!("Delete user[{}] successfully!", username);
}

fn usermod(args: Vec<String>) {
    check_root();

    if args.len() < 2 {
        ErrorHandler::error_handle(
            "usage: usermod [options] username".to_string(),
            error::ExitStatus::InvalidCmdSyntax,
        );
    }

    let cmd = UserParser::parse(args);
    if !cmd.options.is_empty() {
        let info = UModCheck::check(cmd);
        let username = info.username.clone();
        UModExecutor::execute(info);
        println!("Modify user[{}] successfully!", username);
    }
}

fn passwd(args: Vec<String>) {
    let cmd = PasswdParser::parse(args);
    let info = PasswdCheck::check(cmd);
    PasswdExecutor::execute(info);
}

fn groupadd(args: Vec<String>) {
    check_root();

    if args.len() < 2 {
        ErrorHandler::error_handle(
            "usage: groupadd [options] groupname".to_string(),
            error::ExitStatus::InvalidCmdSyntax,
        );
    }

    let cmd = GroupParser::parse(args);
    let info = GAddCheck::check(cmd);
    let groupname = info.groupname.clone();
    GAddExecutor::execute(info);

    println!("Add group [{}] successfully!", groupname);
}

fn groupdel(args: Vec<String>) {
    check_root();

    if args.len() < 2 {
        ErrorHandler::error_handle(
            "usage: groupdel groupname".to_string(),
            error::ExitStatus::InvalidCmdSyntax,
        );
    }

    let cmd = GroupParser::parse(args);
    let info = GDelCheck::check(cmd);
    let groupname = info.groupname.clone();
    GDelExecutor::execute(info);

    println!("Delete group [{}]  successfully!", groupname);
}

fn groupmod(args: Vec<String>) {
    check_root();

    if args.len() < 2 {
        ErrorHandler::error_handle(
            "usage: groupmod [options] groupname".to_string(),
            error::ExitStatus::InvalidCmdSyntax,
        );
    }
    let cmd = GroupParser::parse(args);
    if !cmd.options.is_empty() {
        let info = GModCheck::check(cmd);
        let groupname = info.groupname.clone();
        GModExecutor::execute(info);
        println!("Modify group [{}]  successfully!", groupname);
    }
}
