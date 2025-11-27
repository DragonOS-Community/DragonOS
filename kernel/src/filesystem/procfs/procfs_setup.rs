use crate::filesystem::vfs::{syscall::ModeType, FileSystem, FileType};

use super::{LockedProcFSInode, ProcFS, ProcFileCreationParams, ProcFileType};

impl ProcFS {
    /// @brief 创建 /proc 根目录下的所有文件
    #[inline(never)]
    pub(crate) fn create_root_files(&self) {
        self.create_meminfo_file();
        self.create_kmsg_file();
        self.create_version_signature_file();
        self.create_mounts_file();
        self.create_version_file();
        self.create_cpuinfo_file();
        self.create_self_file();
    }

    /// @brief 创建 /proc/meminfo 文件
    #[inline(never)]
    fn create_meminfo_file(&self) {
        let meminfo_params = ProcFileCreationParams::builder()
            .parent(self.root_inode())
            .name("meminfo")
            .file_type(FileType::File)
            .mode(ModeType::from_bits_truncate(0o444))
            .ftype(ProcFileType::ProcMeminfo)
            .build()
            .unwrap();
        self.create_proc_file(meminfo_params)
            .unwrap_or_else(|_| panic!("create meminfo error"));
    }

    /// @brief 创建 /proc/kmsg 文件
    #[inline(never)]
    fn create_kmsg_file(&self) {
        let kmsg_params = ProcFileCreationParams::builder()
            .parent(self.root_inode())
            .name("kmsg")
            .file_type(FileType::File)
            .mode(ModeType::from_bits_truncate(0o444))
            .ftype(ProcFileType::ProcKmsg)
            .build()
            .unwrap();
        self.create_proc_file(kmsg_params)
            .unwrap_or_else(|_| panic!("create kmsg error"));
    }

    /// @brief 创建 /proc/version_signature 文件
    ///
    /// 这个文件是用来欺骗Aya框架识别内核版本
    /// On Ubuntu LINUX_VERSION_CODE doesn't correspond to info.release,
    /// but Ubuntu provides /proc/version_signature file, as described at
    /// https://ubuntu.com/kernel, with an example contents below, which we
    /// can use to get a proper LINUX_VERSION_CODE.
    ///
    ///   Ubuntu 5.4.0-12.15-generic 5.4.8
    ///
    /// In the above, 5.4.8 is what kernel is actually expecting, while
    /// uname() call will return 5.4.0 in info.release.
    #[inline(never)]
    fn create_version_signature_file(&self) {
        let version_signature_params = ProcFileCreationParams::builder()
            .parent(self.root_inode())
            .name("version_signature")
            .file_type(FileType::File)
            .ftype(ProcFileType::Default)
            .data("DragonOS 6.0.0-generic 6.0.0\n")
            .build()
            .unwrap();
        self.create_proc_file(version_signature_params)
            .unwrap_or_else(|_| panic!("create version_signature error"));
    }

    /// @brief 创建 /proc/mounts 文件
    #[inline(never)]
    fn create_mounts_file(&self) {
        let mounts_params = ProcFileCreationParams::builder()
            .parent(self.root_inode())
            .name("mounts")
            .file_type(FileType::File)
            .ftype(ProcFileType::ProcMounts)
            .build()
            .unwrap();
        self.create_proc_file(mounts_params)
            .unwrap_or_else(|_| panic!("create mounts error"));
    }

    /// @brief 创建 /proc/version 文件
    #[inline(never)]
    fn create_version_file(&self) {
        let version_params = ProcFileCreationParams::builder()
            .parent(self.root_inode())
            .name("version")
            .file_type(FileType::File)
            .mode(ModeType::from_bits_truncate(0o444))
            .ftype(ProcFileType::ProcVersion)
            .build()
            .unwrap();
        self.create_proc_file(version_params)
            .unwrap_or_else(|_| panic!("create version error"));
    }

    /// @brief 创建 /proc/cpuinfo 文件
    #[inline(never)]
    fn create_cpuinfo_file(&self) {
        let cpuinfo_params = ProcFileCreationParams::builder()
            .parent(self.root_inode())
            .name("cpuinfo")
            .file_type(FileType::File)
            .mode(ModeType::from_bits_truncate(0o444))
            .ftype(ProcFileType::ProcCpuinfo)
            .build()
            .unwrap();
        self.create_proc_file(cpuinfo_params)
            .unwrap_or_else(|_| panic!("create cpuinfo error"));
    }

    /// @brief 创建 /proc/self 文件
    #[inline(never)]
    fn create_self_file(&self) {
        let self_params = ProcFileCreationParams::builder()
            .parent(self.root_inode())
            .name("self")
            .file_type(FileType::SymLink)
            .mode(ModeType::from_bits_truncate(0o555))
            .ftype(ProcFileType::ProcSelf)
            .build()
            .unwrap();
        self.create_proc_file(self_params)
            .unwrap_or_else(|_| panic!("create self error"));
    }

    /// @brief 创建 /proc/thread-self 目录结构
    #[inline(never)]
    pub(crate) fn create_thread_self_directories(&self) {
        // Create /proc/thread-self directory
        let thread_self_dir = self
            .root_inode()
            .create(
                "thread-self",
                FileType::Dir,
                ModeType::from_bits_truncate(0o555),
            )
            .unwrap_or_else(|_| panic!("create thread-self error"));

        // Create /proc/thread-self/ns directory
        let ns_dir = thread_self_dir
            .create("ns", FileType::Dir, ModeType::from_bits_truncate(0o555))
            .unwrap_or_else(|_| panic!("create thread-self/ns error"));

        let ns_dir_proc = ns_dir
            .as_any_ref()
            .downcast_ref::<LockedProcFSInode>()
            .unwrap();
        // Mark this directory for dynamic namespace file creation
        ns_dir_proc.0.lock().fdata.ftype = ProcFileType::ProcThreadSelfNsRoot;
    }
}
