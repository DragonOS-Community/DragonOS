use alloc::sync::Arc;
use system_error::SystemError;

use crate::filesystem::epoll::{event_poll::EPollItemList, EPollItem};

#[derive(Debug, Default)]
pub struct EPollItems {
    items: EPollItemList,
}

impl AsRef<EPollItemList> for EPollItems {
    fn as_ref(&self) -> &EPollItemList {
        &self.items
    }
}

impl EPollItems {
    pub fn add(&self, item: Arc<EPollItem>) {
        self.items.add(item);
    }

    pub fn remove(&self, item: &Arc<EPollItem>) -> Result<(), SystemError> {
        self.items.remove(item)
    }
}
