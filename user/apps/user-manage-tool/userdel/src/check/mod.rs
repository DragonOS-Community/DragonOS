use crate::{
    error::{ErrorHandler, ExitStatus},
    parser::{CmdOption, UDelCommand},
};
use std::fs;

#[derive(Debug, Clone)]
pub struct Info {
    pub username: String,
    pub home: Option<String>,
}

impl Info {
    pub fn new() -> Self {
        Self {
            username: "".to_string(),
            home: None,
        }
    }
}

pub struct Check;

impl Check {
    /// **校验userdel命令**
    ///
    /// ## 参数
    /// - cmd: userdel命令
    ///
    /// ## 返回
    /// - Info: 用户信息
    pub fn check(cmd: UDelCommand) -> Info {
        let mut info = Info::new();

        // 检验用户是否存在
        let contents = fs::read_to_string("/etc/passwd");
        if let Ok(contents) = contents {
            for line in contents.lines() {
                let user_info = line.split(":").collect::<Vec<&str>>();
                if user_info[0] == cmd.username {
                    info.username = cmd.username.to_string();
                    info.home = Some(user_info[5].to_string());
                    break;
                }
            }
            if info.username.is_empty() {
                ErrorHandler::error_handle(
                    format!("user {} doesn't exist", cmd.username),
                    ExitStatus::InvalidArg,
                );
            }
        } else {
            ErrorHandler::error_handle(
                "/etc/passwd doesn't exist".to_string(),
                ExitStatus::PasswdFile,
            );
        }

        if let Some(home_dir) = cmd.options.get(&CmdOption::Remove) {
            // 判断home目录是否有效
            if let Ok(file) = fs::File::open(home_dir) {
                if !file.metadata().unwrap().is_dir() {
                    ErrorHandler::error_handle(
                        format!("{} is not a dir", home_dir),
                        ExitStatus::InvalidArg,
                    );
                }
            } else {
                ErrorHandler::error_handle(
                    format!("{} doesn't exist", home_dir),
                    ExitStatus::InvalidArg,
                );
            }
        }

        info
    }
}
