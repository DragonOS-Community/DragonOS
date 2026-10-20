use rootfs::CreateCommandParam;
use user::UserCleanLevel;

use super::*;

#[test]
fn test_command_line_args_default() {
    let args = CommandLineArgs::parse_from(&["dadk", "kernel"]);
    assert_eq!(args.action, Action::Kernel);
    assert_eq!(args.manifest_path, "dadk-manifest.toml");
}

#[test]
fn test_command_line_args_with_manifest() {
    // test short
    let args = CommandLineArgs::parse_from(&["dadk", "-f", "custom-manifest.toml", "kernel"]);
    assert_eq!(args.action, Action::Kernel);
    assert_eq!(args.manifest_path, "custom-manifest.toml");
    // test long
    let args =
        CommandLineArgs::parse_from(&["dadk", "--manifest", "custom-manifest.toml", "kernel"]);
    assert_eq!(args.action, Action::Kernel);
    assert_eq!(args.manifest_path, "custom-manifest.toml");
}

#[test]
fn test_command_line_args_rootfs_subcommand() {
    let args = CommandLineArgs::parse_from(&["dadk", "rootfs", "create"]);
    assert!(matches!(
        args.action,
        Action::Rootfs(RootFSCommand::Create(CreateCommandParam {
            skip_if_exists: false
        }))
    ));

    let args = CommandLineArgs::parse_from(&["dadk", "rootfs", "create", "--skip-if-exists"]);
    assert!(matches!(
        args.action,
        Action::Rootfs(RootFSCommand::Create(CreateCommandParam {
            skip_if_exists: true
        }))
    ));
}

#[test]
fn test_show_mountpoint() {
    let args = CommandLineArgs::parse_from(&["dadk", "rootfs", "show-mountpoint"]);
    assert!(matches!(
        args.action,
        Action::Rootfs(RootFSCommand::ShowMountPoint)
    ));
}

#[test]
fn test_command_line_args_user() {
    let args = CommandLineArgs::parse_from(&["dadk", "user", "build"]);

    assert!(matches!(args.action, Action::User(UserCommand::Build)));
}

/// 该函数测试CommandLineArgs解析器是否正确解析`dadk user clean`命令
#[test]
fn test_command_line_args_user_clean() {
    let args = CommandLineArgs::parse_from(&["dadk", "user", "clean"]);
    assert!(matches!(args.action, Action::User(UserCommand::Clean(_))));
    if let Action::User(UserCommand::Clean(args)) = args.action {
        assert_eq!(args.level, UserCleanLevel::All);
    } else {
        panic!("Expected UserCommand::Clean");
    }

    // 检查 `--level` 参数
    let args = CommandLineArgs::parse_from(&["dadk", "user", "clean", "--level", "in-src"]);
    if let Action::User(UserCommand::Clean(args)) = args.action {
        assert_eq!(args.level, UserCleanLevel::InSrc);
    } else {
        panic!("Expected UserCommand::Clean");
    }

    // 检查 `--task` 参数
    let args = CommandLineArgs::parse_from(&["dadk", "user", "clean", "--task", "a-0.1.0"]);
    if let Action::User(UserCommand::Clean(args)) = args.action {
        assert_eq!(args.task, Some("a-0.1.0".to_string()));
    } else {
        panic!("Expected UserCommand::Clean");
    }
}
