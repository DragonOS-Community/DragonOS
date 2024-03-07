/**
 * Todo:
 * [ ] - 注入式的路径比较：path是否需要设计
 * [ ] -
 */
use alloc::{collections::{linked_list::CursorMut, LinkedList}, sync::{Arc, Weak}, vec::Vec};
use system_error::SystemError;
use core::{hash::{Hash, Hasher, SipHasher}, marker::PhantomData, mem::size_of, ptr::NonNull};

use super::IndexNode;

// use std::path
// pub trait Cacher<H: Hasher + Default> {
//     fn cache(&self) -> Arc<DefaultCache<dyn IndexNode, H>>;
// }

// pub trait Cachable<'a> : IndexNode {
    
// }

// CacheLine = Weak<dyn IndexNode>

pub struct DefaultCache<'a, H: Hasher + Default = SipHasher> {
    _hash_type: PhantomData<H>,
    table: Vec<LinkedList<CursorMut<'a, Weak<dyn IndexNode>>>>,
    deque: LinkedList<Weak<dyn IndexNode>>,
    max_size: u64,
}


impl<'a, 'b, H: Hasher + Default> DefaultCache<'a, H> {
    const DEFAULT_MEMORY_SIZE: u64 = 1024 /* K */ * 1024 /* Byte */;
    ///@brief table size / 2 * (sizeof Cursor + sizeof listnode) = Memory Cost.
    fn new(size: Option<u64>) -> DefaultCache<'a, H> {

        let vec_size = size.unwrap_or(Self::DEFAULT_MEMORY_SIZE) / 
            (size_of::<CursorMut<'a, Weak<dyn IndexNode>>>() + size_of::<Option<NonNull<dyn IndexNode>>>()) as u64 * 2;
        let capacity = vec_size / 2;
        let mut tmp: Vec<LinkedList<CursorMut<'a, Weak<dyn IndexNode>>>> = Vec::new();
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

    fn put(&'a mut self, line: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        // get coresponding table position
        let position = self.position(line.key()?.as_str());

        // extract origin
        if let Some(mut cur) = self.table[position]
            .extract_if(|cur| {
                if let Some(wptr) = cur.as_cursor().current() {
                    if let Some(entry) = wptr.upgrade() {
                        // Check if the same
                        todo!() 
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

    // fn check(&self, key: &str) -> Cursor<'a, Weak<dyn IndexNode>> {
    //     let position = self.possition(key);
    //     self.table[position].contains()
    // }

    fn get(&self, key: &str) -> Option<Arc<dyn IndexNode>> {
        // let position = self.position(key);
        // self.table[position]
        //     .iter()
        //     .filter(|cur| 
        //         self._cur_unwrap(cur).is_some_and(|ent| 
        //             ent.key() == key))
        todo!();
    }

    // fn walk

    fn _cur_unwrap(&self, mut cur: CursorMut<Weak<dyn IndexNode>>) -> Option<Arc<dyn IndexNode>> {
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
                                entry.key().is_ok_and(|k| k == key))})})?
            .index()
    }
}
