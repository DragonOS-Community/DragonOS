mod bridge;
pub(crate) mod dax;
mod mount;
pub(crate) use mount::snapshot_external_user_write;
mod queue;
pub(crate) mod reply;

const VIRTIOFS_MAX_REQUEST_SIZE: usize = 256 * 1024;
const VIRTIOFS_RSP_BUF_SIZE: usize = 256 * 1024;
