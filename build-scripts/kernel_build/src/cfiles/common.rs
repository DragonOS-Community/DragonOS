use std::{collections::HashSet, path::PathBuf};

use crate::utils::FileUtils;

pub(super) fn setup_common_files(files: &mut HashSet<PathBuf>) {
    const DIRS: [&str; 3] = ["src/common", "src/debug/traceback", "src/libs"];
    DIRS.iter().for_each(|dir| {
        FileUtils::list_all_files(&dir.into(), Some("c"), true)
            .into_iter()
            .for_each(|f| {
                files.insert(f);
            });
    });
}

pub(super) fn setup_common_include_dir(include_dirs: &mut HashSet<PathBuf>) {
    const DIRS: [&str; 3] = ["src/include", "src/common", "src"];
    DIRS.iter().for_each(|dir| {
        include_dirs.insert(dir.into());
    });
}
