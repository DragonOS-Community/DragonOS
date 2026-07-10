use alloc::{sync::Arc, vec::Vec};
use core::{mem::size_of, sync::atomic::Ordering};

use system_error::SystemError;

use crate::{
    arch::MMArch,
    exception::workqueue::{schedule_work, Work},
    filesystem::epoll::{event_poll::EventPoll, EPollEventType},
    mm::MemoryManagementArch,
    process::ProcessManager,
};

use super::super::protocol::{
    fuse_pack_struct, fuse_read_struct, FuseAttrOut, FuseEntryOut, FuseForgetIn, FuseGetxattrIn,
    FuseGetxattrOut, FuseInHeader, FuseInitIn, FuseInitOut, FuseInterruptIn, FuseOpenOut,
    FuseOutHeader, FuseReadIn, FuseStatfsOut, FuseWriteOut, FUSE_ACCESS, FUSE_CREATE, FUSE_DESTROY,
    FUSE_FALLOCATE, FUSE_FLUSH, FUSE_FORGET, FUSE_FSYNC, FUSE_FSYNCDIR, FUSE_GETATTR,
    FUSE_GETXATTR, FUSE_INIT, FUSE_INTERRUPT, FUSE_KERNEL_MINOR_VERSION, FUSE_KERNEL_VERSION,
    FUSE_LINK, FUSE_LISTXATTR, FUSE_LOOKUP, FUSE_MKDIR, FUSE_MKNOD, FUSE_OPEN, FUSE_OPENDIR,
    FUSE_READ, FUSE_READDIR, FUSE_READDIRPLUS, FUSE_READLINK, FUSE_RELEASE, FUSE_RELEASEDIR,
    FUSE_REMOVEXATTR, FUSE_RENAME, FUSE_RENAME2, FUSE_RMDIR, FUSE_SETATTR, FUSE_SETXATTR,
    FUSE_STATFS, FUSE_SYMLINK, FUSE_UNLINK, FUSE_WRITE,
};
use super::{
    stats, trace, FuseConn, FusePendingState, FuseReplyCapacity, FuseReplyCapacitySource,
    FuseReplyContract, FuseRequest, FuseRequestCred,
};
use crate::filesystem::fuse::reply::FuseReply;

impl FuseConn {
    fn is_high_priority_opcode(opcode: u32) -> bool {
        matches!(opcode, FUSE_FORGET | FUSE_INTERRUPT)
    }

    fn alloc_unique(&self) -> u64 {
        self.next_unique.fetch_add(2, Ordering::Relaxed)
    }
    /// Queue a FORGET message (no reply expected).
    pub fn queue_forget(&self, nodeid: u64, nlookup: u64) -> Result<(), SystemError> {
        if nodeid == 0 || nlookup == 0 {
            return Ok(());
        }
        let can_send = {
            let g = self.inner.lock();
            g.connected && g.mounted && g.initialized
        };
        if !can_send {
            return Ok(());
        }
        let inarg = FuseForgetIn { nlookup };
        self.enqueue_noreply(FUSE_FORGET, nodeid, fuse_pack_struct(&inarg))
    }

    pub(super) fn queue_interrupt(&self, unique: u64) -> Result<(), SystemError> {
        if unique == 0 {
            return Ok(());
        }
        let can_send = {
            let g = self.inner.lock();
            g.connected
                && g.mounted
                && g.initialized
                && !g.no_interrupt
                && g.processing.contains_key(&unique)
        };
        if !can_send {
            return Ok(());
        }
        let inarg = FuseInterruptIn { unique };
        let req = self.build_request(
            unique | Self::FUSE_INT_REQ_BIT,
            FUSE_INTERRUPT,
            0,
            fuse_pack_struct(&inarg),
            FuseRequestCred::nocreds(),
            false,
        )?;
        self.push_request(req, None, unique | Self::FUSE_INT_REQ_BIT)
    }
    pub fn enqueue_init(&self) -> Result<(), SystemError> {
        let flags = self.inner.lock().init_flags;
        let init_in = FuseInitIn {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            max_readahead: Self::DEFAULT_MAX_READAHEAD as u32,
            flags: flags as u32,
            flags2: (flags >> 32) as u32,
            unused: [0; 11],
        };
        self.enqueue_request(FUSE_INIT, 0, fuse_pack_struct(&init_in))
            .map(|_| ())
    }

    pub fn request(
        &self,
        opcode: u32,
        nodeid: u64,
        payload: &[u8],
    ) -> Result<FuseReply, SystemError> {
        if opcode != FUSE_INIT {
            let cred = ProcessManager::current_pcb().cred();
            if !self.allow_current_process(&cred) {
                return Err(SystemError::EACCES);
            }
            self.wait_initialized()?;
        }
        let pending = self.enqueue_request(opcode, nodeid, payload)?;
        self.wait_request_complete(opcode, pending)
    }

    pub fn request_nocreds(
        &self,
        opcode: u32,
        nodeid: u64,
        payload: &[u8],
    ) -> Result<FuseReply, SystemError> {
        if opcode != FUSE_INIT {
            self.wait_initialized()?;
        }
        let pending =
            self.enqueue_request_with_cred(opcode, nodeid, payload, FuseRequestCred::nocreds())?;
        self.wait_request_complete(opcode, pending)
    }

    pub(crate) fn request_with_cred(
        &self,
        opcode: u32,
        nodeid: u64,
        payload: &[u8],
        req_cred: FuseRequestCred,
    ) -> Result<FuseReply, SystemError> {
        if opcode != FUSE_INIT {
            self.wait_initialized()?;
        }
        let pending = self.enqueue_request_with_cred(opcode, nodeid, payload, req_cred)?;
        self.wait_request_complete(opcode, pending)
    }

    pub fn request_nocreds_background(
        self: &Arc<Self>,
        opcode: u32,
        nodeid: u64,
        payload: &[u8],
    ) -> Result<(), SystemError> {
        if opcode != FUSE_INIT {
            self.wait_initialized()?;
        }
        let pending =
            self.enqueue_request_with_cred(opcode, nodeid, payload, FuseRequestCred::nocreds())?;
        let conn = self.clone();
        schedule_work(Work::new(move || {
            if let Err(err) = conn.wait_request_complete(opcode, pending.clone()) {
                if matches!(err, SystemError::ENOTCONN | SystemError::ENOENT) {
                    log::debug!(
                        "fuse: background request aborted opcode={} nodeid={} err={:?}",
                        opcode,
                        nodeid,
                        err
                    );
                } else {
                    log::warn!(
                        "fuse: background request failed opcode={} nodeid={} err={:?}",
                        opcode,
                        nodeid,
                        err
                    );
                }
            }
        }));
        Ok(())
    }

    fn wait_request_complete(
        &self,
        opcode: u32,
        pending: Arc<FusePendingState>,
    ) -> Result<FuseReply, SystemError> {
        match pending.wait_complete() {
            Err(SystemError::EINTR) | Err(SystemError::ERESTARTSYS) => {
                if opcode != FUSE_INTERRUPT {
                    let _ = self.queue_interrupt(pending.unique());
                }
                Err(SystemError::EINTR)
            }
            x => x,
        }
    }

    fn enqueue_noreply(&self, opcode: u32, nodeid: u64, payload: &[u8]) -> Result<(), SystemError> {
        debug_assert_eq!(opcode, FUSE_FORGET);
        let unique = self.alloc_unique();
        let req = self.build_request(
            unique,
            opcode,
            nodeid,
            payload,
            FuseRequestCred {
                uid: 0,
                gid: 0,
                pid: 0,
            },
            true,
        )?;
        self.push_request(req, None, unique)?;
        Ok(())
    }

    fn enqueue_request(
        &self,
        opcode: u32,
        nodeid: u64,
        payload: &[u8],
    ) -> Result<Arc<FusePendingState>, SystemError> {
        let pcb = ProcessManager::current_pcb();
        let cred = pcb.cred();
        if !self.allow_current_process(&cred) {
            return Err(SystemError::EACCES);
        }
        self.enqueue_request_with_cred(opcode, nodeid, payload, FuseRequestCred::from_current())
    }

    pub(super) fn enqueue_request_with_cred(
        &self,
        opcode: u32,
        nodeid: u64,
        payload: &[u8],
        req_cred: FuseRequestCred,
    ) -> Result<Arc<FusePendingState>, SystemError> {
        let unique = self.alloc_unique();
        let pending_state = Arc::new(FusePendingState::new(unique, opcode));

        let req = self.build_request(unique, opcode, nodeid, payload, req_cred, false)?;
        self.push_request(req, Some(pending_state.clone()), unique)?;
        Ok(pending_state)
    }

    fn reply_capacity(
        &self,
        opcode: u32,
        payload: &[u8],
    ) -> Result<Option<FuseReplyCapacity>, SystemError> {
        let minor = self.reply_layout_minor.load(Ordering::Acquire);
        let header = size_of::<FuseOutHeader>();
        let exact = |payload_len: usize| -> Result<Option<FuseReplyCapacity>, SystemError> {
            let bytes = header
                .checked_add(payload_len)
                .ok_or(SystemError::EOVERFLOW)?;
            if self.backend_reply_limit.is_some_and(|limit| bytes > limit) {
                return Err(SystemError::EOVERFLOW);
            }
            Ok(Some(FuseReplyCapacity {
                bytes,
                source: FuseReplyCapacitySource::Exact,
            }))
        };

        let entry_size = if minor < 9 {
            Self::FUSE_COMPAT_ENTRY_OUT_SIZE
        } else {
            size_of::<FuseEntryOut>()
        };
        let attr_size = if minor < 9 {
            Self::FUSE_COMPAT_ATTR_OUT_SIZE
        } else {
            size_of::<FuseAttrOut>()
        };

        let fixed_payload = match opcode {
            FUSE_LOOKUP | FUSE_SYMLINK | FUSE_MKNOD | FUSE_MKDIR | FUSE_LINK => Some(entry_size),
            FUSE_GETATTR | FUSE_SETATTR => Some(attr_size),
            FUSE_OPEN | FUSE_OPENDIR => Some(size_of::<FuseOpenOut>()),
            FUSE_WRITE => Some(size_of::<FuseWriteOut>()),
            FUSE_STATFS => Some(if minor < 4 {
                Self::FUSE_COMPAT_STATFS_SIZE
            } else {
                size_of::<FuseStatfsOut>()
            }),
            FUSE_CREATE => Some(
                entry_size
                    .checked_add(size_of::<FuseOpenOut>())
                    .ok_or(SystemError::EOVERFLOW)?,
            ),
            FUSE_INIT => Some(size_of::<FuseInitOut>()),
            FUSE_UNLINK | FUSE_RMDIR | FUSE_RENAME | FUSE_RENAME2 | FUSE_RELEASE | FUSE_FSYNC
            | FUSE_SETXATTR | FUSE_REMOVEXATTR | FUSE_FLUSH | FUSE_RELEASEDIR | FUSE_FSYNCDIR
            | FUSE_ACCESS | FUSE_INTERRUPT | FUSE_DESTROY | FUSE_FALLOCATE => Some(0),
            _ => None,
        };
        if let Some(payload_len) = fixed_payload {
            return exact(payload_len);
        }

        let requested = match opcode {
            FUSE_READ | FUSE_READDIR | FUSE_READDIRPLUS => {
                let read_in: FuseReadIn = fuse_read_struct(payload)?;
                let requested = read_in.size as usize;
                let (max_read, max_pages) = {
                    let g = self.inner.lock();
                    (g.max_read as usize, g.init.max_pages as usize)
                };
                let max_pages_bytes = max_pages
                    .checked_mul(MMArch::PAGE_SIZE)
                    .ok_or(SystemError::EOVERFLOW)?;
                if requested > max_read || requested > max_pages_bytes {
                    return Err(SystemError::EINVAL);
                }
                requested
            }
            FUSE_GETXATTR | FUSE_LISTXATTR => {
                let getxattr_in: FuseGetxattrIn = fuse_read_struct(payload)?;
                let requested = getxattr_in.size as usize;
                if requested > Self::XATTR_SIZE_MAX {
                    return Err(SystemError::EINVAL);
                }
                if requested == 0 {
                    size_of::<FuseGetxattrOut>()
                } else {
                    requested
                }
            }
            FUSE_READLINK => MMArch::PAGE_SIZE
                .checked_sub(1)
                .ok_or(SystemError::EOVERFLOW)?,
            _ => {
                return match self.backend_reply_limit {
                    Some(bytes) if bytes >= header => Ok(Some(FuseReplyCapacity {
                        bytes,
                        source: FuseReplyCapacitySource::ExplicitFallback,
                    })),
                    Some(_) => Err(SystemError::EOVERFLOW),
                    None => Ok(None),
                };
            }
        };

        let bytes = header
            .checked_add(requested)
            .ok_or(SystemError::EOVERFLOW)?;
        if self.backend_reply_limit.is_some_and(|limit| bytes > limit) {
            return Err(SystemError::EOVERFLOW);
        }
        Ok(Some(FuseReplyCapacity {
            bytes,
            source: FuseReplyCapacitySource::RequestBounded,
        }))
    }

    fn build_request(
        &self,
        unique: u64,
        opcode: u32,
        nodeid: u64,
        payload: &[u8],
        req_cred: FuseRequestCred,
        no_reply: bool,
    ) -> Result<Arc<FuseRequest>, SystemError> {
        let request_len = size_of::<FuseInHeader>()
            .checked_add(payload.len())
            .ok_or(SystemError::EOVERFLOW)?;
        let request_len = u32::try_from(request_len).map_err(|_| SystemError::EOVERFLOW)?;
        let reply_contract = if no_reply {
            FuseReplyContract::NoReply
        } else {
            FuseReplyContract::Reply {
                capacity: self.reply_capacity(opcode, payload)?,
            }
        };
        let hdr = FuseInHeader {
            len: request_len,
            opcode,
            unique,
            nodeid,
            uid: req_cred.uid,
            gid: req_cred.gid,
            pid: req_cred.pid,
            total_extlen: 0,
            padding: 0,
        };

        let mut bytes = Vec::with_capacity(hdr.len as usize);
        bytes.extend_from_slice(fuse_pack_struct(&hdr));
        bytes.extend_from_slice(payload);
        Ok(Arc::new(FuseRequest {
            bytes,
            unique,
            opcode,
            reply_contract,
        }))
    }

    fn push_request(
        &self,
        req: Arc<FuseRequest>,
        pending_state: Option<Arc<FusePendingState>>,
        unique: u64,
    ) -> Result<(), SystemError> {
        let req_len = req.bytes.len();
        let opcode = req.opcode;
        let no_reply = matches!(req.reply_contract, FuseReplyContract::NoReply);
        debug_assert_eq!(
            no_reply,
            pending_state.is_none() && opcode != FUSE_INTERRUPT
        );
        {
            let mut g = self.inner.lock();
            if !g.connected {
                return Err(SystemError::ENOTCONN);
            }
            if g.teardown_started && opcode != FUSE_DESTROY {
                return Err(SystemError::ENOTCONN);
            }
            if g.separate_hiprio_pending && Self::is_high_priority_opcode(opcode) {
                g.hiprio_pending.push_back(req);
            } else {
                g.pending.push_back(req);
            }
            if let Some(pending) = pending_state {
                g.processing.insert(unique, pending);
            }
        }

        stats::on_fuse_request_queued(req_len, no_reply);
        trace::trace_fuse_request_queue(unique, opcode, req_len as u64, no_reply as u8);
        self.read_wait.wakeup(None);
        self.wake_bridge(stats::VirtioFsBridgeWakeSource::Request);
        let _ = EventPoll::wakeup_epoll(
            &self.epitems,
            EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
        );
        Ok(())
    }
}
#[cfg(test)]
pub(super) fn reply_capacity_for_test(
    conn: &FuseConn,
    opcode: u32,
    payload: &[u8],
) -> Result<Option<FuseReplyCapacity>, SystemError> {
    conn.reply_capacity(opcode, payload)
}

#[cfg(test)]
pub(super) fn queue_test_request(
    conn: &Arc<FuseConn>,
    opcode: u32,
    nodeid: u64,
    payload: &[u8],
    pending_reply: bool,
) -> (u64, *const u8, Option<Arc<FusePendingState>>) {
    let unique = conn.alloc_unique();
    let req = conn
        .build_request(
            unique,
            opcode,
            nodeid,
            payload,
            FuseRequestCred::nocreds(),
            !pending_reply,
        )
        .unwrap();
    let request_ptr = req.bytes().as_ptr();
    let pending = pending_reply.then(|| Arc::new(FusePendingState::new(unique, opcode)));
    conn.push_request(req, pending.clone(), unique).unwrap();
    (unique, request_ptr, pending)
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, vec, vec::Vec};
    use core::{mem::size_of, sync::atomic::Ordering};

    use system_error::SystemError;

    use super::*;

    fn set_minor(conn: &FuseConn, minor: u32) {
        conn.inner.lock().init.minor = minor;
        conn.reply_layout_minor.store(minor, Ordering::Release);
    }

    fn capacity(conn: &FuseConn, opcode: u32, payload: &[u8]) -> (usize, FuseReplyCapacitySource) {
        let capacity = conn.reply_capacity(opcode, payload).unwrap().unwrap();
        (capacity.bytes, capacity.source)
    }

    #[test]
    fn reply_capacity_covers_every_supported_opcode_without_fallback() {
        let conn = FuseConn::new_for_virtiofs(256 * 1024, 256 * 1024);
        set_minor(&conn, 39);
        let header = size_of::<FuseOutHeader>();
        let fixed = [
            (FUSE_LOOKUP, size_of::<FuseEntryOut>()),
            (FUSE_SYMLINK, size_of::<FuseEntryOut>()),
            (FUSE_MKNOD, size_of::<FuseEntryOut>()),
            (FUSE_MKDIR, size_of::<FuseEntryOut>()),
            (FUSE_LINK, size_of::<FuseEntryOut>()),
            (FUSE_GETATTR, size_of::<FuseAttrOut>()),
            (FUSE_SETATTR, size_of::<FuseAttrOut>()),
            (FUSE_OPEN, size_of::<FuseOpenOut>()),
            (FUSE_OPENDIR, size_of::<FuseOpenOut>()),
            (FUSE_WRITE, size_of::<FuseWriteOut>()),
            (FUSE_STATFS, size_of::<FuseStatfsOut>()),
            (
                FUSE_CREATE,
                size_of::<FuseEntryOut>() + size_of::<FuseOpenOut>(),
            ),
            (FUSE_INIT, size_of::<FuseInitOut>()),
        ];
        for (opcode, payload_len) in fixed {
            assert_eq!(
                capacity(&conn, opcode, &[]),
                (header + payload_len, FuseReplyCapacitySource::Exact)
            );
        }

        let header_only = [
            FUSE_UNLINK,
            FUSE_RMDIR,
            FUSE_RENAME,
            FUSE_RENAME2,
            FUSE_RELEASE,
            FUSE_FSYNC,
            FUSE_SETXATTR,
            FUSE_REMOVEXATTR,
            FUSE_FLUSH,
            FUSE_RELEASEDIR,
            FUSE_FSYNCDIR,
            FUSE_ACCESS,
            FUSE_INTERRUPT,
            FUSE_DESTROY,
            FUSE_FALLOCATE,
        ];
        for opcode in header_only {
            assert_eq!(
                capacity(&conn, opcode, &[]),
                (header, FuseReplyCapacitySource::Exact)
            );
        }

        let read_in = FuseReadIn {
            fh: 0,
            offset: 0,
            size: 4096,
            read_flags: 0,
            lock_owner: 0,
            flags: 0,
            padding: 0,
        };
        for opcode in [FUSE_READ, FUSE_READDIR, FUSE_READDIRPLUS] {
            assert_eq!(
                capacity(&conn, opcode, fuse_pack_struct(&read_in)),
                (header + 4096, FuseReplyCapacitySource::RequestBounded)
            );
        }
        assert_eq!(
            capacity(&conn, FUSE_READLINK, &[]),
            (
                header + <crate::arch::MMArch as crate::mm::MemoryManagementArch>::PAGE_SIZE - 1,
                FuseReplyCapacitySource::RequestBounded
            )
        );

        for opcode in [FUSE_GETXATTR, FUSE_LISTXATTR] {
            let query = FuseGetxattrIn {
                size: 0,
                padding: 0,
            };
            assert_eq!(
                capacity(&conn, opcode, fuse_pack_struct(&query)),
                (
                    header + size_of::<FuseGetxattrOut>(),
                    FuseReplyCapacitySource::RequestBounded
                )
            );
            let value = FuseGetxattrIn {
                size: 64 * 1024,
                padding: 0,
            };
            assert_eq!(
                capacity(&conn, opcode, fuse_pack_struct(&value)),
                (header + 64 * 1024, FuseReplyCapacitySource::RequestBounded)
            );
        }

        assert_eq!(
            capacity(&conn, 63, &[]),
            (256 * 1024, FuseReplyCapacitySource::ExplicitFallback)
        );
    }

    #[test]
    fn reply_contract_rejects_malformed_and_out_of_bounds_variable_requests() {
        let conn = FuseConn::new_for_virtiofs(256 * 1024, 256 * 1024);
        conn.configure_mount(0, 0, true, 4096);
        assert_eq!(
            conn.reply_capacity(FUSE_READ, &[]),
            Err(SystemError::EINVAL)
        );
        assert_eq!(
            conn.reply_capacity(FUSE_GETXATTR, &[]),
            Err(SystemError::EINVAL)
        );

        let too_large_read = FuseReadIn {
            fh: 0,
            offset: 0,
            size: 4097,
            read_flags: 0,
            lock_owner: 0,
            flags: 0,
            padding: 0,
        };
        assert_eq!(
            conn.reply_capacity(FUSE_READ, fuse_pack_struct(&too_large_read)),
            Err(SystemError::EINVAL)
        );
        let too_large_xattr = FuseGetxattrIn {
            size: 64 * 1024 + 1,
            padding: 0,
        };
        assert_eq!(
            conn.reply_capacity(FUSE_GETXATTR, fuse_pack_struct(&too_large_xattr)),
            Err(SystemError::EINVAL)
        );

        let undersized = FuseConn::new_for_virtiofs(8192, size_of::<FuseOutHeader>() - 1);
        assert_eq!(
            undersized.reply_capacity(63, &[]),
            Err(SystemError::EOVERFLOW)
        );
    }

    #[test]
    fn forget_is_no_reply_while_interrupt_and_destroy_have_header_capacity() {
        let conn = FuseConn::new_for_virtiofs(8192, 8192);
        let unique = conn.alloc_unique();
        let forget = conn
            .build_request(
                unique,
                FUSE_FORGET,
                1,
                &[],
                FuseRequestCred::nocreds(),
                true,
            )
            .unwrap();
        assert_eq!(forget.reply_contract(), FuseReplyContract::NoReply);

        for opcode in [FUSE_INTERRUPT, FUSE_DESTROY] {
            let req = conn
                .build_request(
                    conn.alloc_unique(),
                    opcode,
                    0,
                    &[],
                    FuseRequestCred::nocreds(),
                    false,
                )
                .unwrap();
            assert!(matches!(
                req.reply_contract(),
                FuseReplyContract::Reply {
                    capacity: Some(capacity)
                } if capacity.bytes == size_of::<FuseOutHeader>()
            ));
        }
    }
}
