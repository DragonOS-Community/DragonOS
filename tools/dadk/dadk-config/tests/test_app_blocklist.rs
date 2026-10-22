//! 测试应用程序黑名单功能

use dadk_config::app_blocklist::AppBlocklistConfigFile;
use std::path::PathBuf;

#[test]
fn test_app_blocklist_exact_match() {
    let config_content = r#"
        [[blocked_apps]]
        name = "app1"
        reason = "Test app 1"

        [[blocked_apps]]
        name = "app2"

        [[blocked_apps]]
        name = "app3"
        reason = "Test app 3"

        strict = true
        log_skipped = true
    "#;

    let config = AppBlocklistConfigFile::load_from_str(config_content).unwrap();

    assert!(config.is_blocked("app1", None));
    assert!(config.is_blocked("app2", None));
    assert!(!config.is_blocked("app4", None));
    assert_eq!(config.blocked_count(), 3);

    // Test reasons
    assert_eq!(
        config.get_blocked_reason("app1", None),
        Some(&"Test app 1".to_string())
    );
    assert_eq!(config.get_blocked_reason("app2", None), None);
    assert_eq!(
        config.get_blocked_reason("app3", None),
        Some(&"Test app 3".to_string())
    );
}

#[test]
fn test_app_blocklist_version_match() {
    let config_content = r#"
        [[blocked_apps]]
        name = "openssl@1.1.1"
        reason = "Vulnerable version"

        [[blocked_apps]]
        name = "nginx@1.20.0"

        [[blocked_apps]]
        name = "libfoo@2.*"
        reason = "Unsupported major version"

        strict = true
        log_skipped = true
    "#;

    let config = AppBlocklistConfigFile::load_from_str(config_content).unwrap();

    assert!(config.is_blocked("openssl", Some("1.1.1")));
    assert!(!config.is_blocked("openssl", Some("3.0.0")));
    assert!(config.is_blocked("nginx", Some("1.20.0")));
    assert!(!config.is_blocked("nginx", Some("1.21.0")));
    assert!(config.is_blocked("libfoo", Some("2.5.0")));
    assert!(!config.is_blocked("libfoo", Some("3.0.0")));
    assert_eq!(config.blocked_count(), 3);
}

#[test]
fn test_app_blocklist_pattern_match() {
    let config_content = r#"
        [[blocked_apps]]
        name = "test-*"
        reason = "Test applications"

        [[blocked_apps]]
        name = "deprecated-*"
        reason = "Deprecated applications"

        [[blocked_apps]]
        name = "nginx-*"

        strict = true
        log_skipped = true
    "#;

    let config = AppBlocklistConfigFile::load_from_str(config_content).unwrap();

    assert!(config.is_blocked("test-app", None));
    assert!(config.is_blocked("test-utils", None));
    assert!(config.is_blocked("deprecated-old", None));
    assert!(config.is_blocked("nginx-main", None));
    assert!(!config.is_blocked("new-app", None));
    assert_eq!(config.blocked_count(), 3);
}

#[test]
fn test_app_blocklist_name_and_version_patterns() {
    let config_content = r#"
        [[blocked_apps]]
        name = "test-*@1.*"
        reason = "Test apps v1"

        [[blocked_apps]]
        name = "deprecated-*"

        [[blocked_apps]]
        name = "nginx@*"
        reason = "All nginx versions"

        [[blocked_apps]]
        name = "lib*@2.*"

        strict = true
        log_skipped = true
    "#;

    let config = AppBlocklistConfigFile::load_from_str(config_content).unwrap();

    // Versioned pattern matching
    assert!(config.is_blocked("test-app", Some("1.5.0")));
    assert!(!config.is_blocked("test-app", Some("2.0.0")));
    assert!(config.is_blocked("deprecated-tool", None));
    assert!(config.is_blocked("nginx", Some("1.20.0")));
    assert!(config.is_blocked("nginx", Some("2.0.0")));
    assert!(config.is_blocked("libfoo", Some("2.1.0")));
    assert!(!config.is_blocked("libfoo", Some("3.0.0")));
    assert_eq!(config.blocked_count(), 4);
}

#[test]
fn test_app_blocklist_empty() {
    let config_content = r#"
        strict = true
        log_skipped = true
    "#;

    let config = AppBlocklistConfigFile::load_from_str(config_content).unwrap();

    assert!(!config.is_blocked("any-app", None));
    assert!(!config.is_blocked("any-app", Some("1.0.0")));
    assert_eq!(config.blocked_count(), 0);
}

#[test]
fn test_app_blocklist_file_not_found() {
    let path = PathBuf::from("/nonexistent/path/app-blocklist.toml");
    let config = AppBlocklistConfigFile::load(&path).unwrap();

    assert!(!config.is_blocked("any-app", None));
    assert_eq!(config.blocked_count(), 0);
    assert!(config.strict); // Default value
    assert!(config.log_skipped); // Default value
}

#[test]
fn test_app_blocklist_invalid_toml() {
    let config_content = r#"
        This is not valid TOML
        blocked_apps = ["app1"]
    "#;

    let result = AppBlocklistConfigFile::load_from_str(config_content);
    assert!(result.is_err());
}

#[test]
fn test_app_blocklist_non_strict_mode() {
    let config_content = r#"strict = false
log_skipped = true

[[blocked_apps]]
name = "app1"
reason = "Should be skipped"
"#;

    let config = AppBlocklistConfigFile::load_from_str(config_content).unwrap();

    assert!(config.is_blocked("app1", None)); // Still detected as blocked
    assert!(!config.strict); // Strict mode is off
    assert!(config.log_skipped);
    assert_eq!(config.blocked_count(), 1);
}
