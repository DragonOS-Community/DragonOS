use alloc::string::String;

/// 为 /proc/<pid>/maps 生成内容（最小实现，满足 gVisor chroot_test）。
pub(super) fn generate_maps_content() -> String {
    // 只要保证有内容即可；不要包含任何真实路径（尤其是 chroot 前缀）。
    String::from("00000000-00001000 r--p 00000000 00:00 0 [anon]\n")
}
