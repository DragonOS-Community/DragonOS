use dadk_config::{self, boot::BootConfigFile};
use test_base::{
    dadk_config::DadkConfigTestContext,
    test_context::{self as test_context, test_context},
};

const BOOT_CONFIG_FILE_NAME: &str = "config/boot.toml";

/// 测试加载模板目录中的 boot.toml 文件，验证它能被加载成功.
#[test_context(DadkConfigTestContext)]
#[test]
fn test_load_boot_config_template(ctx: &DadkConfigTestContext) {
    let boot_config_path = ctx.templates_dir().join(BOOT_CONFIG_FILE_NAME);
    assert_eq!(boot_config_path.exists(), true);
    assert_eq!(boot_config_path.is_file(), true);
    let _manifest = BootConfigFile::load(&boot_config_path).expect("Failed to load boot config");
    // TODO 校验 manifest 中的字段是否齐全
}
