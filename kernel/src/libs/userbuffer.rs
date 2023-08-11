use crate::{include::bindings::bindings::verify_area, syscall::SystemError};

#[derive(Debug)]
pub struct UserBufferWriter<T> {
    addr: *mut T,
    len: usize,
}

#[derive(Debug)]
pub struct UserBufferReader<T> {
    addr: *const T,
    len: usize,
}

impl<T: core::marker::Copy> UserBufferReader<T> {
    /// 构造一个指向用户空间位置的BufferReader
    ///
    /// @param addr 用户空间指针
    /// @param len 是元素数量，不是byte长度
    /// @return 构造成功返回UserbufferReader实例，否则返回错误码
    ///
    pub fn new(addr: *const T, len: usize) -> Result<Self, SystemError> {
        if unsafe { !verify_area(addr as u64, (len * core::mem::size_of::<T>()) as u64) } {
            return Err(SystemError::EFAULT);
        }
        return Ok(Self { addr, len });
    }

    /// 从用户空间读取数据(到变量中)
    ///
    /// @return 返回用户空间数据的切片(对单个结构体就返回长度为一的切片)
    ///
    pub fn read_from_user(&self) -> Result<&[T], SystemError> {
        let items: &[T] = unsafe { core::slice::from_raw_parts(self.addr, self.len) };
        return Ok(items);
    }

    /// 从用户空间拷贝数据(到指定地址中)
    ///
    /// @param dst 目标地址指针
    /// @return 拷贝成功的话返回拷贝的元素数量
    ///
    pub fn copy_from_user(&self, dst: &mut [T]) -> Result<usize, SystemError> {
        let src: &[T] = unsafe { core::slice::from_raw_parts(self.addr, self.len) };
        dst.copy_from_slice(&src);
        return Ok(src.len());
    }
}

impl<T: core::marker::Copy> UserBufferWriter<T> {
    /// 构造一个指向用户空间位置的BufferWriter
    ///
    /// @param addr 用户空间指针
    /// @param len 是元素数量，不是byte长度
    /// @return 构造成功返回UserbufferWriter实例，否则返回错误码
    ///
    pub fn new(addr: *mut T, len: usize) -> Result<Self, SystemError> {
        if unsafe { !verify_area(addr as u64, (len * core::mem::size_of::<T>()) as u64) } {
            return Err(SystemError::EFAULT);
        }
        return Ok(Self { addr, len });
    }

    /// 从结构体写入数据到用户空间
    ///
    /// @param data 要写入的数据(如果是单个对象，也封装成只有一个元素的切片)
    /// @return Result<(), SystemError>
    ///
    pub fn write_to_user(&self, data: &[T]) -> Result<(), SystemError> {
        let buf = unsafe { core::slice::from_raw_parts_mut(self.addr, self.len) };
        buf.copy_from_slice(data);
        return Ok(());
    }

    /// 从指定地址写入数据到用户空间
    ///
    /// @param data 要写入的数据地址
    /// @return 返回写入元素的数量
    ///
    pub fn copy_to_user(&self, src: &[T]) -> Result<usize, SystemError> {
        let dst: &mut [T] = unsafe { core::slice::from_raw_parts_mut(self.addr, self.len) };
        dst.copy_from_slice(&src);
        return Ok(src.len());
    }
}
// 调用方式一：数组
//     pub unsafe fn write_to_user(
//         &self,
//         addr: *mut SockAddr,
//         addr_len: *mut u32,
//     ) -> Result<usize, SystemError> {
//         // 当用户传入的地址或者长度为空时，直接返回0
//         if addr.is_null() || addr_len.is_null() {
//             return Ok(0);
//         }
//         // 检查用户传入的地址是否合法
//         if !verify_area(
//             addr as usize as u64,
//             core::mem::size_of::<SockAddr>() as u64,
//         ) || !verify_area(addr_len as usize as u64, core::mem::size_of::<u32>() as u64)
//         {
//             return Err(SystemError::EFAULT);
//         }

//         let to_write = min(self.len()?, *addr_len as usize);
//         if to_write > 0 {
//             let buf = core::slice::from_raw_parts_mut(addr as *mut u8, to_write);
//             buf.copy_from_slice(core::slice::from_raw_parts(
//                 self as *const SockAddr as *const u8,
//                 to_write,
//             ));
//         }
//         *addr_len = self.len()? as u32;
//         return Ok(to_write);
//     }
// }

// 调用方式二：结构体(Cstr->str)
// SYS_UNLINK_AT => {
//                 let dirfd = args[0] as i32;
//                 let pathname = args[1] as *const c_char;
//                 let flags = args[2] as u32;
//                 if from_user && unsafe { !verify_area(pathname as u64, PAGE_4K_SIZE as u64) } {
//                     Err(SystemError::EFAULT)
//                 } else if pathname.is_null() {
//                     Err(SystemError::EFAULT)
//                 } else {
//                     let get_path = || {
//                         let pathname: &CStr = unsafe { CStr::from_ptr(pathname) };
//                         let pathname: &str = pathname.to_str().map_err(|_| SystemError::EINVAL)?;
//                         if pathname.len() >= MAX_PATHLEN {
//                             return Err(SystemError::ENAMETOOLONG);
//                         }
//                         return Ok(pathname.trim());
//                     };
//                     let pathname = get_path();
//                     if pathname.is_err() {
//                         Err(pathname.unwrap_err())
//                     } else {
//                         Self::unlinkat(dirfd, pathname.unwrap(), flags)
//                     }
//                 }
//             }

//调用方式二：结构体 Iovec
// pub unsafe fn from_user(
//         iov: *const IoVec,
//         iovcnt: usize,
//         _readv: bool,
//     ) -> Result<Self, SystemError> {
//         // 检查iov指针所在空间是否合法
//         if !verify_area(
//             iov as usize as u64,
//             (iovcnt * core::mem::size_of::<IoVec>()) as u64,
//         ) {
//             return Err(SystemError::EFAULT);
//         }

//         // 将用户空间的IoVec转换为引用（注意：这里的引用是静态的，因为用户空间的IoVec不会被释放）
//         let iovs: &[IoVec] = core::slice::from_raw_parts(iov, iovcnt);

//         let mut slices: Vec<&mut [u8]> = vec![];
//         slices.reserve(iovs.len());

//         for iov in iovs.iter() {
//             if iov.iov_len == 0 {
//                 continue;
//             }

//             if !verify_area(iov.iov_base as usize as u64, iov.iov_len as u64) {
//                 return Err(SystemError::EFAULT);
//             }

//             slices.push(core::slice::from_raw_parts_mut(iov.iov_base, iov.iov_len));
//         }

//         return Ok(Self(slices));
//     }
