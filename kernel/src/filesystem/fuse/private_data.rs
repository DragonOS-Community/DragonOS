use alloc::sync::Arc;
use core::any::Any;
use system_error::SystemError;

use super::conn::FuseConn;

#[derive(Debug, Clone)]
pub struct FuseDevPrivateData {
    pub conn: Arc<dyn Any + Send + Sync>,
    pub nonblock: bool,
}

impl FuseDevPrivateData {
    pub fn conn_ref(&self) -> Result<Arc<FuseConn>, SystemError> {
        downcast_conn(&self.conn)
    }
}

#[derive(Debug, Clone)]
pub struct FuseOpenPrivateData {
    pub conn: Arc<dyn Any + Send + Sync>,
    pub fh: u64,
    pub open_flags: u32,
    pub no_open: bool,
}

#[derive(Debug, Clone)]
pub enum FuseFilePrivateData {
    Dev(FuseDevPrivateData),
    File(FuseOpenPrivateData),
    Dir(FuseOpenPrivateData),
}

#[inline]
fn downcast_conn(conn_any: &Arc<dyn Any + Send + Sync>) -> Result<Arc<FuseConn>, SystemError> {
    conn_any
        .clone()
        .downcast::<FuseConn>()
        .map_err(|_| SystemError::EINVAL)
}
