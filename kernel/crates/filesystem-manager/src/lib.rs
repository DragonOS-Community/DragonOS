#![no_std]
#![allow(clippy::needless_return)]

pub use filesystem_manager_macros as macros;
use system_error::SystemError;

pub struct FileSystemMaker {
    function: &'static FileSystemNewFunction,
    name: &'static str,
}

impl FileSystemMaker {
    pub const fn new(
        name: &'static str,
        function: &'static FileSystemNewFunction,
    ) -> FileSystemMaker {
        FileSystemMaker { function, name }
    }

    pub fn call(&self) -> Result<Box<dyn FileSystem>, SystemError> {
        (self.function)()
    }

    pub const fn name(&self) -> &'static str {
        self.name
    }
}

pub type FileSystemNewFunction = fn() -> Result<Box<dyn FileSystem>, SystemError>;

/// 定义文件系统初始化器的分布式切片数组(公开)
#[macro_export]
macro_rules! define_public_filesystem_maker_slice {
    ($name:ident) => {
        #[::linkme::distributed_slice]
        pub static $name: [::vfs::FileSystemMaker] = [..];
    };
    () => {
        compile_error!(
            "define_public_filesystem_maker_slice! requires at least one argument: slice_name"
        );
    };
}
