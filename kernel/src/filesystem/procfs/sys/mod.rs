use crate::filesystem::{
    procfs::{ProcFS, ProcFileCreationParams, ProcFileType},
    vfs::{syscall::ModeType, FileSystem, FileType},
};

pub mod sysctl;

impl ProcFS {
    /// @brief 创建 /proc/sys 目录结构及相关文件
    #[inline(never)]
    pub(super) fn create_sysctl_files(&self) {
        // Create /proc/sys directory
        let sys_dir = self
            .root_inode()
            .create("sys", FileType::Dir, ModeType::from_bits_truncate(0o555))
            .unwrap_or_else(|_| panic!("create /proc/sys error"));

        // Create /proc/sys/kernel directory
        let kernel_dir = sys_dir
            .create("kernel", FileType::Dir, ModeType::from_bits_truncate(0o555))
            .unwrap_or_else(|_| panic!("create /proc/sys/kernel error"));

        // Create /proc/sys/kernel/printk file
        let printk_params = ProcFileCreationParams::builder()
            .parent(kernel_dir)
            .name("printk")
            .file_type(FileType::File)
            .mode(ModeType::from_bits_truncate(0o644))
            .ftype(ProcFileType::ProcSysKernelPrintk)
            .build()
            .unwrap();
        self.create_proc_file(printk_params)
            .unwrap_or_else(|_| panic!("create /proc/sys/kernel/printk error"));
    }
}
