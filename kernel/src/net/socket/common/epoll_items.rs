use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    filesystem::epoll::{
        event_poll::{EventPoll, LockedEPItemLinkedList},
        EPollItem,
    },
    libs::mutex::Mutex,
};

#[derive(Debug, Default)]
pub struct EPollItems {
    items: LockedEPItemLinkedList,
}

impl AsRef<LockedEPItemLinkedList> for EPollItems {
    fn as_ref(&self) -> &LockedEPItemLinkedList {
        &self.items
    }
}

impl EPollItems {
    pub fn add(&self, item: Arc<EPollItem>) {
        self.items.lock().push_back(item);
    }

    pub fn remove(&self, item: &Weak<Mutex<EventPoll>>) -> Result<(), SystemError> {
        let to_remove = self
            .items
            .lock()
            .extract_if(|x| Weak::ptr_eq(&x.epoll(), item))
            .collect::<Vec<_>>();

        let result = if !to_remove.is_empty() {
            Ok(())
        } else {
            Err(SystemError::ENOENT)
        };

        drop(to_remove);
        return result;
    }

    pub fn clear(&self) -> Result<(), SystemError> {
        let mut guard = self.items.lock();
        let mut result = Ok(());
        guard.iter().for_each(|item| {
            if let Some(epoll) = item.epoll().upgrade() {
                let _ =
                    EventPoll::ep_remove(&mut epoll.lock(), item.fd(), None, item).map_err(|e| {
                        result = Err(e);
                    });
            }
        });
        guard.clear();
        return result;
    }
}
