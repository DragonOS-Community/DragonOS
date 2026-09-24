//! Program identity and metadata ABI used by cgroup device policy updates.

use super::{program_by_id, BpfProg};
use crate::bpf::Result;
use crate::filesystem::vfs::file::{File, FileFlags};
use crate::include::bindings::linux_bpf::{
    bpf_attr, bpf_attr__bindgen_ty_8, bpf_attr__bindgen_ty_9, bpf_prog_info, bpf_prog_type,
};
use crate::libs::casting::DowncastArc;
use crate::process::cred::{capable, CAPFlags};
use crate::process::ProcessManager;
use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};
use core::mem::{offset_of, size_of};
use system_error::SystemError;

pub(in crate::bpf) fn get_fd_by_id(attr: &bpf_attr) -> Result<usize> {
    // Linux BPF_PROG_GET_FD_BY_ID_LAST_FIELD is prog_id, not open_flags.
    super::super::attr_tail_zero(attr, offset_of!(bpf_attr__bindgen_ty_8, next_id))?;
    if !capable(CAPFlags::CAP_SYS_ADMIN) {
        return Err(SystemError::EPERM);
    }
    let id = unsafe { attr.__bindgen_anon_6.__bindgen_anon_1.prog_id };
    let program = program_by_id(id).ok_or(SystemError::ENOENT)?;
    let pcb = ProcessManager::current_pcb();
    let file = File::new(program, FileFlags::O_RDWR)?;
    pcb.fd_table()
        .alloc_fd(file, false, pcb.nofile_soft_limit())
        .map(|fd| fd as usize)
}

fn write_info_len(user_attr: *mut u8, len: u32) -> Result<()> {
    let addr = (user_attr as usize)
        .checked_add(offset_of!(bpf_attr__bindgen_ty_9, info_len))
        .ok_or(SystemError::EFAULT)?;
    let mut writer = UserBufferWriter::new(addr as *mut u8, size_of::<u32>(), true)?;
    writer.copy_to_user_protected(&len.to_ne_bytes(), 0)?;
    Ok(())
}

pub(in crate::bpf) fn get_info_by_fd(attr: &bpf_attr, user_attr: *mut u8) -> Result<usize> {
    super::super::attr_tail_zero(attr, size_of::<bpf_attr__bindgen_ty_9>())?;
    let request = unsafe { attr.info };
    let file = ProcessManager::current_pcb()
        .fd_table()
        .get_file_by_fd(request.bpf_fd as i32)
        .ok_or(SystemError::EBADF)?;
    let program = file
        .inode()
        .downcast_arc::<BpfProg>()
        .ok_or(SystemError::EINVAL)?;

    let full_size = size_of::<bpf_prog_info>();
    let requested = request.info_len as usize;
    let copy_len = core::cmp::min(requested, full_size);
    let ptr = request.info as *mut u8;
    // Linux accepts newer, larger structures only when the unknown tail is
    // zero. Check with bounded stack storage before writing anything.
    if requested > full_size {
        let mut tail = [0u8; 64];
        let reader = UserBufferReader::new(ptr, requested, true)?;
        let mut at = full_size;
        while at < requested {
            let n = core::cmp::min(tail.len(), requested - at);
            reader.copy_from_user_protected(&mut tail[..n], at)?;
            if tail[..n].iter().any(|byte| *byte != 0) {
                return Err(SystemError::E2BIG);
            }
            at += n;
        }
    }

    let mut input: bpf_prog_info = unsafe { core::mem::zeroed() };
    if copy_len != 0 {
        let reader = UserBufferReader::new(ptr, copy_len, true)?;
        let bytes = unsafe {
            core::slice::from_raw_parts_mut(
                (&mut input as *mut bpf_prog_info).cast::<u8>(),
                copy_len,
            )
        };
        reader.copy_from_user_protected(bytes, 0)?;
    }

    let mut info: bpf_prog_info = unsafe { core::mem::zeroed() };
    info.type_ = program.prog_type() as u32;
    info.id = program.id();
    info.tag = program.tag();
    // The device program has no relocated kernel pointers. Do not expose the
    // generic verifier's raw, potentially pointer-bearing instruction stream.
    if program.prog_type() == bpf_prog_type::BPF_PROG_TYPE_CGROUP_DEVICE {
        info.verified_insns = program.instruction_count();
        if capable(CAPFlags::CAP_BPF) || capable(CAPFlags::CAP_SYS_ADMIN) {
            info.xlated_prog_len = program.insns().len() as u32;
            if input.xlated_prog_len != 0 {
                let count = core::cmp::min(input.xlated_prog_len as usize, program.insns().len());
                let mut writer =
                    UserBufferWriter::new(input.xlated_prog_insns as *mut u8, count, true)?;
                writer.copy_to_user_protected(&program.insns()[..count], 0)?;
            }
        }
    }
    for (dst, src) in info.name.iter_mut().zip(program.name().as_bytes()) {
        *dst = *src as core::ffi::c_char;
    }
    let bytes = unsafe {
        core::slice::from_raw_parts((&info as *const bpf_prog_info).cast::<u8>(), copy_len)
    };
    if copy_len != 0 {
        let mut writer = UserBufferWriter::new(ptr, copy_len, true)?;
        writer.copy_to_user_protected(bytes, 0)?;
    }
    write_info_len(user_attr, copy_len as u32)?;
    Ok(0)
}
