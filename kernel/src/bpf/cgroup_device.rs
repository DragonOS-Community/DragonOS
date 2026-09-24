//! Legacy cgroup-v2 device-program management commands.

use super::prog::BpfProg;
use super::Result;
use crate::cgroup::{cgroup_root, core::BPF_DEVICE_F_PREORDER};
use crate::filesystem::cgroup2::cgroup2_inode_to_node;
use crate::filesystem::vfs::file::File;
use crate::include::bindings::linux_bpf::{
    bpf_attach_type, bpf_attr, bpf_attr__bindgen_ty_6, bpf_prog_type, BPF_F_ALLOW_MULTI,
    BPF_F_ALLOW_OVERRIDE, BPF_F_QUERY_EFFECTIVE, BPF_F_REPLACE,
};
use crate::libs::casting::DowncastArc;
use crate::process::ProcessManager;
use crate::syscall::user_access::UserBufferWriter;
use alloc::sync::Arc;
use core::mem::{offset_of, size_of};
use system_error::SystemError;

fn target_node(fd: u32) -> Result<Arc<crate::cgroup::CgroupNode>> {
    let file = ProcessManager::current_pcb()
        .fd_table()
        .get_file_by_fd(fd as i32)
        .ok_or(SystemError::EBADF)?;
    cgroup2_inode_to_node(&file.inode()).map_err(|_| SystemError::EBADF)
}

fn program_from_file(file: Arc<File>) -> Result<Arc<BpfProg>> {
    let program = file
        .inode()
        .downcast_arc::<BpfProg>()
        .ok_or(SystemError::EINVAL)?;
    if program.prog_type() != bpf_prog_type::BPF_PROG_TYPE_CGROUP_DEVICE {
        return Err(SystemError::EINVAL);
    }
    Ok(program)
}

fn program(fd: u32) -> Result<Arc<BpfProg>> {
    let file = ProcessManager::current_pcb()
        .fd_table()
        .get_file_by_fd(fd as i32)
        .ok_or(SystemError::EBADF)?;
    program_from_file(file)
}

fn is_device_type(attach_type: u32) -> bool {
    attach_type == bpf_attach_type::BPF_CGROUP_DEVICE as u32
}

pub(super) fn attach(attr: &bpf_attr) -> Result<usize> {
    let used_end = offset_of!(bpf_attr__bindgen_ty_6, expected_revision) + size_of::<u64>();
    super::attr_tail_zero(attr, used_end)?;
    let request = unsafe { attr.__bindgen_anon_5 };
    if !is_device_type(request.attach_type) {
        return Err(SystemError::EINVAL);
    }
    let allowed = BPF_F_ALLOW_MULTI | BPF_F_ALLOW_OVERRIDE | BPF_F_REPLACE | BPF_DEVICE_F_PREORDER;
    if request.attach_flags & !allowed != 0
        || unsafe { request.__bindgen_anon_2.relative_fd } != 0
        || request.expected_revision != 0
    {
        return Err(SystemError::EINVAL);
    }
    let node = target_node(unsafe { request.__bindgen_anon_1.target_fd })?;
    let new_program = program(request.attach_bpf_fd)?;
    let old_program = if request.attach_flags & BPF_F_REPLACE != 0 {
        Some(program(request.replace_bpf_fd)?)
    } else {
        if request.replace_bpf_fd != 0 {
            return Err(SystemError::EINVAL);
        }
        None
    };
    cgroup_root().attach_device_program(&node, new_program, request.attach_flags, old_program)?;
    Ok(0)
}

pub(super) fn detach(attr: &bpf_attr) -> Result<usize> {
    let used_end = offset_of!(bpf_attr__bindgen_ty_6, expected_revision) + size_of::<u64>();
    super::attr_tail_zero(attr, used_end)?;
    let request = unsafe { attr.__bindgen_anon_5 };
    if !is_device_type(request.attach_type) {
        return Err(SystemError::EINVAL);
    }
    if request.attach_flags != 0
        || request.replace_bpf_fd != 0
        || unsafe { request.__bindgen_anon_2.relative_fd } != 0
        || request.expected_revision != 0
    {
        return Err(SystemError::EINVAL);
    }
    let node = target_node(unsafe { request.__bindgen_anon_1.target_fd })?;
    // Linux permits an invalid program FD for a single-program attachment;
    // the cgroup core decides whether one is required for MULTI.
    let old_program = ProcessManager::current_pcb()
        .fd_table()
        .get_file_by_fd(request.attach_bpf_fd as i32)
        .and_then(|file| program_from_file(file).ok());
    cgroup_root().detach_device_program(&node, old_program.as_ref())?;
    Ok(0)
}

fn write_u32_array(ptr: u64, values: &[u32]) -> Result<()> {
    if values.is_empty() {
        return Ok(());
    }
    let len = values.len().checked_mul(4).ok_or(SystemError::EFAULT)?;
    let mut writer = UserBufferWriter::new(ptr as *mut u8, len, true)?;
    for (index, value) in values.iter().enumerate() {
        writer.copy_to_user_protected(&value.to_ne_bytes(), index * 4)?;
    }
    Ok(())
}

pub(super) fn query(attr: &bpf_attr, user_attr: *mut u8) -> Result<usize> {
    let request = unsafe { attr.query };
    let effective = request.query_flags & BPF_F_QUERY_EFFECTIVE != 0;
    let node = target_node(unsafe { request.__bindgen_anon_1.target_fd })?;
    let (flags, ids, per_prog_flags) = cgroup_root().query_device_programs(&node, effective)?;
    super::query::write_query_field(
        user_attr,
        offset_of!(
            crate::include::bindings::linux_bpf::bpf_attr__bindgen_ty_10,
            attach_flags
        ),
        flags,
    )?;
    super::query::write_query_field(
        user_attr,
        offset_of!(
            crate::include::bindings::linux_bpf::bpf_attr__bindgen_ty_10,
            __bindgen_anon_2
        ),
        ids.len() as u32,
    )?;
    let capacity = unsafe { request.__bindgen_anon_2.prog_cnt } as usize;
    if capacity == 0 || request.prog_ids == 0 || ids.is_empty() {
        return Ok(0);
    }
    let copied = core::cmp::min(capacity, ids.len());
    write_u32_array(request.prog_ids, &ids[..copied])?;
    if request.prog_attach_flags != 0 {
        write_u32_array(request.prog_attach_flags, &per_prog_flags[..copied])?;
    }
    if capacity < ids.len() {
        Err(SystemError::ENOSPC)
    } else {
        Ok(0)
    }
}
