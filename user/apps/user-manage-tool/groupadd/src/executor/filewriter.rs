use crate::{check::GroupInfo, error::ErrorHandler};
use std::{fs::OpenOptions, io::Write};

pub struct FileWriter {
    group_info: GroupInfo,
}

impl FileWriter {
    pub fn new(group_info: GroupInfo) -> Self {
        Self { group_info }
    }

    pub fn write(&self) {
        self.write_group_file();
        self.write_gshadow_file();
    }

    /// 写入/etc/group文件: 添加用户组信息
    fn write_group_file(&self) {
        let r = OpenOptions::new()
            .read(true)
            .write(true)
            .append(true)
            .open("/etc/group");
        match r {
            Ok(mut file) => file
                .write_all(self.group_info.to_string_group().as_bytes())
                .unwrap(),
            Err(_) => ErrorHandler::error_handle(
                "Can't open file: /etc/group".to_string(),
                crate::error::ExitStatus::GroupFile,
            ),
        }
    }

    /// 写入/etc/gshadow文件: 添加用户组密码信息
    fn write_gshadow_file(&self) {
        let r = OpenOptions::new()
            .read(true)
            .write(true)
            .append(true)
            .open("/etc/gshadow");
        match r {
            Ok(mut file) => file
                .write_all(self.group_info.to_string_gshadow().as_bytes())
                .unwrap(),
            Err(_) => ErrorHandler::error_handle(
                "Can't open file: /etc/gshadow".to_string(),
                crate::error::ExitStatus::GshadowFile,
            ),
        }
    }
}
