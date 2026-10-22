use std::path::PathBuf;

/// 获取给定路径的绝对路径
pub fn abs_path(path: &PathBuf) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap().join(path)
    }
}
