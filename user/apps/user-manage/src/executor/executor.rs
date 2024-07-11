use crate::{
    check::info::{GAddInfo, GDelInfo, GModInfo, PasswdInfo, UAddInfo, UDelInfo, UModInfo},
    error::error::{ErrorHandler, ExitStatus},
};
use lazy_static::lazy_static;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, Write},
    sync::Mutex,
};

lazy_static! {
    static ref GLOBAL_FILE: Mutex<GlobalFile> = Mutex::new(GlobalFile::new());
}

#[derive(Debug)]
pub struct GlobalFile {
    passwd_file: File,
    shadow_file: File,
    group_file: File,
    gshadow_file: File,
}

impl GlobalFile {
    pub fn new() -> Self {
        let passwd = open_file("/etc/passwd");
        let shadow = open_file("/etc/shadow");
        let group = open_file("/etc/group");
        let gshadow = open_file("/etc/gshadow");
        Self {
            passwd_file: passwd,
            shadow_file: shadow,
            group_file: group,
            gshadow_file: gshadow,
        }
    }
}

fn open_file(file_path: &str) -> File {
    let r = OpenOptions::new()
        .read(true)
        .write(true)
        .append(true)
        .open(file_path);

    let exit_status = match file_path {
        "/etc/group" => ExitStatus::GroupFile,
        "/etc/gshadow" => ExitStatus::GshadowFile,
        "/etc/passwd" => ExitStatus::PasswdFile,
        "/etc/shadow" => ExitStatus::ShadowFile,
        _ => ExitStatus::InvalidArg,
    };

    if r.is_err() {
        ErrorHandler::error_handle(format!("Can't open file: {}", file_path), exit_status);
    }

    r.unwrap()
}

/// useradd执行器
pub struct UAddExecutor;

impl UAddExecutor {
    /// **执行useradd**
    ///
    /// ## 参数
    /// - `info`: 用户信息
    pub fn execute(info: UAddInfo) {
        // 创建用户home目录
        let home = info.home_dir.clone();
        let dir_builder = fs::DirBuilder::new();
        if dir_builder.create(home.clone()).is_err() {
            ErrorHandler::error_handle(
                format!("unable to create {}", home),
                ExitStatus::CreateHomeFail,
            );
        }

        Self::write_passwd_file(&info);
        Self::write_shadow_file(&info);
        Self::write_group_file(&info);
        Self::write_gshadow_file(&info);
    }

    /// 写入/etc/passwd文件：添加用户信息
    fn write_passwd_file(info: &UAddInfo) {
        let userinfo: String = info.clone().into();
        GLOBAL_FILE
            .lock()
            .unwrap()
            .passwd_file
            .write_all(userinfo.as_bytes())
            .unwrap();
    }

    /// 写入/etc/group文件：将用户添加到对应用户组中
    fn write_group_file(info: &UAddInfo) {
        if info.group == info.username {
            return;
        }

        let mut guard = GLOBAL_FILE.lock().unwrap();
        let content = read_to_string(&guard.group_file);
        let mut new_content = String::new();
        for line in content.lines() {
            let mut field = line.split(":").collect::<Vec<&str>>();
            let mut users = field.last().unwrap().split(",").collect::<Vec<&str>>();
            users = users
                .into_iter()
                .filter(|username| !username.is_empty())
                .collect::<Vec<&str>>();
            if field[0].eq(info.group.as_str()) && !users.contains(&info.username.as_str()) {
                users.push(info.username.as_str());
            }

            let new_users = users.join(",");
            field[3] = new_users.as_str();
            new_content.push_str(format!("{}\n", field.join(":")).as_str());
        }

        guard.group_file.set_len(0).unwrap();
        guard.group_file.seek(std::io::SeekFrom::Start(0)).unwrap();
        guard.group_file.write_all(new_content.as_bytes()).unwrap();
        guard.group_file.flush().unwrap();
    }

    /// 写入/etc/shadow文件：添加用户口令相关信息
    fn write_shadow_file(info: &UAddInfo) {
        let data = format!("{}::::::::\n", info.username,);
        GLOBAL_FILE
            .lock()
            .unwrap()
            .shadow_file
            .write_all(data.as_bytes())
            .unwrap();
    }

    /// 写入/etc/gshadow文件：将用户添加到对应用户组中
    fn write_gshadow_file(info: &UAddInfo) {
        if info.group == info.username {
            return;
        }

        let mut guard = GLOBAL_FILE.lock().unwrap();
        let content = read_to_string(&guard.gshadow_file);
        let mut new_content = String::new();
        for line in content.lines() {
            let mut field = line.split(":").collect::<Vec<&str>>();
            let mut users = field.last().unwrap().split(",").collect::<Vec<&str>>();
            users = users
                .into_iter()
                .filter(|username| !username.is_empty())
                .collect::<Vec<&str>>();
            if field[0].eq(info.group.as_str()) && !users.contains(&info.username.as_str()) {
                users.push(info.username.as_str());
            }

            let new_users = users.join(",");
            field[3] = new_users.as_str();
            new_content.push_str(format!("{}\n", field.join(":")).as_str());
        }
        guard.gshadow_file.set_len(0).unwrap();
        guard
            .gshadow_file
            .seek(std::io::SeekFrom::Start(0))
            .unwrap();
        guard
            .gshadow_file
            .write_all(new_content.as_bytes())
            .unwrap();
        guard.gshadow_file.flush().unwrap();
    }
}

/// userdel执行器
pub struct UDelExecutor;

impl UDelExecutor {
    /// **执行userdel**
    ///
    /// ## 参数
    /// - `info`: 用户信息
    pub fn execute(info: UDelInfo) {
        // 移除home目录
        if let Some(home) = info.home.clone() {
            std::fs::remove_dir_all(home).unwrap();
        }

        Self::update_passwd_file(&info);
        Self::update_shadow_file(&info);
        Self::update_group_file(&info);
        Self::update_gshadow_file(&info);
    }

    /// 更新/etc/passwd文件: 删除用户信息
    fn update_passwd_file(info: &UDelInfo) {
        let mut guard = GLOBAL_FILE.lock().unwrap();
        let content = read_to_string(&guard.passwd_file);
        let lines: Vec<&str> = content.lines().collect();
        let new_content = lines
            .into_iter()
            .filter(|&line| {
                let field = line.split(':').collect::<Vec<&str>>();
                field[0] != info.username.as_str()
            })
            .collect::<Vec<&str>>()
            .join("\n");

        guard.passwd_file.set_len(0).unwrap();
        guard.passwd_file.seek(std::io::SeekFrom::Start(0)).unwrap();
        guard.passwd_file.write_all(new_content.as_bytes()).unwrap();
        guard.passwd_file.flush().unwrap();
    }

    /// 更新/etc/group文件: 将用户从组中移除
    fn update_group_file(info: &UDelInfo) {
        let mut guard = GLOBAL_FILE.lock().unwrap();
        let content = read_to_string(&guard.group_file);
        let mut new_content = String::new();
        for line in content.lines() {
            let mut field = line.split(':').collect::<Vec<&str>>();
            let mut users = field.last().unwrap().split(",").collect::<Vec<&str>>();
            if users.contains(&info.username.as_str()) {
                field.remove(field.len() - 1);
                users.remove(
                    users
                        .iter()
                        .position(|&x| x == info.username.as_str())
                        .unwrap(),
                );
                let users = users.join(",");
                field.push(&users.as_str());
                new_content.push_str(format!("{}\n", field.join(":").as_str()).as_str());
            } else {
                new_content.push_str(format!("{}\n", field.join(":").as_str()).as_str());
            }

            guard.group_file.set_len(0).unwrap();
            guard.group_file.seek(std::io::SeekFrom::Start(0)).unwrap();
            guard.group_file.write_all(new_content.as_bytes()).unwrap();
            guard.group_file.flush().unwrap();
        }
    }

    /// 更新/etc/shadow文件: 将用户信息删去
    fn update_shadow_file(info: &UDelInfo) {
        let mut guard = GLOBAL_FILE.lock().unwrap();
        let content = read_to_string(&guard.shadow_file);
        let lines: Vec<&str> = content.lines().collect();
        let new_content = lines
            .into_iter()
            .filter(|&line| !line.contains(&info.username))
            .collect::<Vec<&str>>()
            .join("\n");

        guard.shadow_file.set_len(0).unwrap();
        guard.shadow_file.seek(std::io::SeekFrom::Start(0)).unwrap();
        guard.shadow_file.write_all(new_content.as_bytes()).unwrap();
        guard.shadow_file.flush().unwrap();
    }

    /// 更新/etc/gshadow文件: 将用户从组中移除
    fn update_gshadow_file(info: &UDelInfo) {
        let mut guard = GLOBAL_FILE.lock().unwrap();
        let content = read_to_string(&guard.gshadow_file);
        let mut new_content = String::new();
        for line in content.lines() {
            let mut field = line.split(':').collect::<Vec<&str>>();
            let mut users = field.last().unwrap().split(",").collect::<Vec<&str>>();
            if users.contains(&info.username.as_str()) {
                field.remove(field.len() - 1);
                users.remove(
                    users
                        .iter()
                        .position(|&x| x == info.username.as_str())
                        .unwrap(),
                );
                let users = users.join(",");
                field.push(&users.as_str());
                new_content.push_str(format!("{}\n", field.join(":").as_str()).as_str());
            } else {
                new_content.push_str(format!("{}\n", field.join(":").as_str()).as_str());
            }

            guard.gshadow_file.set_len(0).unwrap();
            guard
                .gshadow_file
                .seek(std::io::SeekFrom::Start(0))
                .unwrap();
            guard
                .gshadow_file
                .write_all(new_content.as_bytes())
                .unwrap();
            guard.gshadow_file.flush().unwrap();
        }
    }
}

/// usermod执行器
pub struct UModExecutor;

impl UModExecutor {
    /// **执行usermod**
    ///
    /// ## 参数
    /// - `info`: 用户信息
    pub fn execute(mut info: UModInfo) {
        // 创建new_home
        if let Some(new_home) = &info.new_home {
            let dir_builder = fs::DirBuilder::new();
            if dir_builder.create(new_home.clone()).is_err() {
                ErrorHandler::error_handle(
                    format!("unable to create {}", new_home),
                    ExitStatus::CreateHomeFail,
                );
            }
        }

        Self::update_passwd_file(&info);
        Self::update_shadow_file(&info);
        Self::update_group_file(&mut info);
        Self::update_gshadow_file(&info);
    }

    /// 更新/etc/passwd文件的username、uid、comment、home、shell
    fn update_passwd_file(info: &UModInfo) {
        let mut new_content = String::new();
        let mut guard = GLOBAL_FILE.lock().unwrap();
        let content = read_to_string(&guard.passwd_file);
        for line in content.lines() {
            let mut fields = line.split(':').collect::<Vec<&str>>();
            if fields[0] == info.username {
                if let Some(new_username) = &info.new_name {
                    fields[0] = new_username;
                }
                if let Some(new_uid) = &info.new_uid {
                    fields[2] = new_uid;
                }
                if let Some(new_gid) = &info.new_gid {
                    fields[3] = new_gid;
                }
                if let Some(new_comment) = &info.new_comment {
                    fields[4] = new_comment;
                }
                if let Some(new_home) = &info.new_home {
                    fields[5] = new_home;
                }
                if let Some(new_shell) = &info.new_shell {
                    fields[6] = new_shell;
                }
                new_content.push_str(format!("{}\n", fields.join(":")).as_str());
            } else {
                new_content.push_str(format!("{}\n", line).as_str());
            }

            guard.passwd_file.set_len(0).unwrap();
            guard.passwd_file.seek(std::io::SeekFrom::Start(0)).unwrap();
            guard.passwd_file.write_all(new_content.as_bytes()).unwrap();
            guard.passwd_file.flush().unwrap();
        }
    }

    /// 更新/etc/group文件中各用户组中的用户
    fn update_group_file(info: &mut UModInfo) {
        let mut name = info.username.clone();
        if let Some(new_name) = &info.new_name {
            name = new_name.clone();
        }
        let mut new_content = String::new();
        let mut guard = GLOBAL_FILE.lock().unwrap();
        let content = read_to_string(&guard.group_file);
        for line in content.lines() {
            let mut fields = line.split(':').collect::<Vec<&str>>();
            let mut users = fields[3].split(",").collect::<Vec<&str>>();
            users = users
                .into_iter()
                .filter(|username| !username.is_empty())
                .collect::<Vec<&str>>();
            if let Some(idx) = users.iter().position(|&r| r == info.username) {
                if let Some(gid) = &info.new_gid {
                    // 换组，将用户从当前组删去
                    if gid != fields[2] {
                        users.remove(idx);
                    } else {
                        info.new_group = Some(fields[0].to_string())
                    }
                } else {
                    // 不换组但是要更新名字
                    users[idx] = &name;
                }
            }

            if let Some(groups) = &info.groups {
                if groups.contains(&fields[0].to_string()) && !users.contains(&name.as_str()) {
                    users.push(&name);
                }
            }

            let new_users = users.join(",");
            fields[3] = new_users.as_str();
            new_content.push_str(format!("{}\n", fields.join(":")).as_str());
        }

        guard.group_file.set_len(0).unwrap();
        guard.group_file.seek(std::io::SeekFrom::Start(0)).unwrap();
        guard.group_file.write_all(new_content.as_bytes()).unwrap();
        guard.group_file.flush().unwrap();
    }

    /// 更新/etc/shadow文件的username
    fn update_shadow_file(info: &UModInfo) {
        if let Some(new_name) = &info.new_name {
            let mut new_content = String::new();
            let mut guard = GLOBAL_FILE.lock().unwrap();
            let content = read_to_string(&guard.shadow_file);
            for line in content.lines() {
                let mut fields = line.split(':').collect::<Vec<&str>>();
                if fields[0] == info.username {
                    fields[0] = new_name;
                    new_content.push_str(format!("{}\n", fields.join(":")).as_str());
                } else {
                    new_content.push_str(format!("{}\n", line).as_str());
                }
            }

            guard.shadow_file.set_len(0).unwrap();
            guard.shadow_file.seek(std::io::SeekFrom::Start(0)).unwrap();
            guard.shadow_file.write_all(new_content.as_bytes()).unwrap();
            guard.shadow_file.flush().unwrap();
        }
    }

    /// 更新/etc/gshadow文件中各用户组中的用户
    fn update_gshadow_file(info: &UModInfo) {
        let mut name = info.username.clone();
        if let Some(new_name) = &info.new_name {
            name = new_name.clone();
        }
        let mut new_content = String::new();
        let mut guard = GLOBAL_FILE.lock().unwrap();
        let content = read_to_string(&guard.gshadow_file);
        for line in content.lines() {
            let mut fields = line.split(':').collect::<Vec<&str>>();
            let mut users = fields[3].split(",").collect::<Vec<&str>>();
            users = users
                .into_iter()
                .filter(|username| !username.is_empty())
                .collect::<Vec<&str>>();
            if let Some(idx) = users.iter().position(|&r| r == info.username) {
                if let Some(group) = &info.new_group {
                    // 换组，将用户从当前组删去
                    if group != fields[0] {
                        users.remove(idx);
                    }
                } else {
                    // 不换组但是要更新名字
                    users[idx] = &name;
                }
            }

            let tmp = format!(",{}", name);
            if let Some(groups) = &info.groups {
                if groups.contains(&fields[0].to_string()) && !users.contains(&name.as_str()) {
                    if users.is_empty() {
                        users.push(&name);
                    } else {
                        users.push(tmp.as_str());
                    }
                }
            }

            let new_users = users.join(",");
            fields[3] = new_users.as_str();
            new_content.push_str(format!("{}\n", fields.join(":")).as_str());
        }

        guard.gshadow_file.set_len(0).unwrap();
        guard
            .gshadow_file
            .seek(std::io::SeekFrom::Start(0))
            .unwrap();
        guard
            .gshadow_file
            .write_all(new_content.as_bytes())
            .unwrap();
        guard.gshadow_file.flush().unwrap();
    }
}

/// passwd执行器
pub struct PasswdExecutor;

impl PasswdExecutor {
    /// **执行passwd**
    ///
    /// ## 参数
    /// - `info`: 用户密码信息
    pub fn execute(info: PasswdInfo) {
        Self::update_passwd_file(&info);
        Self::update_shadow_file(&info);
    }

    /// 更新/etc/passwd文件: 修改用户密码
    fn update_passwd_file(info: &PasswdInfo) {
        let mut new_content = String::new();
        let mut guard = GLOBAL_FILE.lock().unwrap();
        let content = read_to_string(&guard.passwd_file);
        for line in content.lines() {
            let mut field = line.split(':').collect::<Vec<_>>();
            if field[0] == info.username {
                if info.new_password.is_empty() {
                    field[1] = "";
                } else {
                    field[1] = "x";
                }
            }
            new_content.push_str(format!("{}\n", field.join(":")).as_str());
        }

        guard.passwd_file.set_len(0).unwrap();
        guard.passwd_file.seek(std::io::SeekFrom::Start(0)).unwrap();
        guard.passwd_file.write_all(new_content.as_bytes()).unwrap();
        guard.passwd_file.flush().unwrap();
    }

    /// 更新/etc/shadow文件: 修改用户密码
    fn update_shadow_file(info: &PasswdInfo) {
        let mut new_content = String::new();
        let mut guard = GLOBAL_FILE.lock().unwrap();
        let content = read_to_string(&guard.shadow_file);
        for line in content.lines() {
            let mut field = line.split(':').collect::<Vec<_>>();
            if field[0] == info.username {
                field[1] = info.new_password.as_str();
            }
            new_content.push_str(format!("{}\n", field.join(":")).as_str());
        }

        guard.shadow_file.set_len(0).unwrap();
        guard.shadow_file.seek(std::io::SeekFrom::Start(0)).unwrap();
        guard.shadow_file.write_all(new_content.as_bytes()).unwrap();
        guard.shadow_file.flush().unwrap();
    }
}

/// groupadd执行器
pub struct GAddExecutor;

impl GAddExecutor {
    /// **执行groupadd**
    ///
    /// ## 参数
    /// - `info`: 组信息
    pub fn execute(info: GAddInfo) {
        Self::write_group_file(&info);
        Self::write_gshadow_file(&info);
    }

    /// 写入/etc/group文件: 添加用户组信息
    fn write_group_file(info: &GAddInfo) {
        GLOBAL_FILE
            .lock()
            .unwrap()
            .group_file
            .write_all(info.to_string_group().as_bytes())
            .unwrap()
    }

    /// 写入/etc/gshadow文件: 添加用户组密码信息
    fn write_gshadow_file(info: &GAddInfo) {
        GLOBAL_FILE
            .lock()
            .unwrap()
            .gshadow_file
            .write_all(info.to_string_gshadow().as_bytes())
            .unwrap();
    }
}

/// groupdel执行器
pub struct GDelExecutor;

impl GDelExecutor {
    /// **执行groupdel**
    ///
    /// ## 参数
    /// - `info`: 组信息
    pub fn execute(info: GDelInfo) {
        Self::update_group_file(&info);
        Self::update_gshadow_file(&info);
    }

    /// 更新/etc/group文件：删除用户组
    pub fn update_group_file(info: &GDelInfo) {
        let mut new_content = String::new();
        let mut guard = GLOBAL_FILE.lock().unwrap();
        let content = read_to_string(&guard.group_file);
        for line in content.lines() {
            let field = line.split(':').collect::<Vec<&str>>();
            if field[0] != info.groupname {
                new_content.push_str(format!("{}\n", line).as_str());
            }
        }

        guard.group_file.set_len(0).unwrap();
        guard.group_file.seek(std::io::SeekFrom::Start(0)).unwrap();
        guard.group_file.write_all(new_content.as_bytes()).unwrap();
        guard.group_file.flush().unwrap();
    }

    /// 更新/etc/gshadow文件：移除用户组
    pub fn update_gshadow_file(info: &GDelInfo) {
        let mut new_content = String::new();
        let mut guard = GLOBAL_FILE.lock().unwrap();
        let content = read_to_string(&guard.gshadow_file);
        for line in content.lines() {
            let field = line.split(':').collect::<Vec<&str>>();
            if field[0] != info.groupname {
                new_content.push_str(format!("{}\n", line).as_str());
            }
        }

        guard.gshadow_file.set_len(0).unwrap();
        guard
            .gshadow_file
            .seek(std::io::SeekFrom::Start(0))
            .unwrap();
        guard
            .gshadow_file
            .write_all(new_content.as_bytes())
            .unwrap();
        guard.gshadow_file.flush().unwrap();
    }
}

/// groupmod执行器
pub struct GModExecutor;

impl GModExecutor {
    /// **执行groupmod**
    ///
    /// ## 参数
    /// - `info`: 组信息
    pub fn execute(info: GModInfo) {
        Self::update_passwd_file(&info);
        Self::update_group_file(&info);
        Self::update_gshadow_file(&info);
    }

    /// 更新/etc/group文件: 更新用户组信息
    fn update_group_file(info: &GModInfo) {
        let mut new_content = String::new();
        let mut guard = GLOBAL_FILE.lock().unwrap();
        let content = read_to_string(&guard.group_file);
        for line in content.lines() {
            let mut field = line.split(':').collect::<Vec<&str>>();
            if field[0] == info.groupname {
                if let Some(new_groupname) = &info.new_groupname {
                    field[0] = new_groupname;
                }
                if let Some(new_gid) = &info.new_gid {
                    field[2] = new_gid;
                }
            }
            new_content.push_str(format!("{}\n", field.join(":")).as_str());
        }

        guard.group_file.set_len(0).unwrap();
        guard.group_file.seek(std::io::SeekFrom::Start(0)).unwrap();
        guard.group_file.write_all(new_content.as_bytes()).unwrap();
        guard.group_file.flush().unwrap();
    }

    /// 更新/etc/gshadow文件: 更新用户组密码信息
    fn update_gshadow_file(info: &GModInfo) {
        let mut new_content = String::new();
        let mut guard = GLOBAL_FILE.lock().unwrap();
        let content = read_to_string(&guard.gshadow_file);
        for line in content.lines() {
            let mut field = line.split(':').collect::<Vec<&str>>();
            if field[0] == info.groupname {
                if let Some(new_groupname) = &info.new_groupname {
                    field[0] = new_groupname;
                }
            }
            new_content.push_str(format!("{}\n", field.join(":")).as_str());
        }

        guard.gshadow_file.set_len(0).unwrap();
        guard
            .gshadow_file
            .seek(std::io::SeekFrom::Start(0))
            .unwrap();
        guard
            .gshadow_file
            .write_all(new_content.as_bytes())
            .unwrap();
        guard.gshadow_file.flush().unwrap();
    }

    /// 更新/etc/passwd文件: 更新用户组ID信息，因为用户组ID可能会被修改
    fn update_passwd_file(info: &GModInfo) {
        let mut new_content = String::new();
        let mut guard = GLOBAL_FILE.lock().unwrap();
        let content = read_to_string(&guard.passwd_file);
        for line in content.lines() {
            let mut field = line.split(':').collect::<Vec<&str>>();
            if field[3] == info.gid {
                if let Some(new_gid) = &info.new_gid {
                    field[3] = new_gid;
                }
            }
            new_content.push_str(format!("{}\n", field.join(":")).as_str());
        }

        guard.passwd_file.set_len(0).unwrap();
        guard.passwd_file.seek(std::io::SeekFrom::Start(0)).unwrap();
        guard.passwd_file.write_all(new_content.as_bytes()).unwrap();
        guard.passwd_file.flush().unwrap();
    }
}

fn read_to_string(mut file: &File) -> String {
    file.seek(std::io::SeekFrom::Start(0)).unwrap();
    let mut content = String::new();
    file.read_to_string(&mut content).unwrap();
    content
}
