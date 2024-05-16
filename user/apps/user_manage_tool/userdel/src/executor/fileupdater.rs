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
        self.update_shadow_file();
        self.update_group_file();
        self.update_gshadow_file();
    }

    /// 更新/etc/passwd文件: 删除用户信息
    fn update_passwd_file(&self) {
        let r = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/etc/passwd");
        if let Ok(mut file) = r {
            let mut buf = String::new();
            file.read_to_string(&mut buf).unwrap();
            let lines: Vec<&str> = buf.lines().collect();
            let new_content = lines
                .into_iter()
                .filter(|&line| !line.contains(&self.info.username))
                .collect::<Vec<&str>>()
                .join("\n");

            file.set_len(0).unwrap();
            file.seek(std::io::SeekFrom::Start(0)).unwrap();
            file.write_all(new_content.as_bytes()).unwrap();
            file.flush().unwrap();
        } else {
            ErrorHandler::error_handle(
                "Can't find file: /etc/passwd".to_string(),
                crate::error::ExitStatus::PasswdFile,
            )
        }
    }

    /// 更新/etc/group文件: 将用户从组中移除
    fn update_group_file(&self) {
        let r = OpenOptions::new().read(true).write(true).open("/etc/group");
        if let Ok(mut file) = r {
            let mut buf = String::new();
            file.read_to_string(&mut buf).unwrap();
            let mut new_content = String::new();
            for line in buf.lines() {
                let mut info = line.split(':').collect::<Vec<&str>>();
                let mut users = info.last().unwrap().split(",").collect::<Vec<&str>>();
                if users.contains(&self.info.username.as_str()) {
                    info.remove(info.len() - 1);
                    users.remove(
                        users
                            .iter()
                            .position(|&x| x == self.info.username.as_str())
                            .unwrap(),
                    );
                    let users = users.join(",");
                    info.push(&users.as_str());
                    new_content.push_str(format!("{}\n", info.join(":").as_str()).as_str());
                } else {
                    new_content.push_str(format!("{}\n", info.join(":").as_str()).as_str());
                }

                file.set_len(0).unwrap();
                file.seek(std::io::SeekFrom::Start(0)).unwrap();
                file.write_all(new_content.as_bytes()).unwrap();
                file.flush().unwrap();
            }
        } else {
            ErrorHandler::error_handle(
                "Can't find file: /etc/group".to_string(),
                crate::error::ExitStatus::GroupFile,
            );
        }
    }

    /// 更新/etc/shadow文件: 将用户信息删去
    fn update_shadow_file(&self) {
        let r = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/etc/shadow");
        if let Ok(mut file) = r {
            let mut buf = String::new();
            file.read_to_string(&mut buf).unwrap();
            let lines: Vec<&str> = buf.lines().collect();
            let new_content = lines
                .into_iter()
                .filter(|&line| !line.contains(&self.info.username))
                .collect::<Vec<&str>>()
                .join("\n");

            file.set_len(0).unwrap();
            file.seek(std::io::SeekFrom::Start(0)).unwrap();
            file.write_all(new_content.as_bytes()).unwrap();
            file.flush().unwrap();
        } else {
            ErrorHandler::error_handle(
                "Can't find file: /etc/shadow".to_string(),
                crate::error::ExitStatus::ShadowFile,
            )
        }
    }

    /// 更新/etc/gshadow文件: 将用户从组中移除
    fn update_gshadow_file(&self) {
        let r = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/etc/gshadow");
        if let Ok(mut file) = r {
            let mut buf = String::new();
            file.read_to_string(&mut buf).unwrap();
            let mut new_content = String::new();
            for line in buf.lines() {
                let mut info = line.split(':').collect::<Vec<&str>>();
                let mut users = info.last().unwrap().split(",").collect::<Vec<&str>>();
                if users.contains(&self.info.username.as_str()) {
                    info.remove(info.len() - 1);
                    users.remove(
                        users
                            .iter()
                            .position(|&x| x == self.info.username.as_str())
                            .unwrap(),
                    );
                    let users = users.join(",");
                    info.push(&users.as_str());
                    new_content.push_str(format!("{}\n", info.join(":").as_str()).as_str());
                } else {
                    new_content.push_str(format!("{}\n", info.join(":").as_str()).as_str());
                }

                file.set_len(0).unwrap();
                file.seek(std::io::SeekFrom::Start(0)).unwrap();
                file.write_all(new_content.as_bytes()).unwrap();
                file.flush().unwrap();
            }
        } else {
            ErrorHandler::error_handle(
                "Can't find file: /etc/gshadow".to_string(),
                crate::error::ExitStatus::GshadowFile,
            );
        }
    }
}
