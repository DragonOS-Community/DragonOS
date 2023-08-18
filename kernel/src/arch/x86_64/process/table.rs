// === 段选择子在GDT中的索引 ===
/// kernel code segment selector
pub const KERNEL_CS: usize = 0x08;
/// kernel data segment selector
pub const KERNEL_DS: usize = 0x10;
/// user code segment selector
pub const USER_CS: usize = 0x28;
/// user data segment selector
pub const USER_DS: usize = 0x30;
