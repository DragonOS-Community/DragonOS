use alloc::{sync::Arc, vec, vec::Vec};
use core::{mem::size_of, sync::atomic::Ordering};

use num_traits::FromPrimitive;
use system_error::SystemError;

use crate::{
    arch::MMArch,
    filesystem::epoll::{EPollEventType, EPollItem},
    mm::MemoryManagementArch,
};

use super::super::protocol::{
    fuse_read_struct, FuseAttrOut, FuseEntryOut, FuseInHeader, FuseInitOut, FuseNotifyDeleteOut,
    FuseNotifyInvalEntryOut, FuseNotifyInvalInodeOut, FuseOpenOut, FuseOutHeader, FuseStatfsOut,
    FuseWriteIn, FUSE_ASYNC_READ, FUSE_CREATE, FUSE_DESTROY, FUSE_EXPIRE_ONLY, FUSE_FLUSH,
    FUSE_GETATTR, FUSE_GETXATTR, FUSE_INIT, FUSE_INIT_EXT, FUSE_INTERRUPT,
    FUSE_KERNEL_MINOR_VERSION, FUSE_KERNEL_VERSION, FUSE_LINK, FUSE_LISTXATTR, FUSE_LOOKUP,
    FUSE_MAP_ALIGNMENT, FUSE_MAX_PAGES, FUSE_MIN_READ_BUFFER, FUSE_MKDIR, FUSE_MKNOD,
    FUSE_NOTIFY_DELETE, FUSE_NOTIFY_INVAL_ENTRY, FUSE_NOTIFY_INVAL_INODE, FUSE_NOTIFY_POLL,
    FUSE_NOTIFY_RETRIEVE, FUSE_NOTIFY_STORE, FUSE_READ, FUSE_REMOVEXATTR, FUSE_SETATTR,
    FUSE_SETXATTR, FUSE_STATFS, FUSE_SYMLINK, FUSE_WRITEBACK_CACHE,
};
use super::{
    stats, trace, wait_with_recheck, FuseConn, FuseConnInner, FuseInitNegotiated, FuseRequest,
};
use crate::filesystem::fuse::reply::FuseReply;

impl FuseConn {
    fn claim_pending_reply<F>(
        &self,
        unique: u64,
        pending: &Arc<super::FusePendingState>,
        update: F,
    ) -> Result<(), SystemError>
    where
        F: FnOnce(&mut FuseConnInner),
    {
        let mut g = self.inner.lock();
        if !g.connected || (g.teardown_started && pending.opcode != FUSE_DESTROY) {
            return Err(SystemError::ENOENT);
        }
        let Some(current) = g.processing.get(&unique) else {
            return Err(SystemError::ENOENT);
        };
        if !Arc::ptr_eq(current, pending) {
            return Err(SystemError::ENOENT);
        }
        g.processing.remove(&unique);
        update(&mut g);
        Ok(())
    }

    fn complete_claimed_reply_with_error(
        pending: &Arc<super::FusePendingState>,
        unique: u64,
        error: SystemError,
        reply_error: i32,
        payload_len: usize,
    ) {
        if pending.complete(Err(error)) {
            stats::on_fuse_reply_complete(pending.opcode, reply_error, payload_len);
            trace::trace_fuse_reply_complete(
                unique,
                pending.opcode,
                reply_error,
                payload_len as u64,
            );
        }
    }

    fn complete_claimed_daemon_error(
        pending: &Arc<super::FusePendingState>,
        unique: u64,
        error: SystemError,
        reply_error: i32,
        payload_len: usize,
    ) {
        if pending.complete_daemon_error(error) {
            stats::on_fuse_reply_complete(pending.opcode, reply_error, payload_len);
            trace::trace_fuse_reply_complete(
                unique,
                pending.opcode,
                reply_error,
                payload_len as u64,
            );
        }
    }

    pub fn poll_mask(&self, have_pending: bool) -> EPollEventType {
        let mut events = EPollEventType::EPOLLOUT | EPollEventType::EPOLLWRNORM;
        let g = self.inner.lock();
        if !g.connected {
            return EPollEventType::EPOLLERR;
        }
        if have_pending {
            events |= EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM;
        }
        events
    }

    pub fn poll(&self) -> EPollEventType {
        let g = self.inner.lock();
        let have_pending = !g.hiprio_pending.is_empty() || !g.pending.is_empty();
        drop(g);
        self.poll_mask(have_pending)
    }

    pub fn add_epitem(&self, epitem: Arc<EPollItem>) -> Result<(), SystemError> {
        self.epitems.lock().push_back(epitem);
        Ok(())
    }

    pub fn remove_epitem(&self, epitem: &Arc<EPollItem>) -> Result<(), SystemError> {
        let mut guard = self.epitems.lock();
        let len = guard.len();
        guard.retain(|x| !Arc::ptr_eq(x, epitem));
        if len != guard.len() {
            return Ok(());
        }
        Err(SystemError::ENOENT)
    }

    fn calc_min_read_buffer(max_write: usize) -> usize {
        core::cmp::max(
            FUSE_MIN_READ_BUFFER,
            core::mem::size_of::<FuseInHeader>() + core::mem::size_of::<FuseWriteIn>() + max_write,
        )
    }

    fn min_read_buffer(&self) -> usize {
        let g = self.inner.lock();
        Self::calc_min_read_buffer(g.init.max_write as usize)
    }

    fn pop_pending_nonblock(&self) -> Result<Arc<FuseRequest>, SystemError> {
        let mut g = self.inner.lock();
        if !g.connected {
            return Err(SystemError::ENOTCONN);
        }
        Self::pop_pending_locked(&mut g).ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)
    }

    fn pop_pending_blocking(&self) -> Result<Arc<FuseRequest>, SystemError> {
        wait_with_recheck(&self.read_wait, || {
            let mut g = self.inner.lock();
            if !g.connected {
                return Err(SystemError::ENOTCONN);
            }
            if let Some(req) = Self::pop_pending_locked(&mut g) {
                return Ok(Some(req));
            }
            Ok(None)
        })
    }

    fn pop_ordinary_pending_nonblock(&self) -> Result<Arc<FuseRequest>, SystemError> {
        let mut g = self.inner.lock();
        if !g.connected {
            return Err(SystemError::ENOTCONN);
        }
        g.pending
            .pop_front()
            .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)
    }

    fn pop_ordinary_pending_blocking(&self) -> Result<Arc<FuseRequest>, SystemError> {
        wait_with_recheck(&self.read_wait, || {
            let mut g = self.inner.lock();
            if !g.connected {
                return Err(SystemError::ENOTCONN);
            }
            if let Some(req) = g.pending.pop_front() {
                return Ok(Some(req));
            }
            Ok(None)
        })
    }

    fn is_stale_interrupt_locked(req: &FuseRequest, g: &FuseConnInner) -> bool {
        req.opcode == FUSE_INTERRUPT
            && !g
                .processing
                .contains_key(&Self::interrupt_target_unique(req.unique))
    }

    fn pop_high_priority_pending_locked(g: &mut FuseConnInner) -> Option<Arc<FuseRequest>> {
        loop {
            let req = g.hiprio_pending.pop_front()?;
            if Self::is_stale_interrupt_locked(&req, g) {
                stats::on_fuse_requests_aborted(1);
                continue;
            }
            return Some(req);
        }
    }

    fn pop_pending_locked(g: &mut FuseConnInner) -> Option<Arc<FuseRequest>> {
        Self::pop_high_priority_pending_locked(g).or_else(|| g.pending.pop_front())
    }

    fn complete_dequeued_request(
        &self,
        req: Arc<FuseRequest>,
        out: &mut [u8],
    ) -> Result<usize, SystemError> {
        out[..req.bytes.len()].copy_from_slice(&req.bytes);
        req.stats_mark_external_dequeued();
        self.account_dequeued_request(&req);
        Ok(req.bytes.len())
    }

    fn account_dequeued_request(&self, req: &FuseRequest) {
        stats::on_fuse_request_dequeued(req.bytes.len());
        trace::trace_fuse_request_dequeue(req.unique, req.opcode, req.bytes.len() as u64);
    }

    fn dequeue_for_kernel_transport<F>(
        &self,
        max_message_size: usize,
        pop_request: F,
    ) -> Result<Arc<FuseRequest>, SystemError>
    where
        F: FnOnce() -> Result<Arc<FuseRequest>, SystemError>,
    {
        let req = pop_request()?;
        if req.bytes.len() > max_message_size {
            self.fail_oversized_read_request(&req);
            log::warn!(
                "fuse: kernel transport buffer smaller than queued request: got={} need={}",
                max_message_size,
                req.bytes.len()
            );
            // Return to the bridge loop after one malformed request so completion, reset and the
            // other priority queue cannot be starved by a producer of oversized requests.
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }
        req.stats_mark_dispatched();
        self.account_dequeued_request(&req);
        Ok(req)
    }

    pub(crate) fn dequeue_virtiofs_ordinary_request(
        &self,
        max_message_size: usize,
    ) -> Result<Arc<FuseRequest>, SystemError> {
        self.dequeue_for_kernel_transport(max_message_size, || self.pop_ordinary_pending_nonblock())
    }

    pub(crate) fn dequeue_virtiofs_high_priority_request(
        &self,
        max_message_size: usize,
    ) -> Result<Arc<FuseRequest>, SystemError> {
        self.dequeue_for_kernel_transport(max_message_size, || {
            let mut g = self.inner.lock();
            if !g.connected {
                return Err(SystemError::ENOTCONN);
            }
            Self::pop_high_priority_pending_locked(&mut g).ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)
        })
    }

    fn read_dequeued_request<F>(
        &self,
        nonblock: bool,
        out: &mut [u8],
        context: &str,
        mut pop_request: F,
    ) -> Result<usize, SystemError>
    where
        F: FnMut() -> Result<Arc<FuseRequest>, SystemError>,
    {
        let min_read = self.min_read_buffer();
        if out.len() < min_read {
            log::warn!(
                "fuse: read buffer too small for {}: got={} min={} nonblock={}",
                context,
                out.len(),
                min_read,
                nonblock
            );
            return Err(SystemError::EINVAL);
        }

        let req = loop {
            let req = pop_request()?;

            if out.len() >= req.bytes.len() {
                break req;
            }

            self.fail_oversized_read_request(&req);
            log::warn!(
                "fuse: read buffer smaller than queued {} request: got={} need={}",
                context,
                out.len(),
                req.bytes.len()
            );
        };

        self.complete_dequeued_request(req, out)
    }

    pub fn read_request(&self, nonblock: bool, out: &mut [u8]) -> Result<usize, SystemError> {
        // Linux: require a sane minimum read buffer for all reads.
        self.read_dequeued_request(nonblock, out, "request", || {
            // Linux: if O_NONBLOCK and no pending request, return EAGAIN.
            if nonblock {
                self.pop_pending_nonblock()
            } else {
                self.pop_pending_blocking()
            }
        })
    }

    pub fn read_ordinary_request(
        &self,
        nonblock: bool,
        out: &mut [u8],
    ) -> Result<usize, SystemError> {
        self.read_dequeued_request(nonblock, out, "ordinary request", || {
            if nonblock {
                self.pop_ordinary_pending_nonblock()
            } else {
                self.pop_ordinary_pending_blocking()
            }
        })
    }

    pub fn read_high_priority_request(&self, out: &mut [u8]) -> Result<usize, SystemError> {
        self.read_dequeued_request(true, out, "high-priority request", || {
            let mut g = self.inner.lock();
            if !g.connected {
                return Err(SystemError::ENOTCONN);
            }
            Self::pop_high_priority_pending_locked(&mut g).ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)
        })
    }

    fn fail_oversized_read_request(&self, req: &FuseRequest) {
        stats::on_fuse_read_buffer_too_small();
        let err = if req.opcode == FUSE_SETXATTR {
            SystemError::E2BIG
        } else {
            SystemError::EIO
        };
        let pending = {
            let mut g = self.inner.lock();
            g.processing.remove(&req.unique)
        };

        if let Some(pending) = pending {
            let error = -err.to_posix_errno();
            stats::on_fuse_reply_complete(req.opcode, error, 0);
            trace::trace_fuse_reply_complete(req.unique, req.opcode, error, 0);
            pending.complete(Err(err));
        } else {
            stats::on_fuse_requests_aborted(1);
        }
    }
    fn parse_init_reply(payload: &[u8]) -> Result<FuseInitOut, SystemError> {
        if payload.len() < Self::FUSE_COMPAT_INIT_OUT_SIZE
            || payload.len() > size_of::<FuseInitOut>()
        {
            return Err(SystemError::EINVAL);
        }
        let mut normalized = vec![0u8; size_of::<FuseInitOut>()];
        normalized[..payload.len()].copy_from_slice(payload);
        fuse_read_struct(&normalized)
    }

    fn normalize_compat_reply(
        minor: u32,
        opcode: u32,
        payload: &[u8],
    ) -> Result<Option<Vec<u8>>, SystemError> {
        let (compat_len, full_len) = if minor < 4 && opcode == FUSE_STATFS {
            (Self::FUSE_COMPAT_STATFS_SIZE, size_of::<FuseStatfsOut>())
        } else if minor < 9
            && matches!(
                opcode,
                FUSE_LOOKUP | FUSE_SYMLINK | FUSE_MKNOD | FUSE_MKDIR | FUSE_LINK
            )
        {
            (Self::FUSE_COMPAT_ENTRY_OUT_SIZE, size_of::<FuseEntryOut>())
        } else if minor < 9 && matches!(opcode, FUSE_GETATTR | FUSE_SETATTR) {
            (Self::FUSE_COMPAT_ATTR_OUT_SIZE, size_of::<FuseAttrOut>())
        } else if minor < 9 && opcode == FUSE_CREATE {
            let compat_len = Self::FUSE_COMPAT_ENTRY_OUT_SIZE
                .checked_add(size_of::<FuseOpenOut>())
                .ok_or(SystemError::EOVERFLOW)?;
            if payload.len() != compat_len {
                return Err(SystemError::EINVAL);
            }
            let mut normalized = vec![0u8; size_of::<FuseEntryOut>() + size_of::<FuseOpenOut>()];
            normalized[..Self::FUSE_COMPAT_ENTRY_OUT_SIZE]
                .copy_from_slice(&payload[..Self::FUSE_COMPAT_ENTRY_OUT_SIZE]);
            normalized[size_of::<FuseEntryOut>()..]
                .copy_from_slice(&payload[Self::FUSE_COMPAT_ENTRY_OUT_SIZE..compat_len]);
            return Ok(Some(normalized));
        } else {
            return Ok(None);
        };

        if payload.len() != compat_len {
            return Err(SystemError::EINVAL);
        }
        let mut normalized = vec![0u8; full_len];
        normalized[..compat_len].copy_from_slice(payload);
        Ok(Some(normalized))
    }

    pub fn write_reply(&self, data: &[u8]) -> Result<usize, SystemError> {
        if data.len() < core::mem::size_of::<FuseOutHeader>() {
            return Err(SystemError::EINVAL);
        }
        let out_hdr: FuseOutHeader = fuse_read_struct(data)?;
        if out_hdr.len as usize != data.len() {
            return Err(SystemError::EINVAL);
        }
        stats::on_dev_fuse_input_copy(data.len() - core::mem::size_of::<FuseOutHeader>());
        self.write_owned_reply(FuseReply::from_bytes(data.to_vec()))
    }

    pub(crate) fn write_owned_reply(&self, data: FuseReply) -> Result<usize, SystemError> {
        if data.len() < core::mem::size_of::<FuseOutHeader>() {
            return Err(SystemError::EINVAL);
        }

        let out_hdr: FuseOutHeader = fuse_read_struct(&data)?;
        if out_hdr.len as usize != data.len() {
            return Err(SystemError::EINVAL);
        }
        let from_virtiofs = data.is_virtiofs();

        if out_hdr.unique == 0 {
            let payload = &data[core::mem::size_of::<FuseOutHeader>()..];
            self.handle_notify(out_hdr.error, payload)?;
            return Ok(data.len());
        }

        if (out_hdr.unique & Self::FUSE_INT_REQ_BIT) != 0 {
            return self.write_interrupt_reply(&out_hdr, data.len());
        }

        if out_hdr.error <= -512 || out_hdr.error > 0 {
            return Err(SystemError::EINVAL);
        }
        if out_hdr.error != 0 && data.len() != core::mem::size_of::<FuseOutHeader>() {
            return Err(SystemError::EINVAL);
        }

        let (pending, negotiated_minor) = {
            let g = self.inner.lock();
            if !g.connected {
                return Err(SystemError::ENOENT);
            }
            let pending = g
                .processing
                .get(&out_hdr.unique)
                .cloned()
                .ok_or(SystemError::ENOENT)?;
            (pending, g.init.minor)
        };

        let payload = &data[core::mem::size_of::<FuseOutHeader>()..];
        let payload_len = payload.len();
        let error = out_hdr.error;

        if error != 0 {
            // Negative errno from userspace.
            let errno = -error;
            let e = SystemError::from_i32(errno).unwrap_or(SystemError::EIO);
            if Self::is_expected_reply_error(pending.opcode, errno) {
                log::trace!(
                    "fuse: reply error opcode={} unique={} errno={}",
                    pending.opcode,
                    out_hdr.unique,
                    errno
                );
            } else {
                log::warn!(
                    "fuse: reply error opcode={} unique={} errno={}",
                    pending.opcode,
                    out_hdr.unique,
                    errno
                );
            }
            self.claim_pending_reply(out_hdr.unique, &pending, |_| {})?;
            Self::complete_claimed_daemon_error(&pending, out_hdr.unique, e, error, payload_len);
            if matches!(pending.opcode, FUSE_INIT | FUSE_DESTROY) {
                self.abort();
            }
            return Ok(data.len());
        }

        if pending.opcode == FUSE_INIT {
            let init_out: FuseInitOut = match Self::parse_init_reply(payload) {
                Ok(v) => v,
                Err(e) => {
                    let error = -e.to_posix_errno();
                    self.claim_pending_reply(out_hdr.unique, &pending, |_| {})?;
                    Self::complete_claimed_reply_with_error(
                        &pending,
                        out_hdr.unique,
                        e,
                        error,
                        payload_len,
                    );
                    self.abort();
                    return Ok(data.len());
                }
            };

            if init_out.major != FUSE_KERNEL_VERSION {
                let error = -SystemError::EINVAL.to_posix_errno();
                self.claim_pending_reply(out_hdr.unique, &pending, |_| {})?;
                Self::complete_claimed_reply_with_error(
                    &pending,
                    out_hdr.unique,
                    SystemError::EINVAL,
                    error,
                    payload_len,
                );
                self.abort();
                return Ok(data.len());
            }

            let mut negotiated_flags = init_out.flags as u64;
            if (negotiated_flags & FUSE_INIT_EXT) != 0 {
                negotiated_flags |= (init_out.flags2 as u64) << 32;
            }
            let requested_flags = self.inner.lock().init_flags;
            let enabled_flags = negotiated_flags & requested_flags;
            if !Self::dax_map_alignment_valid(enabled_flags, init_out.map_alignment) {
                let error = -SystemError::EINVAL.to_posix_errno();
                self.claim_pending_reply(out_hdr.unique, &pending, |_| {})?;
                Self::complete_claimed_reply_with_error(
                    &pending,
                    out_hdr.unique,
                    SystemError::EINVAL,
                    error,
                    payload_len,
                );
                self.abort();
                return Ok(data.len());
            }
            let negotiated_minor = core::cmp::min(init_out.minor, FUSE_KERNEL_MINOR_VERSION);
            let negotiated_max_pages_raw = if (negotiated_flags & FUSE_MAX_PAGES) != 0 {
                core::cmp::max(init_out.max_pages, 1)
            } else {
                Self::DEFAULT_MAX_PAGES as u16
            };
            let negotiated_max_write =
                core::cmp::max(Self::MIN_MAX_WRITE, init_out.max_write as usize);
            let (max_write_cap, max_pages_limit) = {
                let g = self.inner.lock();
                (
                    core::cmp::max(Self::MIN_MAX_WRITE, g.max_write_cap),
                    g.max_pages_limit,
                )
            };
            let capped_max_write = core::cmp::min(negotiated_max_write, max_write_cap);
            if capped_max_write < negotiated_max_write {
                log::trace!(
                    "fuse: cap negotiated max_write from {} to {} due backend read buffer limit",
                    negotiated_max_write,
                    capped_max_write
                );
            }
            let negotiated_max_pages =
                core::cmp::min(negotiated_max_pages_raw, max_pages_limit as u16);
            let (max_background, congestion_threshold) = if negotiated_minor >= 13 {
                let max_background = if init_out.max_background == 0 {
                    Self::DEFAULT_MAX_BACKGROUND
                } else {
                    init_out.max_background as usize
                };
                let congestion = if init_out.congestion_threshold == 0 {
                    Self::DEFAULT_CONGESTION_THRESHOLD
                } else {
                    init_out.congestion_threshold as usize
                };
                (
                    core::cmp::max(1, max_background),
                    core::cmp::min(
                        core::cmp::max(1, congestion),
                        core::cmp::max(1, max_background),
                    ),
                )
            } else {
                (
                    Self::DEFAULT_MAX_BACKGROUND,
                    Self::DEFAULT_CONGESTION_THRESHOLD,
                )
            };

            self.claim_pending_reply(out_hdr.unique, &pending, |g| {
                // Align with Linux: only enable feature bits that are in the intersection of
                // daemon-supported and locally-requested flags. virtiofsd often sets
                // DO_READDIRPLUS etc. in the INIT reply; if adopted directly, it would
                // trigger READDIRPLUS + cache_child_from_entry, causing stale inode mappings.
                g.initialized = true;
                g.init = FuseInitNegotiated {
                    minor: negotiated_minor,
                    max_readahead: init_out.max_readahead,
                    max_write: capped_max_write as u32,
                    time_gran: init_out.time_gran,
                    max_pages: negotiated_max_pages,
                    flags: enabled_flags,
                    map_alignment: if (enabled_flags & FUSE_MAP_ALIGNMENT) != 0 {
                        init_out.map_alignment
                    } else {
                        0
                    },
                };
                self.reply_layout_minor
                    .store(negotiated_minor, Ordering::Release);
            })?;
            self.background
                .configure(max_background, congestion_threshold);
            stats::on_fuse_io_limits_negotiated(stats::NegotiatedFuseIoLimits {
                max_read: self.max_read(),
                max_write: self.max_write(),
                max_pages: negotiated_max_pages as usize,
                max_readahead: init_out.max_readahead as usize,
                async_read: (enabled_flags & FUSE_ASYNC_READ) != 0,
                writeback_cache: (enabled_flags & FUSE_WRITEBACK_CACHE) != 0,
                effective_read_payload_limit: self.effective_read_payload_limit(),
                effective_write_payload_limit: core::cmp::min(
                    core::cmp::min(
                        negotiated_max_pages as usize,
                        self.max_write() / MMArch::PAGE_SIZE,
                    ),
                    64,
                )
                .saturating_mul(MMArch::PAGE_SIZE),
            });
            self.init_wait.wakeup(None);
        } else {
            self.claim_pending_reply(out_hdr.unique, &pending, |_| {})?;
        }

        let normalized_payload = if pending.opcode == FUSE_INIT {
            None
        } else {
            match Self::normalize_compat_reply(negotiated_minor, pending.opcode, payload) {
                Ok(payload) => payload,
                Err(e) => {
                    let error = -e.to_posix_errno();
                    Self::complete_claimed_reply_with_error(
                        &pending,
                        out_hdr.unique,
                        e,
                        error,
                        payload_len,
                    );
                    if pending.opcode == FUSE_DESTROY {
                        self.abort();
                    }
                    return Ok(data.len());
                }
            }
        };
        let normalized_payload_len = normalized_payload.as_ref().map(Vec::len);

        let data_len = data.len();
        let payload_reply = match data.narrow(core::mem::size_of::<FuseOutHeader>()..data_len) {
            Ok(reply) => reply,
            Err(e) => {
                let error = -e.to_posix_errno();
                Self::complete_claimed_reply_with_error(
                    &pending,
                    out_hdr.unique,
                    e,
                    error,
                    payload_len,
                );
                if matches!(pending.opcode, FUSE_INIT | FUSE_DESTROY) {
                    self.abort();
                }
                return Ok(data_len);
            }
        };
        let payload_reply = if let Some(normalized) = normalized_payload {
            match payload_reply.into_compat_bytes(normalized) {
                Ok(reply) => reply,
                Err(e) => {
                    let error = -e.to_posix_errno();
                    Self::complete_claimed_reply_with_error(
                        &pending,
                        out_hdr.unique,
                        e,
                        error,
                        payload_len,
                    );
                    if pending.opcode == FUSE_DESTROY {
                        self.abort();
                    }
                    return Ok(data_len);
                }
            }
        } else {
            payload_reply
        };

        let transferred = payload_reply.is_device_transfer();
        if pending.complete(Ok(payload_reply)) {
            stats::on_fuse_reply_complete(pending.opcode, 0, payload_len);
            if transferred {
                stats::on_fuse_reply_payload_transfer(pending.opcode, payload_len);
            } else {
                stats::on_fuse_reply_payload_copy(pending.opcode, payload_len);
                if from_virtiofs {
                    stats::on_virtiofs_compat_copy(normalized_payload_len.unwrap_or(payload_len));
                }
            }
            trace::trace_fuse_reply_complete(out_hdr.unique, pending.opcode, 0, payload_len as u64);
        }
        if pending.opcode == FUSE_DESTROY {
            self.abort();
        }
        Ok(data_len)
    }

    /// Linux supplies an output header buffer for DESTROY, while virtiofsd completes the
    /// descriptor with zero used bytes. Linux treats the zero-initialized request header as a
    /// successful completion because DESTROY has no output arguments. Preserve that transport
    /// semantic without relaxing validation for any other opcode.
    pub(crate) fn complete_destroy_without_reply(&self, unique: u64) -> Result<(), SystemError> {
        let pending = {
            let mut g = self.inner.lock();
            if !g.connected {
                return Err(SystemError::ENOENT);
            }
            let pending = g.processing.remove(&unique).ok_or(SystemError::ENOENT)?;
            if pending.opcode != FUSE_DESTROY {
                g.processing.insert(unique, pending);
                return Err(SystemError::EINVAL);
            }
            pending
        };

        stats::on_fuse_reply_complete(FUSE_DESTROY, 0, 0);
        trace::trace_fuse_reply_complete(unique, FUSE_DESTROY, 0, 0);
        pending.complete(Ok(FuseReply::from_bytes(Vec::new())));
        self.abort();
        Ok(())
    }

    /// Retire a successful FUSE_READ whose payload was written into the request's owned page
    /// destination instead of a `FuseReply` allocation.
    pub(crate) fn complete_read_pages_direct(
        &self,
        unique: u64,
        payload_len: usize,
    ) -> Result<(), SystemError> {
        let pending = {
            let g = self.inner.lock();
            if !g.connected || g.teardown_started {
                return Err(SystemError::ENOENT);
            }
            let pending = g
                .processing
                .get(&unique)
                .cloned()
                .ok_or(SystemError::ENOENT)?;
            if pending.opcode != FUSE_READ {
                return Err(SystemError::EINVAL);
            }
            pending
        };
        self.claim_pending_reply(unique, &pending, |_| {})?;
        if pending.complete_read_pages_direct(payload_len) {
            stats::on_fuse_reply_complete(FUSE_READ, 0, payload_len);
            trace::trace_fuse_reply_complete(unique, FUSE_READ, 0, payload_len as u64);
        }
        Ok(())
    }

    fn write_interrupt_reply(
        &self,
        out_hdr: &FuseOutHeader,
        data_len: usize,
    ) -> Result<usize, SystemError> {
        if data_len != core::mem::size_of::<FuseOutHeader>() {
            return Err(SystemError::EINVAL);
        }
        if out_hdr.error <= -512 || out_hdr.error > 0 {
            return Err(SystemError::EINVAL);
        }

        let target_unique = out_hdr.unique & !Self::FUSE_INT_REQ_BIT;
        {
            let mut g = self.inner.lock();
            if !g.connected || !g.processing.contains_key(&target_unique) {
                return Err(SystemError::ENOENT);
            }
            if out_hdr.error == -SystemError::ENOSYS.to_posix_errno() {
                g.no_interrupt = true;
            }
        }

        if out_hdr.error == -SystemError::EAGAIN_OR_EWOULDBLOCK.to_posix_errno() {
            self.queue_interrupt(target_unique)?;
        }

        Ok(data_len)
    }

    fn handle_notify(&self, code: i32, payload: &[u8]) -> Result<(), SystemError> {
        if code <= 0 {
            return Err(SystemError::EINVAL);
        }
        match code {
            FUSE_NOTIFY_INVAL_INODE => {
                if payload.len() != size_of::<FuseNotifyInvalInodeOut>() {
                    return Err(SystemError::EINVAL);
                }
                let arg: FuseNotifyInvalInodeOut = fuse_read_struct(payload)?;
                self.notify_nodes(arg.ino, |node| {
                    node.notify_invalidate_pages(arg.off, arg.len)
                })
            }
            FUSE_NOTIFY_INVAL_ENTRY => {
                let (arg, name) = Self::notify_entry(payload)?;
                if arg.flags & FUSE_EXPIRE_ONLY != 0 {
                    if arg.flags != FUSE_EXPIRE_ONLY {
                        return Err(SystemError::EINVAL);
                    }
                    return self.notify_nodes(arg.parent, |node| node.notify_expire_child(name));
                }
                if arg.flags != 0 {
                    return Err(SystemError::EINVAL);
                }
                self.notify_nodes(arg.parent, |node| node.notify_invalidate_child(name, None))
            }
            FUSE_NOTIFY_DELETE => {
                let (arg, name) = Self::notify_delete(payload)?;
                self.notify_nodes(arg.parent, |node| {
                    node.notify_invalidate_child(name, Some(arg.child))
                })
            }
            FUSE_NOTIFY_POLL | FUSE_NOTIFY_STORE | FUSE_NOTIFY_RETRIEVE => {
                Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
            }
            _ => Err(SystemError::EINVAL),
        }
    }

    fn notify_nodes<F>(&self, nodeid: u64, mut notify: F) -> Result<(), SystemError>
    where
        F: FnMut(&Arc<super::super::inode::FuseNode>) -> Result<(), SystemError>,
    {
        let mut matched = false;
        let mut succeeded = false;
        let mut soft_error = None;
        let mut hard_error = None;
        for fs in self.filesystems() {
            let Some(result) = fs.notify_node(nodeid, |node| notify(node)) else {
                continue;
            };
            matched = true;
            match result {
                Ok(()) => succeeded = true,
                Err(SystemError::ENOENT) => soft_error = Some(SystemError::ENOENT),
                Err(error) => {
                    if hard_error.is_none() {
                        hard_error = Some(error);
                    }
                }
            }
        }
        if let Some(error) = hard_error {
            Err(error)
        } else if !matched {
            Err(SystemError::ENOENT)
        } else if succeeded {
            Ok(())
        } else if let Some(error) = soft_error {
            Err(error)
        } else {
            Ok(())
        }
    }

    fn notify_name(payload: &[u8], header_len: usize, namelen: usize) -> Result<&str, SystemError> {
        if namelen > 255 || payload.len() != header_len.saturating_add(namelen).saturating_add(1) {
            return Err(if namelen > 255 {
                SystemError::ENAMETOOLONG
            } else {
                SystemError::EINVAL
            });
        }
        let bytes = &payload[header_len..header_len + namelen];
        if payload[header_len + namelen] != 0 || bytes.contains(&0) {
            return Err(SystemError::EINVAL);
        }
        core::str::from_utf8(bytes).map_err(|_| SystemError::EINVAL)
    }

    fn notify_entry(payload: &[u8]) -> Result<(FuseNotifyInvalEntryOut, &str), SystemError> {
        if payload.len() < size_of::<FuseNotifyInvalEntryOut>() {
            return Err(SystemError::EINVAL);
        }
        let arg: FuseNotifyInvalEntryOut = fuse_read_struct(payload)?;
        let name = Self::notify_name(
            payload,
            size_of::<FuseNotifyInvalEntryOut>(),
            arg.namelen as usize,
        )?;
        Ok((arg, name))
    }

    fn notify_delete(payload: &[u8]) -> Result<(FuseNotifyDeleteOut, &str), SystemError> {
        if payload.len() < size_of::<FuseNotifyDeleteOut>() {
            return Err(SystemError::EINVAL);
        }
        let arg: FuseNotifyDeleteOut = fuse_read_struct(payload)?;
        let name = Self::notify_name(
            payload,
            size_of::<FuseNotifyDeleteOut>(),
            arg.namelen as usize,
        )?;
        Ok((arg, name))
    }

    fn is_expected_reply_error(opcode: u32, errno: i32) -> bool {
        matches!(
            (opcode, SystemError::from_i32(errno)),
            (FUSE_LOOKUP, Some(SystemError::ENOENT))
                | (FUSE_FLUSH, Some(SystemError::ENOSYS))
                | (FUSE_GETXATTR, Some(SystemError::ENOSYS))
                | (FUSE_SETXATTR, Some(SystemError::ENOSYS))
                | (FUSE_LISTXATTR, Some(SystemError::ENOSYS))
                | (FUSE_REMOVEXATTR, Some(SystemError::ENOSYS))
                | (FUSE_INTERRUPT, Some(SystemError::EAGAIN_OR_EWOULDBLOCK))
        )
    }
}
#[cfg(test)]
pub(super) fn normalize_compat_reply_for_test(
    minor: u32,
    opcode: u32,
    payload: &[u8],
) -> Result<Vec<u8>, SystemError> {
    FuseConn::normalize_compat_reply(minor, opcode, payload)?.ok_or(SystemError::EINVAL)
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, vec, vec::Vec};
    use core::{mem::size_of, sync::atomic::Ordering};

    use system_error::SystemError;

    use crate::filesystem::fuse::protocol::FUSE_FORGET;

    use super::super::request::queue_test_request;
    use super::*;

    fn read_opcode(buf: &[u8]) -> u32 {
        let hdr: FuseInHeader = fuse_read_struct(buf).unwrap();
        hdr.opcode
    }

    #[test]
    fn init_reply_accepts_zero_extended_compat_prefix_only() {
        assert!(matches!(
            FuseConn::parse_init_reply(&[0u8; 7]),
            Err(SystemError::EINVAL)
        ));
        let mut compat = [0u8; FuseConn::FUSE_COMPAT_INIT_OUT_SIZE];
        compat[..4].copy_from_slice(&super::FUSE_KERNEL_VERSION.to_ne_bytes());
        compat[4..].copy_from_slice(&3u32.to_ne_bytes());
        let parsed = FuseConn::parse_init_reply(&compat).unwrap();
        assert_eq!(parsed.major, super::FUSE_KERNEL_VERSION);
        assert_eq!(parsed.minor, 3);
        assert_eq!(parsed.max_write, 0);
        assert!(matches!(
            FuseConn::parse_init_reply(&vec![0u8; size_of::<FuseInitOut>() + 1]),
            Err(SystemError::EINVAL)
        ));
    }

    #[test]
    fn ordinary_read_does_not_consume_high_priority_queue() {
        let conn = FuseConn::new_for_virtiofs(8192, 8192);
        queue_test_request(&conn, FUSE_FORGET, 1, &[], false);
        queue_test_request(&conn, FUSE_LOOKUP, 1, &[], true);

        assert!(conn.has_pending_high_priority_requests());
        assert!(conn.has_pending_ordinary_requests());

        let mut buf = Vec::new();
        buf.resize(conn.min_read_buffer(), 0);
        let len = conn.read_ordinary_request(true, &mut buf).unwrap();
        assert_eq!(read_opcode(&buf[..len]), FUSE_LOOKUP);
        assert!(conn.has_pending_high_priority_requests());
        assert!(!conn.has_pending_ordinary_requests());

        let len = conn.read_high_priority_request(&mut buf).unwrap();
        assert_eq!(read_opcode(&buf[..len]), FUSE_FORGET);
        assert!(!conn.has_pending_high_priority_requests());
    }

    #[test]
    fn ordinary_read_returns_eagain_when_only_high_priority_is_pending() {
        let conn = FuseConn::new_for_virtiofs(8192, 8192);
        queue_test_request(&conn, FUSE_FORGET, 1, &[], false);

        let mut buf = Vec::new();
        buf.resize(conn.min_read_buffer(), 0);
        assert!(matches!(
            conn.read_ordinary_request(true, &mut buf),
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
        ));
        assert!(conn.has_pending_high_priority_requests());
    }

    #[test]
    fn virtiofs_direct_dequeue_transfers_existing_request() {
        let conn = FuseConn::new_for_virtiofs(8192, 8192);
        let (unique, expected_ptr, _) = queue_test_request(&conn, FUSE_LOOKUP, 1, b"name\0", true);

        let dequeued = conn.dequeue_virtiofs_ordinary_request(8192).unwrap();
        assert_eq!(dequeued.opcode(), FUSE_LOOKUP);
        assert_eq!(dequeued.unique(), unique);
        assert_eq!(dequeued.bytes().as_ptr(), expected_ptr);
    }

    #[test]
    fn virtiofs_direct_dequeue_keeps_priority_queues_separate() {
        let conn = FuseConn::new_for_virtiofs(8192, 8192);
        queue_test_request(&conn, FUSE_FORGET, 1, &[], false);
        queue_test_request(&conn, FUSE_LOOKUP, 1, &[], true);

        let ordinary = conn.dequeue_virtiofs_ordinary_request(8192).unwrap();
        assert_eq!(ordinary.opcode(), FUSE_LOOKUP);
        let hiprio = conn.dequeue_virtiofs_high_priority_request(8192).unwrap();
        assert_eq!(hiprio.opcode(), FUSE_FORGET);
    }

    #[test]
    fn virtiofs_direct_dequeue_rejects_oversized_request() {
        let conn = FuseConn::new_for_virtiofs(8192, 8192);
        queue_test_request(&conn, FUSE_FORGET, 1, &[0u8; 128], false);
        queue_test_request(&conn, FUSE_FORGET, 1, &[], false);

        assert!(matches!(
            conn.dequeue_virtiofs_high_priority_request(64),
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
        ));
        assert!(conn.has_pending_high_priority_requests());
        let next = conn.dequeue_virtiofs_high_priority_request(8192).unwrap();
        assert_eq!(next.opcode(), FUSE_FORGET);
        assert!(!conn.has_pending_high_priority_requests());
    }

    #[test]
    fn normal_fuse_read_request_keeps_high_priority_visible() {
        let conn = FuseConn::new();
        queue_test_request(&conn, FUSE_FORGET, 1, &[], false);

        assert!(!conn.has_pending_high_priority_requests());
        assert!(conn.has_pending_ordinary_requests());

        let mut buf = Vec::new();
        buf.resize(conn.min_read_buffer(), 0);
        let len = conn.read_request(true, &mut buf).unwrap();
        assert_eq!(read_opcode(&buf[..len]), FUSE_FORGET);
    }

    #[test]
    fn zero_length_destroy_completion_finishes_unique_and_disconnects() {
        let conn = FuseConn::new_for_virtiofs(8192, 8192);
        let (unique, _, pending) = queue_test_request(&conn, FUSE_DESTROY, 0, &[], true);
        let pending = pending.unwrap();
        conn.dequeue_virtiofs_ordinary_request(8192).unwrap();

        conn.complete_destroy_without_reply(unique).unwrap();
        assert!(!conn.is_connected());
        assert_eq!(pending.wait_complete().unwrap(), Vec::<u8>::new());
    }
}
