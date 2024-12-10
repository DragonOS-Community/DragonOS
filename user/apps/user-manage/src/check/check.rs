use super::info::{GAddInfo, GDelInfo, GModInfo, PasswdInfo, UAddInfo, UDelInfo, UModInfo};
use crate::{
    error::error::{ErrorHandler, ExitStatus},
    parser::cmd::{CmdOption, GroupCommand, PasswdCommand, UserCommand},
};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Write,
};

/// useradd命令检查器
#[derive(Debug)]
pub struct UAddCheck;

impl UAddCheck {
    /// **校验解析后的useradd命令**
    ///
    /// ## 参数
    /// - `cmd`: 解析后的useradd命令
    ///
    /// ## 返回
    /// - `UAddInfo`: 校验后的信息
    pub fn check(cmd: UserCommand) -> UAddInfo {
        let mut info = UAddInfo::default();
        info.username = cmd.username;

        // 填充信息
        for (option, arg) in cmd.options.iter() {
            match option {
                CmdOption::Shell => {
                    info.shell = arg.clone();
                }
                CmdOption::Comment => {
                    info.comment = arg.clone();
                }
                CmdOption::Uid => {
                    info.uid = arg.clone();
                }
                CmdOption::Group => {
                    info.group = arg.clone();
                }
                CmdOption::Gid => {
                    info.gid = arg.clone();
                }
                CmdOption::Dir => {
                    info.home_dir = arg.clone();
                }
                _ => {
                    let op: &str = option.clone().into();
                    ErrorHandler::error_handle(
                        format!("Unimplemented option: {}", op),
                        ExitStatus::InvalidCmdSyntax,
                    );
                }
            }
        }

        // 完善用户信息
        if info.username.is_empty() {
            ErrorHandler::error_handle("Invalid username".to_string(), ExitStatus::InvalidArg);
        }

        if info.uid.is_empty() {
            ErrorHandler::error_handle("Uid is required".to_string(), ExitStatus::InvalidCmdSyntax);
        }

        if info.comment.is_empty() {
            info.comment = info.username.clone() + ",,,";
        }
        if info.home_dir.is_empty() {
            let home_dir = format!("/home/{}", info.username.clone());
            info.home_dir = home_dir;
        }
        if info.shell.is_empty() {
            info.shell = "/bin/NovaShell".to_string();
        }

        // 校验终端是否有效
        check_shell(&info.shell);

        // 校验是否有重复用户名和用户id
        scan_passwd(
            PasswdField {
                username: Some(info.username.clone()),
                uid: Some(info.uid.clone()),
            },
            false,
        );

        // 判断group和gid是否有效
        Self::check_group_gid(&mut info);

        info
    }

    /// 检查组名、组id是否有效，如果组名不存在，则创建新的用户组
    fn check_group_gid(info: &mut UAddInfo) {
        if info.group.is_empty() && info.gid.is_empty() {
            ErrorHandler::error_handle(
                "user must belong to a group".to_string(),
                ExitStatus::InvalidCmdSyntax,
            );
        }

        let r = fs::read_to_string("/etc/group");
        let mut max_gid: u32 = 0;
        match r {
            Ok(content) => {
                for line in content.lines() {
                    let data: Vec<&str> = line.split(":").collect();
                    let (groupname, gid) = (data[0].to_string(), data[2].to_string());
                    if !info.group.is_empty() && info.group == groupname {
                        if !info.gid.is_empty() && info.gid != gid {
                            ErrorHandler::error_handle(
                                format!("The gid of the group [{}] isn't {}", info.group, info.gid),
                                ExitStatus::InvalidArg,
                            )
                        } else if info.gid.is_empty() || info.gid == gid {
                            info.gid = gid;
                            return;
                        }
                    }

                    if !info.gid.is_empty() && info.gid == gid {
                        if !info.group.is_empty() && info.group != groupname {
                            ErrorHandler::error_handle(
                                format!("The gid of the group [{}] isn't {}", info.group, info.gid),
                                ExitStatus::InvalidArg,
                            )
                        } else if info.group.is_empty() || info.group == groupname {
                            info.group = groupname;
                            return;
                        }
                    }

                    max_gid = max_gid.max(u32::from_str_radix(data[2], 10).unwrap());
                }
            }
            Err(_) => {
                ErrorHandler::error_handle(
                    "Can't read file: /etc/group".to_string(),
                    ExitStatus::GroupFile,
                );
            }
        }

        // 没有对应的用户组，默认创建新的用户组
        let mut groupname = info.username.clone();
        let mut gid = (max_gid + 1).to_string();
        if !info.group.is_empty() {
            groupname = info.group.clone();
        } else {
            info.group = groupname.clone();
        }

        if !info.gid.is_empty() {
            gid = info.gid.clone();
        } else {
            info.gid = gid.clone();
        }
        let mut success = true;
        let r = std::process::Command::new("/bin/groupadd")
            .arg("-g")
            .arg(gid.clone())
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
            ErrorHandler::error_handle("groupadd failed".to_string(), ExitStatus::GroupaddFail);
        }
    }
}

/// userdel命令检查器
#[derive(Debug)]
pub struct UDelCheck;

impl UDelCheck {
    /// **校验userdel命令**
    ///
    /// ## 参数
    /// - `cmd`: userdel命令
    ///
    /// ## 返回
    /// - `UDelInfo`: 校验后的用户信息
    pub fn check(cmd: UserCommand) -> UDelInfo {
        let mut info = UDelInfo::default();
        info.username = cmd.username;

        // 检查用户是否存在
        scan_passwd(
            PasswdField {
                username: Some(info.username.clone()),
                uid: None,
            },
            true,
        );

        if let Some(_) = cmd.options.get(&CmdOption::Remove) {
            info.home = Some(Self::home(&info.username));
        }

        info
    }

    /// 获取用户家目录
    fn home(username: &String) -> String {
        let mut home = String::new();
        match std::fs::read_to_string("/etc/passwd") {
            Ok(data) => {
                for line in data.lines() {
                    let data = line.split(':').collect::<Vec<&str>>();
                    if data[0] == username {
                        home = data[5].to_string();
                        break;
                    }
                }
            }
            Err(_) => {
                ErrorHandler::error_handle(
                    "Can't read file: /etc/passwd".to_string(),
                    ExitStatus::PasswdFile,
                );
            }
        }
        home
    }
}

/// usermod命令检查器
#[derive(Debug)]
pub struct UModCheck;

impl UModCheck {
    /// **校验usermod命令**
    ///
    /// ## 参数
    /// - `cmd`: usermod命令
    ///
    /// ## 返回
    /// - `UModInfo`: 校验后的用户信息
    pub fn check(cmd: UserCommand) -> UModInfo {
        let mut info = Self::parse_options(&cmd.options);
        info.username = cmd.username;

        // 校验shell是否有效
        if let Some(shell) = &info.new_shell {
            check_shell(shell);
        }

        // 校验new_home是否有效
        if let Some(new_home) = &info.new_home {
            Self::check_home(new_home);
        }

        // 校验用户是否存在
        scan_passwd(
            PasswdField {
                username: Some(info.username.clone()),
                uid: None,
            },
            true,
        );

        // 校验new_name、new_uid是否有效
        scan_passwd(
            PasswdField {
                username: info.new_name.clone(),
                uid: info.new_uid.clone(),
            },
            false,
        );

        // 校验groups、new_gid是否有效
        scan_group(
            GroupField {
                groups: info.groups.clone(),
                gid: info.new_gid.clone(),
            },
            true,
        );

        info
    }

    /// **校验home目录是否有效**
    ///
    /// ## 参数
    /// - `home`: home目录路径
    fn check_home(home: &String) {
        if fs::File::open(home).is_ok() {
            ErrorHandler::error_handle(format!("{} already exists", home), ExitStatus::InvalidArg);
        }
    }

    /// **解析options**
    ///
    /// ## 参数
    /// - `options`: 命令选项
    ///
    /// ## 返回
    /// - `UModInfo`: 用户信息
    fn parse_options(options: &HashMap<CmdOption, String>) -> UModInfo {
        let mut info = UModInfo::default();
        for (option, arg) in options {
            match option {
                CmdOption::Append => {
                    info.groups = Some(arg.split(",").map(|s| s.to_string()).collect());
                }
                CmdOption::Comment => {
                    info.new_comment = Some(arg.clone());
                }
                CmdOption::Dir => {
                    info.new_home = Some(arg.clone());
                }
                CmdOption::Gid => {
                    info.new_gid = Some(arg.clone());
                }
                CmdOption::Login => {
                    info.new_name = Some(arg.clone());
                }
                CmdOption::Shell => {
                    info.new_shell = Some(arg.clone());
                }
                CmdOption::Uid => {
                    info.new_uid = Some(arg.clone());
                }
                _ => ErrorHandler::error_handle(
                    "Invalid option".to_string(),
                    ExitStatus::InvalidCmdSyntax,
                ),
            }
        }
        info
    }
}

/// passwd命令检查器
#[derive(Debug)]
pub struct PasswdCheck;

impl PasswdCheck {
    /// **校验passwd命令**
    ///
    /// ## 参数
    /// - `cmd`: passwd命令
    ///
    /// ## 返回
    /// - `PasswdInfo`: 校验后的信息
    pub fn check(cmd: PasswdCommand) -> PasswdInfo {
        let uid = unsafe { libc::geteuid().to_string() };
        let cur_username = Self::cur_username(uid.clone());
        let mut to_change_username = String::new();

        if let Some(username) = cmd.username {
            to_change_username = username.clone();

            // 不是root用户不能修改别人的密码
            if uid != "0" && cur_username != username {
                ErrorHandler::error_handle(
                    "You can't change password for other users".to_string(),
                    ExitStatus::PermissionDenied,
                );
            }

            // 检验待修改用户是否存在
            scan_passwd(
                PasswdField {
                    username: Some(username.clone()),
                    uid: None,
                },
                true,
            );
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
                        ExitStatus::InvalidArg,
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
                        ExitStatus::InvalidArg,
                    )
                }
            }
        };

        PasswdInfo {
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
                    ExitStatus::PasswdFile,
                );
            }
        }

        cur_username
    }

    /// **校验密码**
    ///
    /// ## 参数
    /// - `username`: 用户名
    /// - `password`: 密码
    fn check_password(username: String, password: String) {
        let r = fs::read_to_string("/etc/shadow");
        match r {
            Ok(content) => {
                for line in content.lines() {
                    let field = line.split(":").collect::<Vec<&str>>();
                    if username == field[0] {
                        if password != field[1] {
                            ErrorHandler::error_handle(
                                "Password error".to_string(),
                                ExitStatus::InvalidArg,
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
                    ExitStatus::ShadowFile,
                );
            }
        }
    }
}

/// groupadd命令检查器
#[derive(Debug)]
pub struct GAddCheck;

impl GAddCheck {
    /// **校验groupadd命令**
    ///
    /// ## 参数
    /// - `cmd`: groupadd命令
    ///
    /// ## 返回
    /// - `GAddInfo`: 校验后的组信息
    pub fn check(cmd: GroupCommand) -> GAddInfo {
        let mut info = GAddInfo {
            groupname: cmd.groupname.clone(),
            gid: String::new(),
            passwd: None,
        };

        if info.groupname.is_empty() {
            ErrorHandler::error_handle("groupname is required".to_string(), ExitStatus::InvalidArg);
        }

        if let Some(gid) = cmd.options.get(&CmdOption::Gid) {
            info.gid = gid.clone();
        } else {
            ErrorHandler::error_handle("gid is required".to_string(), ExitStatus::InvalidArg);
        }

        if let Some(passwd) = cmd.options.get(&CmdOption::Passwd) {
            info.passwd = Some(passwd.clone());
        }

        // 检查组名或组id是否已存在
        scan_group(
            GroupField {
                groups: Some(vec![info.groupname.clone()]),
                gid: Some(info.gid.clone()),
            },
            false,
        );

        info
    }
}

/// groupdel命令检查器
#[derive(Debug)]
pub struct GDelCheck;

impl GDelCheck {
    /// **校验groupdel命令**
    ///
    /// ## 参数
    /// - `cmd`: groupdel命令
    ///
    /// ## 返回
    /// - `GDelInfo`: 校验后的组信息
    pub fn check(cmd: GroupCommand) -> GDelInfo {
        if let Some(gid) = check_groupname(cmd.groupname.clone()) {
            // 检查group是不是某个用户的主组，如果是的话则不能删除
            Self::is_main_group(gid);
        } else {
            // 用户组不存在
            ErrorHandler::error_handle(
                format!("group:[{}] doesn't exist", cmd.groupname),
                ExitStatus::GroupNotExist,
            );
        }
        GDelInfo {
            groupname: cmd.groupname,
        }
    }

    /// **检查该组是否为某个用户的主用户组**
    ///
    /// ## 参数
    /// - `gid`: 组id
    ///
    /// ## 返回
    /// Some(gid): 组id
    /// None
    fn is_main_group(gid: String) {
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
                            ExitStatus::InvalidArg,
                        )
                    }
                }
            }
            Err(_) => {
                ErrorHandler::error_handle(
                    "Can't read file: /etc/passwd".to_string(),
                    ExitStatus::PasswdFile,
                );
            }
        }
    }
}

/// groupmod命令检查器
#[derive(Debug)]
pub struct GModCheck;

impl GModCheck {
    /// **校验groupmod命令**
    ///
    /// ## 参数
    /// - `cmd`: groupmod命令
    ///
    /// ## 返回
    /// - `GModInfo`: 校验后的组信息
    pub fn check(cmd: GroupCommand) -> GModInfo {
        let mut info = GModInfo::default();
        info.groupname = cmd.groupname;

        if let Some(new_groupname) = cmd.options.get(&CmdOption::NewGroupName) {
            info.new_groupname = Some(new_groupname.clone());
        }

        if let Some(new_gid) = cmd.options.get(&CmdOption::Gid) {
            info.new_gid = Some(new_gid.clone());
        }

        Self::check_group_file(&mut info);

        info
    }

    /// 查看groupname是否存在，同时检测new_gid、new_groupname是否重复
    fn check_group_file(info: &mut GModInfo) {
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
                                ExitStatus::InvalidArg,
                            );
                        }
                    }

                    if let Some(new_groupname) = &info.new_groupname {
                        if new_groupname == field[0] {
                            ErrorHandler::error_handle(
                                format!("groupname:[{}] is already used", new_groupname),
                                ExitStatus::InvalidArg,
                            );
                        }
                    }
                }
            }
            Err(_) => ErrorHandler::error_handle(
                "Can't read file: /etc/group".to_string(),
                ExitStatus::GroupFile,
            ),
        }

        if !is_group_exist {
            ErrorHandler::error_handle(
                format!("groupname:[{}] doesn't exist", info.groupname),
                ExitStatus::GroupNotExist,
            );
        }
    }
}

/// passwd文件待校验字段
pub struct PasswdField {
    username: Option<String>,
    uid: Option<String>,
}

/// group文件待校验字段
pub struct GroupField {
    groups: Option<Vec<String>>,
    gid: Option<String>,
}

/// **校验uid**
///
/// ## 参数
/// - `passwd_field`: passwd文件字段
/// - `should_exist`: 是否应该存在
fn scan_passwd(passwd_field: PasswdField, should_exist: bool) {
    let mut username_check = false;
    let mut uid_check = false;
    match fs::read_to_string("/etc/passwd") {
        Ok(content) => {
            for line in content.lines() {
                let field = line.split(':').collect::<Vec<&str>>();
                if let Some(uid) = &passwd_field.uid {
                    // uid必须是有效的数字
                    let r = uid.parse::<u32>();
                    if r.is_err() {
                        ErrorHandler::error_handle(
                            format!("Uid {} is invalid", uid),
                            ExitStatus::InvalidArg,
                        );
                    }
                    if field[2] == uid {
                        uid_check = true;
                        // username如果不用校验或者被校验过了，才可以return
                        if should_exist && (passwd_field.username.is_none() || username_check) {
                            return;
                        } else {
                            ErrorHandler::error_handle(
                                format!("UID {} already exists", uid),
                                ExitStatus::UidInUse,
                            );
                        }
                    }
                }

                if let Some(username) = &passwd_field.username {
                    if field[0] == username {
                        username_check = true;
                        // uid如果不用校验或者被校验过了，才可以return
                        if should_exist && (passwd_field.uid.is_none() || uid_check) {
                            return;
                        } else {
                            ErrorHandler::error_handle(
                                format!("Username {} already exists", username),
                                ExitStatus::UsernameInUse,
                            );
                        }
                    }
                }
            }

            if should_exist {
                if let Some(uid) = &passwd_field.uid {
                    if !uid_check {
                        ErrorHandler::error_handle(
                            format!("UID {} doesn't exist", uid),
                            ExitStatus::InvalidArg,
                        );
                    }
                }
                if let Some(username) = &passwd_field.username {
                    if !username_check {
                        ErrorHandler::error_handle(
                            format!("User {} doesn't exist", username),
                            ExitStatus::InvalidArg,
                        );
                    }
                }
            }
        }
        Err(_) => ErrorHandler::error_handle(
            "Can't read file: /etc/passwd".to_string(),
            ExitStatus::PasswdFile,
        ),
    }
}

/// **校验gid**
///
/// ## 参数
/// - `group_field`: group文件字段
/// - `should_exist`: 是否应该存在
fn scan_group(group_field: GroupField, should_exist: bool) {
    let mut gid_check = false;
    let mut set1 = HashSet::new();
    let mut set2 = HashSet::new();
    if let Some(groups) = group_field.groups.clone() {
        set2.extend(groups.into_iter());
    }
    match fs::read_to_string("/etc/group") {
        Ok(content) => {
            for line in content.lines() {
                let field = line.split(':').collect::<Vec<&str>>();
                if let Some(gid) = &group_field.gid {
                    // gid必须是有效的数字
                    let r = gid.parse::<u32>();
                    if r.is_err() {
                        ErrorHandler::error_handle(
                            format!("Gid {} is invalid", gid),
                            ExitStatus::InvalidArg,
                        );
                    }
                    if field[2] == gid {
                        gid_check = true;
                        if should_exist && group_field.groups.is_none() {
                            return;
                        } else {
                            ErrorHandler::error_handle(
                                format!("GID {} already exists", gid),
                                ExitStatus::InvalidArg,
                            );
                        }
                    }
                }

                // 统计所有组
                set1.insert(field[0].to_string());
            }

            if should_exist {
                if let Some(gid) = &group_field.gid {
                    if !gid_check {
                        ErrorHandler::error_handle(
                            format!("GID {} doesn't exist", gid),
                            ExitStatus::InvalidArg,
                        );
                    }
                }
                if group_field.groups.is_some() {
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

        Err(_) => ErrorHandler::error_handle(
            "Can't read file: /etc/group".to_string(),
            ExitStatus::GroupFile,
        ),
    }
}

/// **校验shell是否有效**
///
/// ## 参数
/// - `shell`: shell路径
fn check_shell(shell: &String) {
    if let Ok(file) = fs::File::open(shell.clone()) {
        if !file.metadata().unwrap().is_file() {
            ErrorHandler::error_handle(format!("{} is not a file", shell), ExitStatus::InvalidArg);
        }
    } else {
        ErrorHandler::error_handle(format!("{} doesn't exist", shell), ExitStatus::InvalidArg);
    }
}

/// **校验组名，判断该用户组是否存在，以及成员是否为空**
///
/// ## 参数
/// - `groupname`: 组名
///
/// ## 返回
/// Some(gid): 组id
/// None
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
                            ExitStatus::InvalidArg,
                        )
                    }
                }
            }
        }
        Err(_) => {
            ErrorHandler::error_handle(
                "Can't read file: /etc/group".to_string(),
                ExitStatus::GroupFile,
            );
        }
    }

    None
}
