use std::path::PathBuf;

pub mod cargo_handler;

#[allow(dead_code)]
pub struct FileUtils;

#[allow(dead_code)]
impl FileUtils {
    /// 列出指定目录下的所有文件
    ///
    /// ## 参数
    ///
    /// - `path` - 指定的目录
    /// - `ext_name` - 文件的扩展名，如果为None，则列出所有文件
    /// - `recursive` - 是否递归列出所有文件
    pub fn list_all_files(path: &PathBuf, ext_name: Option<&str>, recursive: bool) -> Vec<PathBuf> {
        let mut queue: Vec<PathBuf> = Vec::new();
        let mut result = Vec::new();
        queue.push(path.clone());

        while !queue.is_empty() {
            let path = queue.pop().unwrap();
            let d = std::fs::read_dir(path);
            if d.is_err() {
                continue;
            }
            let d = d.unwrap();

            d.for_each(|ent| {
                if let Ok(ent) = ent {
                    if let Ok(file_type) = ent.file_type() {
                        if file_type.is_file() {
                            if let Some(e) = ext_name {
                                if let Some(ext) = ent.path().extension() {
                                    if ext == e {
                                        result.push(ent.path());
                                    }
                                }
                            }
                        } else if file_type.is_dir() && recursive {
                            queue.push(ent.path());
                        }
                    }
                }
            });
        }

        return result;
    }
}
