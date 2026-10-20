use std::{
    fs::File,
    path::Path,
    process::{Command, Stdio},
};

use reqwest::{blocking::ClientBuilder, Url};

use super::stdio::StdioUtils;

pub struct FileUtils;

impl FileUtils {
    ///从指定url下载文件到指定路径
    pub fn download_file(url: &str, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let tempurl = Url::parse(url).expect("failed to parse the url");
        let file_name = tempurl
            .path_segments()
            .expect("connot be base url")
            .last()
            .expect("failed to get the filename from the url");
        let client = ClientBuilder::new()
            .timeout(std::time::Duration::from_secs(10))
            .build()?;
        let mut response = client.get(url).send()?;
        let mut file = File::create(path.join(file_name))?;
        response.copy_to(&mut file)?;
        Ok(())
    }

    /// 递归地复制给定目录下所有文件到另一个文件夹中
    pub fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), String> {
        log::trace!("FileUtils::copy_dir_all: src: {:?}, dst: {:?}", src, dst);
        let mut cmd = Command::new("cp");
        cmd.arg("-r").arg("-f").arg("./").arg(dst);

        cmd.current_dir(src);

        // 创建子进程，执行命令
        let proc: std::process::Child = cmd
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let output = proc.wait_with_output().map_err(|e| e.to_string())?;

        if !output.status.success() {
            return Err(format!(
                "copy_dir_all failed, status: {:?},  stderr: {:?}",
                output.status,
                StdioUtils::tail_n_str(StdioUtils::stderr_to_lines(&output.stderr), 5)
            ));
        }
        Ok(())
    }
}
