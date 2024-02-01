use core::cmp::Ordering;

use alloc::{boxed::Box, vec::Vec};

use super::cache_config::BLOCK_SIZE;

pub enum CacheBlockFlag{
    Unused,
    Unwrited,
    Writed,
}
#[derive(Copy,Clone)]
pub struct CacheBlockAddr(usize);

impl CacheBlockAddr{
    pub fn new(num:usize)->Self{
        Self(num)
    }
    
    // pub fn get_cache_offset(&self)->usize{
    //     self.0*BLOCK_SIZE
    // }

    pub fn data(&self)->usize{
        self.0
    }
}
impl PartialEq<usize> for CacheBlockAddr {
    fn eq(&self, other: &usize) -> bool {
        self.0 == *other
    }
}

impl PartialOrd<usize> for CacheBlockAddr {
    fn partial_cmp(&self, other: &usize) -> Option<Ordering> {
        Some(self.0.cmp(other))
    }
}

// impl Ord<usize> for CacheBlockAddr {
//     fn cmp(&self, other: &Self) -> Ordering {
//         self.value.cmp(&other.value)
//     }
// }

pub struct CacheBlock{
    data:Box<[u8]>,
    flag:CacheBlockFlag,
    lba_id:usize,
}

impl CacheBlock{
    pub fn new()->Self{
        let space_vec=Vec::with_capacity(BLOCK_SIZE);
        let space_box=space_vec.into_boxed_slice();
        CacheBlock{
            data:space_box,
            flag:CacheBlockFlag::Unused,
            lba_id:0,
        }
    }

    pub fn from_data(lba_id:usize,data:Vec<u8>)->Self{
        let space_box=data.into_boxed_slice();
        CacheBlock{
            data:space_box,
            flag:CacheBlockFlag::Unwrited,
            lba_id,
        }
    }

    pub fn set_flag(&mut self,flag:CacheBlockFlag)->Option<()>{
        todo!()
    }
#[inline]
    pub fn get_data(&self,buf:&mut [u8])->usize{
        buf.copy_from_slice(&self.data);
        return BLOCK_SIZE;
    }

    pub fn get_lba_id(&self)->usize{
        self.lba_id
    }
}