#[derive(Debug)]

pub struct BlockData{
    iba_id:usize,
    data_start_addr:usize,
    block_size:usize,
}

impl BlockData{
    pub fn new(iba_id:usize,data_start_addr:usize,block_size:usize)->Self{
        Self{
            iba_id,
            data_start_addr,
            block_size
        }
    }
#[inline]
    pub fn iba_id(&self)->usize{self.iba_id}
#[inline]
    pub fn data_start_addr(&self)->usize{self.data_start_addr}
#[inline]
    pub fn block_size(&self)->usize{self.block_size}
}

pub struct BlockIter{
    iba_id_start:usize,
    count:usize,
    current:usize,
    block_size:usize,
}

impl BlockIter{
    pub fn new(lba_id_start:usize,count:usize,block_size:usize)->Self{
        Self{
            iba_id_start: lba_id_start,
            count,
            block_size,
            current:0
        }
    }


} 

impl Iterator for BlockIter{

    type Item = BlockData;

    // 定义 next 方法，返回 Option<Self::Item>
    fn next(&mut self) -> Option<Self::Item> {
        if self.current<self.count{
            let ans=BlockData::new(self.iba_id_start+self.current, self.current*self.block_size, self.block_size);
            self.current+=1;
            Some(ans)
        }else{
            None
        }
    }
}

pub struct FailData{
    lba_id:usize,
    index:usize,
}

impl FailData{
    pub fn new(lba_id:usize,index:usize)->Self{
        FailData{
            lba_id,
            index
        }
    }
#[inline]
    pub fn lba_id(&self)->usize{self.lba_id}
    pub fn index(&self)->usize{self.index}
}