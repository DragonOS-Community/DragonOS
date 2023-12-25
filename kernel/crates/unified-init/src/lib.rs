#![no_std]

use system_error::SystemError;
pub use unified_init_macros as macros;

pub type UnifiedInitFunction = fn() -> core::result::Result<(), SystemError>;

#[cfg(test)]
mod tests {
    use linkme::distributed_slice;
    use unified_init_macros::unified_init;

    use super::*;

    #[test]
    fn no_element() {
        #[distributed_slice]
        static TEST_0: [UnifiedInitFunction] = [..];

        assert_eq!(TEST_0.len(), 0);
    }

    #[test]
    fn no_element_ne() {
        #[distributed_slice]
        static TEST_0_NE: [UnifiedInitFunction] = [..];

        #[unified_init(TEST_0_NE)]
        fn x() -> Result<(), SystemError> {
            todo!()
        }

        assert_ne!(TEST_0_NE.len(), 0);
    }

    #[test]
    fn one_element() {
        #[distributed_slice]
        static TEST_1: [UnifiedInitFunction] = [..];

        #[unified_init(TEST_1)]
        fn x() -> Result<(), SystemError> {
            todo!()
        }
        assert_eq!(TEST_1.len(), 1);
    }

    #[test]
    fn two_elements() {
        #[distributed_slice]
        static TEST_2: [UnifiedInitFunction] = [..];

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
