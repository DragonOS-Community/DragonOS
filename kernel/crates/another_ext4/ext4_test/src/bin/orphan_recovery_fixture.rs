//! Produce a crash image using the normal durable unlink/orphan protocol.
//! No filesystem cleanup runs after unlink: mounting the result must recover it.
#[path = "../block_file.rs"]
#[allow(dead_code)]
mod block_file;

use another_ext4::{Ext4, InodeMode, BLOCK_SIZE, EXT4_ROOT_INO};
use block_file::BlockFile;
use std::{env, fs, sync::Arc};

fn main() {
    let args: Vec<_> = env::args().collect();
    let image = match args.as_slice() {
        [_, option, image] if option == "--in-place" => image,
        [_, source, image] => {
            fs::copy(source, image).expect("copy clean ext4 image");
            image
        }
        _ => panic!("usage: orphan_recovery_fixture SOURCE IMAGE | --in-place IMAGE"),
    };
    let device = Arc::new(BlockFile::new(image));
    let ext4 = Ext4::load_writable(device).expect("load fixture filesystem");
    let survivor = ext4
        .generic_create(
            EXT4_ROOT_INO,
            "recovery-baseline",
            InodeMode::FILE | InodeMode::ALL_RW,
        )
        .expect("create surviving file");
    let mut payload = vec![0x5a; BLOCK_SIZE];
    assert_eq!(ext4.write(survivor, 0, &payload).unwrap(), payload.len());
    let baseline = ext4.super_block().expect("read baseline counters");
    let header = format!(
        "{} {}\n",
        baseline.free_blocks_count(),
        baseline.free_inodes_count()
    );
    payload[..header.len()].copy_from_slice(header.as_bytes());
    assert_eq!(ext4.write(survivor, 0, &payload).unwrap(), payload.len());

    for index in 0..2 {
        let name = format!("recovery-orphan-{index}");
        let inode = ext4
            .generic_create(EXT4_ROOT_INO, &name, InodeMode::FILE | InodeMode::ALL_RW)
            .expect("create orphan file");
        let data = vec![0xa5; (index + 2) * BLOCK_SIZE];
        assert_eq!(ext4.write(inode, 0, &data).unwrap(), data.len());
        let handle = ext4
            .unlink(EXT4_ROOT_INO, &name)
            .unwrap()
            .expect("final unlink capability");
        assert_eq!(handle.inode_id(), inode);
        // Intentionally omit final eviction, exactly as loss of the guest does.
        std::mem::forget(handle);
    }
    let orphaned = ext4.super_block().unwrap();
    assert!(orphaned.free_blocks_count() < baseline.free_blocks_count());
    assert_eq!(
        orphaned.free_inodes_count() + 2,
        baseline.free_inodes_count()
    );
    println!(
        "durable orphan fixture: free blocks {} -> {}, free inodes {} -> {}",
        orphaned.free_blocks_count(),
        baseline.free_blocks_count(),
        orphaned.free_inodes_count(),
        baseline.free_inodes_count()
    );
    // Ext4 has no cleanup Drop; leaving RECOVER and s_last_orphan set is the test.
}
