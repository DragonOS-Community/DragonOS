use alloc::{collections::{linked_list::CursorMut, LinkedList}, sync::{Arc, Weak}, vec::Vec};
use system_error::SystemError;
use core::{hash::{Hash, Hasher, SipHasher}, marker::PhantomData, mem::size_of, ptr::NonNull};

use super::IndexNode;

pub trait Cacher<T, H: Hasher + Default> {
    fn cache(&self) -> Arc<DefaultCache<T, H>>;
}

pub trait Cachable<'a> : IndexNode {
    // name for hashing
    fn key(&self) -> Result<String, SystemError>;
    // value to store
    fn path(&self) -> Result<String, SystemError>;
    // fn value(&self) -> Weak<Self>;

    // 
    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }
}

// CacheLine = Weak<T>

pub struct DefaultCache<'a, T: ?Sized, H: Hasher + Default = SipHasher> {
    _hash_type: PhantomData<H>,
    table: Vec<LinkedList<CursorMut<'a, Weak<T>>>>,
    deque: LinkedList<Weak<T>>,
    max_size: u64,
}


impl<'a, 'b, T: Cachable<'b>, H: Hasher + Default> DefaultCache<'a, T, H> {
    const DEFAULT_MEMORY_SIZE: u64 = 1024 /* K */ * 1024 /* Byte */;
    ///@brief table size / 2 * (sizeof Cursor + sizeof listnode) = Memory Cost.
    fn new(size: Option<u64>) -> DefaultCache<'a, T, H> {

        let vec_size = size.unwrap_or(Self::DEFAULT_MEMORY_SIZE) / 
            (size_of::<CursorMut<'a, T>>() + size_of::<Option<NonNull<T>>>()) as u64 * 2;
        let capacity = vec_size / 2;
        let mut tmp: Vec<LinkedList<CursorMut<'a, Weak<T>>>> = Vec::new();
        // tmp.resize(vec_size as usize, LinkedList::new());
        for _ in [0..vec_size] {
            tmp.push(LinkedList::new());
        }

        DefaultCache {
            _hash_type: PhantomData::default(),
            table: tmp,
            deque: LinkedList::new(),
            max_size: capacity,
        }
    }

    // gain possision by spercific hasher
    fn position(&self, key: &str) -> usize {
        let mut state = H::default();
        key.hash(&mut state);
        (state.finish() / (self.max_size * 2)) as usize
    }

    // fn 

    fn put(&'a mut self, line: &Arc<T>) -> Result<(), SystemError> {
        // get coresponding table position
        let position = self.position(line.key()?.as_str());

        // extract origin
        if let Some(mut cur) = self.table[position]
            .extract_if(|cur| {
                if let Some(wptr) = cur.as_cursor().current() {
                    if let Some(entry) = wptr.upgrade() {
                        if entry == *line { // fix
                            return true;
                        }
                    }
                }
                false
            })
            .next()
        {
            cur.remove_current();
        }

        // push in deque
        self.deque.push_back(Arc::downgrade(line));

        // sign in table
        let cur = self.deque.cursor_back_mut();

        self.table[position].push_back(cur);
        Ok(())
    }

    // fn check(&self, key: &str) -> Cursor<'a, Weak<T>> {
    //     let position = self.possition(key);
    //     self.table[position].contains()
    // }

    fn get(&self, key: &str) -> Option<Arc<T>> {
        let position = self.position(key);
        self.table[position]
            .iter()
            .filter(|cur| 
                self._cur_unwrap(cur).is_some_and(|ent| 
                    ent.key() == key))
            

        None
    }

    // fn walk

    fn _cur_unwrap(&self, mut cur: CursorMut<Weak<T>>) -> Option<Arc<T>> {
        match cur.current() {
            Some(wptr) => wptr.upgrade(),
            None => None
        }
    }

    fn _get_helper(&mut self, key: &str) -> Option<usize> {
        self.table[self.position(key)]
            .iter()
            .find(|cur| {
                cur.as_cursor().current()
                    .is_some_and(|wptr| {
                        wptr.upgrade()
                            .is_some_and(|entry|
                                entry.key() == key)})})?
            .index()
    }
}
