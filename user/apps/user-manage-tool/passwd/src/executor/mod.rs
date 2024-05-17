use crate::check::Info;

mod file_updater;

pub struct Executor;

impl Executor {
    pub fn execute(info: Info) {
        let file_updater = file_updater::FileUpdater::new(info);
        file_updater.update();
    }
}
