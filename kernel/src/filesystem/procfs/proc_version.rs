use system_error::SystemError;

use crate::init::version_info;

use super::{ProcFSInode, ProcfsFilePrivateData};

impl ProcFSInode {
    #[inline(never)]
    pub(super) fn open_version(
        &self,
        pdata: &mut ProcfsFilePrivateData,
    ) -> Result<i64, SystemError> {
        let info = version_info::get_kernel_build_info();

        // Linux version 5.15.0-152-generic (buildd@lcy02-amd64-094) (gcc (Ubuntu 11.4.0-1ubuntu1~22.04) 11.4.0, GNU ld (GNU Binutils for Ubuntu) 2.38) #162-Ubuntu SMP Wed Jul 23 09:48:42 UTC 2025
        let version_content = format!(
            "Linux version {} ({}@{}) ({}, {}) {}\n",
            info.release,
            info.build_user,
            info.build_host,
            info.compiler_info,
            info.linker_info,
            info.version
        );

        pdata.data = version_content.into_bytes();
        return Ok(pdata.data.len() as i64);
    }
}
