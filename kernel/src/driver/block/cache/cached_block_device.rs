use alloc::{boxed::Box, vec::Vec};
use hashbrown::HashMap;
use log::debug;

use crate::{driver::base::block::block_device::BlockId, libs::rwlock::RwLock};

use super::{
    cache_block::{CacheBlock, CacheBlockAddr},
    cache_iter::{BlockIter, FailData},
    BlockCacheError, BLOCK_SIZE, BLOCK_SIZE_LOG, CACHE_THRESHOLD,
};

static mut CSPACE: Option<LockedCacheSpace> = None;
static mut CMAPPER: Option<LockedCacheMapper> = None;
/// # 结构功能
/// 该结构体向外提供BlockCache服务
pub struct BlockCache;

#[allow(static_mut_refs)]
unsafe fn mapper() -> Result<&'static mut LockedCacheMapper, BlockCacheError> {
    unsafe {
        match &mut CMAPPER {
            Some(x) => return Ok(x),
            None => return Err(BlockCacheError::StaticParameterError),
        }
    };
}

#[allow(static_mut_refs)]
unsafe fn space() -> Result<&'static mut LockedCacheSpace, BlockCacheError> {
    unsafe {
        match &mut CSPACE {
            Some(x) => return Ok(x),
            None => return Err(BlockCacheError::StaticParameterError),
        }
    };
}

impl BlockCache {
    /// # 函数的功能
    /// 初始化BlockCache需要的结构体
    pub fn init() {
        unsafe {
            CSPACE = Some(LockedCacheSpace::new(CacheSpace::new()));
            CMAPPER = Some(LockedCacheMapper::new(CacheMapper::new()));
        }
        debug!("BlockCache Initialized!");
    }
    /// # 函数的功能
    /// 使用blockcache进行对块设备进行连续块的读操作
    ///
    /// ## 参数：
    /// - 'lba_id_start' :连续块的起始块的lba_id
    /// - 'count' :从连续块算起需要读多少块
    /// - 'buf' :读取出来的数据存放在buf中
    ///
    /// ## 返回值：
    /// - Ok(usize) :表示读取块的个数
    /// - Err(BlockCacheError::BlockFaultError) :缺块的情况下，返回读取失败的块的数据，利用该返回值可以帮助blockcache插入读取失败的块值（见insert函数）
    /// - Err(BlockCacheError::____) :不缺块的情况往往是初始化或者其他问题，这种异常会在block_device中得到处理
    pub fn read(
        lba_id_start: BlockId,
        count: usize,
        buf: &mut [u8],
    ) -> Result<usize, BlockCacheError> {
        // 生成一个块迭代器（BlockIter），它可以迭代地给出所有需要块的数据，其中就包括lba_id
        let block_iter = BlockIter::new(lba_id_start, count, BLOCK_SIZE);
        // 调用检查函数，检查有无缺块，如果没有就可以获得所有块的Cache地址。如果失败了就直接返回FailData向量
        let cache_block_addr = Self::check_able_to_read(block_iter)?;
        // 块地址vec的长度应当等于块迭代器的大小
        assert!(cache_block_addr.len() == block_iter.count());
        // 迭代地读取cache并写入到buf中
        for (index, _) in block_iter.enumerate() {
            Self::read_one_block(cache_block_addr[index], index, buf)?;
        }
        return Ok(count);
    }

    /// # 函数的功能
    /// 检查cache中是否有缺块的函数
    ///
    /// ## 参数：
    /// - 'block_iter' :需要检查的块迭代器（因为块迭代器包含了需要读块的信息，所以传入块迭代器）
    ///
    /// ## 返回值：
    /// - Ok(Vec<CacheBlockAddr>) :如果成功了，那么函数会返回每个块的Cache地址，利用Cache地址就可以访问Cache了
    /// - Err(BlockCacheError::BlockFaultError) :如果发现了缺块，那么我们会返回所有缺块的信息（即FailData）
    /// - Err(BlockCacheError::____) :不缺块的情况往往是初始化或者其他问题                  
    fn check_able_to_read(block_iter: BlockIter) -> Result<Vec<CacheBlockAddr>, BlockCacheError> {
        // 存放缺块信息的向量
        let mut fail_ans = vec![];
        // 存放命中块地址的向量
        let mut success_ans = vec![];
        // 获取mapper
        let mapper = unsafe { mapper()? };
        for (index, i) in block_iter.enumerate() {
            // 在mapper中寻找块的lba_id，判断是否命中
            match mapper.find(i.lba_id()) {
                Some(x) => {
                    success_ans.push(x);
                    continue;
                }
                // 缺块就放入fail_ans
                None => fail_ans.push(FailData::new(i.lba_id(), index)),
                // 缺块不break的原因是，我们需要把所有缺块都找出来，这样才能补上缺块
            }
        }
        // 只要有缺块就认为cache失败，因为需要补块就需要进行io操作
        if !fail_ans.is_empty() {
            return Err(BlockCacheError::BlockFaultError(fail_ans));
        } else {
            return Ok(success_ans);
        }
    }
    /// # 函数的功能
    /// 在cache中读取一个块的数据并放置于缓存的指定位置
    ///
    /// ## 参数：
    /// - 'cache_block_addr' :表示需要读取的cache块的地址
    /// - 'position' :表示该块的数据需要放置在buf的哪个位置，比如position为2，那么读出的数据将放置在buf\[1024..1536\](这里假设块大小是512)
    /// - 'buf' :块数据的缓存
    ///
    /// ## 返回值：
    /// - Ok(usize) :表示读取了多少个字节
    /// - Err(BlockCacheError) :如果输入的cache_block_addr超过了cache的容量，那么将返回Err（由于目前的cache不支持动态变化上限，所以可能出现这种错误;而实际上，由于Cache的地址是由frame_selector给出的,所以正确实现的frame_selector理论上不会出现这种错误）
    fn read_one_block(
        cache_block_addr: CacheBlockAddr,
        position: usize,
        buf: &mut [u8],
    ) -> Result<usize, BlockCacheError> {
        let space = unsafe { space()? };
        space.read(cache_block_addr, position, buf)
    }
    /// # 函数的功能
    /// 根据缺块的数据和io获得的数据，向cache中补充块数据
    ///
    /// ## 参数：
    /// - 'f_data_vec' :这里输入的一般是从read函数中返回的缺块数据
    /// - 'data' :经过一次io后获得的数据
    ///
    /// ## 返回值：
    /// Ok(usize) :表示补上缺页的个数
    /// Err(BlockCacheError) :一般来说不会产生错误，这里产生错误的原因只有插入时还没有初始化（一般也很难发生）
    pub fn insert(f_data_vec: Vec<FailData>, data: &[u8]) -> Result<usize, BlockCacheError> {
        let count = f_data_vec.len();
        for i in f_data_vec {
            let index = i.index();
            Self::insert_one_block(
                i.lba_id(),
                data[index * BLOCK_SIZE..(index + 1) * BLOCK_SIZE].to_vec(),
            )?;
        }
        Ok(count)
    }

    /// # 函数的功能
    /// 将一个块数据插入到cache中
    ///
    /// ## 参数：
    /// - 'lba_id' :表明该块对应的lba_id，用于建立映射
    /// - 'data' :传入的数据
    ///
    /// ## 返回值：
    /// Ok(()):表示插入成功
    /// Err(BlockCacheError) :一般来说不会产生错误，这里产生错误的原因只有插入时还没有初始化（一般也很难发生）
    fn insert_one_block(lba_id: BlockId, data: Vec<u8>) -> Result<(), BlockCacheError> {
        let space = unsafe { space()? };
        space.insert(lba_id, data)
    }
    /// # 函数的功能
    /// 立即回写，这里仅仅作为取消映射的方法，并没有真正写入到cache的功能
    ///
    /// ## 参数：
    /// - 'lba_id_start' :需要读取的连续块的起始块
    /// - 'count' :需要读取块的个数
    /// - '_data' :目前没有写入功能，该参数暂时无用
    ///
    /// ## 返回值：
    /// Ok(usize) :表示写入了多少个块
    /// Err(BlockCacheError) :这里产生错误的原因只有插入时还没有初始化
    pub fn immediate_write(
        lba_id_start: BlockId,
        count: usize,
        _data: &[u8],
    ) -> Result<usize, BlockCacheError> {
        let mapper = unsafe { mapper()? };
        let block_iter = BlockIter::new(lba_id_start, count, BLOCK_SIZE);
        for i in block_iter {
            mapper.remove(i.lba_id());
        }
        Ok(count)
    }
}

struct LockedCacheSpace(RwLock<CacheSpace>);

impl LockedCacheSpace {
    pub fn new(space: CacheSpace) -> Self {
        LockedCacheSpace(RwLock::new(space))
    }

    pub fn read(
        &self,
        addr: CacheBlockAddr,
        position: usize,
        buf: &mut [u8],
    ) -> Result<usize, BlockCacheError> {
        self.0.read().read(addr, position, buf)
    }

    pub fn _write(&mut self, _addr: CacheBlockAddr, _data: CacheBlock) -> Option<()> {
        todo!()
    }

    pub fn insert(&mut self, lba_id: BlockId, data: Vec<u8>) -> Result<(), BlockCacheError> {
        unsafe { self.0.get_mut().insert(lba_id, data) }
    }
}

/// # 结构功能
/// 管理Cache空间的结构体
struct CacheSpace {
    /// 用于存放CacheBlock，是Cache数据的实际存储空间的向量
    root: Vec<CacheBlock>,
    /// 在块换出换入时，用于选择替换块的结构体
    frame_selector: Box<dyn FrameSelector>,
}

impl CacheSpace {
    pub fn new() -> Self {
        Self {
            root: Vec::new(),
            // 如果要修改替换算法，可以设计一个结构体实现FrameSelector trait，再在这里替换掉SimpleFrameSelector
            frame_selector: Box::new(SimpleFrameSelector::new()),
        }
    }
    /// # 函数的功能
    /// 将一个块的数据写入到buf的指定位置
    ///
    /// ## 参数：
    /// - 'addr' :请求块在Cache中的地址
    /// - 'position' :表示需要将Cache放入buf中的位置，例如:若position为1，则块的数据放入buf\[512..1024\]
    /// - 'buf' :存放数据的buf
    ///
    /// ## 返回值：
    /// Some(usize):表示读取的字节数（这里默认固定为BLOCK_SIZE）
    /// Err(BlockCacheError):如果你输入地址大于cache的最大上限，那么就返回InsufficientCacheSpace
    pub fn read(
        &self,
        addr: CacheBlockAddr,
        position: usize,
        buf: &mut [u8],
    ) -> Result<usize, BlockCacheError> {
        if addr > self.frame_selector.size() {
            return Err(BlockCacheError::InsufficientCacheSpace);
        } else {
            // CacheBlockAddr就是用于给root寻址的
            return self.root[addr]
                .data(&mut buf[position * BLOCK_SIZE..(position + 1) * BLOCK_SIZE]);
        }
    }
    /// # 函数的功能
    /// 向cache空间中写入的函数，目前尚未实现
    pub fn _write(&mut self, _addr: CacheBlockAddr, _data: CacheBlock) -> Option<()> {
        todo!()
    }
    /// # 函数的功能
    /// 向cache中插入一个块并建立lba_id到块之间的映射
    ///
    /// ## 参数：
    /// - 'lba_id' :表明你插入的块的lba_id，用于建立映射
    /// - 'data' :要插入块的数据
    ///
    /// ## 返回值：
    /// Ok(())
    pub fn insert(&mut self, lba_id: BlockId, data: Vec<u8>) -> Result<(), BlockCacheError> {
        // CacheBlock是cached block的基本单位，这里使用data生成一个CacheBlock用于向Cache空间中插入块
        let data_block = CacheBlock::from_data(lba_id, data);
        let mapper = unsafe { mapper()? };
        // 这里我设计了cache的一个threshold，如果不超过阈值就可以append，否则只能替换
        if self.frame_selector.can_append() {
            // 这是append的操作逻辑：
            // 从frame_selector获得一个CacheBlockAddr
            let index = self.frame_selector.index_append();
            // 直接将块push进去就可以，因为现在是append操作
            self.root.push(data_block);
            assert!(index == self.root.len() - 1);
            // 建立mapper的映射
            mapper.insert(lba_id, index);
            Ok(())
        } else {
            // 这是replace的操作逻辑
            // 从frame_selector获得一个CacheBlockAddr，这次是它替换出来的
            let index = self.frame_selector.index_replace();
            // 获取被替换的块的lba_id，待会用于取消映射
            let removed_id = self.root[index].lba_id();
            // 直接替换原本的块，由于被替换的块没有引用了，所以会被drop
            self.root[index] = data_block;
            // 建立映射插入块的映射
            mapper.insert(lba_id, index);
            // 取消被替换块的映射
            mapper.remove(removed_id);
            Ok(())
        }
    }
}

struct LockedCacheMapper {
    lock: RwLock<CacheMapper>,
}

impl LockedCacheMapper {
    pub fn new(inner: CacheMapper) -> Self {
        Self {
            lock: RwLock::new(inner),
        }
    }

    pub fn insert(&mut self, lba_id: BlockId, caddr: CacheBlockAddr) -> Option<()> {
        unsafe { self.lock.get_mut().insert(lba_id, caddr) }
    }

    pub fn find(&self, lba_id: BlockId) -> Option<CacheBlockAddr> {
        self.lock.read().find(lba_id)
    }

    pub fn remove(&mut self, lba_id: BlockId) {
        unsafe { self.lock.get_mut().remove(lba_id) }
    }
}

/// # 结构功能
/// 该结构体用于建立lba_id到cached块的映射
struct CacheMapper {
    // 执行键值对操作的map
    map: HashMap<BlockId, CacheBlockAddr>,
}

impl CacheMapper {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }
    /// # 函数的功能
    /// 插入操作
    pub fn insert(&mut self, lba_id: BlockId, caddr: CacheBlockAddr) -> Option<()> {
        self.map.insert(lba_id, caddr)?;
        Some(())
    }
    /// # 函数的功能
    /// 查找操作
    #[inline]
    pub fn find(&self, lba_id: BlockId) -> Option<CacheBlockAddr> {
        Some(*self.map.get(&lba_id)?)
    }
    /// # 函数的功能
    /// 去除操作
    pub fn remove(&mut self, lba_id: BlockId) {
        self.map.remove(&lba_id);
    }
}

/// # 结构功能
/// 该trait用于实现块的换入换出算法，需要设计替换算法只需要实现该trait即可
trait FrameSelector {
    /// # 函数的功能
    /// 给出append操作的index（理论上，如果cache没满，就不需要换出块，就可以使用append操作）
    fn index_append(&mut self) -> CacheBlockAddr;
    /// # 函数的功能
    /// 给出replace操作后的index
    fn index_replace(&mut self) -> CacheBlockAddr;
    /// # 函数的功能
    /// 判断是否可以append
    fn can_append(&self) -> bool;
    /// # 函数的功能
    /// 获取size
    fn size(&self) -> usize;
}

/// # 结构功能
/// 该结构体用于管理块的换入换出过程中，CacheBlockAddr的选择，替换算法在这里实现
struct SimpleFrameSelector {
    // 表示BlockCache的阈值，即最大可以存放多少块，这里目前还不支持动态变化
    threshold: usize,
    // 表示使用过的块帧的数量
    size: usize,
    // 这里使用从头至尾的替换算法，其替换策略为0，1，2，...，threshold，0，1...以此类推（该算法比FIFO还要简陋，后面可以再实现别的：）
    current: usize,
}

impl SimpleFrameSelector {
    pub fn new() -> Self {
        Self {
            threshold: CACHE_THRESHOLD * (1 << (20 - BLOCK_SIZE_LOG)),
            size: 0,
            current: 0,
        }
    }
}

impl FrameSelector for SimpleFrameSelector {
    fn index_append(&mut self) -> CacheBlockAddr {
        let ans = self.current;
        self.size += 1;
        self.current += 1;
        self.current %= self.threshold;
        return ans;
    }

    fn index_replace(&mut self) -> CacheBlockAddr {
        let ans = self.current;
        self.current += 1;
        self.current %= self.threshold;
        return ans;
    }

    fn can_append(&self) -> bool {
        self.size < self.threshold
    }

    fn size(&self) -> usize {
        self.size
    }
}
