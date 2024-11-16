#[cfg(feature = "static_keys_test")]
mod tests {
    use static_keys::{define_static_key_false, static_branch_unlikely};
    define_static_key_false!(MY_STATIC_KEY);
    #[inline(always)]
    fn foo() {
        println!("Entering foo function");
        if static_branch_unlikely!(MY_STATIC_KEY) {
            println!("A branch");
        } else {
            println!("B branch");
        }
    }

    pub(super) fn static_keys_test() {
        foo();
        unsafe {
            MY_STATIC_KEY.enable();
        }
        foo();
    }
}

pub fn static_keys_init() {
    static_keys::global_init();
    #[cfg(feature = "static_keys_test")]
    tests::static_keys_test();
}
