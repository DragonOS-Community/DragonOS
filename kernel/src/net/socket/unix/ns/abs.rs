use crate::libs::spinlock::SpinLock;
use crate::net::socket::Inode;
use alloc::string::String;
use alloc::sync::Arc;
use hashbrown::HashMap;
use ida::IdAllocator;
use system_error::SystemError;

lazy_static! {
    pub static ref ABSHANDLE_MAP: AbsHandleMap = AbsHandleMap::new();
}

lazy_static! {
    pub static ref INODE_MAP: SpinLock<HashMap<AbsHandle, Inode>> = SpinLock::new(HashMap::new());
}

static ABS_ADDRESS_ALLOCATOR: SpinLock<IdAllocator> =
    SpinLock::new(IdAllocator::new(0, (1 << 20) as usize).unwrap());

#[derive(Debug)]
pub struct AbsHandle(Arc<[u8]>);

impl AbsHandle {
    pub fn new(name: Arc<[u8]>) -> Self {
        Self(name)
    }

    pub fn name(&self) -> Arc<[u8]> {
        self.0.clone().into()
    }
}

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
    pub fn insert(&self, name: String) -> Result<(), SystemError> {
        let mut guard = self.abs_handle_map.lock();

        //检查name是否被占用
        if guard.contains_key(&name) {
            return Err(SystemError::ENOMEM);
        }

        let ads_addr = match self.alloc() {
            Some(addr) => addr,
            None => return Err(SystemError::ENOMEM),
        };
        guard.insert(name, ads_addr);
        return Ok(());
    }

    /// 抽象空间地址分配器
    ///
    /// ## 返回
    ///
    /// 分配到的可用地址
    pub fn alloc(&self) -> Option<Arc<AbsHandle>> {
        let abs_addr = match ABS_ADDRESS_ALLOCATOR.lock().alloc() {
            Some(addr) => addr as u32,
            //地址被分配
            None => return None,
        };

        //将分配到的abs_addr格式化为16进制的五位字符

        let ads_addr_fmt = format!("{:05x}", abs_addr);

        return Some(Arc::new(AbsHandle::new(Arc::from(ads_addr_fmt.as_bytes()))));
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
        let abs_addr = String::from_utf8(match self.look_up(&name) {
            Some(addr) => addr,
            None => return Err(SystemError::EINVAL),
        }
        .name()
        .to_vec())
        .expect("Failed to convert abs bytes to String");

        let parsed_abs_addr = 
            u32::from_str_radix(&abs_addr, 16)
            .expect("Failed to parse address!");

        //释放abs地址分配实例
        ABS_ADDRESS_ALLOCATOR.lock().free(parsed_abs_addr as usize);

        //释放entry
        let mut guard = self.abs_handle_map.lock();
        guard.remove(&name);
        
        Ok(())

    }
}
