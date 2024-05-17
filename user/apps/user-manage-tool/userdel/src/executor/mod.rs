use self::fileupdater::FileUpdater;
use crate::check::Info;

mod fileupdater;

pub struct Executor;

impl Executor {
    pub fn execute(info: Info) {
        // 移除home目录
        if let Some(home) = info.home.clone() {
            std::fs::remove_dir_all(home).unwrap();
        }

        let file_updater = FileUpdater::new(info);
        file_updater.update();
    }
}
