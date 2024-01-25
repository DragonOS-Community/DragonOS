use alloc::{boxed::Box, vec::Vec, collections::BTreeMap, borrow::ToOwned};

// use crate::arch::init;

// use crate::driver::base::block::block_device::BlockIter;

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
        let mut success_flag=true;
        let mut success_vec:Vec<Vec<u8>>=vec![];
        let mut fail_vec:Vec<FailData>=vec![];
        let mut index=0;
        for i in block_iter{
            
            match Self::read_one_block(i.iba_id()){
                Some(x)=>{if success_flag {success_vec.push(x)}},
                None=>{
                    success_flag=false;
                    let f_data=FailData::new(i.iba_id(), index);
                    fail_vec.push(f_data)
                }
            }
            index+=1;
        }
        if success_flag{
            kdebug!("cache hit！");
            for i in 0..success_vec.len(){
                buf[i*BLOCK_SIZE..(i+1)*BLOCK_SIZE].copy_from_slice(&success_vec
                [i]);
            }
            return Ok(count);
        }else{
            return Err(fail_vec);
        }
        
    }

    pub fn read_one_block(lba_id:usize)->Option<Vec<u8>>{
        //todo:这里实际上最好要在某个合适的地方进行初始化，这里做这个检查只是权宜之计
        unsafe {
            if !INITIAL_FLAG{
                Self::init()
            }   
        }


        let mapper=unsafe {
            match &CMAPPER{
                Some(x)=>{x},
                None=>{return None}
            }
        };
        let addr=mapper.find(lba_id)?;
        let space=unsafe {
            match &CSPACE{
                Some(x)=>x,
                None=>{return None}
            }
        };
        Some(space.read(*addr)?)
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
        let mapper=unsafe {
            match &mut CMAPPER{
                Some(x)=>{x},
                None=>{return None}
            }
        };
        let space=unsafe {
            match &mut CSPACE{
                Some(x)=>x,
                None=>{return None}
            }
        };
        let addr=space.insert(lba_id,data)?;
        mapper.insert(lba_id,addr)
    }

    pub fn test_write(lba_id_start:usize,count:usize,data:&[u8])->Result<usize,()>{
        let block_iter=BlockIter::new(lba_id_start, count, BLOCK_SIZE);
        for i in block_iter{
            Self::test_write_one_block(i.iba_id());
        }
        Ok(count)
    }

    pub fn test_write_one_block(lba_id:usize)->Option<()>{
        unsafe {
            if !INITIAL_FLAG{
                Self::init()
            }   
        }
        let mapper=unsafe {
            match &mut CMAPPER{
                Some(x)=>{x},
                None=>{return None}
            }
        };
        mapper.remove(lba_id);
        Some(())
    }
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

    pub fn read(&self,addr:CacheBlockAddr)->Option<Vec<u8>>{
        if(addr>self.frame_selector.get_size()){
            return None;
        }else{
            return Some(self.root[addr.data()].get_data())
        }
        
    }

    pub fn write(&mut self,addr:CacheBlockAddr,data:CacheBlock)->Option<()>{
        todo!()
    }

    pub fn insert(&mut self,lba_id:usize,data:Vec<u8>)->Option<CacheBlockAddr>{
        let data_block=CacheBlock::from_data(lba_id,data);
        
        if(self.frame_selector.can_append()){
            let index=self.frame_selector.get_index();
            self.root.push(data_block);
            // kdebug!("index:{},root.len:{}",index.data(),self.root.len());
            assert!(index.data()==self.root.len()-1);
            Some(index)
        }else{
            let index=self.frame_selector.get_index();
            self.root[index.data()]=data_block;
            Some(index)
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
        Self { threshold: 1024, size: 0,current:0 }
    }

    pub fn get_index(&mut self)->CacheBlockAddr{
        if(self.size>=self.threshold){
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