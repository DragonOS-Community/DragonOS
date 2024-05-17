use crate::{error::ErrorHandler, parser::PwdCommand};
use std::{fs, io::Write};

#[derive(Debug)]
pub struct Info {
    pub username: String,
    pub new_password: String,
}

pub struct Check;

impl Check {
    /// **校验解析后的passwd命令**
    ///
    /// ## 参数
    /// - `cmd`: 解析后的passwd命令
    ///
    /// ## 返回
    /// - `Info`: 校验后的passwd命令
    pub fn check(cmd: PwdCommand) -> Info {
        let uid = unsafe { libc::geteuid().to_string() };
        let cur_username = Self::cur_username(uid.clone());
        let mut to_change_username = String::new();

        if let Some(username) = cmd.username {
            to_change_username = username.clone();

            // 不是root用户不能修改别人的密码
            if uid != "0" && cur_username != username {
                ErrorHandler::error_handle(
                    "You can't change password for other users".to_string(),
                    crate::error::ExitStatus::PermissionDenied,
                );
            }

            // 要修改密码的用户不存在
            if !Self::is_user_exist(&username) {
                ErrorHandler::error_handle(
                    format!("User: [{}] doesn't exist", username),
                    crate::error::ExitStatus::InvalidArg,
                );
            }
        }

        let mut new_password = String::new();
        match uid.as_str() {
            "0" => {
                if to_change_username.is_empty() {
                    to_change_username = cur_username;
                }
                print!("New password: ");
                std::io::stdout().flush().unwrap();
                std::io::stdin().read_line(&mut new_password).unwrap();
                new_password = new_password.trim().to_string();
                let mut check_password = String::new();
                print!("\nRe-enter new password: ");
                std::io::stdout().flush().unwrap();
                std::io::stdin().read_line(&mut check_password).unwrap();
                check_password = check_password.trim().to_string();
                if new_password != check_password {
                    ErrorHandler::error_handle(
                        "\nThe two passwords that you entered do not match.".to_string(),
                        crate::error::ExitStatus::InvalidArg,
                    )
                }
            }
            _ => {
                to_change_username = cur_username.clone();
                print!("Old password: ");
                std::io::stdout().flush().unwrap();
                let mut old_password = String::new();
                std::io::stdin().read_line(&mut old_password).unwrap();
                old_password = old_password.trim().to_string();
                Self::check_password(cur_username, old_password);
                print!("\nNew password: ");
                std::io::stdout().flush().unwrap();
                std::io::stdin().read_line(&mut new_password).unwrap();
                new_password = new_password.trim().to_string();
                print!("\nRe-enter new password: ");
                std::io::stdout().flush().unwrap();
                let mut check_password = String::new();
                std::io::stdin().read_line(&mut check_password).unwrap();
                check_password = check_password.trim().to_string();
                if new_password != check_password {
                    println!("{}", new_password);
                    ErrorHandler::error_handle(
                        "\nThe two passwords that you entered do not match.".to_string(),
                        crate::error::ExitStatus::InvalidArg,
                    )
                }
            }
        };

        Info {
            username: to_change_username,
            new_password,
        }
    }

    /// **获取uid对应的用户名**
    /// 
    /// ## 参数
    /// - `uid`: 用户id
    /// 
    /// ## 返回
    /// 用户名
    fn cur_username(uid: String) -> String {
        let r = fs::read_to_string("/etc/passwd");
        let mut cur_username = String::new();

        match r {
            Ok(content) => {
                for line in content.lines() {
                    let field = line.split(":").collect::<Vec<&str>>();
                    if uid == field[2] {
                        cur_username = field[0].to_string();
                    }
                }
            }
            Err(_) => {
                ErrorHandler::error_handle(
                    "Can't read /etc/passwd".to_string(),
                    crate::error::ExitStatus::PasswdFile,
                );
            }
        }

        cur_username
    }

    /// 扫描/etc/passwd文件，判断要修改密码的用户是否存在
    fn is_user_exist(username: &String) -> bool {
        let r = fs::read_to_string("/etc/passwd");
        match r {
            Ok(content) => {
                for line in content.lines() {
                    let field = line.split(":").collect::<Vec<&str>>();
                    if field[0] == username {
                        return true;
                    }
                }
            }
            Err(_) => ErrorHandler::error_handle(
                "Can't read /etc/passwd".to_string(),
                crate::error::ExitStatus::PasswdFile,
            ),
        }

        false
    }

    pub fn check_password(username: String, password: String) {
        let r = fs::read_to_string("/etc/shadow");
        match r {
            Ok(content) => {
                for line in content.lines() {
                    let field = line.split(":").collect::<Vec<&str>>();
                    if username == field[0] {
                        if password != field[1] {
                            ErrorHandler::error_handle(
                                "Password error".to_string(),
                                crate::error::ExitStatus::InvalidArg,
                            );
                        } else {
                            return;
                        }
                    }
                }
            }
            Err(_) => {
                ErrorHandler::error_handle(
                    "Can't read /etc/shadow".to_string(),
                    crate::error::ExitStatus::ShadowFile,
                );
            }
        }
    }
}
