//! 需要测试的时候可以在这里写测试代码，
//! 然后在当前目录执行 `cargo expand --bin unified-init-expand`
//! 就可以看到把proc macro展开后的代码了
#![no_std]
#![allow(internal_features)]
#![feature(lang_items)]

fn main() {
    todo!()
}

#[cfg(target_os = "none")]
#[panic_handler]
#[no_mangle]
pub fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[cfg(target_os = "none")]
#[lang = "eh_personality"]
unsafe extern "C" fn eh_personality() {}

#[cfg(test)]
mod tests {
    use system_error::SystemError;
    use unified_init::define_unified_initializer_slice;
    use unified_init_macros::unified_init;

    use super::*;

    #[test]
    fn no_element() {
        define_unified_initializer_slice!(TEST_0);

        assert_eq!(TEST_0.len(), 0);
    }

    #[test]
    fn no_element_ne() {
        define_unified_initializer_slice!(TEST_0_NE);

        #[unified_init(TEST_0_NE)]
        fn x() -> Result<(), SystemError> {
            todo!()
        }

        assert_ne!(TEST_0_NE.len(), 0);
    }

    #[test]
    fn one_element() {
        define_unified_initializer_slice!(TEST_1);

        #[unified_init(TEST_1)]
        fn x() -> Result<(), SystemError> {
            todo!()
        }
        assert_eq!(TEST_1.len(), 1);
    }

    #[test]
    fn two_elements() {
        define_unified_initializer_slice!(TEST_2);

        #[unified_init(TEST_2)]
        fn x() -> Result<(), SystemError> {
            todo!()
        }

        #[unified_init(TEST_2)]
        fn y() -> Result<(), SystemError> {
            todo!()
        }
        assert_eq!(TEST_2.len(), 2);
    }
}
