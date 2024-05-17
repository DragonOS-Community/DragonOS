use std::fs;

use crate::{error::ErrorHandler, parser::GDelCommand};

#[derive(Debug, Clone)]
pub struct Info {
    pub groupname: String,
}

pub struct Check;

impl Check {
    /// **校验解析后的groupdel命令**
    ///
    /// ## 参数
    /// - `cmd`: 解析后的groupdel命令
    ///
    /// ## 返回
    /// - `Info`: 校验后的信息
    pub fn check(cmd: GDelCommand) -> Info {
        if let Some(gid) = Self::check_groupname(cmd.groupname.clone()) {
            // 检查group是不是某个用户的主组，如果是的话则不能删除
            Self::check_gid(gid);
        } else {
            // 用户组不存在
            ErrorHandler::error_handle(
                format!("group:[{}] doesn't exist", cmd.groupname),
                crate::error::ExitStatus::GroupNotExist,
            );
        }
        Info {
            groupname: cmd.groupname,
        }
    }

    /// 校验组名，判断该用户组是否存在，以及成员是否为空
    fn check_groupname(groupname: String) -> Option<String> {
        let r = fs::read_to_string("/etc/group");
        match r {
            Ok(content) => {
                for line in content.lines() {
                    let field = line.split(":").collect::<Vec<&str>>();
                    let users = field[3].split(",").collect::<Vec<&str>>();
                    let filter_users = users
                        .iter()
                        .filter(|&x| !x.is_empty())
                        .collect::<Vec<&&str>>();
                    if field[0] == groupname {
                        if filter_users.is_empty() {
                            return Some(field[2].to_string());
                        } else {
                            ErrorHandler::error_handle(
                                format!("group:[{}] is not empty, unable to delete", groupname),
                                crate::error::ExitStatus::InvalidArg,
                            )
                        }
                    }
                }
            }
            Err(_) => {
                ErrorHandler::error_handle(
                    "Can't read file: /etc/group".to_string(),
                    crate::error::ExitStatus::GroupFile,
                );
            }
        }

        None
    }

    /// 检查/etc/passwd文件：判断gid是不是某个用户的主组
    fn check_gid(gid: String) {
        // 读取/etc/passwd文件
        let r = fs::read_to_string("/etc/passwd");
        match r {
            Ok(content) => {
                for line in content.lines() {
                    let field = line.split(":").collect::<Vec<&str>>();
                    if field[3] == gid {
                        ErrorHandler::error_handle(
                            format!(
                                "groupdel failed: group is main group of user:[{}]",
                                field[0]
                            ),
                            crate::error::ExitStatus::InvalidArg,
                        )
                    }
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
}
