use crate::include::bindings::bindings::{wait_queue_head_t};

use super::{list::list_init};


impl Default for wait_queue_head_t{
    fn default() -> Self {
        let mut x = Self { wait_list: Default::default(), lock: Default::default() };
        list_init(&mut x.wait_list);
        return x;
    }
}