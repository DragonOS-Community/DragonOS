//! /proc/vmstat - 虚拟内存统计信息

use crate::libs::mutex::MutexGuard;
use crate::{
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::{proc_read, trim_string},
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    mm::page_cache_stats,
};
use alloc::{borrow::ToOwned, format, sync::Arc, sync::Weak, vec::Vec};
use system_error::SystemError;

#[derive(Clone, Copy, Debug)]
enum VmstatSource {
    Zero,
    FilePages,
    FileMapped,
    FileDirty,
    FileWriteback,
    Shmem,
    Unevictable,
    DropPagecache,
}

#[derive(Clone, Copy, Debug)]
struct VmstatField {
    name: &'static str,
    source: VmstatSource,
}

// Match Linux 6.6 vmstat_text ordering with minimal features enabled.
const VMSTAT_FIELDS: &[VmstatField] = &[
    // enum zone_stat_item counters
    VmstatField {
        name: "nr_free_pages",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_zone_inactive_anon",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_zone_active_anon",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_zone_inactive_file",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_zone_active_file",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_zone_unevictable",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_zone_write_pending",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_mlock",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_bounce",
        source: VmstatSource::Zero,
    },
    // enum node_stat_item counters
    VmstatField {
        name: "nr_inactive_anon",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_active_anon",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_inactive_file",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_active_file",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_unevictable",
        source: VmstatSource::Unevictable,
    },
    VmstatField {
        name: "nr_slab_reclaimable",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_slab_unreclaimable",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_isolated_anon",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_isolated_file",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "workingset_nodes",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "workingset_refault_anon",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "workingset_refault_file",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "workingset_activate_anon",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "workingset_activate_file",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "workingset_restore_anon",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "workingset_restore_file",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "workingset_nodereclaim",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_anon_pages",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_mapped",
        source: VmstatSource::FileMapped,
    },
    VmstatField {
        name: "nr_file_pages",
        source: VmstatSource::FilePages,
    },
    VmstatField {
        name: "nr_dirty",
        source: VmstatSource::FileDirty,
    },
    VmstatField {
        name: "nr_writeback",
        source: VmstatSource::FileWriteback,
    },
    VmstatField {
        name: "nr_writeback_temp",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_shmem",
        source: VmstatSource::Shmem,
    },
    VmstatField {
        name: "nr_shmem_hugepages",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_shmem_pmdmapped",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_file_hugepages",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_file_pmdmapped",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_anon_transparent_hugepages",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_vmscan_write",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_vmscan_immediate_reclaim",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_dirtied",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_written",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_throttled_written",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_kernel_misc_reclaimable",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_foll_pin_acquired",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_foll_pin_released",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_kernel_stack",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_page_table_pages",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_sec_page_table_pages",
        source: VmstatSource::Zero,
    },
    // enum writeback_stat_item counters
    VmstatField {
        name: "nr_dirty_threshold",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "nr_dirty_background_threshold",
        source: VmstatSource::Zero,
    },
    // enum vm_event_item counters (CONFIG_VM_EVENT_COUNTERS)
    VmstatField {
        name: "pgpgin",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgpgout",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pswpin",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pswpout",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgalloc_normal",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgalloc_movable",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "allocstall_normal",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "allocstall_movable",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgskip_normal",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgskip_movable",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgfree",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgactivate",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgdeactivate",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pglazyfree",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgfault",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgmajfault",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pglazyfreed",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgrefill",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgreuse",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgsteal_kswapd",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgsteal_direct",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgsteal_khugepaged",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgdemote_kswapd",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgdemote_direct",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgdemote_khugepaged",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgscan_kswapd",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgscan_direct",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgscan_khugepaged",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgscan_direct_throttle",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgscan_anon",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgscan_file",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgsteal_anon",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgsteal_file",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pginodesteal",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "slabs_scanned",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "kswapd_inodesteal",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "kswapd_low_wmark_hit_quickly",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "kswapd_high_wmark_hit_quickly",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pageoutrun",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "pgrotated",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "drop_pagecache",
        source: VmstatSource::DropPagecache,
    },
    VmstatField {
        name: "drop_slab",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "oom_kill",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "unevictable_pgs_culled",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "unevictable_pgs_scanned",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "unevictable_pgs_rescued",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "unevictable_pgs_mlocked",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "unevictable_pgs_munlocked",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "unevictable_pgs_cleared",
        source: VmstatSource::Zero,
    },
    VmstatField {
        name: "unevictable_pgs_stranded",
        source: VmstatSource::Zero,
    },
];

/// /proc/vmstat 文件的 FileOps 实现
#[derive(Debug)]
pub struct VmstatFileOps;

impl VmstatFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }

    fn generate_vmstat_content() -> Vec<u8> {
        let stats = page_cache_stats::snapshot();
        let mut data: Vec<u8> = Vec::new();

        for field in VMSTAT_FIELDS {
            let value = match field.source {
                VmstatSource::Zero => 0,
                VmstatSource::FilePages => stats.file_pages,
                VmstatSource::FileMapped => stats.file_mapped,
                VmstatSource::FileDirty => stats.file_dirty,
                VmstatSource::FileWriteback => stats.file_writeback,
                VmstatSource::Shmem => stats.shmem_pages,
                VmstatSource::Unevictable => stats.unevictable,
                VmstatSource::DropPagecache => stats.drop_pagecache,
            };
            data.append(&mut format!("{} {}\n", field.name, value).as_bytes().to_owned());
        }

        data.append(&mut b"nr_unstable 0\n".to_vec());
        trim_string(&mut data);
        data
    }
}

impl FileOps for VmstatFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let content = Self::generate_vmstat_content();
        proc_read(offset, len, buf, &content)
    }
}
