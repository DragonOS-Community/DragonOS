use alloc::string::ToString;
use system_error::SystemError;

use crate::{init::boot_params, process::ProcessManager};

use super::{ProcFSInode, ProcfsFilePrivateData};

impl ProcFSInode {
    /// /proc/cmdline
    ///
    /// Linux 语义：输出启动时的 kernel command line，末尾带 '\n'。
    #[inline(never)]
    pub(super) fn open_cmdline(
        &self,
        pdata: &mut ProcfsFilePrivateData,
    ) -> Result<i64, SystemError> {
        let cmdline = boot_params().read().boot_cmdline_str().to_string();
        pdata.data = cmdline.into_bytes();
        if !pdata.data.ends_with(b"\n") {
            pdata.data.push(b'\n');
        }
        Ok(pdata.data.len() as i64)
    }

    /// /proc/<pid>/cmdline（也覆盖 /proc/self/cmdline）
    ///
    /// Linux 语义：以 '\0' 分隔 argv，通常末尾带一个额外的 '\0'。
    #[inline(never)]
    pub(super) fn open_pid_cmdline(
        &self,
        pdata: &mut ProcfsFilePrivateData,
    ) -> Result<i64, SystemError> {
        let pid = self
            .fdata
            .pid
            .expect("ProcFS: pid is None when opening 'cmdline' file.");
        let pcb = ProcessManager::find_task_by_vpid(pid).ok_or(SystemError::ESRCH)?;

        pdata.data = pcb.cmdline_bytes();
        Ok(pdata.data.len() as i64)
    }
}
