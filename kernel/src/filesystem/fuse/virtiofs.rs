mod bridge;
mod mount;
mod queue;

const VIRTIOFS_MAX_REQUEST_SIZE: usize = 256 * 1024;
const VIRTIOFS_RSP_BUF_SIZE: usize = 256 * 1024;
