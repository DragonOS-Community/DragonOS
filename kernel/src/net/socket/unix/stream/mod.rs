use core::sync::atomic::AtomicBool;

use alloc::{sync::{Arc,Weak}, vec::Vec};
use inner::{Init, Inner, Listener};
use system_error::SystemError;

use crate::{
    filesystem::vfs::IndexNode, include::bindings::bindings::{EINVAL, EISCONN}, libs::{rwlock::RwLock, spinlock::SpinLock}, net::{
        socket::{
            common::{
                poll_unit::{EPollItems, WaitQueue},
                Shutdown,
            },
            Socket,
        },
        Endpoint,
    }
};

pub mod inner;

#[derive(Debug)]
pub struct StreamSocket {
    buffer: Arc<SpinLock<Vec<u8>>>,
    inner: RwLock<Option<Inner>>,
    shutdown: Shutdown,
    nonblock: AtomicBool,
    epitems: EPollItems,
    wait_queue: WaitQueue,
    self_ref: Weak<Self>,
}

impl StreamSocket {
    /// 默认的元数据缓冲区大小
    pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
    /// 默认的缓冲区大小
    pub const DEFAULT_BUF_SIZE: usize = 64 * 1024;

    pub fn new() -> Self {
        todo!();
    }

    pub fn bind(&self, local_endpoint: Endpoint) -> Result<(), SystemError> {
        let mut guard = self.inner.write();
        match guard.take().expect("Unix Stream Socket is None") {
            Inner::Init(mut inner) => inner.bind(local_endpoint),
            _ => Err(SystemError::EINVAL),
        }
    }

    pub fn do_listen(&self, backlog: usize) -> Result<(), SystemError> {
        let inner = self.inner.write();
        let addr = match inner.take().expect("Unix Stream Socket is None") {
            Inner::Init(init) => init.addr().unwrap(),
            Inner::Connected(_) => {
                return Err(SystemError::EINVAL);
            }
            Inner::Listener(listener) => {
                return listener.listen(backlog);
            }
        };

        let listener = Listener::new(Some(addr), backlog);
        inner.replace(Listener(listener));
        return Ok(());
    }

    pub fn connect(&self, remote_endpoint: Endpoint) -> Result<(), SystemError> {
        let mut client = self.inner.write();
        let client_endpoint = match client.take() {
            Inner::Init(inner) => inner.addr().unwrap(),
            Inner::Connected(_) => Err(EISCONN),
            Inner::Listener(_) => Err(EINVAL),
        };

    }

    pub fn accept(&self) -> Result<(), SystemError> {
        todo!();
    }

    pub fn inner(&self) -> RwLock<Option<Inner>> {
        return self.inner;
    }

    pub fn write(buf: &[u8], remote_endpoint: Endpoint) -> Result<(), SystemError> {
        todo!();
    }

    pub fn read(&mut buf: &mut [u8]) -> Result<(), SystemError> {
        todo!();
    }

    pub fn write_state<F>(&self, mut f: F) -> Result<(), SystemError>
    where
        F: FnMut(Inner) -> Result<Inner, SystemError>,
    {
        let mut inner_guard = self.inner.write();
        let inner = inner_guard.take().expect("Unix Stream Inner is None");
        let update = f(inner)?;
        inner_guard.replace(update);
        Ok(())
    }
}

impl IndexNode for StreamSocket {
    fn open(
        &self,
        _data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
        _mode: &crate::filesystem::vfs::file::FileMode,
    ) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
    }

    fn close(&self, _data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
    }

    fn poll(&self, _private_data: &crate::filesystem::vfs::FilePrivateData) -> Result<usize, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
    }

    fn metadata(&self) -> Result<crate::filesystem::vfs::Metadata, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
    }

    fn set_metadata(&self, _metadata: &crate::filesystem::vfs::Metadata) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
    }

    fn resize(&self, _len: usize) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
    }

    fn create(
        &self,
        name: &str,
        file_type: crate::filesystem::vfs::FileType,
        mode: crate::filesystem::vfs::syscall::ModeType,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 若文件系统没有实现此方法，则默认调用其create_with_data方法。如果仍未实现，则会得到一个Err(-ENOSYS)的返回值
        return self.create_with_data(name, file_type, mode, 0);
    }

    fn create_with_data(
        &self,
        _name: &str,
        _file_type: crate::filesystem::vfs::FileType,
        _mode: crate::filesystem::vfs::syscall::ModeType,
        _data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
    }

    fn link(&self, _name: &str, _other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
    }

    fn unlink(&self, _name: &str) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
    }

    fn rmdir(&self, _name: &str) -> Result<(), SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn move_to(
        &self,
        _old_name: &str,
        _target: &Arc<dyn IndexNode>,
        _new_name: &str,
    ) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
    }

    fn find(&self, _name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
    }

    fn get_entry_name(&self, _ino: crate::filesystem::vfs::InodeId) -> Result<std::string::String, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
    }

    fn get_entry_name_and_metadata(&self, ino: crate::filesystem::vfs::InodeId) -> Result<(std::string::String, crate::filesystem::vfs::Metadata), SystemError> {
        // 如果有条件，请在文件系统中使用高效的方式实现本接口，而不是依赖这个低效率的默认实现。
        let name = self.get_entry_name(ino)?;
        let entry = self.find(&name)?;
        return Ok((name, entry.metadata()?));
    }

    fn ioctl(
        &self,
        _cmd: u32,
        _data: usize,
        _private_data: &crate::filesystem::vfs::FilePrivateData,
    ) -> Result<usize, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
    }

    fn kernel_ioctl(
        &self,
        _arg: Arc<dyn crate::net::event_poll::KernelIoctlData>,
        _data: &crate::filesystem::vfs::FilePrivateData,
    ) -> Result<usize, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn mount(&self, _fs: Arc<dyn crate::filesystem::vfs::FileSystem>) -> Result<Arc<crate::filesystem::vfs::MountFS>, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn mount_from(&self, _des: Arc<dyn IndexNode>) -> Result<Arc<crate::filesystem::vfs::MountFS>, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn umount(&self) -> Result<Arc<crate::filesystem::vfs::MountFS>, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn absolute_path(&self) -> Result<std::string::String, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn truncate(&self, _len: usize) -> Result<(), SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn sync(&self) -> Result<(), SystemError> {
        return Ok(());
    }

    fn mknod(
        &self,
        _filename: &str,
        _mode: crate::filesystem::vfs::syscall::ModeType,
        _dev_t: crate::driver::base::device::device_number::DeviceNumber,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn mkdir(&self, name: &str, mode: crate::filesystem::vfs::syscall::ModeType) -> Result<Arc<dyn IndexNode>, SystemError> {
        match self.find(name) {
            Ok(inode) => {
                if inode.metadata()?.file_type == crate::filesystem::vfs::FileType::Dir {
                    Ok(inode)
                } else {
                    Err(SystemError::EEXIST)
                }
            }
            Err(SystemError::ENOENT) => self.create(name, crate::filesystem::vfs::FileType::Dir, mode),
            Err(err) => Err(err),
        }
    }

    fn special_node(&self) -> Option<crate::filesystem::vfs::SpecialNodeData> {
        None
    }

    fn dname(&self) -> Result<crate::filesystem::vfs::utils::DName, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        return self.find("..");
    }
    
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        todo!()
    }
    
    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: crate::libs::spinlock::SpinLockGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        todo!()
    }
    
    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        todo!()
    }
    
    fn as_any_ref(&self) -> &dyn core::any::Any {
        todo!()
    }
    
    fn list(&self) -> Result<Vec<std::string::String>, SystemError> {
        todo!()
    }
}

impl Socket for StreamSocket {
    fn write_buffer(&self, buf: &[u8]) -> Result<usize, SystemError> {
        todo!();
    }

    fn epoll_items(&self) -> &super::common::poll_unit::EPollItems {
        todo!()
    }

    fn wait_queue(&self) -> &super::common::poll_unit::WaitQueue {
        todo!()
    }

    fn update_io_events(&self) -> Result<crate::net::event_poll::EPollEventType, SystemError> {
        todo!()
    }
    
    fn connect(&self, _endpoint: Endpoint) -> Result<(), SystemError> {
        let remote_socket = match _endpoint {
            Endpoint::Inode(socket) => socket,
            _ => return Err(SystemError::EINVAL),
        };

        let remote_stream_socket: Arc<StreamSocket> = Arc::clone(&remote_socket).arc_any().downcast().map_err(|_| SystemError::EINVAL)?;

        Ok(())
    }
    
    fn bind(&self, _endpoint: Endpoint) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }
    
    fn shutdown(&self, _type: Shutdown) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }
    
    fn listen(&self, _backlog: usize) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }
    
    fn accept(&self) -> Result<(Arc<dyn crate::filesystem::vfs::IndexNode>, Endpoint), SystemError> {
        Err(SystemError::ENOSYS)
    }
    
    fn endpoint(&self) -> Option<Endpoint> {
        None
    }
    
    fn peer_endpoint(&self) -> Option<Endpoint> {
        None
    }
    
    fn set_option(
        &self,
        _level: crate::net::socket::SocketOptionsLevel,
        _optname: usize,
        _optval: &[u8],
    ) -> Result<(), SystemError> {
        log::warn!("setsockopt is not implemented");
        Ok(())
    }
    
    fn poll(&self, _private_data: &crate::filesystem::vfs::FilePrivateData) -> Result<usize, SystemError> {
        Ok(self.update_io_events()?.bits() as usize)
    }
    
    fn as_any(&self) -> &dyn core::any::Any {
        todo!()
    }
}
