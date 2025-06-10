

# ```MountableFileSystem``` 可以被挂载的文件系统应该实现的trait
```MountableFileSystem``` 继承自FileSystem，每个支持挂载的文件系统都应该实现这个trait，主要用于统一注册文件系统到```FAMAKER```中
## 方法

- ```make_mount_data```: 根据传入的raw_data以及source生成相应的mount_data
- ```make_fs```: 根据上面生成的mount_data生成相应的文件系统实例

```Rust
pub trait MountableFileSystem: FileSystem {
    fn make_mount_data(
        _raw_data: Option<&str>,
        _source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        log::error!("This filesystem does not support make_mount_data");
        Err(SystemError::ENOSYS)
    }

    fn make_fs(
        _data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        log::error!("This filesystem does not support make_fs");
        Err(SystemError::ENOSYS)
    }
}
```

# ```register_mountable_fs!```，用于注册一个可以被挂载文件系统
此宏用于注册一个可以被挂载的文件系统。
它会将文件系统的创建函数和挂载数据创建函数注册到全局的`FSMAKER`数组中。

## 参数
- `$fs`: 文件系统对应的结构体
- `$maker_name`: 文件系统的注册名
- `$fs_name`: 文件系统的名称（字符串字面量）
```Rust
#[macro_export]
macro_rules! register_mountable_fs {
    ($fs:ident, $maker_name:ident, $fs_name:literal) => {
        impl $fs {
            fn make_fs_bridge(
                data: Option<&dyn FileSystemMakerData>,
            ) -> Result<Arc<dyn FileSystem>, SystemError> {
                <$fs as MountableFileSystem>::make_fs(data)
            }

            fn make_mount_data_bridge(
                raw_data: Option<&str>,
                source: &str,
            ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
                <$fs as MountableFileSystem>::make_mount_data(raw_data, source)
            }
        }

        #[distributed_slice(FSMAKER)]
        static $maker_name: FileSystemMaker = FileSystemMaker::new(
            $fs_name,
            &($fs::make_fs_bridge
                as fn(
                    Option<&dyn FileSystemMakerData>,
                ) -> Result<Arc<dyn FileSystem + 'static>, SystemError>),
            &($fs::make_mount_data_bridge
                as fn(
                    Option<&str>,
                    &str,
                )
                    -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError>),
        );
    };
}
```


# 使用示例（以RamFS为例子）
首先为ramfs文件系统实现 ```MountableFileSystem``` trait，生命这是一个目前已经支持了挂载的文件系统

然后使用 ```register_mountable_fs!``` 宏将该文件系统注册到统一的文件系统生成器中，以便后面通过文件系统名称来生成对应的文件系统
```Rust
impl MountableFileSystem for RamFS {
    fn make_mount_data(
        _raw_data: Option<&str>,
        _source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>,SystemError> {
        // 目前ramfs不需要任何额外的mount数据
        Ok(None)
    }
    fn make_fs(
        _data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let fs = RamFS::new();
        return Ok(fs);
    }
}


register_mountable_fs!(RamFS, RAMFSMAKER, "ramfs");
```

# ```produce_fs```方法，通过文件系统的名称和数据创建一个文件系统实例
## 参数
- `filesystem`: 文件系统的名称
- `data`: 可选的挂载数据
- `source`: 挂载源
## 返回值
- `Ok(Arc<dyn FileSystem>)`: 成功时返回文件系统的共享引用
- `Err(SystemError)`: 如果找不到对应的文件系统或创建失败，则返回错误

这样我们便可以在mount系统调用中通过传入的文件系统名称创建相应的文件系统示例，之后便可以正常完成挂载
```Rust
pub fn produce_fs(
    filesystem: &str,
    data: Option<&str>,
    source: &str,
) -> Result<Arc<dyn FileSystem>, SystemError> {
    match FSMAKER.iter().find(|&m| m.name == filesystem) {
        Some(maker) => {
            let mount_data = (maker.builder)(data, source).unwrap();
            let mount_data_ref = mount_data.as_ref().map(|arc| arc.as_ref());
            maker.build(mount_data_ref)
        }
        None => {
            log::error!("mismatch filesystem type : {}", filesystem);
            Err(SystemError::EINVAL)
        }
    }
}
```
对应```sys_mount```中的实现
```Rust 
    // sys_mount.rs
    let fs = produce_fs(fstype_str, data, source)?;

    do_mount(fs, &target)?;
```