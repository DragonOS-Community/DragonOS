use log::info;
use regex::Regex;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::PermissionsExt;
use std::{
    fs::File,
    path::PathBuf,
    process::{Command, Stdio},
};
use zip::ZipArchive;

use crate::utils::{file::FileUtils, stdio::StdioUtils};

use super::cache::CacheDir;

use anyhow::{Error, Result};

/// # Git源
///
/// 从Git仓库获取源码
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitSource {
    /// Git仓库地址
    url: String,
    /// 分支（可选，如果为空，则拉取master）branch和revision只能同时指定一个
    branch: Option<String>,
    /// 特定的提交的hash值（可选，如果为空，则拉取branch的最新提交）
    revision: Option<String>,
}

impl GitSource {
    pub fn new(url: String, branch: Option<String>, revision: Option<String>) -> Self {
        Self {
            url,
            branch,
            revision,
        }
    }
    /// # 验证参数合法性
    ///
    /// 仅进行形式校验，不会检查Git仓库是否存在，以及分支是否存在、是否有权限访问等
    pub fn validate(&mut self) -> Result<()> {
        if self.url.is_empty() {
            return Err(Error::msg("url is empty"));
        }
        // branch和revision不能同时为空
        if self.branch.is_none() && self.revision.is_none() {
            self.branch = Some("master".to_string());
        }
        // branch和revision只能同时指定一个
        if self.branch.is_some() && self.revision.is_some() {
            return Err(Error::msg("branch and revision are both specified"));
        }

        if self.branch.is_some() {
            if self.branch.as_ref().unwrap().is_empty() {
                return Err(Error::msg("branch is empty"));
            }
        }
        if self.revision.is_some() {
            if self.revision.as_ref().unwrap().is_empty() {
                return Err(Error::msg("revision is empty"));
            }
        }
        return Ok(());
    }

    pub fn trim(&mut self) {
        self.url = self.url.trim().to_string();
        if let Some(branch) = &mut self.branch {
            *branch = branch.trim().to_string();
        }

        if let Some(revision) = &mut self.revision {
            *revision = revision.trim().to_string();
        }
    }

    /// # 确保Git仓库已经克隆到指定目录，并且切换到指定分支/Revision
    ///
    /// 如果目录不存在，则会自动创建
    ///
    /// ## 参数
    ///
    /// - `target_dir` - 目标目录
    ///
    /// ## 返回
    ///
    /// - `Ok(())` - 成功
    /// - `Err(String)` - 失败，错误信息
    pub fn prepare(&self, target_dir: &CacheDir) -> Result<(), String> {
        info!(
            "Preparing git repo: {}, branch: {:?}, revision: {:?}",
            self.url, self.branch, self.revision
        );

        target_dir.create().map_err(|e| {
            format!(
                "Failed to create target dir: {}, message: {e:?}",
                target_dir.path.display()
            )
        })?;

        if target_dir.is_empty().map_err(|e| {
            format!(
                "Failed to check if target dir is empty: {}, message: {e:?}",
                target_dir.path.display()
            )
        })? {
            info!("Target dir is empty, cloning repo");
            self.clone_repo(target_dir)?;
        }

        self.checkout(target_dir)?;

        self.pull(target_dir)?;

        return Ok(());
    }

    fn check_repo(&self, target_dir: &CacheDir) -> Result<bool, String> {
        let path: &PathBuf = &target_dir.path;
        let mut cmd = Command::new("git");
        cmd.arg("remote").arg("get-url").arg("origin");

        // 设置工作目录
        cmd.current_dir(path);

        // 创建子进程，执行命令
        let proc: std::process::Child = cmd
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let output = proc.wait_with_output().map_err(|e| e.to_string())?;

        if output.status.success() {
            let mut r = String::from_utf8(output.stdout).unwrap();
            r.pop();
            Ok(r == self.url)
        } else {
            return Err(format!(
                "git remote get-url origin failed, status: {:?},  stderr: {:?}",
                output.status,
                StdioUtils::tail_n_str(StdioUtils::stderr_to_lines(&output.stderr), 5)
            ));
        }
    }

    fn set_url(&self, target_dir: &CacheDir) -> Result<(), String> {
        let path: &PathBuf = &target_dir.path;
        let mut cmd = Command::new("git");
        cmd.arg("remote")
            .arg("set-url")
            .arg("origin")
            .arg(self.url.as_str());

        // 设置工作目录
        cmd.current_dir(path);

        // 创建子进程，执行命令
        let proc: std::process::Child = cmd
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let output = proc.wait_with_output().map_err(|e| e.to_string())?;

        if !output.status.success() {
            return Err(format!(
                "git remote set-url origin failed, status: {:?},  stderr: {:?}",
                output.status,
                StdioUtils::tail_n_str(StdioUtils::stderr_to_lines(&output.stderr), 5)
            ));
        }
        Ok(())
    }

    fn checkout(&self, target_dir: &CacheDir) -> Result<(), String> {
        // 确保目标目录中的仓库为所指定仓库
        if !self.check_repo(target_dir).map_err(|e| {
            format!(
                "Failed to check repo: {}, message: {e:?}",
                target_dir.path.display()
            )
        })? {
            info!("Target dir isn't specified repo, change remote url");
            self.set_url(target_dir)?;
        }

        let do_checkout = || -> Result<(), String> {
            let mut cmd = Command::new("git");
            cmd.current_dir(&target_dir.path);
            cmd.arg("checkout");

            if let Some(branch) = &self.branch {
                cmd.arg(branch);
            }
            if let Some(revision) = &self.revision {
                cmd.arg(revision);
            }

            // 强制切换分支，且安静模式
            cmd.arg("-f").arg("-q");

            // 创建子进程，执行命令
            let proc: std::process::Child = cmd
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| e.to_string())?;
            let output = proc.wait_with_output().map_err(|e| e.to_string())?;

            if !output.status.success() {
                return Err(format!(
                    "Failed to checkout {}, message: {}",
                    target_dir.path.display(),
                    String::from_utf8_lossy(&output.stdout)
                ));
            }

            let mut subcmd = Command::new("git");
            subcmd.current_dir(&target_dir.path);
            subcmd.arg("submodule").arg("update").arg("--remote");

            //当checkout仓库的子进程结束后，启动checkout子模块的子进程
            let subproc: std::process::Child = subcmd
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| e.to_string())?;
            let suboutput = subproc.wait_with_output().map_err(|e| e.to_string())?;

            if !suboutput.status.success() {
                return Err(format!(
                    "Failed to checkout submodule {}, message: {}",
                    target_dir.path.display(),
                    String::from_utf8_lossy(&suboutput.stdout)
                ));
            }
            return Ok(());
        };

        if let Err(_) = do_checkout() {
            // 如果切换分支失败，则尝试重新fetch
            if self.revision.is_some() {
                self.set_fetch_config(target_dir)?;
                self.unshallow(target_dir)?
            };

            self.fetch_all(target_dir).ok();
            do_checkout()?;
        }

        return Ok(());
    }

    pub fn clone_repo(&self, cache_dir: &CacheDir) -> Result<(), String> {
        let path: &PathBuf = &cache_dir.path;
        let mut cmd = Command::new("git");
        cmd.arg("clone").arg(&self.url).arg(".").arg("--recursive");

        if let Some(branch) = &self.branch {
            cmd.arg("--branch").arg(branch).arg("--depth").arg("1");
        }

        // 对于克隆，如果指定了revision，则直接克隆整个仓库，稍后再切换到指定的revision

        // 设置工作目录
        cmd.current_dir(path);

        // 创建子进程，执行命令
        let proc: std::process::Child = cmd
            .stderr(Stdio::piped())
            .stdout(Stdio::inherit())
            .spawn()
            .map_err(|e| e.to_string())?;
        let output = proc.wait_with_output().map_err(|e| e.to_string())?;

        if !output.status.success() {
            return Err(format!(
                "clone git repo failed, status: {:?},  stderr: {:?}",
                output.status,
                StdioUtils::tail_n_str(StdioUtils::stderr_to_lines(&output.stderr), 5)
            ));
        }

        let mut subcmd = Command::new("git");
        subcmd
            .arg("submodule")
            .arg("update")
            .arg("--init")
            .arg("--recursive")
            .arg("--force");

        subcmd.current_dir(path);

        //当克隆仓库的子进程结束后，启动保证克隆子模块的子进程
        let subproc: std::process::Child = subcmd
            .stderr(Stdio::piped())
            .stdout(Stdio::inherit())
            .spawn()
            .map_err(|e| e.to_string())?;
        let suboutput = subproc.wait_with_output().map_err(|e| e.to_string())?;

        if !suboutput.status.success() {
            return Err(format!(
                "clone submodule failed, status: {:?},  stderr: {:?}",
                suboutput.status,
                StdioUtils::tail_n_str(StdioUtils::stderr_to_lines(&suboutput.stderr), 5)
            ));
        }
        return Ok(());
    }

    /// 设置fetch所有分支
    fn set_fetch_config(&self, target_dir: &CacheDir) -> Result<(), String> {
        let mut cmd = Command::new("git");
        cmd.current_dir(&target_dir.path);
        cmd.arg("config")
            .arg("remote.origin.fetch")
            .arg("+refs/heads/*:refs/remotes/origin/*");

        // 创建子进程，执行命令
        let proc: std::process::Child = cmd
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let output = proc.wait_with_output().map_err(|e| e.to_string())?;

        if !output.status.success() {
            return Err(format!(
                "Failed to set fetch config {}, message: {}",
                target_dir.path.display(),
                StdioUtils::tail_n_str(StdioUtils::stderr_to_lines(&output.stderr), 5)
            ));
        }
        return Ok(());
    }
    /// # 把浅克隆的仓库变成深克隆
    fn unshallow(&self, target_dir: &CacheDir) -> Result<(), String> {
        if self.is_shallow(target_dir)? == false {
            return Ok(());
        }

        let mut cmd = Command::new("git");
        cmd.current_dir(&target_dir.path);
        cmd.arg("fetch").arg("--unshallow");

        cmd.arg("-f");

        // 创建子进程，执行命令
        let proc: std::process::Child = cmd
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let output = proc.wait_with_output().map_err(|e| e.to_string())?;

        if !output.status.success() {
            return Err(format!(
                "Failed to unshallow {}, message: {}",
                target_dir.path.display(),
                StdioUtils::tail_n_str(StdioUtils::stderr_to_lines(&output.stderr), 5)
            ));
        }
        return Ok(());
    }

    /// 判断当前仓库是否是浅克隆
    fn is_shallow(&self, target_dir: &CacheDir) -> Result<bool, String> {
        let mut cmd = Command::new("git");
        cmd.current_dir(&target_dir.path);
        cmd.arg("rev-parse").arg("--is-shallow-repository");

        let proc: std::process::Child = cmd
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let output = proc.wait_with_output().map_err(|e| e.to_string())?;

        if !output.status.success() {
            return Err(format!(
                "Failed to check if shallow {}, message: {}",
                target_dir.path.display(),
                StdioUtils::tail_n_str(StdioUtils::stderr_to_lines(&output.stderr), 5)
            ));
        }

        let is_shallow = String::from_utf8_lossy(&output.stdout).trim() == "true";
        return Ok(is_shallow);
    }

    fn fetch_all(&self, target_dir: &CacheDir) -> Result<(), String> {
        self.set_fetch_config(target_dir)?;
        let mut cmd = Command::new("git");
        cmd.current_dir(&target_dir.path);
        cmd.arg("fetch").arg("--all");

        // 安静模式
        cmd.arg("-f").arg("-q");

        // 创建子进程，执行命令
        let proc: std::process::Child = cmd
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let output = proc.wait_with_output().map_err(|e| e.to_string())?;

        if !output.status.success() {
            return Err(format!(
                "Failed to fetch all {}, message: {}",
                target_dir.path.display(),
                StdioUtils::tail_n_str(StdioUtils::stderr_to_lines(&output.stderr), 5)
            ));
        }

        return Ok(());
    }

    fn pull(&self, target_dir: &CacheDir) -> Result<(), String> {
        // 如果没有指定branch，则不执行pull
        if !self.branch.is_some() {
            return Ok(());
        }
        info!("git pulling: {}", target_dir.path.display());

        let mut cmd = Command::new("git");
        cmd.current_dir(&target_dir.path);
        cmd.arg("pull");

        // 安静模式
        cmd.arg("-f").arg("-q");

        // 创建子进程，执行命令
        let proc: std::process::Child = cmd
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let output = proc.wait_with_output().map_err(|e| e.to_string())?;

        // 如果pull失败，且指定了branch，则报错
        if !output.status.success() {
            return Err(format!(
                "Failed to pull {}, message: {}",
                target_dir.path.display(),
                StdioUtils::tail_n_str(StdioUtils::stderr_to_lines(&output.stderr), 5)
            ));
        }

        return Ok(());
    }
}

/// # 本地源
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalSource {
    /// 本地目录/文件的路径
    path: PathBuf,
}

impl LocalSource {
    #[allow(dead_code)]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn validate(&self, expect_file: Option<bool>) -> Result<()> {
        if !self.path.exists() {
            return Err(Error::msg(format!("path {:?} not exists", self.path)));
        }

        if let Some(expect_file) = expect_file {
            if expect_file && !self.path.is_file() {
                return Err(Error::msg(format!("path {:?} is not a file", self.path)));
            }

            if !expect_file && !self.path.is_dir() {
                return Err(Error::msg(format!(
                    "path {:?} is not a directory",
                    self.path
                )));
            }
        }

        return Ok(());
    }

    pub fn trim(&mut self) {}

    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

/// # 在线压缩包源
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArchiveSource {
    /// 压缩包的URL
    url: String,
    /// 把压缩包中的哪个目录作为根目录（可选）
    /// 默认是压缩包内的根目录
    #[serde(default)]
    rootdir: Option<String>,
}

impl ArchiveSource {
    #[allow(dead_code)]
    pub fn new(url: String, rootdir: Option<String>) -> Self {
        Self { url, rootdir }
    }

    pub fn validate(&self) -> Result<()> {
        if self.url.is_empty() {
            return Err(Error::msg("url is empty"));
        }

        // 判断是一个网址
        if let Ok(url) = Url::parse(&self.url) {
            if url.scheme() != "http" && url.scheme() != "https" {
                return Err(Error::msg(format!(
                    "url {:?} is not a http/https url",
                    self.url
                )));
            }
        } else {
            return Err(Error::msg(format!("url {:?} is not a valid url", self.url)));
        }

        if self.rootdir.is_some() && self.rootdir.as_ref().unwrap().starts_with('/') {
            return Err(Error::msg(format!(
                "archive rootdir {:?} starts with '/'",
                self.rootdir
            )));
        }
        return Ok(());
    }

    pub fn trim(&mut self) {
        self.url = self.url.trim().to_string();
    }

    /// @brief 下载压缩包并把其中的文件提取至target_dir目录下
    ///
    ///从URL中下载压缩包到临时文件夹 target_dir/DRAGONOS_ARCHIVE_TEMP 后
    ///原地解压，提取文件后删除下载的压缩包。如果 target_dir 非空，就直接使用
    ///其中内容，不进行重复下载和覆盖
    ///
    /// @param target_dir 文件缓存目录
    ///
    /// @return 根据结果返回OK或Err
    pub fn download_unzip(&self, target_dir: &CacheDir) -> Result<(), String> {
        let url = Url::parse(&self.url).unwrap();
        let archive_name = url.path_segments().unwrap().last().unwrap();
        let path = &(target_dir.path.join("DRAGONOS_ARCHIVE_TEMP"));
        //如果source目录没有临时文件夹，且不为空，说明之前成功执行过一次，那么就直接使用之前的缓存
        if !path.exists()
            && !target_dir.is_empty().map_err(|e| {
                format!(
                    "Failed to check if target dir is empty: {}, message: {e:?}",
                    target_dir.path.display()
                )
            })?
        {
            //如果source文件夹非空，就直接使用，不再重复下载压缩文件，这里可以考虑加入交互
            info!("Source files already exist. Using previous source file cache. You should clean {:?} before re-download the archive ", target_dir.path);
            return Ok(());
        }

        if path.exists() {
            std::fs::remove_dir_all(path).map_err(|e| e.to_string())?;
        }
        //创建临时目录
        std::fs::create_dir(path).map_err(|e| e.to_string())?;
        info!("downloading {:?}, url: {:?}", archive_name, self.url);
        FileUtils::download_file(&self.url, path).map_err(|e| e.to_string())?;
        //下载成功，开始尝试解压
        info!("download {:?} finished, start unzip", archive_name);
        let archive_file = ArchiveFile::new(&path.join(archive_name));
        archive_file.unzip(self.rootdir.as_ref())?;
        //删除创建的临时文件夹
        // std::fs::remove_dir_all(path).map_err(|e| e.to_string())?;
        return Ok(());
    }
}

pub struct ArchiveFile {
    /// archive file所在目录
    archive_path: PathBuf,
    /// 压缩文件名
    archive_name: String,
    archive_type: ArchiveType,
}

impl ArchiveFile {
    pub fn new(archive_path: &PathBuf) -> Self {
        info!("archive_path: {:?}", archive_path);
        //匹配压缩文件类型
        let archive_name = archive_path.file_name().unwrap().to_str().unwrap();
        for (regex, archivetype) in [
            (Regex::new(r"^(.+)\.tar\.gz$").unwrap(), ArchiveType::TarGz),
            (Regex::new(r"^(.+)\.tar\.xz$").unwrap(), ArchiveType::TarXz),
            (Regex::new(r"^(.+)\.zip$").unwrap(), ArchiveType::Zip),
        ] {
            if regex.is_match(archive_name) {
                return Self {
                    archive_path: archive_path.parent().unwrap().to_path_buf(),
                    archive_name: archive_name.to_string(),
                    archive_type: archivetype,
                };
            }
        }
        Self {
            archive_path: archive_path.parent().unwrap().to_path_buf(),
            archive_name: archive_name.to_string(),
            archive_type: ArchiveType::Undefined,
        }
    }

    fn do_unzip_tar_file(&self, in_archive_rootdir: Option<&String>) -> Result<(), String> {
        let mut cmd = Command::new("tar");
        cmd.arg("-xf").arg(&self.archive_name);

        // 处理in_archive_rootdir参数，只解压压缩文件内的指定目录
        if let Some(in_archive_rootdir) = in_archive_rootdir {
            let mut components = 0;
            in_archive_rootdir.split('/').for_each(|x| {
                if x != "" {
                    components += 1;
                }
            });

            cmd.arg(format!("--strip-components={}", components));
            cmd.arg(&in_archive_rootdir);
        }

        cmd.current_dir(&self.archive_path);

        log::debug!("unzip tar file: {:?}", cmd);

        let proc: std::process::Child = cmd
            .stderr(Stdio::piped())
            .stdout(Stdio::inherit())
            .spawn()
            .map_err(|e| e.to_string())?;
        let output = proc.wait_with_output().map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "unzip tar file failed, status: {:?},  stderr: {:?}",
                output.status,
                StdioUtils::tail_n_str(StdioUtils::stderr_to_lines(&output.stderr), 5)
            ));
        }
        Ok(())
    }

    fn do_unzip_zip_file(&self, in_archive_rootdir: Option<&String>) -> Result<(), String> {
        let file =
            File::open(&self.archive_path.join(&self.archive_name)).map_err(|e| e.to_string())?;
        let mut archive = ZipArchive::new(file).map_err(|e| e.to_string())?;
        for i in 0..archive.len() {
            let mut file = archive.by_index(i).map_err(|e| e.to_string())?;
            let file_name = file.name();

            // 处理in_archive_rootdir参数，只解压指定目录下的内容
            let outpath = if let Some(rootdir) = in_archive_rootdir {
                if !file_name.starts_with(rootdir) {
                    continue;
                }
                // 去除rootdir前缀，保留剩余路径
                let relative_path = file_name.strip_prefix(rootdir).unwrap();
                let relative_path = relative_path.trim_start_matches("/");
                self.archive_path.join(relative_path)
            } else {
                match file.enclosed_name() {
                    Some(path) => self.archive_path.join(path),
                    None => continue,
                }
            };
            if (*file.name()).ends_with('/') {
                std::fs::create_dir_all(&outpath).map_err(|e| e.to_string())?;
            } else {
                if let Some(p) = outpath.parent() {
                    if !p.exists() {
                        std::fs::create_dir_all(&p).map_err(|e| e.to_string())?;
                    }
                }
                let mut outfile = File::create(&outpath).map_err(|e| e.to_string())?;
                std::io::copy(&mut file, &mut outfile).map_err(|e| e.to_string())?;
            }
            //设置解压后权限，在Linux中Unzip会丢失权限
            #[cfg(unix)]
            {
                if let Some(mode) = file.unix_mode() {
                    std::fs::set_permissions(&outpath, std::fs::Permissions::from_mode(mode))
                        .map_err(|e| e.to_string())?;
                }
            }
        }
        Ok(())
    }

    /// @brief 对self.archive_path路径下名为self.archive_name的压缩文件(tar.gz或zip)进行解压缩
    ///
    /// 在此函数中进行路径和文件名有效性的判断，如果有效的话就开始解压缩，根据ArchiveType枚举类型来
    /// 生成不同的命令来对压缩文件进行解压缩，暂时只支持tar.gz和zip格式，并且都是通过调用bash来解压缩
    /// 没有引入第三方rust库
    ///
    ///
    /// @return 根据结果返回OK或Err
    pub fn unzip(&self, in_archive_rootdir: Option<&String>) -> Result<(), String> {
        let path = &self.archive_path;
        if !path.is_dir() {
            return Err(format!("Archive directory {:?} is wrong", path));
        }
        if !path.join(&self.archive_name).is_file() {
            return Err(format!(
                " {:?} is not a file",
                path.join(&self.archive_name)
            ));
        }
        //根据压缩文件的类型生成cmd指令
        match &self.archive_type {
            ArchiveType::TarGz | ArchiveType::TarXz => {
                self.do_unzip_tar_file(in_archive_rootdir)?;
            }

            ArchiveType::Zip => {
                self.do_unzip_zip_file(in_archive_rootdir)?;
            }
            _ => {
                return Err("unsupported archive type".to_string());
            }
        }
        //删除下载的压缩包
        info!("unzip successfully, removing archive ");
        std::fs::remove_file(path.join(&self.archive_name)).map_err(|e| e.to_string())?;
        //从解压的文件夹中提取出文件并删除下载的压缩包等价于指令"cd *;mv ./* ../../"
        // for entry in path.read_dir().map_err(|e| e.to_string())? {
        //     let entry = entry.map_err(|e| e.to_string())?;
        //     let path = entry.path();
        //     FileUtils::move_files(&path, &self.archive_path.parent().unwrap())
        //         .map_err(|e| e.to_string())?;
        //     //删除空的单独文件夹
        //     std::fs::remove_dir_all(&path).map_err(|e| e.to_string())?;
        // }
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "mv {p}/* {parent} && rm -rf {p}",
                p = &self.archive_path.to_string_lossy(),
                parent = self.archive_path.parent().unwrap().to_string_lossy()
            ))
            .output()
            .map_err(|e| e.to_string())?;
        return Ok(());
    }
}

pub enum ArchiveType {
    TarGz,
    TarXz,
    Zip,
    Undefined,
}
