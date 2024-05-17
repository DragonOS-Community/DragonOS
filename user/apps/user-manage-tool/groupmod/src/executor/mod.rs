use crate::check::Info;

use self::fileupdater::FileUpdaer;

mod fileupdater;

pub struct Executor;

impl Executor {
    pub fn execute(info: Info) {
        let file_updater = FileUpdaer::new(info);
        file_updater.update();
    }
}
