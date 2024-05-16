use crate::{
    error::ErrorHandler,
    parser::{CmdOption, GAddCommand},
};
use std::fs;

#[derive(Debug, Clone)]
pub struct GroupInfo {
    pub groupname: String,
    pub gid: String,
    pub passwd: String,
}

impl GroupInfo {
    pub fn to_string_group(&self) -> String {
        let mut passwd = String::from("");
        if !self.passwd.is_empty() {
            passwd = "x".to_string();
        }
        format!("{}:{}:{}:\n", self.groupname, passwd, self.gid)
    }

    pub fn to_string_gshadow(&self) -> String {
        let mut passwd = String::from("!");
        if !self.passwd.is_empty() {
            passwd = self.passwd.clone();
        }

        format!("{}:{}::\n", self.groupname, passwd)
    }
}

pub struct Check;

impl Check {
    /// **校验解析后的groupadd命令**
    ///
    /// ## 参数
    /// - `cmd`: 解析后的groupadd命令
    ///
    /// ## 返回
    /// - `GroupInfo`: 校验后的组信息
    pub fn check(cmd: GAddCommand) -> GroupInfo {
        let mut group_info = GroupInfo {
            groupname: cmd.groupname.clone(),
            gid: String::new(),
            passwd: String::new(),
        };

        if let Some(gid) = cmd.options.get(&CmdOption::Gid) {
            group_info.gid = gid.clone();
        }

        if let Some(passwd) = cmd.options.get(&CmdOption::Passwd) {
            group_info.passwd = passwd.clone();
        }

        Self::check_name_gid(&mut group_info);

        group_info
    }

    /// 校验组名和gid
    fn check_name_gid(group_info: &mut GroupInfo) {
        // 组名只能由字母组成
        for c in group_info.groupname.chars() {
            if !c.is_ascii_alphabetic() && !c.is_ascii_digit() {
                ErrorHandler::error_handle(
                    format!(
                        "'{}' is invalid, groupname should be composed of letters and numbers",
                        c
                    ),
                    crate::error::ExitStatus::InvalidArg,
                );
            }
        }

        // 不能有重复的groupname和gid
        let r = fs::read_to_string("/etc/group");
        match r {
            Ok(content) => {
                let mut max_gid = 0;
                for line in content.lines() {
                    let field = line.split(':').collect::<Vec<&str>>();
                    if field[0] == group_info.groupname {
                        ErrorHandler::error_handle(
                            format!("Group {} already exists", group_info.groupname),
                            crate::error::ExitStatus::InvalidArg,
                        );
                    }

                    if field[2] == group_info.gid.as_str() {
                        ErrorHandler::error_handle(
                            format!("Gid {} already exists", group_info.gid),
                            crate::error::ExitStatus::InvalidArg,
                        );
                    }

                    max_gid = max_gid.max(field[2].parse::<u32>().unwrap());
                }

                if group_info.gid.is_empty() {
                    group_info.gid = format!("{}", max_gid + 1);
                }

                // gid必须是有效的数字
                let gid = group_info.gid.parse::<u32>();
                if gid.is_err() {
                    ErrorHandler::error_handle(
                        format!("Gid {} is invalid", group_info.gid),
                        crate::error::ExitStatus::InvalidArg,
                    );
                }
            }
            Err(_) => {
                ErrorHandler::error_handle(
                    "Can't read file: /etc/group".to_string(),
                    crate::error::ExitStatus::GroupFile,
                );
            }
        }
    }
}
