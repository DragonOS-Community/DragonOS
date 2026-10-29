//! Linux /proc/kmsg: a privileged view of the global consuming log stream.
use crate::{
    filesystem::{
        epoll::{EPollEventType, EPollItem},
        procfs::{
            kmsg::{check_syslog_permission, kmsg, read_stream, KMSG_EPITEMS},
            template::{Builder, FileOps, ProcFileBuilder},
        },
        vfs::{
            file::FileFlags, FilePrivateData, IndexNode, InodeMode, PollableInode, UserReadMode,
        },
    },
    libs::mutex::MutexGuard,
    syscall::user_buffer::UserBuffer,
};
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

#[derive(Debug)]
pub struct KmsgFileOps;

impl KmsgFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::S_IRUSR)
            .parent(parent)
            .build()
            .unwrap()
    }

    fn check_readable(data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        let nonblocking = matches!(
            &*data,
            FilePrivateData::Kmsg(flags) if flags.contains(FileFlags::O_NONBLOCK)
        );
        drop(data);
        // Linux kmsg_read performs this one precheck before do_syslog(READ).
        // If another consumer drains the stream afterwards, READ may wait.
        if nonblocking && !kmsg()?.lock_irqsave().has_unread() {
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
        } else {
            Ok(())
        }
    }
}

impl FileOps for KmsgFileOps {
    fn supports_read_iter_flags(&self) -> bool {
        false
    }

    fn open(
        &self,
        data: &mut MutexGuard<FilePrivateData>,
        flags: &FileFlags,
    ) -> Result<(), SystemError> {
        check_syslog_permission(1)?;
        **data = FilePrivateData::Kmsg(*flags);
        Ok(())
    }

    fn user_read_mode(&self) -> UserReadMode {
        UserReadMode::Scalar
    }

    fn read_user_at(
        &self,
        _offset: usize,
        len: usize,
        writer: &mut UserBuffer<'_>,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<Option<usize>, SystemError> {
        Self::check_readable(data)?;
        // do_syslog checks NULL even for a zero-length read, but only after
        // the proc nonblocking precheck. Reject before reserving stream bytes.
        if writer
            .as_contiguous_address()
            .is_some_and(|address| address.data() == 0)
        {
            return Err(SystemError::EINVAL);
        }
        let mut offset = 0;
        read_stream(len, |bytes| {
            writer.write_to_user(offset, bytes)?;
            offset += bytes.len();
            Ok(())
        })
        .map(Some)
    }

    fn as_pollable_inode(&self) -> Result<&dyn PollableInode, SystemError> {
        Ok(self)
    }

    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Self::check_readable(data)?;
        let mut offset = 0;
        read_stream(len.min(buf.len()), |bytes| {
            buf[offset..offset + bytes.len()].copy_from_slice(bytes);
            offset += bytes.len();
            Ok(())
        })
    }
}

impl PollableInode for KmsgFileOps {
    fn poll(&self, _data: &FilePrivateData) -> Result<usize, SystemError> {
        if kmsg()?.lock_irqsave().has_unread() {
            Ok((EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM).bits() as usize)
        } else {
            Ok(0)
        }
    }

    fn add_epitem(&self, item: Arc<EPollItem>, _data: &FilePrivateData) -> Result<(), SystemError> {
        KMSG_EPITEMS.add(item);
        Ok(())
    }

    fn remove_epitem(
        &self,
        item: &Arc<EPollItem>,
        _data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        KMSG_EPITEMS.remove(item)
    }
}
