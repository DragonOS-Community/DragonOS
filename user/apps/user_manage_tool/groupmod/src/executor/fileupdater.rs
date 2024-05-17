use crate::{check::Info, error::ErrorHandler};
use std::{
    fs::OpenOptions,
    io::{Read, Seek, Write},
};

pub struct FileUpdaer {
    info: Info,
}

impl FileUpdaer {
    pub fn new(info: Info) -> Self {
        Self { info }
    }

    pub fn update(&self) {
        self.update_group_file();
        self.update_gshadow_file();
        self.update_passwd_file();
    }

    /// 更新/etc/group文件: 更新用户组信息
    fn update_group_file(&self) {
        let r = OpenOptions::new().read(true).write(true).open("/etc/group");
        match r {
            Ok(mut file) => {
                let mut content = String::new();
                let mut new_content = String::new();
                file.read_to_string(&mut content).unwrap();
                for line in content.lines() {
                    let mut field = line.split(':').collect::<Vec<&str>>();
                    if field[0] == self.info.groupname {
                        if let Some(new_groupname) = &self.info.new_groupname {
                            field[0] = new_groupname;
                        }
                        if let Some(new_gid) = &self.info.new_gid {
                            field[2] = new_gid;
                        }
                    }
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

    /// 更新/etc/gshadow文件: 更新用户组密码信息
    fn update_gshadow_file(&self) {
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
                    let mut field = line.split(':').collect::<Vec<&str>>();
                    if field[0] == self.info.groupname {
                        if let Some(new_groupname) = &self.info.new_groupname {
                            field[0] = new_groupname;
                        }
                    }
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

    /// 更新/etc/passwd文件: 更新用户组ID信息，因为用户组ID可能会被修改
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
                    let mut field = line.split(':').collect::<Vec<&str>>();
                    if field[3] == self.info.gid {
                        if let Some(new_gid) = &self.info.new_gid {
                            field[3] = new_gid;
                        }
                    }
                    new_content.push_str(format!("{}\n", field.join(":")).as_str());
                }

                file.set_len(0).unwrap();
                file.seek(std::io::SeekFrom::Start(0)).unwrap();
                file.write_all(new_content.as_bytes()).unwrap();
                file.flush().unwrap();
            }
            Err(_) => ErrorHandler::error_handle(
                "Can't open file: /etc/passwd".to_string(),
                crate::error::ExitStatus::PasswdFile,
            ),
        }
    }
}
