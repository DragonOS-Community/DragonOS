use core::str;
use std::{path::PathBuf, process::Command, thread::sleep, time::Duration};

use anyhow::{anyhow, Result};
use regex::Regex;

use crate::utils::abs_path;

const LOOP_DEVICE_LOSETUP_A_REGEX: &str = r"^/dev/loop(\d+)";

pub struct LoopDevice {
    img_path: Option<PathBuf>,
    loop_device_path: Option<String>,
    /// 尝试在drop时自动detach
    try_detach_when_drop: bool,
}
impl LoopDevice {
    pub fn attached(&self) -> bool {
        self.loop_device_path.is_some()
    }

    pub fn dev_path(&self) -> Option<&String> {
        self.loop_device_path.as_ref()
    }

    pub fn attach(&mut self) -> Result<()> {
        if self.attached() {
            return Ok(());
        }
        if self.img_path.is_none() {
            return Err(anyhow!("Image path not set"));
        }

        let output = Command::new("losetup")
            .arg("-f")
            .arg("--show")
            .arg("-P")
            .arg(self.img_path.as_ref().unwrap())
            .output()?;

        if output.status.success() {
            let loop_device = String::from_utf8(output.stdout)?.trim().to_string();
            self.loop_device_path = Some(loop_device);
            sleep(Duration::from_millis(100));
            log::trace!(
                "Loop device attached: {}",
                self.loop_device_path.as_ref().unwrap()
            );
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Failed to mount disk image: losetup command exited with status {}",
                output.status
            ))
        }
    }

    /// 尝试连接已经存在的loop device
    pub fn attach_by_exists(&mut self) -> Result<()> {
        if self.attached() {
            return Ok(());
        }
        if self.img_path.is_none() {
            return Err(anyhow!("Image path not set"));
        }
        log::trace!(
            "Try to attach loop device by exists: image path: {}",
            self.img_path.as_ref().unwrap().display()
        );
        // losetup -a 查看是否有已经attach了的，如果有，就附着上去
        let cmd = Command::new("losetup")
            .arg("-a")
            .output()
            .map_err(|e| anyhow!("Failed to run losetup -a: {}", e))?;
        let output = String::from_utf8(cmd.stdout)?;
        let s = __loop_device_path_by_disk_image_path(
            self.img_path.as_ref().unwrap().to_str().unwrap(),
            &output,
        )
        .map_err(|e| anyhow!("Failed to find loop device: {}", e))?;
        self.loop_device_path = Some(s);
        Ok(())
    }

    /// 获取指定分区的路径
    ///
    /// # 参数
    ///
    /// * `nth` - 分区的编号
    ///
    /// # 返回值
    ///
    /// 返回一个 `Result<String>`，包含分区路径的字符串。如果循环设备未附加，则返回错误。
    ///
    /// # 错误
    ///
    /// 如果循环设备未附加，则返回 `anyhow!("Loop device not attached")` 错误。
    pub fn partition_path(&self, nth: u8) -> Result<PathBuf> {
        if !self.attached() {
            return Err(anyhow!("Loop device not attached"));
        }
        let s = format!("{}p{}", self.loop_device_path.as_ref().unwrap(), nth);
        let s = PathBuf::from(s);
        // 判断路径是否存在
        if !s.exists() {
            return Err(anyhow!("Partition not exist"));
        }
        Ok(s)
    }

    pub fn detach(&mut self) -> Result<()> {
        if self.loop_device_path.is_none() {
            return Ok(());
        }
        let loop_device = self.loop_device_path.take().unwrap();
        let p = PathBuf::from(&loop_device);
        log::trace!(
            "Detach loop device: {}, exists: {}",
            p.display(),
            p.exists()
        );
        let output = Command::new("losetup")
            .arg("-d")
            .arg(loop_device)
            .output()?;

        if output.status.success() {
            self.loop_device_path = None;
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Failed to detach loop device: {}, {}",
                output.status,
                str::from_utf8(output.stderr.as_slice()).unwrap_or("<Unknown>")
            ))
        }
    }

    pub fn try_detach_when_drop(&self) -> bool {
        self.try_detach_when_drop
    }

    #[allow(dead_code)]
    pub fn set_try_detach_when_drop(&mut self, try_detach_when_drop: bool) {
        self.try_detach_when_drop = try_detach_when_drop;
    }
}

impl Drop for LoopDevice {
    fn drop(&mut self) {
        if self.try_detach_when_drop() {
            if let Err(e) = self.detach() {
                log::warn!("Failed to detach loop device: {}", e);
            }
        }
    }
}

pub struct LoopDeviceBuilder {
    img_path: Option<PathBuf>,
    loop_device_path: Option<String>,
    try_detach_when_drop: bool,
}

impl LoopDeviceBuilder {
    pub fn new() -> Self {
        LoopDeviceBuilder {
            img_path: None,
            loop_device_path: None,
            try_detach_when_drop: true,
        }
    }

    pub fn img_path(mut self, img_path: PathBuf) -> Self {
        self.img_path = Some(abs_path(&img_path));
        self
    }

    #[allow(dead_code)]
    pub fn try_detach_when_drop(mut self, try_detach_when_drop: bool) -> Self {
        self.try_detach_when_drop = try_detach_when_drop;
        self
    }

    pub fn build(self) -> Result<LoopDevice> {
        let loop_dev = LoopDevice {
            img_path: self.img_path,
            loop_device_path: self.loop_device_path,
            try_detach_when_drop: self.try_detach_when_drop,
        };

        Ok(loop_dev)
    }
}

fn __loop_device_path_by_disk_image_path(
    disk_img_path: &str,
    losetup_a_output: &str,
) -> Result<String> {
    let re = Regex::new(LOOP_DEVICE_LOSETUP_A_REGEX)?;
    for line in losetup_a_output.lines() {
        if !line.contains(disk_img_path) {
            continue;
        }
        let caps = re.captures(line);
        if caps.is_none() {
            continue;
        }
        let caps = caps.unwrap();
        let loop_device = caps.get(1).unwrap().as_str();
        let loop_device = format!("/dev/loop{}", loop_device);
        return Ok(loop_device);
    }
    Err(anyhow!("Loop device not found"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_regex_find_loop_device() {
        const DEVICE_NAME_SHOULD_MATCH: [&str; 3] =
            ["/dev/loop11", "/dev/loop11p1", "/dev/loop11p1 "];
        let device_name = "/dev/loop11";
        let re = Regex::new(LOOP_DEVICE_LOSETUP_A_REGEX).unwrap();
        for name in DEVICE_NAME_SHOULD_MATCH {
            assert!(re.find(name).is_some(), "{} should match", name);
            assert_eq!(
                re.find(name).unwrap().as_str(),
                device_name,
                "{} should match {}",
                name,
                device_name
            );
        }
    }

    #[test]
    fn test_parse_losetup_a_output() {
        let losetup_a_output = r#"/dev/loop1: []: (/data/bin/disk-image-x86_64.img)
/dev/loop29: []: (/var/lib/abc.img)
/dev/loop13: []: (/var/lib/snapd/snaps/gtk-common-themes_1535.snap
/dev/loop19: []: (/var/lib/snapd/snaps/gnome-42-2204_172.snap)"#;
        let disk_img_path = "/data/bin/disk-image-x86_64.img";
        let loop_device_path =
            __loop_device_path_by_disk_image_path(disk_img_path, losetup_a_output).unwrap();
        assert_eq!(loop_device_path, "/dev/loop1");
    }

    #[test]
    fn test_parse_lsblk_output_not_match() {
        let losetup_a_output = r#"/dev/loop1: []: (/data/bin/disk-image-x86_64.img)
/dev/loop29: []: (/var/lib/abc.img)
/dev/loop13: []: (/var/lib/snapd/snaps/gtk-common-themes_1535.snap
/dev/loop19: []: (/var/lib/snapd/snaps/gnome-42-2204_172.snap)"#;
        let disk_img_path = "/data/bin/disk-image-riscv64.img";
        let loop_device_path =
            __loop_device_path_by_disk_image_path(disk_img_path, losetup_a_output);
        assert!(
            loop_device_path.is_err(),
            "should not match any loop device"
        );
    }
}
