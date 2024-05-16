use self::fileupdater::FileUpdater;
use crate::check::Info;

mod fileupdater;

pub struct Executor;

impl Executor {
    pub fn execute(info: Info) {
        let file_updater = FileUpdater::new(info);
        file_updater.update();
    }
}
