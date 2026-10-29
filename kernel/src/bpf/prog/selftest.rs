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
    let task = crate::process::ProcessManager::current_pcb();
    let table = File::recover_io(|| {
        FileDescriptorTable::try_new(
            FdTableState::new(),
            &crate::cgroup::memory::object_for_task(&task),
        )
    })?;
    let table = Arc::try_new(table).map_err(|_| SystemError::ENOMEM)?;
    let storage = crate::filesystem::vfs::file::PreparedFile::new_recovering()?;
    let file = Arc::try_new(File::new_prepared_recovering(
        map,
        FileFlags::O_RDWR,
        storage,
    )?)
    .map_err(|_| SystemError::ENOMEM)?;
    let fd = File::recover_io(|| {
        table.alloc_fd_arc(
            file.clone(),
            false,
            16,
            &crate::cgroup::memory::object_for_task(&task),
        )
    })?;
    // The descriptor must be the only File owner for the close/rollback
    // checks below; alloc_fd_arc clones rather than consumes this handle.
    drop(file);
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
        license: b"GPL".to_vec(),
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

/// Real preparation failures and the no-allocation ID/fd publication boundary.
/// Uses a private fd table and a normal globally unique program identity.
#[cfg(feature = "mm_selftest")]
pub(crate) fn program_birth_selftest() -> Result<bool, SystemError> {
    use super::{publish_program, util, NEXT_PROG_ID};
    use crate::libs::rbtree::PreparedNode;
    use crate::mm::oom::denial;
    use core::sync::atomic::Ordering;

    fn program(id: u32) -> BpfProg {
        BpfProg::new(
            BpfProgMeta {
                prog_flags: 0,
                prog_type: bpf_prog_type::BPF_PROG_TYPE_SOCKET_FILTER,
                expected_attach_type: bpf_attach_type::BPF_CGROUP_INET_INGRESS,
                insns: Vec::new(),
                license: Vec::new(),
                kern_version: 0,
                name: String::new(),
            },
            id,
            None,
        )
    }

    let mut passed = util::name_birth_selftest();
    let map_meta = BpfMapMeta {
        map_type: bpf_map_type::BPF_MAP_TYPE_ARRAY,
        key_size: 4,
        value_size: 8,
        max_entries: 1,
        _map_flags: 0,
        _map_name: String::new(),
    };
    let map = Arc::try_new(BpfMap::new(
        Box::try_new(ArrayMap::new(&map_meta)?).map_err(|_| SystemError::ENOMEM)?,
        map_meta,
    ))
    .map_err(|_| SystemError::ENOMEM)?;
    let mut unpublished = program(0);
    let weak_map = Arc::downgrade(&map);
    {
        let _denied = denial::Scope::enter(true);
        passed &= denial::probe_denied();
        passed &= matches!(unpublished.hold_map(map.clone()), Err(SystemError::ENOMEM));
        passed &= unpublished.maps.is_empty() && weak_map.strong_count() == 1;
        passed &= Arc::try_new(program(0)).is_err();
        passed &= PreparedNode::try_new(0u32, alloc::sync::Weak::<BpfProg>::new()).is_err();
    }
    // The same operation succeeds after denial; duplicates retain no extra Arc.
    unpublished.hold_map(map.clone())?;
    {
        let _denied = denial::Scope::enter(true);
        passed &= unpublished.hold_map(map.clone()).is_ok();
    }
    passed &= unpublished.maps.len() == 1 && weak_map.strong_count() == 2;
    drop(unpublished);
    drop(map);
    passed &= weak_map.upgrade().is_none();

    let task = crate::process::ProcessManager::current_pcb();
    let creator = crate::cgroup::memory::object_for_task(&task);
    let table = Arc::try_new(File::recover_io(|| {
        FileDescriptorTable::try_new(FdTableState::new(), &creator)
    })?)
    .map_err(|_| SystemError::ENOMEM)?;
    let id = NEXT_PROG_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .map_err(|_| SystemError::ENOSPC)?;
    let prog = Arc::try_new(program(id)).map_err(|_| SystemError::ENOMEM)?;
    let weak_prog = Arc::downgrade(&prog);
    let storage = crate::filesystem::vfs::file::PreparedFile::new_recovering()?;
    let file = Arc::try_new(File::new_prepared_recovering(
        prog.clone(),
        FileFlags::O_RDWR,
        storage,
    )?)
    .map_err(|_| SystemError::ENOMEM)?;
    passed &= matches!(
        File::recover_io(|| table.reserve::<1>(0, 0, false, &creator)),
        Err(SystemError::EMFILE)
    );
    passed &= super::program_by_id(id).is_none();
    let node = PreparedNode::try_new(id, Arc::downgrade(&prog)).map_err(|_| SystemError::ENOMEM)?;
    let reservation = File::recover_io(|| table.reserve::<1>(16, 0, false, &creator))?;
    let fd = {
        let _denied = denial::Scope::enter(true);
        let fd = publish_program(node, reservation, file);
        passed &= table.get_file_by_fd(fd as i32).is_some();
        passed &= super::program_by_id(id).is_some();
        fd
    };
    drop(prog);
    let observer = super::program_by_id(id).ok_or(SystemError::ENOENT)?;
    table.drop_fd(fd as i32)?.finish_close()?;
    passed &= super::program_by_id(id).is_some();
    drop(observer);
    passed &= weak_prog.upgrade().is_none() && super::program_by_id(id).is_none();
    Ok(passed)
}
