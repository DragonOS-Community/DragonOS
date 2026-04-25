//! /proc/[pid]/limits - 进程资源限制信息
//!
//! 以 Linux 兼容格式返回进程的 rlimit 快照。

use core::fmt::Write;

use crate::libs::mutex::MutexGuard;
use crate::{
    filesystem::{
        procfs::{
            pid::find_process_by_vpid,
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    process::{resource::RLimitID, RawPid},
};
use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};
use system_error::SystemError;

/// /proc/[pid]/limits 文件的 FileOps 实现
#[derive(Debug)]
pub struct LimitsFile {
    pid: RawPid,
}

#[derive(Clone, Copy)]
struct LimitName {
    name: &'static str,
    unit: Option<&'static str>,
}

impl LimitsFile {
    const LIMIT_NAMES: [LimitName; RLimitID::Nlimits as usize] = [
        LimitName {
            name: "Max cpu time",
            unit: Some("seconds"),
        },
        LimitName {
            name: "Max file size",
            unit: Some("bytes"),
        },
        LimitName {
            name: "Max data size",
            unit: Some("bytes"),
        },
        LimitName {
            name: "Max stack size",
            unit: Some("bytes"),
        },
        LimitName {
            name: "Max core file size",
            unit: Some("bytes"),
        },
        LimitName {
            name: "Max resident set",
            unit: Some("bytes"),
        },
        LimitName {
            name: "Max processes",
            unit: Some("processes"),
        },
        LimitName {
            name: "Max open files",
            unit: Some("files"),
        },
        LimitName {
            name: "Max locked memory",
            unit: Some("bytes"),
        },
        LimitName {
            name: "Max address space",
            unit: Some("bytes"),
        },
        LimitName {
            name: "Max file locks",
            unit: Some("locks"),
        },
        LimitName {
            name: "Max pending signals",
            unit: Some("signals"),
        },
        LimitName {
            name: "Max msgqueue size",
            unit: Some("bytes"),
        },
        LimitName {
            name: "Max nice priority",
            unit: None,
        },
        LimitName {
            name: "Max realtime priority",
            unit: None,
        },
        LimitName {
            name: "Max realtime timeout",
            unit: Some("us"),
        },
    ];

    pub fn new_inode(pid: RawPid, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self { pid }, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }

    #[inline]
    fn format_limit_value(value: u64) -> String {
        if value == u64::MAX || value == usize::MAX as u64 {
            "unlimited".to_string()
        } else {
            value.to_string()
        }
    }

    fn generate_limits_content(&self) -> Result<String, SystemError> {
        let pcb = find_process_by_vpid(self.pid).ok_or(SystemError::ESRCH)?;

        // 与 Linux fs/proc/base.c 的表头保持一致。
        let mut content = String::from(
            "Limit                     Soft Limit           Hard Limit           Units\n",
        );

        for i in 0..(RLimitID::Nlimits as usize) {
            let rid = RLimitID::try_from(i)?;
            let rlim = pcb.get_rlimit(rid);
            let name = Self::LIMIT_NAMES[i];

            let soft = Self::format_limit_value(rlim.rlim_cur);
            let hard = Self::format_limit_value(rlim.rlim_max);

            if let Some(unit) = name.unit {
                let _ = writeln!(
                    content,
                    "{:<25} {:<20} {:<20} {:<10}",
                    name.name, soft, hard, unit
                );
            } else {
                // Linux 对无单位项仅输出换行，保留 hard 列后的尾随空格。
                let _ = writeln!(content, "{:<25} {:<20} {:<20} ", name.name, soft, hard);
            }
        }

        Ok(content)
    }
}

impl FileOps for LimitsFile {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let content = self.generate_limits_content()?;
        proc_read(offset, len, buf, content.as_bytes())
    }
}
