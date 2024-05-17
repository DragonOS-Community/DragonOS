use self::userinfo::UserInfo;
use crate::{
    error::ErrorHandler,
    parser::{CmdOption, UAddCommand},
};
use std::fs;

pub mod userinfo;

/// 检查参数
pub struct Check;

impl Check {
    /// **校验函数**
    ///
    /// ## 参数
    /// - `cmd`: useradd指令
    ///
    /// ## 返回
    /// - `UserInfo`: 用户信息
    pub fn check(cmd: UAddCommand) -> UserInfo {
        // 用户名只能由字母、数字和下划线组成
        for c in cmd.username.chars() {
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

        // 填充用户信息
        let mut userinfo = UserInfo::default();
        userinfo.username = cmd.username.clone();
        for (option, arg) in cmd.options.iter() {
            match option {
                CmdOption::Shell => {
                    userinfo.shell = arg.clone();
                }
                CmdOption::Comment => {
                    userinfo.comment = arg.clone();
                }
                CmdOption::Uid => {
                    userinfo.uid = arg.clone();
                }
                CmdOption::Group => {
                    userinfo.group = arg.clone();
                }
                CmdOption::Home => {
                    userinfo.home_dir = arg.clone();
                }
                _ => unimplemented!(),
            }
        }
        // 完善用户信息
        if userinfo.username.is_empty() {
            ErrorHandler::error_handle(
                "Invalid username".to_string(),
                crate::error::ExitStatus::InvalidArg,
            );
        }
        if userinfo.comment.is_empty() {
            userinfo.comment = userinfo.username.clone() + ",,,";
        }
        if userinfo.home_dir.is_empty() {
            let home_dir = format!("/home/{}", userinfo.username.clone());
            userinfo.home_dir = home_dir;
        }
        if userinfo.shell.is_empty() {
            userinfo.shell = "/bin/NovaShell".to_string();
        }

        // 判断终端程序是否有效
        Self::check_shell(&userinfo);

        // 判断是否有重复的用户名和uid
        Self::check_dup_name_uid(&mut userinfo);

        // 判断group和gid
        Self::check_group_gid(&mut userinfo);

        userinfo
    }

    /// 检验终端程序是否有效
    fn check_shell(userinfo: &UserInfo) {
        if let Ok(file) = fs::File::open(userinfo.shell.clone()) {
            if !file.metadata().unwrap().is_file() {
                ErrorHandler::error_handle(
                    format!("{} is not a file", userinfo.shell),
                    crate::error::ExitStatus::InvalidArg,
                );
            }
        } else {
            ErrorHandler::error_handle(
                format!("{} doesn't exist", userinfo.shell),
                crate::error::ExitStatus::InvalidArg,
            );
        }
    }

    /// 检查是否有重复的用户名和uid，如果uid为空，则自动分配一个uid
    fn check_dup_name_uid(userinfo: &mut UserInfo) {
        let r = fs::read_to_string("/etc/passwd");
        match r {
            Ok(content) => {
                let mut max_uid: u32 = 0;
                for line in content.lines() {
                    let data: Vec<&str> = line.split(":").collect();
                    let (username, uid) = (data[0], data[2]);

                    max_uid = max_uid.max(u32::from_str_radix(uid, 10).unwrap());
                    if userinfo.username == username {
                        ErrorHandler::error_handle(
                            format!("username: {} had been used.", username),
                            crate::error::ExitStatus::UsernameInUse,
                        );
                    }
                    if userinfo.uid == uid {
                        ErrorHandler::error_handle(
                            format!("uid: {} already exists", uid),
                            crate::error::ExitStatus::UidInUse,
                        );
                    }
                }

                if userinfo.uid.is_empty() {
                    userinfo.uid = (max_uid + 1).to_string();
                }

                // 校验uid是否有效
                let uid = userinfo.uid.parse::<u32>();
                if uid.is_err() {
                    ErrorHandler::error_handle(
                        format!("uid: {} is invalid", userinfo.uid),
                        crate::error::ExitStatus::InvalidArg,
                    );
                }
            }
            Err(_) => {
                ErrorHandler::error_handle(
                    "Can't read file: /etc/passwd".to_string(),
                    crate::error::ExitStatus::PasswdFile,
                );
            }
        }
    }

    /// 检查组名、组id是否有效
    fn check_group_gid(userinfo: &mut UserInfo) {
        if userinfo.group.is_empty() {
            ErrorHandler::error_handle(
                "user must belong to a group".to_string(),
                crate::error::ExitStatus::InvalidCmdSyntax,
            );
        }

        let r = fs::read_to_string("/etc/group");
        let mut max_gid: u32 = 0;
        match r {
            Ok(content) => {
                for line in content.lines() {
                    let data: Vec<&str> = line.split(":").collect();
                    if data[0].eq(userinfo.group.as_str()) {
                        // 填充gid
                        userinfo.gid = data[2].to_string();
                        return;
                    }
                    max_gid = max_gid.max(u32::from_str_radix(data[2], 10).unwrap());
                }
            }
            Err(_) => {
                ErrorHandler::error_handle(
                    "Can't read file: /etc/group".to_string(),
                    crate::error::ExitStatus::GroupFile,
                );
            }
        }

        // 没有对应的用户组，默认创建新的用户组
        let groupname = userinfo.group.clone();
        let gid = max_gid + 1;
        let mut success = true;
        let r = std::process::Command::new("/bin/groupadd")
            .arg("-g")
            .arg(gid.to_string())
            .arg(groupname)
            .status();
        if let Ok(exit_status) = r {
            if exit_status.code() != Some(0) {
                success = false;
            }
        } else {
            success = false;
        }
        if !success {
            ErrorHandler::error_handle(
                "groupadd failed".to_string(),
                crate::error::ExitStatus::GroupaddFail,
            );
        }

        userinfo.gid = gid.to_string();

        // 校验gid是否有效
        let gid = userinfo.gid.parse::<u32>();
        if gid.is_err() {
            ErrorHandler::error_handle(
                format!("gid: {} is invalid", userinfo.gid),
                crate::error::ExitStatus::InvalidArg,
            );
        }
    }
}
