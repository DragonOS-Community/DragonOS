use clap::Parser;

// 定义一个枚举类型 RootFSCommand，表示根文件系统操作命令
#[derive(Debug, Parser, Clone, PartialEq, Eq)]
pub enum RootFSCommand {
    /// 创建根文件系统（磁盘镜像）
    Create(CreateCommandParam),
    /// 删除根文件系统（磁盘镜像）
    Delete,
    /// 删除系统根目录（sysroot文件夹）
    DeleteSysroot,
    /// 挂载根文件系统（磁盘镜像）
    Mount,
    /// 卸载根文件系统（磁盘镜像）
    Umount,
    /// 输出磁盘镜像的挂载点
    #[clap(name = "show-mountpoint")]
    ShowMountPoint,
    /// 输出磁盘镜像挂载到的loop设备
    ShowLoopDevice,
    /// 检查磁盘镜像文件是否存在
    CheckDiskImageExists,
}

#[derive(Debug, Parser, Clone, PartialEq, Eq)]
pub struct CreateCommandParam {
    /// 当磁盘镜像文件存在时，跳过创建
    #[clap(long = "skip-if-exists", default_value = "false")]
    pub skip_if_exists: bool,
}
