use anyhow::Result;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::PathBuf;

/// 被屏蔽的应用程序信息
#[derive(Debug, Clone, Deserialize)]
pub struct BlockedApp {
    /// 应用名称或模式
    pub name: String,
    /// 屏蔽原因（可选）
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppBlocklistConfigFile {
    /// 被屏蔽的应用程序列表，每个应用可独立设置reason
    #[serde(default)]
    pub blocked_apps: Vec<BlockedApp>,

    /// 是否启用严格模式
    #[serde(default = "default_strict_mode")]
    pub strict: bool,

    /// 是否记录被跳过的应用
    #[serde(default = "default_log_skipped")]
    pub log_skipped: bool,

    #[serde(skip)]
    app_patterns: HashSet<String>,
}

impl AppBlocklistConfigFile {
    /// 从文件加载应用黑名单配置
    ///
    /// # Arguments
    /// * `path` - 配置文件路径
    ///
    /// # Returns
    /// * `Result<Self>` - 解析后的配置或错误
    ///
    /// # Notes
    /// 如果文件不存在，返回一个空的默认配置
    pub fn load(path: &PathBuf) -> Result<Self> {
        if !path.exists() {
            // 文件不存在时返回空配置
            return Ok(Self {
                blocked_apps: Vec::new(),
                strict: true,
                log_skipped: true,
                app_patterns: HashSet::new(),
            });
        }

        let content = std::fs::read_to_string(path)?;
        let mut config = Self::load_from_str(&content)?;

        // 预处理模式匹配
        config.app_patterns = config
            .blocked_apps
            .iter()
            .map(|app| app.name.clone())
            .collect();

        Ok(config)
    }

    /// 从字符串内容加载应用黑名单配置
    ///
    /// # Arguments
    /// * `content` - TOML 格式的配置内容
    ///
    /// # Returns
    /// * `Result<Self>` - 解析后的配置或错误
    pub fn load_from_str(content: &str) -> Result<Self> {
        let config: AppBlocklistConfigFile = toml::from_str(content)?;
        Ok(config)
    }

    /// 检查应用是否被屏蔽
    ///
    /// # Arguments
    /// * `app_name` - 应用名称
    /// * `version` - 可选的版本号
    ///
    /// # Returns
    /// * `true` - 如果应用被屏蔽
    /// * `false` - 如果应用未被屏蔽
    pub fn is_blocked(&self, app_name: &str, version: Option<&str>) -> bool {
        // 1. 精确匹配（无版本）
        if self.blocked_apps.iter().any(|app| app.name == app_name) {
            return true;
        }

        // 2. 带版本的精确匹配
        if let Some(version) = version {
            let versioned_name = format!("{}@{}", app_name, version);
            if self
                .blocked_apps
                .iter()
                .any(|app| app.name == versioned_name)
            {
                return true;
            }
        }

        // 3. 模式匹配（支持通配符）
        for app in &self.blocked_apps {
            if self.match_pattern(app_name, version, &app.name) {
                return true;
            }
        }

        false
    }

    /// 模式匹配（支持通配符和版本匹配）
    ///
    /// # Arguments
    /// * `name` - 应用名称
    /// * `version` - 可选的版本号
    /// * `pattern` - 匹配模式，支持以下格式：
    ///   - 精确名称：`app1`
    ///   - 带版本：`app1@1.0.0`
    ///   - 通配符名称：`test-*`, `test-?`
    ///   - 通配符版本：`app1@1.*`, `app1@1.?.0`
    ///
    /// # Returns
    /// * `true` - 如果应用名称和版本匹配模式
    /// * `false` - 如果不匹配
    fn match_pattern(&self, name: &str, version: Option<&str>, pattern: &str) -> bool {
        // 检查是否包含 @ 符号
        if let Some(at_pos) = pattern.find('@') {
            // 分离名称模式和版本模式
            let name_pattern = &pattern[..at_pos];
            let version_pattern = &pattern[at_pos + 1..];

            // 检查名称是否匹配
            if name_pattern.contains('*') || name_pattern.contains('?') {
                let regex_name_pattern = name_pattern
                    .replace('.', "\\.")
                    .replace('*', ".*")
                    .replace('?', ".");

                if let Ok(re) = regex::Regex::new(&format!("^{}$", regex_name_pattern)) {
                    if !re.is_match(name) {
                        return false;
                    }
                }
            } else if name_pattern != name {
                return false;
            }

            // 检查版本是否匹配
            if let Some(version) = version {
                if version_pattern.contains('*') || version_pattern.contains('?') {
                    let regex_version_pattern = version_pattern
                        .replace('.', "\\.")
                        .replace('*', ".*")
                        .replace('?', ".");

                    if let Ok(re) = regex::Regex::new(&format!("^{}$", regex_version_pattern)) {
                        return re.is_match(version);
                    }
                } else {
                    return version_pattern == version;
                }
            }

            false
        } else {
            // 没有 @ 符号，只匹配名称
            if pattern.contains('*') || pattern.contains('?') {
                let regex_pattern = pattern
                    .replace('.', "\\.")
                    .replace('*', ".*")
                    .replace('?', ".");

                if let Ok(re) = regex::Regex::new(&format!("^{}$", regex_pattern)) {
                    return re.is_match(name);
                }
            }
            false
        }
    }

    /// 获取被屏蔽的应用数量
    ///
    /// # Returns
    /// * `usize` - 黑名单中的应用数量
    pub fn blocked_count(&self) -> usize {
        self.blocked_apps.len()
    }

    /// 获取应用的屏蔽原因
    ///
    /// # Arguments
    /// * `app_name` - 应用名称
    /// * `version` - 可选的版本号
    ///
    /// # Returns
    /// * `Option<&String>` - 如果应用被屏蔽则返回原因，否则返回None
    pub fn get_blocked_reason(&self, app_name: &str, version: Option<&str>) -> Option<&String> {
        // 1. 精确匹配（无版本）
        if let Some(app) = self.blocked_apps.iter().find(|app| app.name == app_name) {
            return app.reason.as_ref();
        }

        // 2. 带版本的精确匹配
        if let Some(version) = version {
            let versioned_name = format!("{}@{}", app_name, version);
            if let Some(app) = self
                .blocked_apps
                .iter()
                .find(|app| app.name == versioned_name)
            {
                return app.reason.as_ref();
            }
        }

        // 3. 模式匹配（支持通配符）
        for app in &self.blocked_apps {
            if self.match_pattern(app_name, version, &app.name) {
                return app.reason.as_ref();
            }
        }

        None
    }

    /// 获取所有带reason的屏蔽应用
    ///
    /// # Returns
    /// * `Vec<(&String, &String)>` - 返回(name, reason)对的向量
    pub fn blocked_apps_with_reason(&self) -> Vec<(&String, &String)> {
        self.blocked_apps
            .iter()
            .filter(|app| app.reason.is_some())
            .map(|app| (&app.name, app.reason.as_ref().unwrap()))
            .collect()
    }

    /// 获取所有被屏蔽的应用名称
    ///
    /// # Returns
    /// * `Vec<&String>` - 所有被屏蔽应用的名称列表
    pub fn blocked_app_names(&self) -> Vec<&String> {
        self.blocked_apps.iter().map(|app| &app.name).collect()
    }
}

fn default_strict_mode() -> bool {
    true
}

fn default_log_skipped() -> bool {
    true
}

impl Default for AppBlocklistConfigFile {
    fn default() -> Self {
        Self {
            blocked_apps: Vec::new(),
            strict: true,
            log_skipped: true,
            app_patterns: HashSet::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact_match() {
        let config = AppBlocklistConfigFile {
            blocked_apps: vec![
                BlockedApp {
                    name: "app1".to_string(),
                    reason: Some("Test reason 1".to_string()),
                },
                BlockedApp {
                    name: "app2".to_string(),
                    reason: None,
                },
            ],
            strict: true,
            log_skipped: true,
            app_patterns: HashSet::new(),
        };

        assert!(config.is_blocked("app1", None));
        assert!(config.is_blocked("app2", None));
        assert!(!config.is_blocked("app3", None));

        // Test getting reasons
        assert_eq!(
            config.get_blocked_reason("app1", None),
            Some(&"Test reason 1".to_string())
        );
        assert_eq!(config.get_blocked_reason("app2", None), None);
        assert_eq!(config.get_blocked_reason("app3", None), None);
    }

    #[test]
    fn test_pattern_match() {
        let config = AppBlocklistConfigFile {
            blocked_apps: vec![
                BlockedApp {
                    name: "test-*".to_string(),
                    reason: Some("Test applications".to_string()),
                },
                BlockedApp {
                    name: "deprecated-*".to_string(),
                    reason: Some("Deprecated applications".to_string()),
                },
            ],
            strict: true,
            log_skipped: true,
            app_patterns: HashSet::new(),
        };

        assert!(config.is_blocked("test-app", None));
        assert!(config.is_blocked("test-utils", None));
        assert!(config.is_blocked("deprecated-old", None));
        assert!(!config.is_blocked("new-app", None));

        // Test pattern matching returns reason
        assert_eq!(
            config.get_blocked_reason("test-app", None),
            Some(&"Test applications".to_string())
        );
        assert_eq!(
            config.get_blocked_reason("deprecated-old", None),
            Some(&"Deprecated applications".to_string())
        );
    }

    #[test]
    fn test_empty_blocklist() {
        let config = AppBlocklistConfigFile {
            blocked_apps: vec![],
            strict: true,
            log_skipped: true,
            app_patterns: HashSet::new(),
        };

        assert!(!config.is_blocked("any-app", None));
        assert_eq!(config.blocked_count(), 0);
    }

    #[test]
    fn test_versioned_match() {
        let config = AppBlocklistConfigFile {
            blocked_apps: vec![
                BlockedApp {
                    name: "openssl@1.1.1".to_string(),
                    reason: Some("Vulnerable version".to_string()),
                },
                BlockedApp {
                    name: "nginx".to_string(),
                    reason: None,
                },
            ],
            strict: true,
            log_skipped: true,
            app_patterns: HashSet::new(),
        };

        assert!(config.is_blocked("openssl", Some("1.1.1")));
        assert!(!config.is_blocked("openssl", Some("3.0.0")));
        assert!(config.is_blocked("nginx", None));

        assert_eq!(
            config.get_blocked_reason("openssl", Some("1.1.1")),
            Some(&"Vulnerable version".to_string())
        );
        assert_eq!(config.get_blocked_reason("nginx", None), None);
    }

    #[test]
    fn test_blocked_apps_with_reason() {
        let config = AppBlocklistConfigFile {
            blocked_apps: vec![
                BlockedApp {
                    name: "app1".to_string(),
                    reason: Some("Reason 1".to_string()),
                },
                BlockedApp {
                    name: "app2".to_string(),
                    reason: None,
                },
                BlockedApp {
                    name: "app3".to_string(),
                    reason: Some("Reason 3".to_string()),
                },
            ],
            strict: true,
            log_skipped: true,
            app_patterns: HashSet::new(),
        };

        let with_reason = config.blocked_apps_with_reason();
        assert_eq!(with_reason.len(), 2);
        assert!(with_reason.contains(&(&"app1".to_string(), &"Reason 1".to_string())));
        assert!(with_reason.contains(&(&"app3".to_string(), &"Reason 3".to_string())));
    }

    #[test]
    fn test_blocked_app_names() {
        let config = AppBlocklistConfigFile {
            blocked_apps: vec![
                BlockedApp {
                    name: "app1".to_string(),
                    reason: Some("Reason 1".to_string()),
                },
                BlockedApp {
                    name: "app2".to_string(),
                    reason: None,
                },
            ],
            strict: true,
            log_skipped: true,
            app_patterns: HashSet::new(),
        };

        let names = config.blocked_app_names();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&&"app1".to_string()));
        assert!(names.contains(&&"app2".to_string()));
    }
}
