use alloc::{
    collections::LinkedList,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    libs::spinlock::SpinLock,
    net::event_poll::{EPollItem, EventPoll},
};

#[derive(Debug, Clone)]
pub struct EPollItems {
    items: Arc<SpinLock<LinkedList<Arc<EPollItem>>>>,
}

impl Default for EPollItems {
    fn default() -> Self {
        Self {
            items: Arc::new(SpinLock::new(LinkedList::new())),
        }
    }
}

impl EPollItems {
    pub fn add(&self, item: Arc<EPollItem>) {
        self.items.lock_irqsave().push_back(item);
    }

    pub fn remove(&self, item: &Weak<SpinLock<EventPoll>>) -> Result<(), SystemError> {
        let to_remove = self
            .items
            .lock_irqsave()
            .extract_if(|x| x.epoll().ptr_eq(item))
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
        let mut guard = self.items.lock_irqsave();
        let mut result = Ok(());
        guard.iter().for_each(|item| {
            if let Some(epoll) = item.epoll().upgrade() {
                let _ =
                    EventPoll::ep_remove(&mut epoll.lock_irqsave(), item.fd(), None).map_err(|e| {
                        result = Err(e);
                    });
            }
        });
        guard.clear();
        return result;
    }
}
