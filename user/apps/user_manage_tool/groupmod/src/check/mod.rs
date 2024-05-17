use crate::{
    error::ErrorHandler,
    parser::{CmdOption, GModCommand},
};
use std::fs;

#[derive(Debug, Default, Clone)]
pub struct Info {
    pub groupname: String,
    pub gid: String,
    pub new_groupname: Option<String>,
    pub new_gid: Option<String>,
}

pub struct Check;

impl Check {
    /// **校验解析后的groupadd命令**
    ///
    /// ## 参数
    /// - `cmd`: 解析后的groupadd命令
    ///
    /// ## 返回
    /// - `Info`: 校验后的信息
    pub fn check(cmd: GModCommand) -> Info {
        let mut info = Info::default();
        info.groupname = cmd.groupname;

        // 检查new_groupname是否有效
        if let Some(new_groupname) = cmd.options.get(&CmdOption::Group) {
            for c in new_groupname.chars() {
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
            info.new_groupname = Some(new_groupname.clone());
        }

        // 检查new_gid是否有效
        if let Some(new_gid) = cmd.options.get(&CmdOption::Gid) {
            if new_gid.parse::<u32>().is_err() {
                ErrorHandler::error_handle(
                    format!("'{}' is invalid, gid should be a number", new_gid),
                    crate::error::ExitStatus::InvalidArg,
                )
            }
            info.new_gid = Some(new_gid.clone());
        }

        Self::check_group_file(&mut info);

        info
    }

    /// 扫描/etc/group，查看groupname是否存在，同时检测new_gid、new_groupname是否重复
    fn check_group_file(info: &mut Info) {
        let mut is_group_exist = false;
        let r = fs::read_to_string("/etc/group");
        match r {
            Ok(content) => {
                for line in content.lines() {
                    let field = line.split(':').collect::<Vec<&str>>();
                    if field[0] == info.groupname {
                        is_group_exist = true;
                        info.gid = field[2].to_string();
                    }

                    if let Some(new_gid) = &info.new_gid {
                        if new_gid == field[2] {
                            ErrorHandler::error_handle(
                                format!("gid:[{}] is already used", new_gid),
                                crate::error::ExitStatus::InvalidArg,
                            );
                        }
                    }

                    if let Some(new_groupname) = &info.new_groupname {
                        if new_groupname == field[0] {
                            ErrorHandler::error_handle(
                                format!("groupname:[{}] is already used", new_groupname),
                                crate::error::ExitStatus::InvalidArg,
                            );
                        }
                    }
                }
            }
            Err(_) => ErrorHandler::error_handle(
                "Can't read file: /etc/group".to_string(),
                crate::error::ExitStatus::GroupFile,
            ),
        }

        if !is_group_exist {
            ErrorHandler::error_handle(
                format!("groupname:[{}] doesn't exist", info.groupname),
                crate::error::ExitStatus::GroupNotExist,
            );
        }
    }
}
