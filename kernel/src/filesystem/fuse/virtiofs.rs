use alloc::{
    boxed::Box,
    collections::{BTreeMap, VecDeque},
    string::String,
    sync::Arc,
    vec,
    vec::Vec,
};

use linkme::distributed_slice;
use log::{debug, warn};
use system_error::SystemError;
use virtio_drivers::{
    queue::VirtQueue,
    transport::{DeviceStatus, Transport},
    Error as VirtioError, PAGE_SIZE,
};

use crate::{
    driver::virtio::{
        transport::VirtIOTransport,
        virtio_drivers_error_to_system_error,
        virtio_fs::{virtio_fs_find_instance, VirtioFsInstance},
        virtio_impl::HalImpl,
    },
    filesystem::vfs::{
        FileSystem, FileSystemMakerData, FsInfo, IndexNode, MountableFileSystem, SuperBlock,
        FSMAKER,
    },
    process::{kthread::KernelThreadClosure, kthread::KernelThreadMechanism, ProcessManager},
    register_mountable_fs,
    time::{sleep::nanosleep, PosixTimeSpec},
};

use super::{
    conn::FuseConn,
    fs::{FuseFS, FuseMountData},
    protocol::{
        fuse_pack_struct, fuse_read_struct, FuseInHeader, FuseOutHeader, FUSE_DESTROY, FUSE_FORGET,
    },
};

const VIRTIOFS_REQ_QUEUE_SIZE: usize = 8;
const VIRTIOFS_REQ_BUF_SIZE: usize = 256 * 1024;
const VIRTIOFS_RSP_BUF_SIZE: usize = 256 * 1024;
const VIRTIOFS_POLL_NS: i64 = 1_000_000;
const VIRTIOFS_PUMP_BUDGET: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueKind {
    Hiprio,
    Request(usize),
}

#[derive(Debug)]
struct PendingReq {
    req: Vec<u8>,
    unique: u64,
    opcode: u32,
    noreply: bool,
    queue: QueueKind,
}

#[derive(Debug)]
struct InflightReq {
    pending: PendingReq,
    rsp: Option<Vec<u8>>,
}

struct VirtioFsBridgeContext {
    instance: Arc<VirtioFsInstance>,
    conn: Arc<FuseConn>,
    transport: Option<VirtIOTransport>,
    hiprio_vq: Option<VirtQueue<HalImpl, { VIRTIOFS_REQ_QUEUE_SIZE }>>,
    request_vqs: Vec<VirtQueue<HalImpl, { VIRTIOFS_REQ_QUEUE_SIZE }>>,
    hiprio_pending: VecDeque<PendingReq>,
    request_pending: Vec<VecDeque<PendingReq>>,
    hiprio_inflight: BTreeMap<u16, InflightReq>,
    request_inflight: Vec<BTreeMap<u16, InflightReq>>,
    next_request_slot: usize,
    req_buf: Vec<u8>,
}

impl VirtioFsBridgeContext {
    fn poll_pause() {
        let _ = nanosleep(PosixTimeSpec::new(0, VIRTIOFS_POLL_NS));
    }

    fn has_internal_pending(&self) -> bool {
        !self.hiprio_pending.is_empty() || self.request_pending.iter().any(|q| !q.is_empty())
    }

    fn has_inflight(&self) -> bool {
        !self.hiprio_inflight.is_empty() || self.request_inflight.iter().any(|m| !m.is_empty())
    }

    fn push_pending_back(&mut self, pending: PendingReq) -> Result<(), SystemError> {
        match pending.queue {
            QueueKind::Hiprio => self.hiprio_pending.push_back(pending),
            QueueKind::Request(slot) => self
                .request_pending
                .get_mut(slot)
                .ok_or(SystemError::EINVAL)?
                .push_back(pending),
        }
        Ok(())
    }

    fn push_pending_front(&mut self, pending: PendingReq) -> Result<(), SystemError> {
        match pending.queue {
            QueueKind::Hiprio => self.hiprio_pending.push_front(pending),
            QueueKind::Request(slot) => self
                .request_pending
                .get_mut(slot)
                .ok_or(SystemError::EINVAL)?
                .push_front(pending),
        }
        Ok(())
    }

    fn pop_pending_front(&mut self, kind: QueueKind) -> Result<Option<PendingReq>, SystemError> {
        Ok(match kind {
            QueueKind::Hiprio => self.hiprio_pending.pop_front(),
            QueueKind::Request(slot) => self
                .request_pending
                .get_mut(slot)
                .ok_or(SystemError::EINVAL)?
                .pop_front(),
        })
    }

    fn queue_index(&self, kind: QueueKind) -> Result<u16, SystemError> {
        match kind {
            QueueKind::Hiprio => Ok(self.instance.hiprio_queue_index()),
            QueueKind::Request(slot) => self
                .instance
                .request_queue_index_by_slot(slot)
                .ok_or(SystemError::EINVAL),
        }
    }

    fn take_inflight(&mut self, kind: QueueKind, token: u16) -> Result<InflightReq, SystemError> {
        match kind {
            QueueKind::Hiprio => self.hiprio_inflight.remove(&token).ok_or(SystemError::EIO),
            QueueKind::Request(slot) => self
                .request_inflight
                .get_mut(slot)
                .ok_or(SystemError::EINVAL)?
                .remove(&token)
                .ok_or(SystemError::EIO),
        }
    }

    fn put_back_inflight(
        &mut self,
        kind: QueueKind,
        token: u16,
        inflight: InflightReq,
    ) -> Result<(), SystemError> {
        let replaced = match kind {
            QueueKind::Hiprio => self.hiprio_inflight.insert(token, inflight),
            QueueKind::Request(slot) => self
                .request_inflight
                .get_mut(slot)
                .ok_or(SystemError::EINVAL)?
                .insert(token, inflight),
        };
        debug_assert!(replaced.is_none());
        Ok(())
    }

    fn complete_request_with_errno(conn: &Arc<FuseConn>, unique: u64, errno: i32) {
        if unique == 0 {
            return;
        }
        let out_hdr = FuseOutHeader {
            len: core::mem::size_of::<FuseOutHeader>() as u32,
            error: errno,
            unique,
        };
        let payload = fuse_pack_struct(&out_hdr);
        let _ = conn.write_reply(payload);
    }

    fn complete_request_with_error(&self, unique: u64, err: SystemError) {
        Self::complete_request_with_errno(&self.conn, unique, err.to_posix_errno());
    }

    fn choose_request_slot(&mut self) -> Result<usize, SystemError> {
        if self.request_vqs.is_empty() {
            return Err(SystemError::ENODEV);
        }
        let slot = self.next_request_slot % self.request_vqs.len();
        self.next_request_slot = (self.next_request_slot + 1) % self.request_vqs.len();
        Ok(slot)
    }

    fn pump_new_requests(&mut self) -> Result<bool, SystemError> {
        let mut progressed = false;
        for _ in 0..VIRTIOFS_PUMP_BUDGET {
            let len = match self.conn.read_request(true, &mut self.req_buf) {
                Ok(len) => len,
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => break,
                Err(e) => return Err(e),
            };
            let req = self.req_buf[..len].to_vec();
            let in_hdr: FuseInHeader = fuse_read_struct(&req)?;
            let noreply = matches!(in_hdr.opcode, FUSE_FORGET | FUSE_DESTROY);
            let queue = if in_hdr.opcode == FUSE_FORGET {
                QueueKind::Hiprio
            } else {
                QueueKind::Request(self.choose_request_slot()?)
            };
            self.push_pending_back(PendingReq {
                req,
                unique: in_hdr.unique,
                opcode: in_hdr.opcode,
                noreply,
                queue,
            })?;
            progressed = true;
        }
        Ok(progressed)
    }

    fn submit_one_pending(&mut self, kind: QueueKind) -> Result<bool, SystemError> {
        let pending = match self.pop_pending_front(kind)? {
            Some(p) => p,
            None => return Ok(false),
        };
        let queue_idx = self.queue_index(kind)?;
        let mut rsp = if pending.noreply {
            None
        } else {
            Some(vec![0u8; VIRTIOFS_RSP_BUF_SIZE])
        };

        let (token, should_notify) = match kind {
            QueueKind::Hiprio => {
                let queue = self.hiprio_vq.as_mut().ok_or(SystemError::EIO)?;
                let token = if let Some(rsp_buf) = rsp.as_mut() {
                    let inputs = [pending.req.as_slice()];
                    let mut outputs = [rsp_buf.as_mut_slice()];
                    unsafe { queue.add(&inputs, &mut outputs) }
                } else {
                    let inputs = [pending.req.as_slice()];
                    let mut outputs: [&mut [u8]; 0] = [];
                    unsafe { queue.add(&inputs, &mut outputs) }
                };
                (token, queue.should_notify())
            }
            QueueKind::Request(slot) => {
                let queue = self.request_vqs.get_mut(slot).ok_or(SystemError::EINVAL)?;
                let token = if let Some(rsp_buf) = rsp.as_mut() {
                    let inputs = [pending.req.as_slice()];
                    let mut outputs = [rsp_buf.as_mut_slice()];
                    unsafe { queue.add(&inputs, &mut outputs) }
                } else {
                    let inputs = [pending.req.as_slice()];
                    let mut outputs: [&mut [u8]; 0] = [];
                    unsafe { queue.add(&inputs, &mut outputs) }
                };
                (token, queue.should_notify())
            }
        };

        let token = match token {
            Ok(token) => token,
            Err(VirtioError::QueueFull) | Err(VirtioError::NotReady) => {
                self.push_pending_front(pending)?;
                return Ok(false);
            }
            Err(e) => {
                let se = virtio_drivers_error_to_system_error(e);
                warn!(
                    "virtiofs bridge: submit failed opcode={} unique={} queue={:?} err={:?}",
                    pending.opcode, pending.unique, kind, se
                );
                if !pending.noreply {
                    self.complete_request_with_error(pending.unique, se);
                }
                return Ok(true);
            }
        };

        if should_notify {
            self.transport
                .as_mut()
                .ok_or(SystemError::EIO)?
                .notify(queue_idx);
        }

        let inflight = InflightReq { pending, rsp };
        match kind {
            QueueKind::Hiprio => {
                self.hiprio_inflight.insert(token, inflight);
            }
            QueueKind::Request(slot) => {
                self.request_inflight
                    .get_mut(slot)
                    .ok_or(SystemError::EINVAL)?
                    .insert(token, inflight);
            }
        }
        Ok(true)
    }

    fn submit_pending(&mut self) -> Result<bool, SystemError> {
        let mut progressed = false;
        while self.submit_one_pending(QueueKind::Hiprio)? {
            progressed = true;
        }

        for slot in 0..self.request_vqs.len() {
            while self.submit_one_pending(QueueKind::Request(slot))? {
                progressed = true;
            }
        }
        Ok(progressed)
    }

    fn pop_one_used(&mut self, kind: QueueKind) -> Result<bool, SystemError> {
        let token = match kind {
            QueueKind::Hiprio => {
                let queue = self.hiprio_vq.as_mut().ok_or(SystemError::EIO)?;
                if !queue.can_pop() {
                    return Ok(false);
                }
                queue.peek_used().ok_or(SystemError::EIO)?
            }
            QueueKind::Request(slot) => {
                let queue = self.request_vqs.get_mut(slot).ok_or(SystemError::EINVAL)?;
                if !queue.can_pop() {
                    return Ok(false);
                }
                queue.peek_used().ok_or(SystemError::EIO)?
            }
        };

        let mut inflight = self.take_inflight(kind, token)?;

        let used_len_res = match kind {
            QueueKind::Hiprio => {
                let queue = self.hiprio_vq.as_mut().ok_or(SystemError::EIO)?;
                let inputs = [inflight.pending.req.as_slice()];
                if let Some(rsp_buf) = inflight.rsp.as_mut() {
                    let mut outputs = [rsp_buf.as_mut_slice()];
                    unsafe { queue.pop_used(token, &inputs, &mut outputs) }
                        .map_err(virtio_drivers_error_to_system_error)
                } else {
                    let mut outputs: [&mut [u8]; 0] = [];
                    unsafe { queue.pop_used(token, &inputs, &mut outputs) }
                        .map_err(virtio_drivers_error_to_system_error)
                }
            }
            QueueKind::Request(slot) => {
                let queue = self.request_vqs.get_mut(slot).ok_or(SystemError::EINVAL)?;
                let inputs = [inflight.pending.req.as_slice()];
                if let Some(rsp_buf) = inflight.rsp.as_mut() {
                    let mut outputs = [rsp_buf.as_mut_slice()];
                    unsafe { queue.pop_used(token, &inputs, &mut outputs) }
                        .map_err(virtio_drivers_error_to_system_error)
                } else {
                    let mut outputs: [&mut [u8]; 0] = [];
                    unsafe { queue.pop_used(token, &inputs, &mut outputs) }
                        .map_err(virtio_drivers_error_to_system_error)
                }
            }
        };

        let used_len = match used_len_res {
            Ok(v) => v as usize,
            Err(e) => {
                let unique = inflight.pending.unique;
                self.put_back_inflight(kind, token, inflight)?;
                warn!(
                    "virtiofs bridge: pop_used failed unique={} token={} queue={:?} err={:?}",
                    unique, token, kind, e
                );
                return Err(e);
            }
        };

        if inflight.pending.noreply {
            return Ok(true);
        }

        let rsp_buf = inflight.rsp.as_ref().ok_or(SystemError::EIO)?;
        if used_len > rsp_buf.len() {
            self.complete_request_with_error(inflight.pending.unique, SystemError::EIO);
            return Ok(true);
        }

        match self.conn.write_reply(&rsp_buf[..used_len]) {
            Ok(_) | Err(SystemError::ENOENT) => {}
            Err(e) => return Err(e),
        }
        Ok(true)
    }

    fn drain_completions(&mut self) -> Result<bool, SystemError> {
        let mut progressed = false;
        while self.pop_one_used(QueueKind::Hiprio)? {
            progressed = true;
        }

        for slot in 0..self.request_vqs.len() {
            while self.pop_one_used(QueueKind::Request(slot))? {
                progressed = true;
            }
        }

        Ok(progressed)
    }

    fn fail_all_unfinished(&mut self, err: SystemError) {
        let conn = self.conn.clone();
        let errno = err.to_posix_errno();
        let mut need_reply = Vec::new();

        while let Some(req) = self.hiprio_pending.pop_front() {
            if !req.noreply {
                need_reply.push(req.unique);
            }
        }

        for pending_q in &mut self.request_pending {
            while let Some(req) = pending_q.pop_front() {
                if !req.noreply {
                    need_reply.push(req.unique);
                }
            }
        }

        for (_, req) in self.hiprio_inflight.iter() {
            if !req.pending.noreply {
                need_reply.push(req.pending.unique);
            }
        }
        self.hiprio_inflight.clear();

        for inflight_map in &mut self.request_inflight {
            for (_, req) in inflight_map.iter() {
                if !req.pending.noreply {
                    need_reply.push(req.pending.unique);
                }
            }
            inflight_map.clear();
        }

        for unique in need_reply {
            Self::complete_request_with_errno(&conn, unique, errno);
        }
    }

    fn run_loop(&mut self) -> Result<(), SystemError> {
        loop {
            let mut progressed = false;

            match self.pump_new_requests() {
                Ok(v) => progressed |= v,
                Err(SystemError::ENOTCONN) => {}
                Err(e) => {
                    warn!("virtiofs bridge: read_request failed: {:?}", e);
                    if !self.conn.is_connected() {
                        break;
                    }
                }
            }

            progressed |= self.submit_pending()?;

            if let Some(transport) = self.transport.as_mut() {
                let _ = transport.ack_interrupt();
            }
            progressed |= self.drain_completions()?;

            if !self.conn.is_connected() && !self.has_inflight() {
                break;
            }

            if !self.conn.is_mounted()
                && !self.conn.has_pending_requests()
                && !self.has_internal_pending()
                && !self.has_inflight()
            {
                break;
            }

            if !progressed {
                Self::poll_pause();
            }
        }

        Ok(())
    }

    fn finish(&mut self) {
        self.fail_all_unfinished(SystemError::ENOTCONN);

        if let Some(transport) = self.transport.as_mut() {
            if self.hiprio_vq.take().is_some() {
                transport.queue_unset(self.instance.hiprio_queue_index());
            }
            for slot in 0..self.request_vqs.len() {
                if let Some(idx) = self.instance.request_queue_index_by_slot(slot) {
                    transport.queue_unset(idx);
                }
            }
            self.request_vqs.clear();
            transport.set_status(DeviceStatus::empty());
        }

        if let Some(transport) = self.transport.take() {
            self.instance.put_transport_after_session(transport);
        }
    }
}

fn virtiofs_bridge_thread_entry(arg: usize) -> i32 {
    let mut ctx = unsafe { Box::from_raw(arg as *mut VirtioFsBridgeContext) };
    let result = ctx.run_loop();
    if let Err(e) = &result {
        warn!("virtiofs bridge thread exit with error: {:?}", e);
    }
    ctx.finish();
    result.map(|_| 0).unwrap_or_else(|e| e.to_posix_errno())
}

fn start_bridge(instance: Arc<VirtioFsInstance>, conn: Arc<FuseConn>) -> Result<(), SystemError> {
    let mut transport = instance.take_transport_for_session()?;
    if instance.request_queue_count() == 0 {
        warn!(
            "virtiofs bridge: no request queues: tag='{}' dev={:?}",
            instance.tag(),
            instance.dev_id(),
        );
        instance.put_transport_after_session(transport);
        return Err(SystemError::EINVAL);
    }

    debug!(
        "virtiofs bridge: start tag='{}' dev={:?} request_queues={}",
        instance.tag(),
        instance.dev_id(),
        instance.num_request_queues()
    );

    transport.set_status(DeviceStatus::empty());
    transport.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER);
    let _device_features = transport.read_device_features();
    transport.write_driver_features(0);
    transport
        .set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER | DeviceStatus::FEATURES_OK);
    transport.set_guest_page_size(PAGE_SIZE as u32);

    let hiprio_vq = match VirtQueue::<HalImpl, { VIRTIOFS_REQ_QUEUE_SIZE }>::new(
        &mut transport,
        instance.hiprio_queue_index(),
        false,
        false,
    ) {
        Ok(vq) => vq,
        Err(e) => {
            let se = virtio_drivers_error_to_system_error(e);
            transport.set_status(DeviceStatus::FAILED);
            instance.put_transport_after_session(transport);
            return Err(se);
        }
    };

    let mut request_vqs = Vec::with_capacity(instance.request_queue_count());
    for slot in 0..instance.request_queue_count() {
        let idx = instance
            .request_queue_index_by_slot(slot)
            .ok_or(SystemError::EINVAL)?;
        let vq = match VirtQueue::<HalImpl, { VIRTIOFS_REQ_QUEUE_SIZE }>::new(
            &mut transport,
            idx,
            false,
            false,
        ) {
            Ok(vq) => vq,
            Err(e) => {
                let se = virtio_drivers_error_to_system_error(e);
                transport.set_status(DeviceStatus::FAILED);
                instance.put_transport_after_session(transport);
                return Err(se);
            }
        };
        request_vqs.push(vq);
    }
    transport.finish_init();

    let ctx = Box::new(VirtioFsBridgeContext {
        instance,
        conn,
        transport: Some(transport),
        hiprio_vq: Some(hiprio_vq),
        request_pending: core::iter::repeat_with(VecDeque::new)
            .take(request_vqs.len())
            .collect(),
        request_inflight: core::iter::repeat_with(BTreeMap::new)
            .take(request_vqs.len())
            .collect(),
        request_vqs,
        hiprio_pending: VecDeque::new(),
        hiprio_inflight: BTreeMap::new(),
        next_request_slot: 0,
        req_buf: vec![0u8; VIRTIOFS_REQ_BUF_SIZE],
    });

    let raw = Box::into_raw(ctx);
    if KernelThreadMechanism::create_and_run(
        KernelThreadClosure::StaticUsizeClosure((
            &(virtiofs_bridge_thread_entry as fn(usize) -> i32),
            raw as usize,
        )),
        String::from("virtiofs-bridge"),
    )
    .is_none()
    {
        let mut ctx = unsafe { Box::from_raw(raw) };
        ctx.finish();
        return Err(SystemError::ENOMEM);
    }

    Ok(())
}

#[derive(Debug)]
struct VirtioFsMountData {
    rootmode: u32,
    user_id: u32,
    group_id: u32,
    allow_other: bool,
    default_permissions: bool,
    dax_mode: VirtioFsDaxMode,
    conn: Arc<FuseConn>,
    instance: Arc<VirtioFsInstance>,
}

impl FileSystemMakerData for VirtioFsMountData {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

#[derive(Debug)]
struct VirtioFsFs {
    inner: Arc<dyn FileSystem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VirtioFsDaxMode {
    Never,
    Always,
    Inode,
}

impl VirtioFsFs {
    fn parse_opt_u32_decimal(v: &str) -> Result<u32, SystemError> {
        v.parse::<u32>().map_err(|_| SystemError::EINVAL)
    }

    fn parse_opt_u32_octal(v: &str) -> Result<u32, SystemError> {
        u32::from_str_radix(v, 8).map_err(|_| SystemError::EINVAL)
    }

    fn parse_opt_bool_switch(v: &str) -> bool {
        v.is_empty() || v != "0"
    }

    fn parse_dax_mode(v: &str) -> Result<VirtioFsDaxMode, SystemError> {
        if v.is_empty() {
            return Ok(VirtioFsDaxMode::Always);
        }

        match v {
            "always" => Ok(VirtioFsDaxMode::Always),
            "never" => Ok(VirtioFsDaxMode::Never),
            "inode" => Ok(VirtioFsDaxMode::Inode),
            _ => Err(SystemError::EINVAL),
        }
    }

    fn parse_mount_options(
        raw: Option<&str>,
    ) -> Result<(u32, u32, u32, bool, bool, VirtioFsDaxMode), SystemError> {
        let pcb = ProcessManager::current_pcb();
        let cred = pcb.cred();

        let mut rootmode: Option<u32> = None;
        let mut user_id: Option<u32> = None;
        let mut group_id: Option<u32> = None;
        let mut default_permissions = true;
        let mut allow_other = true;
        let mut dax_mode = VirtioFsDaxMode::Never;

        for part in raw.unwrap_or("").split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (k, v) = match part.split_once('=') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => (part, ""),
            };

            match k {
                "rootmode" => rootmode = Some(Self::parse_opt_u32_octal(v)?),
                "user_id" => user_id = Some(Self::parse_opt_u32_decimal(v)?),
                "group_id" => group_id = Some(Self::parse_opt_u32_decimal(v)?),
                "default_permissions" => default_permissions = Self::parse_opt_bool_switch(v),
                "allow_other" => allow_other = Self::parse_opt_bool_switch(v),
                "dax" => dax_mode = Self::parse_dax_mode(v)?,
                _ => return Err(SystemError::EINVAL),
            }
        }

        if dax_mode != VirtioFsDaxMode::Never {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }

        Ok((
            rootmode.unwrap_or(0o040755),
            user_id.unwrap_or(cred.fsuid.data() as u32),
            group_id.unwrap_or(cred.fsgid.data() as u32),
            default_permissions,
            allow_other,
            dax_mode,
        ))
    }
}

impl FileSystem for VirtioFsFs {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        self.inner.root_inode()
    }

    fn info(&self) -> FsInfo {
        self.inner.info()
    }

    fn support_readahead(&self) -> bool {
        self.inner.support_readahead()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn name(&self) -> &str {
        "virtiofs"
    }

    fn super_block(&self) -> SuperBlock {
        self.inner.super_block()
    }

    fn statfs(&self, inode: &Arc<dyn IndexNode>) -> Result<SuperBlock, SystemError> {
        self.inner.statfs(inode)
    }

    fn permission_policy(&self) -> crate::filesystem::vfs::FsPermissionPolicy {
        self.inner.permission_policy()
    }

    fn on_umount(&self) {
        self.inner.on_umount();
    }
}

impl MountableFileSystem for VirtioFsFs {
    fn make_mount_data(
        raw_data: Option<&str>,
        source: &str,
    ) -> Result<Option<Arc<dyn FileSystemMakerData + 'static>>, SystemError> {
        if source.is_empty() {
            return Err(SystemError::EINVAL);
        }

        let (rootmode, user_id, group_id, default_permissions, allow_other, dax_mode) =
            Self::parse_mount_options(raw_data)?;
        let instance = virtio_fs_find_instance(source).ok_or(SystemError::ENODEV)?;
        let conn = FuseConn::new_for_virtiofs(core::cmp::min(
            VIRTIOFS_REQ_BUF_SIZE,
            VIRTIOFS_RSP_BUF_SIZE,
        ));

        Ok(Some(Arc::new(VirtioFsMountData {
            rootmode,
            user_id,
            group_id,
            allow_other,
            default_permissions,
            dax_mode,
            conn,
            instance,
        })))
    }

    fn make_fs(
        data: Option<&dyn FileSystemMakerData>,
    ) -> Result<Arc<dyn FileSystem + 'static>, SystemError> {
        let md = data
            .and_then(|d| d.as_any().downcast_ref::<VirtioFsMountData>())
            .ok_or(SystemError::EINVAL)?;

        let fuse_mount_data = FuseMountData {
            rootmode: md.rootmode,
            user_id: md.user_id,
            group_id: md.group_id,
            allow_other: md.allow_other,
            default_permissions: md.default_permissions,
            conn: md.conn.clone(),
        };

        if md.dax_mode != VirtioFsDaxMode::Never {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }

        let inner = <FuseFS as MountableFileSystem>::make_fs(Some(
            &fuse_mount_data as &dyn FileSystemMakerData,
        ))?;

        if let Err(e) = start_bridge(md.instance.clone(), md.conn.clone()) {
            inner.on_umount();
            return Err(e);
        }

        Ok(Arc::new(VirtioFsFs { inner }))
    }
}

register_mountable_fs!(VirtioFsFs, VIRTIOFSMAKER, "virtiofs");
