use crate::include::bindings::bindings::List;

/// @brief 初始化链表
#[inline]
pub fn list_init(list: *mut List) {
    unsafe { *list }.prev = list;
    unsafe { *list }.next = list;
}

impl Default for List {
    fn default() -> Self {
        let x = Self {
            prev: 0 as *mut List,
            next: 0 as *mut List,
        };
        return x;
    }
}
