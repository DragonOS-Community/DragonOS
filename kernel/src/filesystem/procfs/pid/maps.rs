//! /proc/[pid]/maps - 进程内存映射信息
//!
//! 返回进程的内存映射信息，格式兼容 Linux procfs

use crate::libs::mutex::MutexGuard;
use crate::{
    arch::MMArch,
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::proc_read,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    mm::{ucontext::LockedVMA, MemoryManagementArch, VmFlags},
    process::{ProcessManager, RawPid},
};
use alloc::{
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

/// /proc/[pid]/maps 文件的 FileOps 实现
#[derive(Debug)]
pub struct MapsFileOps {
    pid: RawPid,
}

impl MapsFileOps {
    pub fn new_inode(pid: RawPid, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self { pid }, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }
}

/// 将 VmFlags 转换为权限字符串
#[inline(never)]
fn perms_from_vm_flags(vm_flags: VmFlags) -> [u8; 4] {
    let r = if vm_flags.contains(VmFlags::VM_READ) {
        b'r'
    } else {
        b'-'
    };
    let w = if vm_flags.contains(VmFlags::VM_WRITE) {
        b'w'
    } else {
        b'-'
    };
    let x = if vm_flags.contains(VmFlags::VM_EXEC) {
        b'x'
    } else {
        b'-'
    };
    let s = if vm_flags.contains(VmFlags::VM_SHARED) {
        b's'
    } else {
        b'p'
    };
    [r, w, x, s]
}

/// 格式化设备号、inode 和路径
#[inline(never)]
fn format_dev_inode_and_path(
    file_inode: Option<&dyn IndexNode>,
    root_prefix: &str,
) -> (String, String) {
    if let Some(inode) = file_inode {
        let (dev, ino, path) = match inode.metadata() {
            Ok(md) => {
                let dev = format!("{:02x}:{:02x}", (md.dev_id >> 8) & 0xff, md.dev_id & 0xff);
                let ino = md.inode_id.into();
                let mut path = inode.absolute_path().unwrap_or_default();
                // 尊重进程的 chroot：去掉根目录前缀
                if !root_prefix.is_empty() && root_prefix != "/" {
                    if let Some(rest) = path.strip_prefix(root_prefix) {
                        path = if rest.is_empty() {
                            "/".to_string()
                        } else if rest.starts_with('/') {
                            rest.to_string()
                        } else {
                            format!("/{}", rest)
                        };
                    }
                }
                (dev, ino, path)
            }
            Err(_) => (String::from("00:00"), 0usize, String::new()),
        };
        let mut tail = String::new();
        if !path.is_empty() {
            tail.push(' ');
            tail.push_str(&path);
        }
        return (format!("{} {}", dev, ino), tail);
    }
    (String::from("00:00 0"), String::new())
}

/// 生成 /proc/[pid]/maps 内容
fn generate_maps_content(pid: RawPid) -> Result<Vec<u8>, SystemError> {
    let target_pcb = ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;

    let vm = target_pcb.basic().user_vm().ok_or(SystemError::EINVAL)?;
    let root_prefix = target_pcb
        .fs_struct()
        .root()
        .absolute_path()
        .unwrap_or_default();

    let as_guard = vm.read();

    // 收集并按地址排序
    let mut vmas: Vec<Arc<LockedVMA>> = as_guard.mappings.iter_vmas().cloned().collect();
    vmas.sort_by_key(|v| v.lock().region().start().data());

    let mut out: Vec<u8> = Vec::new();

    for vma in vmas {
        let g = vma.lock();
        let region = *g.region();
        let vm_flags = *g.vm_flags();

        let perms = perms_from_vm_flags(vm_flags);
        let offset = g
            .backing_page_offset()
            .unwrap_or(0)
            .saturating_mul(MMArch::PAGE_SIZE);

        let (dev_ino, path_tail) = if let Some(f) = g.vm_file() {
            let inode = f.inode();
            format_dev_inode_and_path(Some(inode.as_ref()), &root_prefix)
        } else {
            format_dev_inode_and_path(None, &root_prefix)
        };

        let line = format!(
            "{:016x}-{:016x} {}{}{}{} {:08x} {}{}\n",
            region.start().data(),
            region.end().data(),
            perms[0] as char,
            perms[1] as char,
            perms[2] as char,
            perms[3] as char,
            offset,
            dev_ino,
            path_tail
        );
        out.extend_from_slice(line.as_bytes());
    }

    // 确保文件以换行符结尾
    if out.is_empty() {
        out.extend_from_slice(b"\n");
    }

    Ok(out)
}

impl FileOps for MapsFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let content = generate_maps_content(self.pid)?;
        proc_read(offset, len, buf, &content)
    }
}
