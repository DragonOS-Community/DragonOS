use alloc::{boxed::Box, collections::BTreeMap, vec::Vec};

use super::{
    cache_block::{CacheBlock, CacheBlockAddr},
    cache_config::{BLOCK_SIZE, BLOCK_SIZE_LOG, CACHE_THRESHOLD},
    cache_iter::{BlockIter, FailData},
};

// static mut INITIAL_FLAG: bool = false;
static mut CSPACE: Option<CacheSpace> = None;
static mut CMAPPER: Option<CacheMapper> = None;
pub struct BlockCache;      //该结构体向外提供BlockCache服务

impl BlockCache {
/// @brief 初始化BlockCache需要的结构体
    pub fn init() {         
        unsafe {
            CSPACE = Some(CacheSpace::new());
            CMAPPER = Some(CacheMapper::new());
            // INITIAL_FLAG = true;
        }
        kdebug!("BlockCache Initialized!")
    }
/// @brief 使用blockcache进行对块设备进行连续块的读操作
/// 
/// #参数：
/// - 'lba_id_start' :连续块的起始块的lba_id
/// - 'count' :从连续块算起需要读多少块
/// - 'buf' :读取出来的数据存放在buf中
/// 
/// #返回值：
/// - Ok(usize) :表示读取块的个数
/// - Err(Vec<FailData>) :返回读取失败的块的数据，利用该返回值可以帮助blockcache插入读取失败的块值（见insert函数）
    pub fn read(lba_id_start: usize, count: usize, buf: &mut [u8]) -> Result<usize, Vec<FailData>> {
        let block_iter = BlockIter::new(lba_id_start, count, BLOCK_SIZE);               //生成一个块迭代器（BlockIter），它可以迭代地给出所有需要块的数据，其中就包括lba_id
        let cache_block_addr = Self::check_able_to_read(block_iter)?;         //调用检查函数，检查有无缺块，如果没有就可以获得所有块的Cache地址。如果失败了就直接返回FailData向量
        assert!(cache_block_addr.len() == block_iter.count());  //块地址vec的长度应当等于块迭代器的大小
        for (index, _) in block_iter.enumerate() {       //迭代地读取cache并写入到buf中
            Self::read_one_block(cache_block_addr[index], index, buf);
        }
        return Ok(count);
    }

/// @brief 检查cache中是否有缺块的函数
/// 
/// # 参数：
/// - 'block_iter' :需要检查的块迭代器（因为块迭代器包含了需要读块的信息，所以传入块迭代器）
/// 
/// # 返回值：
/// - Ok(Vec<CacheBlockAddr>) :如果成功了，那么函数会返回每个块的Cache地址，利用Cache地址就可以访问Cache了
/// - Err(FailData) :如果发现了缺块，那么我们会返回所有缺块的信息（即FailData）
    fn check_able_to_read(block_iter: BlockIter) -> Result<Vec<CacheBlockAddr>, Vec<FailData>> {
        // unsafe {
        //     if !INITIAL_FLAG {
        //         Self::init()
        //     }
        // }
        let mut fail_ans = vec![];          //存放缺块信息的向量
        let mut success_ans = vec![]; //存放命中块地址的向量
        let mapper = unsafe {                //获取mapper
            match &CMAPPER {
                Some(x) => x,
                None => {
                    panic!("cache fail");
                }
            }
        };
        for (index,i) in block_iter.enumerate() {
            match mapper.find(i.iba_id()) {         //在mapper中寻找块的iba_id，判断是否命中
                Some(x) => {
                    success_ans.push(*x);                   
                    continue;
                }
                None => fail_ans.push(FailData::new(i.iba_id(), index)),    //缺块就放入fail_ans
                //缺块不break的原因是，我们需要把所有缺块都找出来，这样才能补上缺块
            }
        }
        if fail_ans.len() != 0 {    //只要有缺块就认为cache失败，因为需要补块就需要进行io操作
            return Err(fail_ans);
        } else {
            return Ok(success_ans);
        }
    }
/// @brief 在cache中读取一个块的数据并放置于缓存的指定位置
/// 
/// #参数：
/// - 'cache_block_addr' :表示需要读取的cache块的地址
/// - 'position' :表示该块的数据需要放置在buf的哪个位置，比如position为2，那么读出的数据将放置在buf\[1024..1536\](这里假设块大小是512)
/// - 'buf' :块数据的缓存
/// 
/// #返回值：
/// - Some(usize) :表示读取了多少个字节
/// - None :如果输入的cache_block_addr超过了cache的容量，那么将返回None（由于目前的cache不支持动态变化上限，所以可能出现这种错误）
    #[inline]
    pub fn read_one_block(
        cache_block_addr: CacheBlockAddr,
        position: usize,
        buf: &mut [u8],
    ) -> Option<usize> {
        let space = unsafe {    //获取管理cache空间的结构体
            match &CSPACE {
                Some(x) => x,
                None => return None,
            }
        };
        Some(space.read(cache_block_addr, position, buf)?)  
    }
/// @brief 根据缺块的数据和io获得的数据，向cache中补充块数据
/// 
/// #参数：
/// - 'f_data_vec' :这里输入的一般是从read函数中返回的缺块数据
/// - 'data' :经过一次io后获得的数据
/// 
/// #返回值：
/// Ok(usize) :表示补上缺页的个数
/// Err() :一般来说不会产生错误，这里产生错误的原因貌似只有插入时还没有初始化（一般也很难发生）
    pub fn insert(f_data_vec: Vec<FailData>, data: &[u8]) -> Result<usize, ()> {
        let count=f_data_vec.len();
        for i in f_data_vec {
            let index = i.index();
            Self::insert_one_block(
                i.lba_id(),
                data[index * BLOCK_SIZE..(index + 1) * BLOCK_SIZE].to_vec(),
            )?;
        }
        Ok(count)
    }

/// @brief 将一个块数据插入到cache中
/// 
/// #参数：
/// - 'lba_id' :表明该块对应的lba_id，用于建立映射
/// - 'data' :传入的数据
/// 
/// #返回值：
/// 由于目前没有安排systemError的位置，所以目前并没有有意义的返回值，但是后续可能需要进行异常处理之类的东西
    pub fn insert_one_block(lba_id: usize, data: Vec<u8>) -> Result<(),()> {
        // unsafe {
        //     if !INITIAL_FLAG {
        //         Self::init()
        //     }
        // }
        let space = unsafe {
            match &mut CSPACE {
                Some(x) => x,
                None => return Err(()),
            }
        };
        space.insert(lba_id, data)
    }
/// @brief 测试版本的写入操作，这里仅仅作为取消映射的方法，并没有真正写入到cache的功能
/// 
/// #参数：
/// - 'lba_id_start' :需要读取的连续块的起始块
/// - 'count' :需要读取块的个数
/// - '_data' :目前没有写入功能，该参数暂时无用
/// 
/// #返回值：
/// Ok(usize) :表示写入了多少个块
    pub fn test_write(lba_id_start: usize, count: usize, _data: &[u8]) -> Result<usize, ()> {
        // unsafe {
        //     if !INITIAL_FLAG {
        //         Self::init()
        //     }
        // }
        let mapper = unsafe {
            match &mut CMAPPER {
                Some(x) => x,
                None => return Err(()),
            }
        };
        let block_iter = BlockIter::new(lba_id_start, count, BLOCK_SIZE);
        for i in block_iter {
            mapper.remove(i.iba_id());
        }
        Ok(count)
    }
}

/// @brief 管理Cache空间的结构体
/// 
/// # 数据成员：
/// - 'root' :用于存放CacheBlock，是Cache数据的实际存储空间的向量
/// - 'frame_selector' :在块换出换入时，用于选择替换块的结构体
struct CacheSpace {
    root: Vec<CacheBlock>,
    frame_selector: Box<dyn FrameSelector>,
    // cache_mapper:CacheMapper,
}

impl CacheSpace {
    pub fn new() -> Self {
        Self {
            root: Vec::new(),
            frame_selector: Box::new(SimpleFrameSelector::new()),   //如果要修改替换算法，可以设计一个结构体实现FrameSelector trait，再在这里替换掉SimpleFrameSelector
            // cache_mapper: CacheMapper::new(),
        }
    }
/// @brief 将一个块的数据写入到buf的指定位置
/// 
/// #参数：
/// - 'addr' :请求块在Cache中的地址
/// - 'position' :表示需要将Cache放入buf中的位置，例如:若position为1，则块的数据放入buf\[512..1024\]
/// - 'buf' :存放数据的buf
/// 
/// #返回值：
/// Some(usize):表示读取的字节数（这里默认固定为BLOCK_SIZE）
/// None:如果你输入地址大于cache的最大上限，那么就返回None
    #[inline]
    pub fn read(&self, addr: CacheBlockAddr, position: usize, buf: &mut [u8]) -> Option<usize> {
        if addr > self.frame_selector.get_size() {
            return None;
        } else {
            return Some(
                self.root[addr.data()]  //CacheBlockAddr就是用于给root寻址的
                    .get_data(&mut buf[position * BLOCK_SIZE..(position + 1) * BLOCK_SIZE]),
            );
        }
    }
/// @brief 向cache空间中写入的函数，目前尚未实现
    pub fn _write(&mut self, _addr: CacheBlockAddr, _data: CacheBlock) -> Option<()> {
        todo!()
    }
/// @brief 向cache中插入一个块并建立lba_id到块之间的映射
/// 
/// #参数：
/// - 'lba_id' :表明你插入的块的lba_id，用于建立映射
/// - 'data' :要插入块的数据
/// 
/// #返回值：
/// todo：设计该函数的返回异常处理
    pub fn insert(&mut self, lba_id: usize, data: Vec<u8>) -> Result<(),()>{
        let data_block = CacheBlock::from_data(lba_id, data);   //CacheBlock是cached block的基本单位，这里使用data生成一个CacheBlock用于向Cache空间中插入块
        let mapper = unsafe {
            match &mut CMAPPER {
                Some(x) => x,
                None => return Err(()),
            }
        };
        if self.frame_selector.can_append() {   //这里我设计了cache的一个threshold，如果不超过阈值就可以append，否则只能替换
            //这是append的操作逻辑：
            let index = self.frame_selector.get_index_append(); //从frame_selector获得一个CacheBlockAddr
            self.root.push(data_block); //直接将块push进去就可以，因为现在是append操作
            assert!(index.data() == self.root.len() - 1);   
            mapper.insert(lba_id, index);   //建立mapper的映射
            Ok(())
        } else {
            //这是replace的操作逻辑
            let index = self.frame_selector.get_index_replace();    //从frame_selector获得一个CacheBlockAddr，这次是它替换出来的
            let removed_id = self.root[index.data()].get_lba_id();  //获取被替换的块的lba_id，待会用于取消映射

            self.root[index.data()] = data_block;   //直接替换原本的块，由于被替换的块没有引用了，所以会被drop
            mapper.insert(lba_id, index);   //建立映射插入块的映射
            mapper.remove(removed_id);      //取消被替换块的映射
            Ok(())
        }
    }
}

/// @brief 该结构体用于建立lba_id到cached块的映射
/// 
/// #数据成员：
/// - 'map' :执行键值对操作的map
struct CacheMapper {
    map: BTreeMap<usize, CacheBlockAddr>,
}

impl CacheMapper {
    pub fn new() -> Self {
        Self {
            map: BTreeMap::new(),
        }
    }
/// @brief 插入操作
    pub fn insert(&mut self, lba_id: usize, caddr: CacheBlockAddr) -> Option<()> {
        self.map.insert(lba_id, caddr)?;
        Some(())
    }
/// @brief 查找操作
    #[inline]
    pub fn find(&self, lba_id: usize) -> Option<&CacheBlockAddr> {
        self.map.get(&lba_id)
    }
/// @brief 去除操作
    pub fn remove(&mut self, lba_id: usize) {
        match self.map.remove(&lba_id) {
            Some(_) => {}
            None => {}
        }
    }
}

/// @brief 该trait用于实现块的换入换出算法，需要设计替换算法只需要实现该trait即可
trait FrameSelector{
    /// @brief 给出append操作的index（理论上，如果cache没满，就不需要换出块，就可以使用append操作）
    fn get_index_append(&mut self) -> CacheBlockAddr;
    /// @brief 给出replace操作后的index
    fn get_index_replace(&mut self) -> CacheBlockAddr;
    /// @brief 判断是否可以append
    fn can_append(&self) -> bool;
    /// @获取size
    fn get_size(&self) -> usize;
}

/// @brief 该结构体用于管理块的换入换出过程中，CacheBlockAddr的选择，替换算法在这里实现
/// 
/// #数据成员：
/// - 'threshold' :表示BlockCache的阈值，即最大可以存放多少块，这里目前还不支持动态变化
/// - 'size' :表示使用过的块帧的数量
/// - 'current' :这里使用从头至的替换算法，其替换策略为0，1，2，...，threshold，0，1...以此类推（该算法比FIFO还要简陋，后面可以再实现别的：）
struct SimpleFrameSelector {
    threshold: usize,
    size: usize,
    current: usize,
}

impl SimpleFrameSelector {
    pub fn new() -> Self {
        Self {
            threshold: CACHE_THRESHOLD*(1<<(20-BLOCK_SIZE_LOG)),    //这里定义了cache的threshold，见cache_config.rs
            size: 0,
            current: 0,
        }
    }
}

impl FrameSelector for SimpleFrameSelector{
    fn get_index_append(&mut self) -> CacheBlockAddr {
        let ans = self.current;
        self.size += 1;
        self.current += 1;
        self.current %= self.threshold;
        return CacheBlockAddr::new(ans);
    }

    fn get_index_replace(&mut self) -> CacheBlockAddr{
        let ans = self.current;
        self.current += 1;
        self.current %= self.threshold;
        return CacheBlockAddr::new(ans);
    }

    fn can_append(&self) -> bool {
        self.size < self.threshold
    }

    fn get_size(&self) -> usize {
        self.size
    }
}
