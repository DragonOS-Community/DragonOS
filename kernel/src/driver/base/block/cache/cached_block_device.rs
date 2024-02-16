use alloc::{boxed::Box, vec::Vec, collections::BTreeMap, borrow::ToOwned};

// use crate::arch::init;

// use crate::driver::base::block::block_device::BlockIter;

use crate::driver::base::block;

use super::{cache_block::{CacheBlockAddr, CacheBlock}, cache_config:: BLOCK_SIZE, cache_iter::{BlockIter, FailData}};
// use virtio_drivers::PhysAddr;

static mut INITIAL_FLAG:bool=false;
static mut CSPACE:Option<CacheSpace>=None;
static mut CMAPPER:Option<CacheMapper>=None;
pub struct BlockCache;

impl BlockCache{
    fn init(){
        unsafe {
            
            CSPACE=Some(CacheSpace::new());
            CMAPPER=Some(CacheMapper::new());
            INITIAL_FLAG=true;
        }
    }

    pub fn read(lba_id_start:usize,count:usize,buf:&mut [u8])->Result<usize,Vec<FailData>>{
        let block_iter=BlockIter::new(lba_id_start,count,BLOCK_SIZE);
        // let mut success_flag=true;
        // let mut success_vec:Vec<Vec<u8>>=vec![];
        // let mut fail_vec:Vec<FailData>=vec![];
        let cache_block_addr=Self::check_able_to_read(block_iter)?;
        // let mut index=0;
        // for i in block_iter{
        //     match Self::read_one_block(i.iba_id()){
        //         Some(x)=>{if success_flag {success_vec.push(x)}},
        //         None=>{
        //             success_flag=false;
        //             let f_data=FailData::new(i.iba_id(), index);
        //             fail_vec.push(f_data)
        //         }
        //     }
        //     index+=1;
        // }
        assert!(cache_block_addr.len()==block_iter.count());
        for (index,i) in block_iter.enumerate(){
            Self::read_one_block(i.iba_id(), cache_block_addr[index],index, buf);
        }
        // kdebug!("cache hit！");
        return Ok(count);
        // if success_flag{
        //     // kdebug!("cache hit！");
        //     for i in 0..success_vec.len(){
        //         buf[i*BLOCK_SIZE..(i+1)*BLOCK_SIZE].copy_from_slice(&success_vec
        //         [i]);
        //     }
        //     return Ok(count);
        // }else{
        //     return Err(fail_vec);
        // }
        
    }

    fn check_able_to_read(block_iter:BlockIter)->Result<Vec<CacheBlockAddr>,Vec<FailData>>{
        unsafe {
            if !INITIAL_FLAG{
                Self::init()
            }   
        }
        let mut ans=vec![];
        let mut success_ans=vec![];
        let mapper=unsafe {
            match &CMAPPER{
                Some(x)=>{x},
                None=>{panic!("cache fail");}
            }
        };
        let mut index=0;
        for i in block_iter{
            match mapper.find(i.iba_id()){
                Some(x)=>{success_ans.push(*x);continue;}
                None=>{
                    ans.push(FailData::new(i.iba_id(),index))
                }
            }
            index+=1;
        }
        if ans.len()!=0{
            return Err(ans);
        }else{
            return Ok(success_ans);
        }

    }

#[inline]
    pub fn read_one_block(lba_id:usize,cache_block_addr:CacheBlockAddr,position:usize,buf:&mut [u8])->Option<usize>{
        let space=unsafe {
            match &CSPACE{
                Some(x)=>x,
                None=>{return None}
            }
        };
        Some(space.read(cache_block_addr,position,buf)?)
    }

    pub fn insert(f_data_vec:Vec<FailData>,data:&[u8])->Result<usize,()>{
        // assert!(f_data_vec.len()*BLOCK_SIZE==data.len());
        for i in f_data_vec{
            let index=i.index();
            Self::insert_one_block(i.lba_id(), data[index*BLOCK_SIZE..(index+1)*BLOCK_SIZE].to_vec());
        }
        Ok(0)
    }

    pub fn insert_one_block(lba_id:usize,data:Vec<u8>)->Option<()>{
        unsafe {
            if !INITIAL_FLAG{
                Self::init()
            }   
        }
        // let mapper=unsafe {
        //     match &mut CMAPPER{
        //         Some(x)=>{x},
        //         None=>{return None}
        //     }
        // };
        let space=unsafe {
            match &mut CSPACE{
                Some(x)=>x,
                None=>{return None}
            }
        };
        space.insert(lba_id,data)
        // mapper.insert(lba_id,addr)
    }

    pub fn test_write(lba_id_start:usize,count:usize,data:&[u8])->Result<usize,()>{
        unsafe {
            if !INITIAL_FLAG{
                Self::init()
            }   
        }
        let mapper=unsafe {
            match &mut CMAPPER{
                Some(x)=>{x},
                None=>{return Err(())}
            }
        };
        let block_iter=BlockIter::new(lba_id_start, count, BLOCK_SIZE);
        for i in block_iter{
            mapper.remove(i.iba_id());
        }
        Ok(count)
    }

    // pub fn test_write_one_block(lba_id:usize)->Option<()>{
        
    //     mapper.remove(lba_id);
    //     Some(())
    // }
}

struct CacheSpace{
    root:Vec<CacheBlock>,
    frame_selector:FrameSelector
}

impl CacheSpace{
    pub fn new()->Self{
        Self{
            root:Vec::new(),
            frame_selector:FrameSelector::new()
        }
    }
#[inline]
    pub fn read(&self,addr:CacheBlockAddr,position:usize,buf:&mut [u8])->Option<usize>{
        if addr>self.frame_selector.get_size() {
            return None;
        }else{
            return Some(self.root[addr.data()].get_data(&mut buf[position*BLOCK_SIZE..(position+1)*BLOCK_SIZE]));
        }
        
    }

    pub fn write(&mut self,addr:CacheBlockAddr,data:CacheBlock)->Option<()>{
        todo!()
    }

    pub fn insert(&mut self,lba_id:usize,data:Vec<u8>)->Option<()>{
        let data_block=CacheBlock::from_data(lba_id,data);
        let mapper=unsafe {
                match &mut CMAPPER{
                    Some(x)=>{x},
                    None=>{return None}
                }
            };
        if(self.frame_selector.can_append()){
            let index=self.frame_selector.get_index();
            self.root.push(data_block);
            // kdebug!("index:{},root.len:{}",index.data(),self.root.len());
            assert!(index.data()==self.root.len()-1);
            mapper.insert(lba_id, index);
            Some(())
        }else{
            let index=self.frame_selector.get_index();
            let removed_id=self.root[index.data()].get_lba_id();
            
            self.root[index.data()]=data_block;
            mapper.insert(lba_id, index);
            mapper.remove(removed_id);
            Some(())
        }
        

    }
}

struct CacheMapper{
    map:BTreeMap<usize,CacheBlockAddr>,
    count:usize,
}

impl CacheMapper{
    pub fn new()->Self{
        Self { map: BTreeMap::new(), count: 0 }
    }

    pub fn insert(&mut self,lba_id:usize,caddr:CacheBlockAddr)->Option<()>{
        self.map.insert(lba_id, caddr)?;
        Some(())
    }
#[inline]
    pub fn find(&self,lba_id:usize)->Option<&CacheBlockAddr>{
        self.map.get(&lba_id)
    }

    pub fn remove(&mut self,lba_id:usize){
        match self.map.remove(&lba_id){
            Some(_)=>{self.count-=1},
            None=>{}
        }
    }
}

struct FrameSelector{
    threshold:usize,
    size:usize,
    current:usize,
}

impl FrameSelector{
    pub fn new()->Self{
        Self { threshold: 1310720, size: 0,current:0 }
    }

    pub fn get_index(&mut self)->CacheBlockAddr{
        if self.size>=self.threshold{
            let ans=self.current;
            self.current+=1;
            self.current%=self.threshold;
            return CacheBlockAddr::new(ans);
        }else{
            let ans=self.current;
            self.size+=1;
            self.current+=1;
            self.current%=self.threshold;
            return CacheBlockAddr::new(ans);
            
        }
    }

    pub fn can_append(&self)->bool{
        self.size<self.threshold
    }

    pub fn get_size(&self)->usize{
        self.size
    }
}