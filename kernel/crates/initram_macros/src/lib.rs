#![no_std]
#![deny(clippy::all)]

#[macro_export]
#[allow(clippy::crate_in_macro_def)]
macro_rules! include_initramfs {
    () => {
        #[cfg(all(target_arch = "x86_64", has_initramfs))]
        #[allow(non_upper_case_globals)]
        #[used]
        pub static INITRAM_DATA: &[u8] =
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/initram/x86.cpio.xz"));

        #[cfg(all(target_arch = "x86_64", not(has_initramfs)))]
        #[allow(non_upper_case_globals)]
        #[used]
        pub static INITRAM_DATA: &[u8] = &[];

        #[cfg(all(target_arch = "riscv64", has_initramfs))]
        #[allow(non_upper_case_globals)]
        #[used]
        pub static INITRAM_DATA: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/initram/riscv64.cpio.xz"
        ));

        #[cfg(all(target_arch = "riscv64", not(has_initramfs)))]
        #[allow(non_upper_case_globals)]
        #[used]
        pub static INITRAM_DATA: &[u8] = &[];

        #[cfg(all(target_arch = "loongarch64", has_initramfs))]
        #[allow(non_upper_case_globals)]
        #[used]
        pub static INITRAM_DATA: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/initram/loongarch64.cpio.xz"
        ));

        #[cfg(all(target_arch = "loongarch64", not(has_initramfs)))]
        #[allow(non_upper_case_globals)]
        #[used]
        pub static INITRAM_DATA: &[u8] = &[];

        /// 获取内核中 initramfs 的数据的起始地址
        pub fn get_initramfs_start_addr() -> usize {
            INITRAM_DATA.as_ptr() as usize
        }

        /// 获取内核中 initramfs 的数据的 Size
        pub fn get_initramfs_size() -> usize {
            INITRAM_DATA.len() as usize
        }

        /// 获取 initramfs 的数据的全新 Vec
        /// 此函数会复制内核中包含的 initramfs 内容到一个新的 Vec 中
        pub fn get_initram_data() -> Vec<u8> {
            INITRAM_DATA.to_vec()
        }

        /// 获取 initramfs 的数据的 Vec 引用
        /// 此函数会返回内核中包含的 initramfs 内容的引用
        pub fn get_initram() -> &'static [u8] {
            INITRAM_DATA
        }
    };
}
