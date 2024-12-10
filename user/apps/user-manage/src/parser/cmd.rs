use std::collections::HashMap;

/// 命令类型
pub enum CmdType {
    User,
    Passwd,
    Group,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub enum CmdOption {
    /// 用户描述
    Comment,
    /// 用户主目录
    Dir,
    /// 组名
    Group,
    /// 组id
    Gid,
    /// 终端程序
    Shell,
    /// 用户id
    Uid,
    /// 删除用户的home目录
    Remove,
    /// 添加到其它用户组中
    Append,
    /// 修改用户名
    Login,
    /// 设置组密码
    Passwd,
    /// 修改组名
    NewGroupName,
    /// 无效选项
    Invalid,
}

impl From<String> for CmdOption {
    fn from(s: String) -> Self {
        match s.as_str() {
            "-c" => CmdOption::Comment,
            "-d" => CmdOption::Dir,
            "-G" => CmdOption::Group,
            "-g" => CmdOption::Gid,
            "-s" => CmdOption::Shell,
            "-u" => CmdOption::Uid,
            "-r" => CmdOption::Remove,
            "-a" => CmdOption::Append,
            "-l" => CmdOption::Login,
            "-p" => CmdOption::Passwd,
            "-n" => CmdOption::NewGroupName,
            _ => CmdOption::Invalid,
        }
    }
}

impl From<CmdOption> for &str {
    fn from(option: CmdOption) -> Self {
        match option {
            CmdOption::Comment => "-c",
            CmdOption::Dir => "-d",
            CmdOption::Group => "-G",
            CmdOption::Shell => "-s",
            CmdOption::Uid => "-u",
            CmdOption::Login => "-l",
            CmdOption::Append => "-a",
            CmdOption::Gid => "-g",
            CmdOption::NewGroupName => "-n",
            CmdOption::Passwd => "-p",
            CmdOption::Remove => "-r",
            CmdOption::Invalid => "Invalid option",
        }
    }
}

/// useradd/userdel/usermod命令
#[derive(Debug)]
pub struct UserCommand {
    /// 用户名
    pub username: String,
    /// 选项
    pub options: HashMap<CmdOption, String>,
}

/// passwd命令
#[derive(Debug)]
pub struct PasswdCommand {
    pub username: Option<String>,
}

/// groupadd/groupdel/groupmod命令
#[derive(Debug)]
pub struct GroupCommand {
    pub groupname: String,
    pub options: HashMap<CmdOption, String>,
}
