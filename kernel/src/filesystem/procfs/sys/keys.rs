//! Linux keyring quota and garbage-collection controls at /proc/sys/kernel/keys.

use alloc::{
    format,
    string::ToString,
    sync::{Arc, Weak},
};
use core::sync::atomic::{AtomicU32, Ordering};

use system_error::SystemError;

use crate::{
    filesystem::{
        procfs::{
            template::{Builder, DirOps, FileOps, ProcDir, ProcDirBuilder, ProcFileBuilder},
            utils::proc_read,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    libs::mutex::MutexGuard,
    process::ProcessManager,
    security::keys::{object::schedule_gc, QUOTA_LIMITS},
};

use super::numeric::parse_numeric_sysctl;

#[derive(Debug)]
pub(super) struct KeysDirOps;

impl KeysDirOps {
    pub(super) fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self, InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl DirOps for KeysDirOps {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let setting = KeySetting::from_name(name).ok_or(SystemError::ENOENT)?;
        let mut children = dir.cached_children().write();
        if let Some(child) = children.get(name) {
            return Ok(child.clone());
        }
        let child = KeySettingFileOps::new_inode(dir.self_ref_weak().clone(), setting);
        children.insert(name.to_string(), child.clone());
        Ok(child)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        let mut children = dir.cached_children().write();
        for setting in KeySetting::ALL {
            children
                .entry(setting.name().to_string())
                .or_insert_with(|| {
                    KeySettingFileOps::new_inode(dir.self_ref_weak().clone(), setting)
                });
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum KeySetting {
    Maxkeys,
    Maxbytes,
    RootMaxkeys,
    RootMaxbytes,
    GcDelay,
}

impl KeySetting {
    const ALL: [Self; 5] = [
        Self::Maxkeys,
        Self::Maxbytes,
        Self::RootMaxkeys,
        Self::RootMaxbytes,
        Self::GcDelay,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Maxkeys => "maxkeys",
            Self::Maxbytes => "maxbytes",
            Self::RootMaxkeys => "root_maxkeys",
            Self::RootMaxbytes => "root_maxbytes",
            Self::GcDelay => "gc_delay",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|setting| setting.name() == name)
    }

    fn value(self) -> &'static AtomicU32 {
        match self {
            Self::Maxkeys => &QUOTA_LIMITS.maxkeys,
            Self::Maxbytes => &QUOTA_LIMITS.maxbytes,
            Self::RootMaxkeys => &QUOTA_LIMITS.root_maxkeys,
            Self::RootMaxbytes => &QUOTA_LIMITS.root_maxbytes,
            Self::GcDelay => &QUOTA_LIMITS.gc_delay,
        }
    }

    fn minimum(self) -> i64 {
        match self {
            Self::GcDelay => 0,
            _ => 1,
        }
    }
}

#[derive(Debug)]
struct KeySettingFileOps {
    setting: KeySetting,
}

impl KeySettingFileOps {
    fn new_inode(parent: Weak<dyn IndexNode>, setting: KeySetting) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self { setting }, InodeMode::from_bits_truncate(0o644))
            .parent(parent)
            .sysctl_permissions()
            .build()
            .unwrap()
    }
}

impl FileOps for KeySettingFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if offset != 0 {
            return Ok(0);
        }
        let value = self.setting.value().load(Ordering::Relaxed);
        proc_read(0, len, buf, format!("{value}\n").as_bytes())
    }

    fn write_at(
        &self,
        offset: usize,
        _len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // These sysctls are global. Child user namespace capabilities must
        // never authorize modifying host-wide key quotas or GC behaviour.
        if ProcessManager::current_pcb().cred().euid.data() != 0 {
            return Err(SystemError::EPERM);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        if offset != 0 {
            return Ok(buf.len());
        }
        let (value, consumed) = parse_numeric_sysctl(buf)?;
        if !(self.setting.minimum()..=i32::MAX as i64).contains(&value) {
            return Err(SystemError::EINVAL);
        }
        self.setting.value().store(value as u32, Ordering::Relaxed);
        if matches!(self.setting, KeySetting::GcDelay) {
            schedule_gc();
        }
        Ok(consumed)
    }
}
