use alloc::sync::{Arc, Weak};

use crate::{
    filesystem::{
        devfs::{DeviceINode, DevFS},
        vfs::{file::FileMode, FilePrivateData, IndexNode},
    },
    kerror, libs::rwlock::RwLock, syscall::SystemError,
};

use super::{TtyCore, TtyError, TtyFileFlag, TtyFilePrivateData};

#[derive(Debug)]
pub struct TtyDevice {
    core: TtyCore,
    fs: RwLock<Weak<DevFS>>
    
}

impl TtyDevice {
    pub fn new() -> Arc<TtyDevice> {
        return Arc::new(TtyDevice {
            core: TtyCore::new(),
            fs: RwLock::new(Weak::default()),
        });
    }

    /// @brief 判断文件私有信息是否为TTY的私有信息
    #[inline]
    fn verify_file_private_data<'a>(
        &self,
        private_data: &'a mut FilePrivateData,
    ) -> Result<&'a mut TtyFilePrivateData, SystemError> {
        if let FilePrivateData::Tty(t) = private_data {
            return Ok(t);
        }
        return Err(SystemError::EIO);
    }
}

impl DeviceINode for TtyDevice {
    fn set_fs(&self, fs: alloc::sync::Weak<crate::filesystem::devfs::DevFS>) {
        *self.fs.write() = fs;
    }
}

impl IndexNode for TtyDevice {
    fn open(&self, data: &mut FilePrivateData, mode: &FileMode) -> Result<(), SystemError> {
        let p = TtyFilePrivateData::default();
        *data = FilePrivateData::Tty(p);
        return Ok(());
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: &mut crate::filesystem::vfs::FilePrivateData,
    ) -> Result<usize, SystemError> {
        let _data: &mut TtyFilePrivateData = match self.verify_file_private_data(data) {
            Ok(t) => t,
            Err(e) => {
                kerror!("Try to read tty device, but file private data type mismatch!");
                return Err(e);
            }
        };

        // 读取stdin队列
        let r: Result<usize, TtyError> = self.core.read_stdin(buf, true);
        if r.is_ok() {
            return Ok(r.unwrap());
        }

        match r.unwrap_err() {
            TtyError::EOF(n) => {
                return Ok(n);
            }
            x => {
                kerror!("Error occurred when reading tty, msg={x:?}");
                return Err(SystemError::ECONNABORTED);
            }
        }
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: &mut crate::filesystem::vfs::FilePrivateData,
    ) -> Result<usize, SystemError> {
        let data: &mut TtyFilePrivateData = match self.verify_file_private_data(data) {
            Ok(t) => t,
            Err(e) => {
                kerror!("Try to write tty device, but file private data type mismatch!");
                return Err(e);
            }
        };

        // 根据当前文件是stdout还是stderr,选择不同的发送方式
        let r: Result<usize, TtyError> = if data.flags.contains(TtyFileFlag::STDOUT) {
            self.core.stdout(buf, true)
        } else if data.flags.contains(TtyFileFlag::STDERR) {
            self.core.stderr(buf, true)
        } else {
            return Err(SystemError::EPERM);
        };

        if r.is_ok() {
            return Ok(r.unwrap());
        }

        let r: TtyError = r.unwrap_err();
        kerror!("Error occurred when writing tty deivce. Error msg={r:?}");
        return Err(SystemError::EIO);
    }

    fn poll(&self) -> Result<crate::filesystem::vfs::PollStatus, SystemError> {
        return Err(SystemError::ENOTSUP);
    }

    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        return self.fs.read().upgrade().unwrap();
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, SystemError> {
        return Err(SystemError::ENOTSUP);
    }
}
