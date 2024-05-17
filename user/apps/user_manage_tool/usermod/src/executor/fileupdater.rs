use crate::{check::Info, error::ErrorHandler};
use std::{
    fs::OpenOptions,
    io::{Read, Seek, Write},
};

pub struct FileUpdater {
    info: Info,
}

impl FileUpdater {
    pub fn new(info: Info) -> Self {
        Self { info }
    }

    pub fn update(&self) {
        self.update_passwd_file();
        self.update_group_file();
        self.update_shadow_file();
        self.update_gshadow_file();
    }

    /// 更新/etc/passwd文件的username、uid、comment、home、shell
    fn update_passwd_file(&self) {
        let r = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/etc/passwd");
        match r {
            Ok(mut file) => {
                let mut content = String::new();
                let mut new_content = String::new();
                file.read_to_string(&mut content).unwrap();
                for line in content.lines() {
                    let mut fields = line.split(':').collect::<Vec<&str>>();
                    if fields[0] == self.info.username {
                        if let Some(new_username) = &self.info.new_name {
                            fields[0] = new_username;
                        }
                        if let Some(new_uid) = &self.info.new_uid {
                            fields[2] = new_uid;
                        }
                        if let Some(new_comment) = &self.info.new_comment {
                            fields[4] = new_comment;
                        }
                        if let Some(new_home) = &self.info.new_home {
                            fields[5] = new_home;
                        }
                        if let Some(new_shell) = &self.info.new_shell {
                            fields[6] = new_shell;
                        }
                        new_content.push_str(format!("{}\n", fields.join(":")).as_str());
                    } else {
                        new_content.push_str(format!("{}\n", line).as_str());
                    }

                    file.set_len(0).unwrap();
                    file.seek(std::io::SeekFrom::Start(0)).unwrap();
                    file.write_all(new_content.as_bytes()).unwrap();
                    file.flush().unwrap();
                }
            }
            Err(_) => ErrorHandler::error_handle(
                "Can't open file /etc/passwd".to_string(),
                crate::error::ExitStatus::PasswdFile,
            ),
        }
    }

    /// 更新/etc/group文件中各用户组中的用户
    fn update_group_file(&self) {
        let r = OpenOptions::new().read(true).write(true).open("/etc/group");

        match r {
            Ok(mut file) => {
                let mut name = self.info.username.clone();
                if let Some(new_name) = &self.info.new_name {
                    name = new_name.clone();
                }
                let mut content = String::new();
                let mut new_content = String::new();
                file.read_to_string(&mut content).unwrap();
                for line in content.lines() {
                    let mut fields = line.split(':').collect::<Vec<&str>>();
                    let mut users = fields[3].split(",").collect::<Vec<&str>>();
                    users = users
                        .into_iter()
                        .filter(|username| !username.is_empty())
                        .collect::<Vec<&str>>();
                    if let Some(idx) = users.iter().position(|&r| r == self.info.username) {
                        if let Some(group) = &self.info.new_group {
                            // 换组，将用户从当前组删去
                            if group != fields[0] {
                                users.remove(idx);
                            }
                        } else {
                            // 不换组但是要更新名字
                            users[idx] = &name;
                        }
                    }

                    if let Some(groups) = &self.info.groups {
                        if groups.contains(&fields[0].to_string())
                            && !users.contains(&name.as_str())
                        {
                            users.push(&name);
                        }
                    }

                    let new_users = users.join(",");
                    fields[3] = new_users.as_str();
                    new_content.push_str(format!("{}\n", fields.join(":")).as_str());
                }

                file.set_len(0).unwrap();
                file.seek(std::io::SeekFrom::Start(0)).unwrap();
                file.write_all(new_content.as_bytes()).unwrap();
                file.flush().unwrap();
            }
            Err(_) => ErrorHandler::error_handle(
                "Can't open file /etc/group".to_string(),
                crate::error::ExitStatus::GshadowFile,
            ),
        }
    }

    /// 更新/etc/shadow文件的username
    fn update_shadow_file(&self) {
        if let Some(new_name) = &self.info.new_name {
            let r = OpenOptions::new()
                .read(true)
                .write(true)
                .open("/etc/shadow");
            match r {
                Ok(mut file) => {
                    let mut content = String::new();
                    let mut new_content = String::new();
                    file.read_to_string(&mut content).unwrap();
                    for line in content.lines() {
                        let mut fields = line.split(':').collect::<Vec<&str>>();
                        if fields[0] == self.info.username {
                            fields[0] = new_name;
                            new_content.push_str(format!("{}\n", fields.join(":")).as_str());
                        } else {
                            new_content.push_str(format!("{}\n", line).as_str());
                        }
                    }

                    file.set_len(0).unwrap();
                    file.seek(std::io::SeekFrom::Start(0)).unwrap();
                    file.write_all(new_content.as_bytes()).unwrap();
                    file.flush().unwrap();
                }
                Err(_) => ErrorHandler::error_handle(
                    "Can't open file /etc/shadow".to_string(),
                    crate::error::ExitStatus::ShadowFile,
                ),
            }
        }
    }

    /// 更新/etc/gshadow文件中各用户组中的用户
    fn update_gshadow_file(&self) {
        let r = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/etc/gshadow");

        match r {
            Ok(mut file) => {
                let mut name = self.info.username.clone();
                if let Some(new_name) = &self.info.new_name {
                    name = new_name.clone();
                }
                let mut content = String::new();
                let mut new_content = String::new();
                file.read_to_string(&mut content).unwrap();
                for line in content.lines() {
                    let mut fields = line.split(':').collect::<Vec<&str>>();
                    let mut users = fields[3].split(",").collect::<Vec<&str>>();
                    users = users
                        .into_iter()
                        .filter(|username| !username.is_empty())
                        .collect::<Vec<&str>>();
                    if let Some(idx) = users.iter().position(|&r| r == self.info.username) {
                        if let Some(group) = &self.info.new_group {
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
                    if let Some(groups) = &self.info.groups {
                        if groups.contains(&fields[0].to_string())
                            && !users.contains(&name.as_str())
                        {
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

                file.set_len(0).unwrap();
                file.seek(std::io::SeekFrom::Start(0)).unwrap();
                file.write_all(new_content.as_bytes()).unwrap();
                file.flush().unwrap();
            }
            Err(_) => ErrorHandler::error_handle(
                "Can't open file /etc/gshadow".to_string(),
                crate::error::ExitStatus::GshadowFile,
            ),
        }
    }
}
