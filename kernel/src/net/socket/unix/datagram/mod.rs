use alloc::boxed::Box;
use alloc::sync::Arc;

use crate::filesystem::vfs::{IndexNode, PollableInode};
use crate::libs::spinlock::SpinLock;
use crate::net::socket::Socket;

/// TODO: Write documentation
struct UnDatagram {
    buffer: Arc<SpinLock<Box<[u8]>>>,
}

impl IndexNode for UnDatagram {}

impl Socket for UnDatagram {
    fn wait_queue(&self) -> &crate::libs::wait_queue::WaitQueue {
        todo!()
    }

    fn send_buffer_size(&self) -> usize {
        todo!()
    }

    fn recv_buffer_size(&self) -> usize {
        todo!()
    }

    fn accept(
        &self,
    ) -> Result<(Arc<dyn IndexNode>, crate::net::socket::endpoint::Endpoint), system_error::SystemError>
    {
        todo!()
    }

    fn bind(
        &self,
        endpoint: crate::net::socket::endpoint::Endpoint,
    ) -> Result<(), system_error::SystemError> {
        todo!()
    }

    fn close(&self) -> Result<(), system_error::SystemError> {
        todo!()
    }

    fn connect(
        &self,
        endpoint: crate::net::socket::endpoint::Endpoint,
    ) -> Result<(), system_error::SystemError> {
        todo!()
    }

    fn get_peer_name(
        &self,
    ) -> Result<crate::net::socket::endpoint::Endpoint, system_error::SystemError> {
        todo!()
    }

    fn get_name(
        &self,
    ) -> Result<crate::net::socket::endpoint::Endpoint, system_error::SystemError> {
        todo!()
    }

    fn get_option(
        &self,
        level: crate::net::socket::PSOL,
        name: usize,
        value: &mut [u8],
    ) -> Result<usize, system_error::SystemError> {
        todo!()
    }

    fn listen(&self, backlog: usize) -> Result<(), system_error::SystemError> {
        todo!()
    }

    fn recv(
        &self,
        buffer: &mut [u8],
        flags: crate::net::socket::PMSG,
    ) -> Result<usize, system_error::SystemError> {
        todo!()
    }

    fn recv_from(
        &self,
        buffer: &mut [u8],
        flags: crate::net::socket::PMSG,
        address: Option<crate::net::socket::endpoint::Endpoint>,
    ) -> Result<(usize, crate::net::socket::endpoint::Endpoint), system_error::SystemError> {
        todo!()
    }

    fn recv_msg(
        &self,
        msg: &mut crate::net::posix::MsgHdr,
        flags: crate::net::socket::PMSG,
    ) -> Result<usize, system_error::SystemError> {
        todo!()
    }

    fn send_msg(
        &self,
        msg: &crate::net::posix::MsgHdr,
        flags: crate::net::socket::PMSG,
    ) -> Result<usize, system_error::SystemError> {
        todo!()
    }

    fn send_to(
        &self,
        buffer: &[u8],
        flags: crate::net::socket::PMSG,
        address: crate::net::socket::endpoint::Endpoint,
    ) -> Result<usize, system_error::SystemError> {
        todo!()
    }

    fn set_option(
        &self,
        level: crate::net::socket::PSOL,
        name: usize,
        val: &[u8],
    ) -> Result<(), system_error::SystemError> {
        todo!()
    }

    fn shutdown(&self, how: usize) -> Result<(), system_error::SystemError> {
        todo!()
    }
}

impl PollableInode for UnDatagram {
    fn poll(&self, private_data: &crate::filesystem::vfs::FilePrivateData) -> Result<usize, system_error::SystemError> {
        todo!()
    }

    fn add_epitem(
        &self,
        epitem: Arc<crate::filesystem::epoll::EPollItem>,
        private_data: &crate::filesystem::vfs::FilePrivateData,
    ) -> Result<(), system_error::SystemError> {
        todo!()
    }

    fn remove_epitem(
        &self,
        epitm: &Arc<crate::filesystem::epoll::EPollItem>,
        private_data: &crate::filesystem::vfs::FilePrivateData,
    ) -> Result<(), system_error::SystemError> {
        todo!()
    }
}
