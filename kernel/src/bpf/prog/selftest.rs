//! Bounded, isolated ownership checks using the real relocation path.

use super::{util::BpfProgMeta, util::VerifierLogLevel, verifier::BpfProgVerifier, BpfProg};
use crate::bpf::map::{array_map::ArrayMap, util::BpfMapMeta, BpfMap};
use crate::filesystem::vfs::{
    fdtable::FdTableState, fdtable::FileDescriptorTable, file::File, file::FileFlags,
};
use crate::include::bindings::linux_bpf::{
    bpf_attach_type, bpf_map_type, bpf_prog_type, BPF_PSEUDO_MAP_FD, BPF_PSEUDO_MAP_VALUE,
};
use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use system_error::SystemError;

fn append_load(insns: &mut Vec<u8>, src: u32, fd: i32) {
    insns.extend_from_slice(&[rbpf::ebpf::LD_DW_IMM, ((src as u8) << 4) | 1, 0, 0]);
    insns.extend_from_slice(&fd.to_le_bytes());
    insns.extend_from_slice(&[0; 8]);
}

fn ownership_case(src: u32, fail: bool) -> Result<bool, SystemError> {
    let meta = BpfMapMeta {
        map_type: bpf_map_type::BPF_MAP_TYPE_ARRAY,
        key_size: 4,
        value_size: 8,
        max_entries: 1,
        _map_flags: 0,
        _map_name: String::new(),
    };
    let map = Arc::new(BpfMap::new(Box::new(ArrayMap::new(&meta)?), meta));
    let weak = Arc::downgrade(&map);
    let table = Arc::new(FileDescriptorTable::new(FdTableState::new()));
    let fd = table.alloc_fd(File::new(map, FileFlags::O_RDWR)?, false, 16)?;
    let mut insns = Vec::new();
    append_load(&mut insns, src, fd);
    // A duplicate must not acquire a second persistent map reference.
    append_load(&mut insns, src, fd);
    if fail {
        // The earlier relocations have already acquired a map reference.
        append_load(&mut insns, src, -1);
    }
    insns.extend_from_slice(&[rbpf::ebpf::MOV64_IMM, 0, 0, 0, 0, 0, 0, 0]);
    insns.extend_from_slice(&[rbpf::ebpf::EXIT, 0, 0, 0, 0, 0, 0, 0]);
    let meta = BpfProgMeta {
        prog_flags: 0,
        prog_type: bpf_prog_type::BPF_PROG_TYPE_SOCKET_FILTER,
        expected_attach_type: bpf_attach_type::BPF_CGROUP_INET_INGRESS,
        insns,
        license: String::from("GPL"),
        kern_version: 0,
        name: String::new(),
    };
    // ID zero is reserved; this private program is never globally published.
    // VFS File retains the inode through several guards. Compare against its
    // actual baseline rather than coupling this check to those internals.
    let descriptor_refs = weak.strong_count();
    let result = BpfProgVerifier::new(
        BpfProg::new(meta, 0, None),
        VerifierLogLevel::DISABLE,
        &mut [],
    )
    .verify(&table);
    if fail {
        let rejected = matches!(&result, Err(SystemError::EBADF));
        // Only the descriptor may remain after verifier rollback.
        let rolled_back = weak.strong_count() == descriptor_refs;
        table.drop_fd(fd)?.finish_close()?;
        let released = weak.strong_count() == 0;
        Ok(rejected && rolled_back && released)
    } else {
        let program = result?;
        let deduplicated = weak.strong_count() == descriptor_refs + 1;
        table.drop_fd(fd)?.finish_close()?;
        let kept_alive = weak.strong_count() == 1;
        drop(program);
        Ok(deduplicated && kept_alive && weak.strong_count() == 0)
    }
}

pub(crate) fn run_map_lifetime_selftests() -> Result<String, SystemError> {
    let mut body = String::new();
    let mut failures = 0;
    for (name, src, fail) in [
        ("map_fd_ownership", BPF_PSEUDO_MAP_FD, false),
        ("map_value_ownership", BPF_PSEUDO_MAP_VALUE, false),
        ("map_fd_rollback", BPF_PSEUDO_MAP_FD, true),
        ("map_value_rollback", BPF_PSEUDO_MAP_VALUE, true),
    ] {
        let passed = ownership_case(src, fail)?;
        failures += usize::from(!passed);
        body.push_str(&alloc::format!(
            "{name}={}\n",
            if passed { "ok" } else { "fail" }
        ));
    }
    Ok(alloc::format!(
        "status={} failures={failures}\n{body}",
        if failures == 0 { "ok" } else { "fail" }
    ))
}
