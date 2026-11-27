//! 任务日志
//!
//! DADK在执行任务时，会把一些日志记录到任务的文件夹下。

use chrono::{DateTime, Utc};
use log::warn;
use serde::{Deserialize, Deserializer, Serialize};

/// 任务日志（输出到任务构建日志目录下的）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskLog {
    /// 任务执行完成时间
    #[serde(
        default,
        deserialize_with = "ok_or_default",
        skip_serializing_if = "Option::is_none"
    )]
    build_timestamp: Option<DateTime<Utc>>,
    install_timestamp: Option<DateTime<Utc>>,
    /// 任务构建状态
    build_status: Option<BuildStatus>,
    /// 任务安装状态
    install_status: Option<InstallStatus>,
    /// dadk配置文件的时间戳
    dadk_config_timestamp: Option<DateTime<Utc>>,
}

fn ok_or_default<'a, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: Deserialize<'a> + Default,
    D: Deserializer<'a>,
{
    let r = Option::deserialize(deserializer).map(|x: Option<T>| x.unwrap_or_default());

    Ok(r.unwrap_or_default())
}

impl TaskLog {
    pub fn new() -> Self {
        Self {
            build_timestamp: None,
            build_status: None,
            install_timestamp: None,
            install_status: None,
            dadk_config_timestamp: None,
        }
    }

    pub fn dadk_config_timestamp(&self) -> Option<&DateTime<Utc>> {
        self.dadk_config_timestamp.as_ref()
    }

    pub fn set_dadk_config_timestamp(&mut self, time: DateTime<Utc>) {
        self.dadk_config_timestamp = Some(time);
    }

    #[allow(dead_code)]
    pub fn set_build_time(&mut self, time: DateTime<Utc>) {
        self.build_timestamp = Some(time);
    }

    pub fn build_time(&self) -> Option<&DateTime<Utc>> {
        self.build_timestamp.as_ref()
    }

    pub fn set_build_time_now(&mut self) {
        self.build_timestamp = Some(Utc::now());
    }

    pub fn install_time(&self) -> Option<&DateTime<Utc>> {
        self.install_timestamp.as_ref()
    }

    pub fn set_install_time_now(&mut self) {
        self.install_timestamp = Some(Utc::now());
    }

    pub fn set_build_status(&mut self, status: BuildStatus) {
        self.build_status = Some(status);
    }

    pub fn clean_build_status(&mut self) {
        self.build_status = None;
    }

    pub fn build_status(&self) -> Option<&BuildStatus> {
        self.build_status.as_ref()
    }

    pub fn install_status(&self) -> Option<&InstallStatus> {
        self.install_status.as_ref()
    }

    pub fn set_install_status(&mut self, status: InstallStatus) {
        self.install_status = Some(status);
    }

    pub fn clean_install_status(&mut self) {
        self.install_status = None;
    }
}

/// 任务构建状态
#[derive(Debug, Clone, Serialize, PartialEq)]
pub enum BuildStatus {
    #[serde(rename = "success")]
    Success,
    #[serde(rename = "failed")]
    Failed,
}

impl<'de> Deserialize<'de> for BuildStatus {
    fn deserialize<D>(deserializer: D) -> Result<BuildStatus, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?.to_ascii_lowercase();
        match s.as_str() {
            "success" => Ok(BuildStatus::Success),
            "failed" => Ok(BuildStatus::Failed),
            _ => {
                warn!("invalid build status: {}", s);
                Ok(BuildStatus::Failed)
            }
        }
    }
}

/// 任务安装状态
#[derive(Debug, Clone, Serialize, PartialEq)]
pub enum InstallStatus {
    #[serde(rename = "success")]
    Success,
    #[serde(rename = "failed")]
    Failed,
}

impl<'de> Deserialize<'de> for InstallStatus {
    fn deserialize<D>(deserializer: D) -> Result<InstallStatus, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?.to_ascii_lowercase();
        match s.as_str() {
            "success" => Ok(InstallStatus::Success),
            "failed" => Ok(InstallStatus::Failed),
            _ => {
                warn!("invalid install status: {}", s);
                Ok(InstallStatus::Failed)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TaskLog;

    #[test]
    fn should_deserialize_legacy_task_log_without_build_timestamp() {
        let content = "dadk_config_timestamp = \"2026-02-09T19:42:17.464516620Z\"\n";
        let task_log: TaskLog = toml::from_str(content).expect("legacy task_log must be readable");
        assert!(task_log.build_time().is_none());
    }
}
