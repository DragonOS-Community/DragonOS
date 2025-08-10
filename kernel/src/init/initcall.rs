use system_error::SystemError;
use unified_init::{define_public_unified_initializer_slice, unified_init};

define_public_unified_initializer_slice!(INITCALL_PURE);
define_public_unified_initializer_slice!(INITCALL_CORE);
define_public_unified_initializer_slice!(INITCALL_POSTCORE);
define_public_unified_initializer_slice!(INITCALL_ARCH);
define_public_unified_initializer_slice!(INITCALL_SUBSYS);
define_public_unified_initializer_slice!(INITCALL_FS);
define_public_unified_initializer_slice!(INITCALL_ROOTFS);
define_public_unified_initializer_slice!(INITCALL_DEVICE);
define_public_unified_initializer_slice!(INITCALL_LATE);

pub fn do_initcalls() -> Result<(), SystemError> {
    unified_init!(INITCALL_PURE);
    unified_init!(INITCALL_CORE);
    unified_init!(INITCALL_POSTCORE);
    unified_init!(INITCALL_ARCH);
    unified_init!(INITCALL_SUBSYS);
    unified_init!(INITCALL_FS);
    unified_init!(INITCALL_ROOTFS);
    unified_init!(INITCALL_DEVICE);
    unified_init!(INITCALL_LATE);
    return Ok(());
}
