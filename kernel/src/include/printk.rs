use crate::include::bindings::bindings::{printk_color};

#[macro_export]
macro_rules! print {
    () => {
        ($($arg:tt)*) => ($crate::)
    };
}