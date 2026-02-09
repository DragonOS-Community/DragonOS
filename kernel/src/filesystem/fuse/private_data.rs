use alloc::sync::Arc;
use core::any::Any;

#[derive(Debug, Clone)]
pub struct FuseDevPrivateData {
    pub conn: Arc<dyn Any + Send + Sync>,
    pub nonblock: bool,
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
