use crate::libs::spinlock::SpinLock;
use crate::net::socket::Endpoint;
use alloc::string::String;
use alloc::sync::Arc;
use hashbrown::HashMap;
use ida::IdAllocator;
use system_error::SystemError;

lazy_static! {
    pub static ref ABSHANDLE_MAP: AbsHandleMap = AbsHandleMap::new();
}

lazy_static! {
    pub static ref ABS_INODE_MAP: SpinLock<HashMap<usize, Endpoint>> = SpinLock::new(HashMap::new());
}

static ABS_ADDRESS_ALLOCATOR: SpinLock<IdAllocator> =
    SpinLock::new(IdAllocator::new(0, (1 << 20) as usize).unwrap());

#[derive(Debug)]
pub struct AbsHandle(usize);

impl AbsHandle {
    pub fn new(name: usize) -> Self {
        Self(name)
    }

    pub fn name(&self) -> usize {
        self.0
    }
}

/// 抽象地址映射表
///
/// 负责管理抽象命名空间内的地址
pub struct AbsHandleMap {
    abs_handle_map: SpinLock<HashMap<String, Arc<AbsHandle>>>,
}

impl AbsHandleMap {
    pub fn new() -> Self {
        Self {
            abs_handle_map: SpinLock::new(HashMap::new()),
        }
    }

    /// 插入新的地址映射
    pub fn insert(&self, name: String) -> Result<Arc<AbsHandle>, SystemError> {
        let mut guard = self.abs_handle_map.lock();

        //检查name是否被占用
        if guard.contains_key(&name) {
            return Err(SystemError::ENOMEM);
        }

        let ads_addr = match self.alloc() {
            Some(addr) => addr.clone(),
            None => return Err(SystemError::ENOMEM),
        };
        guard.insert(name, ads_addr.clone());
        return Ok(ads_addr);
    }

    /// 抽象空间地址分配器
    ///
    /// ## 返回
    ///
    /// 分配到的可用地址
    pub fn alloc(&self) -> Option<Arc<AbsHandle>> {
        let abs_addr = match ABS_ADDRESS_ALLOCATOR.lock().alloc() {
            Some(addr) => addr,
            //地址被分配
            None => return None,
        };

        //将分配到的abs_addr格式化为16进制的五位字符
        return Some(Arc::new(AbsHandle::new(abs_addr)));
    }

    /// 进行地址映射
    ///
    /// ## 参数
    ///
    /// name：用户定义的地址
    pub fn look_up(&self, name: &String) -> Option<Arc<AbsHandle>> {
        let guard = self.abs_handle_map.lock();
        return guard.get(name).cloned();
    }

    /// 移除绑定的地址
    ///
    /// ## 参数
    ///
    /// name：待删除的地址
    pub fn remove(&self, name: String) -> Result<(), SystemError> {
        let abs_addr = match look_up_abs_addr(&name) {
            Ok(result) => result.name(),
            Err(_) => return Err(SystemError::EINVAL),
        };

        //释放abs地址分配实例
        ABS_ADDRESS_ALLOCATOR.lock().free(abs_addr);

        //释放entry
        let mut guard = self.abs_handle_map.lock();
        guard.remove(&name);

        Ok(())
    }
}

/// 分配抽象地址
///
/// ## 返回
///
/// 分配到的抽象地址
pub fn alloc_abs_addr(name: String) -> Result<Arc<AbsHandle>, SystemError> {
    match ABSHANDLE_MAP.insert(name) {
        Ok(result) => return Ok(result),
        Err(e) => return Err(e),
    }
}

/// 查找抽象地址
///
/// ## 参数
///
/// name：用户socket字符地址
///
/// ## 返回
///
/// 查询到的抽象地址
pub fn look_up_abs_addr(name: &String) -> Result<Arc<AbsHandle>, SystemError> {
    match ABSHANDLE_MAP.look_up(name) {
        Some(result) => return Ok(result),
        None => return Err(SystemError::EINVAL),
    }
}
