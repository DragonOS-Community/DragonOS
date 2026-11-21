use std::path::PathBuf;

#[cfg(test)]
mod tests {
    use super::*;
    use dadk_config::app_blocklist::AppBlocklistConfigFile;
    use std::fs;

    #[test]
    fn test_template_config_file() {
        let template_path = PathBuf::from("templates/config/app-blocklist.toml");

        // 检查文件是否存在
        assert!(template_path.exists(), "Template file should exist");

        // 读取文件内容
        let content =
            fs::read_to_string(&template_path).expect("Should be able to read template file");

        // 尝试解析配置
        let config =
            AppBlocklistConfigFile::load_from_str(&content).expect("Template should be valid TOML");

        // 验证默认配置
        assert!(config.strict, "Default strict mode should be true");
        assert!(config.log_skipped, "Default log_skipped should be true");

        // 验证有被屏蔽的应用程序
        assert!(
            config.blocked_count() > 0,
            "Template should contain example blocked apps"
        );

        println!("✅ Template configuration is valid!");
        println!("📊 Blocked apps count: {}", config.blocked_count());

        // 测试一些匹配案例
        let test_cases = [
            ("busybox", None, true),
            ("test-app", None, true),
            ("test-example", None, true), // 应该匹配 "test-*" 模式
            ("openssl", Some("1.1.1"), true),
            ("openssl", Some("3.0.0"), false),
            ("nginx", Some("1.20.0"), true), // 应该匹配 "nginx@*" 模式
            ("old-app", None, true),         // 应该匹配 "old-*" 模式
            ("app-debug", None, true),       // 应该匹配 "*-debug" 模式
            ("libfoo", Some("2.5.0"), true), // 应该匹配 "lib*@2.*" 模式
            ("libfoo", Some("3.0.0"), false),
            ("random-app", None, false),
        ];

        for (name, version, expected_blocked) in test_cases {
            let blocked = config.is_blocked(name, version);
            let version_str = version.map(|v| format!("@{}", v)).unwrap_or_default();
            let status = if blocked { "BLOCKED" } else { "ALLOWED" };
            println!("  - {}{}: {}", name, version_str, status);

            if expected_blocked {
                assert!(blocked, "Expected {} to be blocked", name);
            } else {
                assert!(!blocked, "Expected {} to be allowed", name);
            }
        }
    }
}
