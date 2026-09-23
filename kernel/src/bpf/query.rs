//! Query cgroup BPF attachments. Program attachment is not implemented yet,
//! so every live cgroup currently has an empty attachment set.

use crate::cgroup::cgroup_root;
use crate::filesystem::cgroup2::cgroup2_inode_to_node;
use crate::include::bindings::linux_bpf::{
    bpf_attach_type, bpf_attr, bpf_attr__bindgen_ty_10, BPF_F_QUERY_EFFECTIVE,
};
use crate::mm::VirtAddr;
use crate::process::cred::{capable, CAPFlags};
use crate::process::ProcessManager;
use crate::syscall::user_access::write_one_to_user_protected;
use core::mem::{offset_of, size_of};
use num_traits::FromPrimitive;
use system_error::SystemError;

fn is_cgroup_attach_type(value: u32) -> bool {
    use bpf_attach_type::*;
    matches!(
        bpf_attach_type::from_u32(value),
        Some(
            BPF_CGROUP_INET_INGRESS
                | BPF_CGROUP_INET_EGRESS
                | BPF_CGROUP_INET_SOCK_CREATE
                | BPF_CGROUP_INET_SOCK_RELEASE
                | BPF_CGROUP_INET4_BIND
                | BPF_CGROUP_INET6_BIND
                | BPF_CGROUP_INET4_POST_BIND
                | BPF_CGROUP_INET6_POST_BIND
                | BPF_CGROUP_INET4_CONNECT
                | BPF_CGROUP_INET6_CONNECT
                | BPF_CGROUP_INET4_GETPEERNAME
                | BPF_CGROUP_INET6_GETPEERNAME
                | BPF_CGROUP_INET4_GETSOCKNAME
                | BPF_CGROUP_INET6_GETSOCKNAME
                | BPF_CGROUP_UDP4_SENDMSG
                | BPF_CGROUP_UDP6_SENDMSG
                | BPF_CGROUP_UDP4_RECVMSG
                | BPF_CGROUP_UDP6_RECVMSG
                | BPF_CGROUP_SOCK_OPS
                | BPF_CGROUP_DEVICE
                | BPF_CGROUP_SYSCTL
                | BPF_CGROUP_GETSOCKOPT
                | BPF_CGROUP_SETSOCKOPT
                | BPF_LSM_CGROUP
        )
    )
}

fn user_query_field(base: *mut u8, offset: usize) -> Result<VirtAddr, SystemError> {
    (base as usize)
        .checked_add(offset)
        .map(VirtAddr::new)
        .ok_or(SystemError::EFAULT)
}

pub(super) fn bpf_prog_query(attr: &bpf_attr, user_attr: *mut u8) -> Result<usize, SystemError> {
    if !capable(CAPFlags::CAP_NET_ADMIN) {
        return Err(SystemError::EPERM);
    }

    // Linux CHECK_ATTR(BPF_PROG_QUERY) checks only the union bytes after
    // query.revision. It does not reject other fields within query.
    let used_end = offset_of!(bpf_attr__bindgen_ty_10, revision) + size_of::<u64>();
    let bytes = unsafe {
        core::slice::from_raw_parts(
            (attr as *const bpf_attr).cast::<u8>(),
            size_of::<bpf_attr>(),
        )
    };
    if bytes[used_end..].iter().any(|byte| *byte != 0) {
        return Err(SystemError::EINVAL);
    }

    let query = unsafe { attr.query };
    if query.query_flags & !BPF_F_QUERY_EFFECTIVE != 0 {
        return Err(SystemError::EINVAL);
    }
    if !is_cgroup_attach_type(query.attach_type) {
        return Err(SystemError::EINVAL);
    }

    let fd = unsafe { query.__bindgen_anon_1.target_fd } as i32;
    let file = ProcessManager::current_pcb()
        .fd_table()
        .read()
        .get_file_by_fd(fd)
        .ok_or(SystemError::EBADF)?;
    let node = cgroup2_inode_to_node(&file.inode()).map_err(|_| SystemError::EBADF)?;
    // A removed directory may still have an open fd, but is no longer online.
    if cgroup_root().lookup_by_id(node.id()).is_none() {
        return Err(SystemError::ENOENT);
    }

    let effective = query.query_flags & BPF_F_QUERY_EFFECTIVE != 0;
    if effective && query.prog_attach_flags != 0 {
        return Err(SystemError::EINVAL);
    }
    let prog_cnt = unsafe { query.__bindgen_anon_2.prog_cnt };
    if query.attach_type == bpf_attach_type::BPF_LSM_CGROUP as u32
        && !effective
        && prog_cnt != 0
        && query.prog_ids != 0
        && query.prog_attach_flags == 0
    {
        return Err(SystemError::EINVAL);
    }

    // BPF_PROG_ATTACH and BPF_LINK_CREATE cannot install cgroup programs yet.
    // When they are added, query must read the same attachment state.
    let zero: u32 = 0;
    let flags_addr =
        user_query_field(user_attr, offset_of!(bpf_attr__bindgen_ty_10, attach_flags))?;
    unsafe { write_one_to_user_protected(flags_addr, &zero)? };
    let count_addr = user_query_field(
        user_attr,
        offset_of!(bpf_attr__bindgen_ty_10, __bindgen_anon_2),
    )?;
    unsafe { write_one_to_user_protected(count_addr, &zero)? };
    Ok(0)
}
