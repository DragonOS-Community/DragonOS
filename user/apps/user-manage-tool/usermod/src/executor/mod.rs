use self::fileupdater::FileUpdater;
use crate::{check::Info, error::ErrorHandler};
use std::fs;
mod fileupdater;

pub struct Executor;

impl Executor {
    pub fn execute(info: Info) {
        // 创建new_home
        if let Some(new_home) = &info.new_home {
            let dir_builder = fs::DirBuilder::new();
            if dir_builder.create(new_home.clone()).is_err() {
                ErrorHandler::error_handle(
                    format!("unable to create {}", new_home),
                    crate::error::ExitStatus::CreateHomeFail,
                );
            }
        }

        let file_updater = FileUpdater::new(info);
        file_updater.update();
    }
}
