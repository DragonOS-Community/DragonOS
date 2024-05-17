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
        self.update_group_file();
        self.update_gshadow_file();
    }

    /// 更新/etc/group文件：删除用户组
    pub fn update_group_file(&self) {
        let r = OpenOptions::new().read(true).write(true).open("/etc/group");

        match r {
            Ok(mut file) => {
                let mut content = String::new();
                let mut new_content = String::new();
                file.read_to_string(&mut content).unwrap();
                for line in content.lines() {
                    let field = line.split(':').collect::<Vec<&str>>();
                    if field[0] != self.info.groupname {
                        new_content.push_str(format!("{}\n", line).as_str());
                    }
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

    /// 更新/etc/gshadow文件：移除用户组
    pub fn update_gshadow_file(&self) {
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
                    let field = line.split(':').collect::<Vec<&str>>();
                    if field[0] != self.info.groupname {
                        new_content.push_str(format!("{}\n", line).as_str());
                    }
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
