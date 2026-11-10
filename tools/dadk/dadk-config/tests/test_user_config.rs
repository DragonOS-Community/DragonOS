use std::path::PathBuf;

use dadk_config::{
    common::{
        target_arch::TargetArch,
        task::{
            BuildConfig, CleanConfig, Dependency, InstallConfig, Source, TaskEnv, TaskSource,
            TaskSourceType,
        },
    },
    user::UserConfigFile,
};
use test_base::{
    dadk_config::DadkConfigTestContext,
    test_context::{self as test_context, test_context},
};

const USER_CONFIG_LOCAL_FILE: &str = "config/userapp_config.toml";

/// 测试解析DADK用户配置文件
#[test_context(DadkConfigTestContext)]
#[test]
fn test_parse_dadk_user_config(ctx: &mut DadkConfigTestContext) {
    let config_file = ctx.templates_dir().join(USER_CONFIG_LOCAL_FILE);
    assert!(config_file.exists());
    assert!(config_file.is_file());
    let r = UserConfigFile::load(&config_file);
    assert!(r.is_ok());
    let mut user_config = r.unwrap();
    let mut expected_user_config = UserConfigFile {
        name: "userapp_config".to_string(),
        version: "0.2.0".to_string(),
        description: "".to_string(),
        build_once: false,
        install_once: false,
        task_source: TaskSource {
            source_type: TaskSourceType::BuildFromSource,
            source: Source::Git,
            source_path: "https://git.mirrors.dragonos.org.cn/DragonOS-Community/test_git.git"
                .to_string(),
            branch: None,
            archive_rootdir: None,
            revision: Some("01cdc56863".to_string()),
        },
        depends: vec![
            Dependency {
                name: "depend1".to_string(),
                version: "0.1.1".to_string(),
            },
            Dependency {
                name: "depend2".to_string(),
                version: "0.1.2".to_string(),
            },
        ],
        build: BuildConfig::new(
            Some("make install".to_string()),
            Some(PathBuf::from("config/pre_build.sh")),
            Some(PathBuf::from("config/post_build.sh")),
        ),
        install: InstallConfig::new(Some(PathBuf::from("/bin"))),
        clean: CleanConfig::new(Some("make clean".to_string())),
        envs: vec![
            TaskEnv::new("PATH".to_string(), "/usr/bin".to_string()),
            TaskEnv::new("LD_LIBRARY_PATH".to_string(), "/usr/lib".to_string()),
        ],
        target_arch: vec![TargetArch::try_from("x86_64").unwrap()],
    };

    user_config.target_arch.sort();
    expected_user_config.target_arch.sort();
    user_config.depends.sort();
    expected_user_config.depends.sort();
    user_config.envs.sort();
    expected_user_config.envs.sort();

    assert_eq!(user_config, expected_user_config)
}
