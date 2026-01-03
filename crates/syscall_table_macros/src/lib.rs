#![no_std]
#![deny(clippy::all)]

#[macro_export]
#[allow(clippy::crate_in_macro_def)]
macro_rules! declare_syscall {
    ($nr:ident, $inner_handle:ident) => {
        paste::paste! {
            #[allow(non_upper_case_globals)]
            #[link_section = ".syscall_table"]
            #[used]
            pub static [<HANDLE_ $nr>]: crate::syscall::table::SyscallHandle = crate::syscall::table::SyscallHandle {
                nr: $nr,
                inner_handle: &$inner_handle,
                name: stringify!($nr),
            };
        }
    };
}
