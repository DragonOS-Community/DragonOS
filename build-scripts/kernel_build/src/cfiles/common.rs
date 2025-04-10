use std::{collections::HashSet, path::PathBuf};

pub(super) fn setup_common_include_dir(include_dirs: &mut HashSet<PathBuf>) {
    const DIRS: [&str; 2] = ["src/common", "src"];
    DIRS.iter().for_each(|dir| {
        include_dirs.insert(dir.into());
    });
}
