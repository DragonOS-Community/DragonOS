use alloc::{boxed::Box, string::String, sync::Arc, vec, vec::Vec};

use linkme::distributed_slice;
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
    protocol::{fuse_read_struct, FuseInHeader, FUSE_DESTROY, FUSE_FORGET},
};

const VIRTIOFS_REQ_QUEUE_SIZE: usize = 8;
const VIRTIOFS_REQ_BUF_SIZE: usize = 256 * 1024;
const VIRTIOFS_RSP_BUF_SIZE: usize = 256 * 1024;
const VIRTIOFS_POLL_NS: i64 = 1_000_000;

struct VirtioFsBridgeContext {
    instance: Arc<VirtioFsInstance>,
    conn: Arc<FuseConn>,
    transport: Option<VirtIOTransport>,
    request_vq: Option<VirtQueue<HalImpl, { VIRTIOFS_REQ_QUEUE_SIZE }>>,
    req_buf: Vec<u8>,
    rsp_buf: Vec<u8>,
}

impl VirtioFsBridgeContext {
    fn poll_pause() {
        let _ = nanosleep(PosixTimeSpec::new(0, VIRTIOFS_POLL_NS));
    }

    fn process_one(&mut self, req: &[u8]) -> Result<(), SystemError> {
        let in_hdr: FuseInHeader = fuse_read_struct(req)?;
        let noreply = matches!(in_hdr.opcode, FUSE_FORGET | FUSE_DESTROY);

        let queue = self.request_vq.as_mut().ok_or(SystemError::EIO)?;
        let transport = self.transport.as_mut().ok_or(SystemError::EIO)?;
        let queue_idx = self.instance.request_queue_index();

        if noreply {
            let inputs = [req];
            let mut outputs: [&mut [u8]; 0] = [];
            let token = loop {
                match unsafe { queue.add(&inputs, &mut outputs) } {
                    Ok(t) => break t,
                    Err(VirtioError::QueueFull) | Err(VirtioError::NotReady) => Self::poll_pause(),
                    Err(e) => return Err(virtio_drivers_error_to_system_error(e)),
                }
            };

            if queue.should_notify() {
                transport.notify(queue_idx);
            }

            loop {
                if queue.can_pop() {
                    let _ = unsafe { queue.pop_used(token, &inputs, &mut outputs) }
                        .map_err(virtio_drivers_error_to_system_error)?;
                    return Ok(());
                }
                let _ = transport.ack_interrupt();
                if !self.conn.is_connected() {
                    return Err(SystemError::ENOTCONN);
                }
                Self::poll_pause();
            }
        }

        self.rsp_buf.fill(0);
        let inputs = [req];
        let mut outputs = [self.rsp_buf.as_mut_slice()];
        let token = loop {
            match unsafe { queue.add(&inputs, &mut outputs) } {
                Ok(t) => break t,
                Err(VirtioError::QueueFull) | Err(VirtioError::NotReady) => Self::poll_pause(),
                Err(e) => return Err(virtio_drivers_error_to_system_error(e)),
            }
        };

        if queue.should_notify() {
            transport.notify(queue_idx);
        }

        let used_len = loop {
            if queue.can_pop() {
                let used = unsafe { queue.pop_used(token, &inputs, &mut outputs) }
                    .map_err(virtio_drivers_error_to_system_error)?;
                break used as usize;
            }
            let _ = transport.ack_interrupt();
            if !self.conn.is_connected() {
                return Err(SystemError::ENOTCONN);
            }
            Self::poll_pause();
        };

        if used_len > self.rsp_buf.len() {
            return Err(SystemError::EIO);
        }

        match self.conn.write_reply(&self.rsp_buf[..used_len]) {
            Ok(_) | Err(SystemError::ENOENT) => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn run_loop(&mut self) -> Result<(), SystemError> {
        loop {
            match self.conn.read_request(true, &mut self.req_buf) {
                Ok(len) => {
                    let req = self.req_buf[..len].to_vec();
                    if let Err(e) = self.process_one(&req) {
                        if e == SystemError::ENOTCONN {
                            break;
                        }
                        log::warn!("virtiofs bridge: process request failed: {:?}", e);
                    }
                }
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                    if !self.conn.is_mounted() && !self.conn.has_pending_requests() {
                        break;
                    }
                    Self::poll_pause();
                }
                Err(SystemError::ENOTCONN) => break,
                Err(e) => {
                    log::warn!("virtiofs bridge: read_request failed: {:?}", e);
                    if !self.conn.is_connected()
                        || (!self.conn.is_mounted() && !self.conn.has_pending_requests())
                    {
                        break;
                    }
                    Self::poll_pause();
                }
            }
        }

        Ok(())
    }

    fn finish(&mut self) {
        if self.request_vq.take().is_some() {
            if let Some(transport) = self.transport.as_mut() {
                transport.queue_unset(self.instance.request_queue_index());
                transport.set_status(DeviceStatus::empty());
            }
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
        log::warn!("virtiofs bridge thread exit with error: {:?}", e);
    }
    ctx.finish();
    result.map(|_| 0).unwrap_or_else(|e| e.to_posix_errno())
}

fn start_bridge(instance: Arc<VirtioFsInstance>, conn: Arc<FuseConn>) -> Result<(), SystemError> {
    let mut transport = instance.take_transport_for_session()?;
    if !instance.request_queue_index_valid() {
        log::warn!(
            "virtiofs bridge: invalid request queue index: tag='{}' dev={:?} idx={} request_queues={}",
            instance.tag(),
            instance.dev_id(),
            instance.request_queue_index(),
            instance.num_request_queues()
        );
        instance.put_transport_after_session(transport);
        return Err(SystemError::EINVAL);
    }
    log::debug!(
        "virtiofs bridge: start tag='{}' dev={:?} idx={} request_queues={}",
        instance.tag(),
        instance.dev_id(),
        instance.request_queue_index(),
        instance.num_request_queues()
    );

    transport.set_status(DeviceStatus::empty());
    transport.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER);
    let _device_features = transport.read_device_features();
    transport.write_driver_features(0);
    transport
        .set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER | DeviceStatus::FEATURES_OK);
    transport.set_guest_page_size(PAGE_SIZE as u32);

    let request_vq = match VirtQueue::<HalImpl, { VIRTIOFS_REQ_QUEUE_SIZE }>::new(
        &mut transport,
        instance.request_queue_index(),
        false,
        false,
    ) {
        Ok(vq) => vq,
        Err(e) => {
            let se = virtio_drivers_error_to_system_error(e);
            transport.set_status(DeviceStatus::FAILED);
            log::warn!(
                "virtiofs bridge: queue create failed: tag='{}' dev={:?} idx={} err={:?}",
                instance.tag(),
                instance.dev_id(),
                instance.request_queue_index(),
                se
            );
            instance.put_transport_after_session(transport);
            return Err(se);
        }
    };
    transport.finish_init();

    let ctx = Box::new(VirtioFsBridgeContext {
        instance,
        conn,
        transport: Some(transport),
        request_vq: Some(request_vq),
        req_buf: vec![0u8; VIRTIOFS_REQ_BUF_SIZE],
        rsp_buf: vec![0u8; VIRTIOFS_RSP_BUF_SIZE],
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

    fn parse_mount_options(raw: Option<&str>) -> Result<(u32, u32, u32, bool, bool), SystemError> {
        let pcb = ProcessManager::current_pcb();
        let cred = pcb.cred();

        let mut rootmode: Option<u32> = None;
        let mut user_id: Option<u32> = None;
        let mut group_id: Option<u32> = None;
        let mut default_permissions = true;
        let mut allow_other = true;

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
                _ => {}
            }
        }

        Ok((
            rootmode.unwrap_or(0o040755),
            user_id.unwrap_or(cred.fsuid.data() as u32),
            group_id.unwrap_or(cred.fsgid.data() as u32),
            default_permissions,
            allow_other,
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

        let (rootmode, user_id, group_id, default_permissions, allow_other) =
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
