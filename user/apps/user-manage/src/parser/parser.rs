use super::cmd::{CmdOption, GroupCommand, PasswdCommand, UserCommand};
use crate::error::error::{ErrorHandler, ExitStatus};
use std::collections::HashMap;

/// 用户命令(useradd/userdel/usermod)解析器
pub struct UserParser;

impl UserParser {
    /// **解析用户命令**
    ///
    /// ## 参数
    /// - `args`: 用户命令参数
    ///
    /// ## 返回
    /// - `UserCommand`: 用户命令
    pub fn parse(args: Vec<String>) -> UserCommand {
        let username = args.last().unwrap().clone();
        let args = &args[1..args.len() - 1];
        let mut options = HashMap::new();

        let mut idx = 0;
        loop {
            if idx >= args.len() {
                break;
            }
            let option: CmdOption = args[idx].clone().into();
            match option {
                CmdOption::Invalid => invalid_handle(),
                CmdOption::Remove => {
                    if idx + 1 < args.len() {
                        let op: &str = option.clone().into();
                        ErrorHandler::error_handle(
                            format!("Invalid arg {} of option: {}", args[idx + 1], op),
                            ExitStatus::InvalidCmdSyntax,
                        )
                    }
                    options.insert(option, "".to_string());
                }
                CmdOption::Append => {
                    if idx + 1 >= args.len() || idx + 2 >= args.len() || args[idx + 1] != "-G" {
                        ErrorHandler::error_handle(
                            "Invalid option: -a -G <group1,group2,...>".to_string(),
                            ExitStatus::InvalidCmdSyntax,
                        );
                    }
                    idx += 2;
                    let groups = &args[idx];
                    options.insert(option, groups.clone());
                }
                _ => {
                    if idx + 1 >= args.len() {
                        let op: &str = option.clone().into();
                        ErrorHandler::error_handle(
                            format!("Invalid arg of option: {}", op),
                            ExitStatus::InvalidCmdSyntax,
                        );
                    }
                    idx += 1;
                    let value = args[idx].clone();
                    options.insert(option, value);
                }
            }
            idx += 1;
        }

        UserCommand { username, options }
    }
}

/// passwd命令解析器
pub struct PasswdParser;

impl PasswdParser {
    /// **解析passwd命令**
    ///
    /// ## 参数
    /// - `args`: passwd命令参数
    ///
    /// ## 返回
    /// - `PasswdCommand`: passwd命令
    pub fn parse(args: Vec<String>) -> PasswdCommand {
        let mut username = None;
        if args.len() > 1 {
            username = Some(args.last().unwrap().clone());
        }
        PasswdCommand { username }
    }
}

/// 组命令(groupadd/groupdel/groupmod)解析器
pub struct GroupParser;

impl GroupParser {
    /// **解析组命令**
    ///
    /// ## 参数
    /// - `args`: 组命令参数
    ///
    /// ## 返回
    /// - `GroupCommand`: 组命令
    pub fn parse(args: Vec<String>) -> GroupCommand {
        let groupname = args.last().unwrap().clone();
        let args = &args[1..args.len() - 1];
        let mut options = HashMap::new();

        let mut idx = 0;
        loop {
            if idx >= args.len() {
                break;
            }
            let option: CmdOption = args[idx].clone().into();
            match option {
                CmdOption::Invalid => invalid_handle(),
                _ => {
                    if idx + 1 >= args.len() {
                        let op: &str = option.clone().into();
                        ErrorHandler::error_handle(
                            format!("Invalid arg of option: {}", op),
                            ExitStatus::InvalidCmdSyntax,
                        );
                    }
                    idx += 1;
                    let value = args[idx].clone();
                    options.insert(option, value);
                }
            }
            idx += 1;
        }

        GroupCommand { groupname, options }
    }
}

#[inline]
fn invalid_handle() {
    ErrorHandler::error_handle("Invalid option".to_string(), ExitStatus::InvalidCmdSyntax);
}
