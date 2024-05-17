use self::filewriter::FileWriter;
use crate::{check::userinfo::UserInfo, error::ErrorHandler};
use std::fs;

mod filewriter;

/// 执行器
pub struct Executor;

impl Executor {
    pub fn execute(userinfo: UserInfo) {
        // 创建用户home目录
        let home_dir = userinfo.home_dir.clone();
        let dir_builder = fs::DirBuilder::new();
        if dir_builder.create(home_dir.clone()).is_err() {
            ErrorHandler::error_handle(
                format!("unable to create {}", home_dir),
                crate::error::ExitStatus::CreateHomeFail,
            );
        }

        // 写入文件
        let writer = FileWriter::new(userinfo);
        writer.write();
    }
}
