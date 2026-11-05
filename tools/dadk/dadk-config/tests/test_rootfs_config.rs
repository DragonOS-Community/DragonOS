use dadk_config::{
    self,
    rootfs::{partition::PartitionType, RootFSConfigFile},
};
use test_base::{
    dadk_config::DadkConfigTestContext,
    test_context::{self as test_context, test_context},
};

const ROOTFS_CONFIG_FILE_NAME: &str = "config/rootfs.toml";

/// 测试加载模板目录中的 rootfs.toml 文件，验证它能被加载成功，并且已经包含了所有字段
#[test_context(DadkConfigTestContext)]
#[test]
fn test_load_rootfs_manifest_template(ctx: &DadkConfigTestContext) {
    let rootfs_manifest_path = ctx.templates_dir().join(ROOTFS_CONFIG_FILE_NAME);
    assert_eq!(rootfs_manifest_path.exists(), true);
    assert_eq!(rootfs_manifest_path.is_file(), true);
    let manifest =
        RootFSConfigFile::load(&rootfs_manifest_path).expect("Failed to load rootfs manifest");
    assert_eq!(manifest.partition.partition_type, PartitionType::None);
    // TODO 校验 manifest 中的字段是否齐全
}
