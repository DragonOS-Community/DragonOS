use crate::{check::userinfo::UserInfo, error::ErrorHandler};
use std::{
    fs::OpenOptions,
    io::{Read, Seek, Write},
};

pub struct FileWriter {
    userinfo: UserInfo,
}

impl FileWriter {
    pub fn new(userinfo: UserInfo) -> Self {
        Self { userinfo }
    }

    pub fn write(&self) {
        self.write_passwd_file();
        self.write_shadow_file();
        self.write_group_file();
        self.write_gshadow_file();
    }

    /// 写入/etc/passwd文件：添加用户信息
    fn write_passwd_file(&self) {
        let file = OpenOptions::new().append(true).open("/etc/passwd");

        if let Ok(mut file) = file {
            let userinfo: String = self.userinfo.clone().into();
            file.write_all(userinfo.as_bytes()).unwrap();
        } else {
            ErrorHandler::error_handle(
                "Can't find file: /etc/passwd".to_string(),
                crate::error::ExitStatus::PasswdFile,
            )
        }
    }

    /// 写入/etc/group文件：将用户添加到对应用户组中
    fn write_group_file(&self) {
        if self.userinfo.group == self.userinfo.username {
            return;
        }

        let r = OpenOptions::new().read(true).write(true).open("/etc/group");

        match r {
            Ok(mut file) => {
                let mut content = String::new();
                let mut new_content = String::new();
                file.read_to_string(&mut content).unwrap();
                for line in content.lines() {
                    let mut field = line.split(":").collect::<Vec<&str>>();
                    let mut users = field.last().unwrap().split(",").collect::<Vec<&str>>();
                    users = users
                        .into_iter()
                        .filter(|username| !username.is_empty())
                        .collect::<Vec<&str>>();
                    if field[0].eq(self.userinfo.group.as_str())
                        && !users.contains(&self.userinfo.username.as_str())
                    {
                        users.push(self.userinfo.username.as_str());
                    }

                    let new_users = users.join(",");
                    field[3] = new_users.as_str();
                    new_content.push_str(format!("{}\n", field.join(":")).as_str());
                }
                file.set_len(0).unwrap();
                file.seek(std::io::SeekFrom::Start(0)).unwrap();
                file.write_all(new_content.as_bytes()).unwrap();
                file.flush().unwrap();
            }
            Err(_) => {
                ErrorHandler::error_handle(
                    "Can't open file: /etc/group".to_string(),
                    crate::error::ExitStatus::GroupFile,
                );
            }
        }
    }

    /// 写入/etc/shadow文件：添加用户口令相关信息
    fn write_shadow_file(&self) {
        let r = OpenOptions::new().append(true).open("/etc/shadow");

        match r {
            Ok(mut file) => {
                let data = format!("{}::::::::\n", self.userinfo.username,);
                file.write_all(data.as_bytes()).unwrap();
            }
            Err(_) => ErrorHandler::error_handle(
                "Can't find file: /etc/shadow".to_string(),
                crate::error::ExitStatus::ShadowFile,
            ),
        }
    }

    /// 写入/etc/gshadow文件：将用户添加到对应用户组中
    fn write_gshadow_file(&self) {
        if self.userinfo.group == self.userinfo.username {
            return;
        }

        let r = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/etc/gshadow");

        match r {
            Ok(mut file) => {
                let mut content = String::new();
                let mut new_content = String::new();
                file.read_to_string(&mut content).unwrap();
                for line in content.lines() {
                    let mut field = line.split(":").collect::<Vec<&str>>();
                    let mut users = field.last().unwrap().split(",").collect::<Vec<&str>>();
                    users = users
                        .into_iter()
                        .filter(|username| !username.is_empty())
                        .collect::<Vec<&str>>();
                    if field[0].eq(self.userinfo.group.as_str())
                        && !users.contains(&self.userinfo.username.as_str())
                    {
                        users.push(self.userinfo.username.as_str());
                    }

                    let new_users = users.join(",");
                    field[3] = new_users.as_str();
                    new_content.push_str(format!("{}\n", field.join(":")).as_str());
                }
                file.set_len(0).unwrap();
                file.seek(std::io::SeekFrom::Start(0)).unwrap();
                file.write_all(new_content.as_bytes()).unwrap();
                file.flush().unwrap();
            }
            Err(_) => {
                ErrorHandler::error_handle(
                    "Can't open file: /etc/gshadow".to_string(),
                    crate::error::ExitStatus::GshadowFile,
                );
            }
        }
    }
}
