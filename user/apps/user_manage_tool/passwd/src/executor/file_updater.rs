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
    }

    /// 更新/etc/passwd文件: 修改用户密码
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
                    let mut field = line.split(':').collect::<Vec<_>>();
                    if field[0] == self.info.username {
                        if self.info.new_password.is_empty() {
                            field[1] = "";
                        } else {
                            field[1] = "x";
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
                    "Can't open file: /etc/passwd".to_string(),
                    crate::error::ExitStatus::PasswdFile,
                );
            }
        }
    }

    /// 更新/etc/shadow文件: 修改用户密码
    fn update_shadow_file(&self) {
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
                    let mut field = line.split(':').collect::<Vec<_>>();
                    if field[0] == self.info.username {
                        field[1] = self.info.new_password.as_str();
                    }
                    new_content.push_str(format!("{}\n", field.join(":")).as_str());
                }

                file.set_len(0).unwrap();
                file.seek(std::io::SeekFrom::Start(0)).unwrap();
                file.write_all(new_content.as_bytes()).unwrap();
                file.flush().unwrap();
            }
            Err(_) => ErrorHandler::error_handle(
                "Can't open file: /etc/shadow".to_string(),
                crate::error::ExitStatus::ShadowFile,
            ),
        }
    }
}
