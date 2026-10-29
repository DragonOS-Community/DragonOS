//! Linux TCP urgent policy. Transport owns the mark; socket adapters own user
//! access and never retain a SocketSet guard while a user page may fault.
use smoltcp::socket::tcp::{State, UrgentState};
use system_error::SystemError;

use super::{inner, TcpSocket};
use crate::{net::socket::PMSG, syscall::user_buffer::UserBuffer};

impl TcpSocket {
    pub(super) fn take_urgent_for_receive(&self, flags: PMSG) -> Result<Option<u8>, SystemError> {
        let _receive = self.recv_lock.lock();
        let result = {
            let inner = self.inner.read();
            match inner.as_ref() {
                Some(inner::Inner::Listening(_)) => Err(SystemError::ENOTCONN),
                Some(inner::Inner::Established(established)) => established.with_mut(|socket| {
                    if socket.urgent_inline() {
                        return Err(SystemError::EINVAL);
                    }
                    let state = socket.urgent_state();
                    if matches!(state, None | Some(UrgentState::Read)) {
                        return Err(SystemError::EINVAL);
                    }
                    if socket.state() == State::Closed && !socket.received_fin() {
                        return Err(SystemError::ENOTCONN);
                    }
                    match state {
                        None | Some(UrgentState::Read) => unreachable!(),
                        Some(UrgentState::Valid(byte)) => {
                            if !flags.contains(PMSG::PEEK) {
                                socket.consume_urgent();
                            }
                            Ok(Some(byte))
                        }
                        Some(UrgentState::Pending) => {
                            if socket.state() == State::Closed
                                || socket.received_fin()
                                || self.is_recv_shutdown()
                            {
                                Ok(None)
                            } else {
                                Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
                            }
                        }
                    }
                }),
                _ => Err(SystemError::EINVAL),
            }
        };
        self.update_events();
        result
    }

    pub(super) fn recv_urgent_to_user(
        &self,
        buffer: &mut UserBuffer<'_>,
        flags: PMSG,
    ) -> Result<usize, SystemError> {
        self.recv_urgent_msg(buffer, flags).map(|(len, _)| len)
    }

    pub(super) fn recv_urgent_msg(
        &self,
        buffer: &mut UserBuffer<'_>,
        flags: PMSG,
    ) -> Result<(usize, PMSG), SystemError> {
        // Linux consumes OOB before memcpy_to_msg, including fault and len=0.
        let byte = self.take_urgent_for_receive(flags)?;
        let Some(byte) = byte else {
            return Ok((0, PMSG::empty()));
        };
        if buffer.is_empty() {
            return Ok((0, PMSG::OOB | PMSG::TRUNC));
        }
        if !flags.contains(PMSG::TRUNC) {
            buffer.write_to_user(0, &[byte])?;
        }
        Ok((1, PMSG::OOB))
    }

    pub(super) fn notify_urgent_owner(&self) {
        let notification = match self.inner.read().as_ref() {
            Some(inner::Inner::Established(owner)) => {
                owner.with_mut(|socket| socket.take_urgent_notification())
            }
            Some(inner::Inner::Connecting(owner)) => {
                owner.with_mut(|socket| socket.take_urgent_notification())
            }
            _ => false,
        };
        if notification {
            let file = self.owner_file.lock().clone().upgrade();
            if let Some(file) = file {
                crate::filesystem::vfs::fasync::send_sigurg(file.owner_snapshot());
            }
        }
    }
}
