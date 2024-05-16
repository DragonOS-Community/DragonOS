use crate::{
    error::{ErrorHandler, ExitStatus},
    parser::{CmdOption, UModCommand},
};
use std::{collections::HashSet, fs};

#[derive(Debug, Clone)]
pub struct Info {
    pub username: String,
    pub groups: Option<Vec<String>>,
    pub new_comment: Option<String>,
    pub new_home: Option<String>,
    pub new_group: Option<String>,
    pub new_name: Option<String>,
    pub new_shell: Option<String>,
    pub new_uid: Option<String>,
}

impl Info {
    pub fn new(username: String) -> Self {
        Self {
            username,
            groups: None,
            new_comment: None,
            new_home: None,
            new_group: None,
            new_name: None,
            new_shell: None,
            new_uid: None,
        }
    }
}

pub struct Check;

impl Check {
    /// **校验解析后的usermod命令**
    ///
    /// ## 参数
    /// - `cmd`: usermod命令
    ///
    /// ## 返回
    /// - `Info`: 校验后的信息
    pub fn check(cmd: UModCommand) -> Info {
        let mut info = Info::new(cmd.username);
        for (option, arg) in cmd.options {
            match option {
                CmdOption::Append => {
                    info.groups = Some(arg.split(",").map(|s| s.to_string()).collect());
                }
                CmdOption::Comment => {
                    info.new_comment = Some(arg);
                }
                CmdOption::Home => {
                    info.new_home = Some(arg);
                }
                CmdOption::Group => {
                    info.new_group = Some(arg);
                }
                CmdOption::Name => {
                    let new_name = arg;
                    for c in new_name.chars() {
                        if !c.is_ascii_alphabetic() && !c.is_ascii_digit() {
                            ErrorHandler::error_handle(
                                format!(
                                    "'{}' is invalid, username should be composed of letters and numbers",
                                    c
                                ),
                                crate::error::ExitStatus::InvalidArg,
                            );
                        }
                    }
                    info.new_name = Some(new_name);
                }
                CmdOption::Shell => {
                    info.new_shell = Some(arg);
                }
                CmdOption::Uid => {
                    let uid = arg.parse::<u32>();
                    if uid.is_err() {
                        ErrorHandler::error_handle(
                            format!("uid: {} is invalid", arg),
                            crate::error::ExitStatus::InvalidArg,
                        );
                    }
                    info.new_uid = Some(arg);
                }
                _ => ErrorHandler::error_handle(
                    "Invalid option".to_string(),
                    crate::error::ExitStatus::InvalidCmdSyntax,
                ),
            }
        }

        Self::check_shell(&info);
        Self::check_passwd(&info);
        Self::check_group(&info);

        info
    }

    /// 检验终端程序是否有效
    fn check_shell(info: &Info) {
        if let Some(shell) = &info.new_shell {
            if let Ok(file) = fs::File::open(shell) {
                if !file.metadata().unwrap().is_file() {
                    ErrorHandler::error_handle(
                        format!("{} is not a file", shell),
                        crate::error::ExitStatus::InvalidArg,
                    );
                }
            } else {
                ErrorHandler::error_handle(
                    format!("{} doesn't exist", shell),
                    crate::error::ExitStatus::InvalidArg,
                );
            }
        }
    }

    // 扫描/etc/passwd文件检验new_home、new_name、new_uid、username
    fn check_passwd(info: &Info) {
        if !info.new_home.is_none() || !info.new_name.is_none() || !info.new_uid.is_none() {
            let mut is_user_exist = false;
            let content = fs::read_to_string("/etc/passwd");
            match content {
                Ok(content) => {
                    for line in content.lines() {
                        let fields = line.split(":").collect::<Vec<&str>>();
                        let (username, home, uid) = (fields[0], fields[5], fields[2]);
                        if let Some(new_name) = &info.new_name {
                            if new_name == username {
                                ErrorHandler::error_handle(
                                    format!("{} already exists", new_name),
                                    crate::error::ExitStatus::UsernameInUse,
                                );
                            }
                        }
                        if let Some(new_home) = &info.new_home {
                            if new_home == home {
                                ErrorHandler::error_handle(
                                    format!("{} already exists", new_home),
                                    crate::error::ExitStatus::InvalidArg,
                                );
                            }
                        }
                        if let Some(new_uid) = &info.new_uid {
                            if new_uid == uid {
                                ErrorHandler::error_handle(
                                    format!("{} already exists", new_uid),
                                    crate::error::ExitStatus::UidInUse,
                                );
                            }
                        }
                        if username == info.username {
                            is_user_exist = true;
                        }
                    }
                    if !is_user_exist {
                        ErrorHandler::error_handle(
                            format!("{} doesn't exist", info.username),
                            crate::error::ExitStatus::InvalidArg,
                        );
                    }
                }
                Err(_) => {
                    ErrorHandler::error_handle(
                        format!("/etc/passwd doesn't exist"),
                        ExitStatus::PasswdFile,
                    );
                }
            }
        }
    }

    // 扫描/etc/group文件检验groups、new_group
    fn check_group(info: &Info) {
        if !info.groups.is_none() || !info.new_group.is_none() {
            let content = fs::read_to_string("/etc/group");
            let mut set1 = HashSet::new();
            let mut set2 = HashSet::new();
            if let Some(groups) = info.groups.clone() {
                set2.extend(groups.into_iter())
            }
            if let Some(group) = &info.new_group {
                set2.insert(group.clone());
            }

            match content {
                Ok(content) => {
                    for line in content.lines() {
                        let fields = line.split(":").collect::<Vec<&str>>();
                        set1.insert(fields[0].to_string());
                    }
                }
                Err(_) => {
                    ErrorHandler::error_handle(
                        format!("/etc/group doesn't exist"),
                        ExitStatus::GroupFile,
                    );
                }
            }

            let mut non_exist_group = Vec::new();
            for group in set2.iter() {
                if !set1.contains(group) {
                    non_exist_group.push(group.clone());
                }
            }

            if non_exist_group.len() > 0 {
                ErrorHandler::error_handle(
                    format!("group: {} doesn't exist", non_exist_group.join(",")),
                    ExitStatus::GroupNotExist,
                );
            }
        }
    }
}
