use another_ext4::{ErrCode, Ext4, InodeMode, EXT4_ROOT_INO};
use block_file::BlockFile;
use simple_logger::SimpleLogger;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;

mod block_file;
mod cache_test;
mod rename_exchange_test;

const ROOT_INO: u32 = EXT4_ROOT_INO;

struct TestImageCleanup(&'static str);

impl Drop for TestImageCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0);
    }
}

fn make_ext4_image(path: &str) {
    let _ = std::process::Command::new("rm")
        .args(["-rf", path])
        .status();
    let _ = std::process::Command::new("dd")
        .args(["if=/dev/zero", &format!("of={path}"), "bs=1M", "count=512"])
        .status();
    let _ = std::process::Command::new("mkfs.ext4")
        .args(["-O", "^orphan_file", path])
        .output();
}

fn make_ext4() {
    make_ext4_image("ext4.img");
}

fn nojournal_seeded_image_test() {
    const IMAGE: &str = "ext4-nojournal-seeded.img";
    let _image_cleanup = TestImageCleanup(IMAGE);
    let _ = std::fs::remove_file(IMAGE);
    assert!(std::process::Command::new("truncate")
        .args(["-s", "1G", IMAGE])
        .status()
        .expect("truncate failed")
        .success());
    assert!(std::process::Command::new("mkfs.ext4")
        .args([
            "-F",
            "-b",
            "4096",
            "-I",
            "256",
            "-O",
            "^has_journal,^orphan_file,metadata_csum_seed",
            IMAGE,
        ])
        .status()
        .expect("mkfs.ext4 failed")
        .success());

    let ext4 = Ext4::load_writable(Arc::new(BlockFile::new(IMAGE)))
        .expect("Cube-equivalent nojournal image must mount writable");
    let dir = ext4
        .generic_create(ROOT_INO, "cube", InodeMode::DIRECTORY | InodeMode::ALL_RWX)
        .expect("create directory failed");
    let file = ext4
        .generic_create(dir, "payload", InodeMode::FILE | InodeMode::ALL_RWX)
        .expect("create file failed");
    ext4.write(file, 0, b"dragonos-on-cube")
        .expect("write failed");
    ext4.rename(dir, "payload", dir, "renamed")
        .expect("rename failed");
    ext4.commit_inode_metadata(file, None, None, Some(12_345), Some(12_346))
        .expect("nojournal cached timestamp commit failed");
    ext4.shutdown_writable().expect("shutdown failed");
    drop(ext4);

    let ext4 = Ext4::load_writable(Arc::new(BlockFile::new(IMAGE)))
        .expect("nojournal timestamp verification remount failed");
    let attr = ext4
        .getattr(file)
        .expect("nojournal timestamp verification getattr failed");
    assert_eq!(attr.mtime, 12_345);
    assert_eq!(attr.ctime, 12_346);
    ext4.generic_remove(dir, "renamed").expect("unlink failed");
    ext4.shutdown_writable().expect("second shutdown failed");
    drop(ext4);

    let output = std::process::Command::new("e2fsck")
        .args(["-fn", IMAGE])
        .output()
        .expect("e2fsck failed");
    assert!(
        output.status.success(),
        "nojournal seeded e2fsck failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!("nojournal metadata_csum_seed image test done");
}

fn open_ext4() -> Ext4 {
    let file = BlockFile::new("ext4.img");
    println!("creating ext4");
    let mut ext4 = Ext4::load_writable(Arc::new(file)).expect("open writable ext4 failed");
    ext4.init().expect("init ext4 failed");
    ext4
}

fn load_ext4() -> Ext4 {
    load_ext4_image("ext4.img")
}

fn load_ext4_image(path: &str) -> Ext4 {
    let file = BlockFile::new(path);
    Ext4::load_writable(Arc::new(file)).expect("open writable ext4 failed")
}

fn read_u16_le(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

fn read_u32_le(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn read_exact_at(file: &mut std::fs::File, off: u64, len: usize) -> Vec<u8> {
    let mut data = vec![0u8; len];
    file.seek(SeekFrom::Start(off)).expect("seek failed");
    file.read_exact(&mut data).expect("read failed");
    data
}

fn write_all_at(file: &mut std::fs::File, off: u64, data: &[u8]) {
    file.seek(SeekFrom::Start(off)).expect("seek failed");
    file.write_all(data).expect("write failed");
}

fn corrupt_inode_extent_root_magic(path: &str, inode_id: u32) {
    // ext4 inode.i_block starts at offset 40, extent header magic is the first 2 bytes in i_block.
    const INODE_I_BLOCK_OFF: usize = 40;

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open image failed");

    let sb = read_exact_at(&mut file, 1024, 1024);
    let log_block_size = read_u32_le(&sb, 24);
    let block_size = 1024u64 << log_block_size;
    let inodes_per_group = read_u32_le(&sb, 40);
    let inode_size = read_u16_le(&sb, 88) as u64;
    let mut desc_size = read_u16_le(&sb, 254) as u64;
    if desc_size == 0 {
        desc_size = 32;
    }

    let bgid = (inode_id - 1) / inodes_per_group;
    let idx_in_bg = (inode_id - 1) % inodes_per_group;

    let bgdt_off = if block_size == 1024 {
        2 * block_size
    } else {
        block_size
    };
    let desc_off = bgdt_off + bgid as u64 * desc_size;
    let desc = read_exact_at(&mut file, desc_off, desc_size as usize);

    let inode_table_lo = read_u32_le(&desc, 8) as u64;
    let inode_table_hi = if desc_size >= 64 {
        read_u32_le(&desc, 40) as u64
    } else {
        0
    };
    let inode_table_block = (inode_table_hi << 32) | inode_table_lo;

    let inode_off = inode_table_block * block_size + idx_in_bg as u64 * inode_size;
    let mut inode = read_exact_at(&mut file, inode_off, inode_size as usize);
    inode[INODE_I_BLOCK_OFF] = 0;
    inode[INODE_I_BLOCK_OFF + 1] = 0;
    write_all_at(&mut file, inode_off, &inode);
}

fn extent_corruption_test() {
    make_ext4();
    let ino = {
        let ext4 = open_ext4();
        let file_mode: InodeMode = InodeMode::FILE | InodeMode::ALL_RWX;
        let ino = ext4
            .generic_create(ROOT_INO, "corrupt_target", file_mode)
            .expect("create failed");
        ext4.write(ino, 0, b"seed-data").expect("seed write failed");
        ino
    };

    corrupt_inode_extent_root_magic("ext4.img", ino);

    let ext4 = load_ext4();
    let err = ext4
        .write(ino, 0, b"x")
        .expect_err("corrupted extent should fail");
    assert_eq!(err.code(), ErrCode::EIO);
}

fn mkdir_test(ext4: &mut Ext4) {
    let dir_mode: InodeMode = InodeMode::DIRECTORY | InodeMode::ALL_RWX;
    ext4.generic_create(ROOT_INO, "d1", dir_mode)
        .expect("mkdir failed");
    ext4.generic_create(ROOT_INO, "d1/d2", dir_mode)
        .expect("mkdir failed");
    ext4.generic_create(ROOT_INO, "d1/d2/d3", dir_mode)
        .expect("mkdir failed");
    ext4.generic_create(ROOT_INO, "d1/d2/d3/d4", dir_mode)
        .expect("mkdir failed");
    ext4.generic_create(ROOT_INO, "d2", dir_mode)
        .expect("mkdir failed");
    ext4.generic_create(ROOT_INO, "d2/d3", dir_mode)
        .expect("mkdir failed");
    ext4.generic_create(ROOT_INO, "d2/d3/d4", dir_mode)
        .expect("mkdir failed");
    ext4.generic_create(ROOT_INO, "d3", dir_mode)
        .expect("mkdir failed");
}

fn create_test(ext4: &mut Ext4) {
    let file_mode: InodeMode = InodeMode::FILE | InodeMode::ALL_RWX;
    ext4.generic_create(ROOT_INO, "d1/d2/d3/d4/f1", file_mode)
        .expect("open failed");
    ext4.generic_create(ROOT_INO, "d3/f0", file_mode)
        .expect("open failed");
    ext4.generic_create(ROOT_INO, "d3/f1", file_mode)
        .expect("open failed");
    ext4.generic_create(ROOT_INO, "f1", file_mode)
        .expect("open failed");
}

fn read_write_test(ext4: &mut Ext4) {
    let wbuffer = "hello world".as_bytes();
    let file = ext4.generic_lookup(ROOT_INO, "d3/f0").expect("open failed");
    ext4.write(file, 0, wbuffer).expect("write failed");
    let mut rbuffer = vec![0u8; wbuffer.len() + 100]; // Test end of file
    let rcount = ext4.read(file, 0, &mut rbuffer).expect("read failed");
    assert_eq!(wbuffer, &rbuffer[..rcount]);
}

fn large_read_write_test(ext4: &mut Ext4) {
    let wbuffer = vec![99u8; 1024 * 1024 * 16];
    let file = ext4.generic_lookup(ROOT_INO, "d3/f1").expect("open failed");
    ext4.write(file, 0, &wbuffer).expect("write failed");
    let mut rbuffer = vec![0u8; wbuffer.len()];
    let rcount = ext4.read(file, 0, &mut rbuffer).expect("read failed");
    assert_eq!(wbuffer, &rbuffer[..rcount]);
}

fn remove_file_test(ext4: &mut Ext4) {
    let removed = ext4
        .generic_lookup(ROOT_INO, "d3/f0")
        .expect("lookup before remove failed");
    ext4.generic_remove(ROOT_INO, "d3/f0")
        .expect("remove file failed");
    ext4.generic_lookup(ROOT_INO, "d3/f0")
        .expect_err("file not removed");
    ext4.getattr(removed)
        .expect_err("removed inode was not reclaimed");
    ext4.generic_remove(ROOT_INO, "d3/f1")
        .expect("remove file failed");
    ext4.generic_lookup(ROOT_INO, "d3/f1")
        .expect_err("file not removed");
    ext4.generic_remove(ROOT_INO, "f1")
        .expect("remove file failed");
    ext4.generic_lookup(ROOT_INO, "f1")
        .expect_err("file not removed");
    ext4.generic_remove(ROOT_INO, "d1/not_exist")
        .expect_err("remove file failed");
}

fn generic_rename_reclaims_replaced_inode_test(ext4: &mut Ext4) {
    let file_mode: InodeMode = InodeMode::FILE | InodeMode::ALL_RWX;
    let source = ext4
        .generic_create(ROOT_INO, "rename_source", file_mode)
        .expect("create rename source failed");
    let replaced = ext4
        .generic_create(ROOT_INO, "rename_target", file_mode)
        .expect("create rename target failed");

    ext4.generic_rename(ROOT_INO, "rename_source", "rename_target")
        .expect("generic rename failed");
    assert_eq!(
        ext4.generic_lookup(ROOT_INO, "rename_target")
            .expect("lookup renamed file failed"),
        source
    );
    ext4.getattr(replaced)
        .expect_err("replaced inode was not reclaimed");
}

fn xattr_test(ext4: &mut Ext4) {
    let file_mode: InodeMode = InodeMode::FILE | InodeMode::ALL_RWX;
    let file = ext4
        .generic_create(ROOT_INO, "f2", file_mode)
        .expect("Create failed");
    ext4.setxattr(file, "user.testone", "hello world".as_bytes())
        .expect("setxattr failed");
    ext4.setxattr(file, "user.testtwo", "world hello".as_bytes())
        .expect("setxattr failed");

    let names = ext4.listxattr(file).expect("listxattr failed");
    assert_eq!(names, vec!["user.testone", "user.testtwo"]);

    let value = ext4
        .getxattr(file, "user.testone")
        .expect("getxattr failed");
    assert_eq!(value, "hello world".as_bytes());
    let value = ext4
        .getxattr(file, "user.testtwo")
        .expect("getxattr failed");
    assert_eq!(value, "world hello".as_bytes());

    let names = ext4.listxattr(file).expect("listxattr failed");
    assert_eq!(names, vec!["user.testone", "user.testtwo"]);

    ext4.removexattr(file, "user.testone")
        .expect("removexattr failed");
    ext4.getxattr(file, "user.testone")
        .expect_err("getxattr failed");
    let names = ext4.listxattr(file).expect("listxattr failed");
    assert_eq!(names, vec!["user.testtwo"]);
}

/// Simulate the apt update scenario: multiple files grow and dirty page-cache
/// ranges in an interleaved pattern, then writeback verifies the prepared
/// extent mappings via write_data_only.
fn interleaved_setattr_writeback_test() {
    use another_ext4::SetAttr;

    make_ext4();
    let ext4 = load_ext4();
    let file_mode: InodeMode = InodeMode::FILE | InodeMode::ALL_RWX;

    // Create 3 files to simulate concurrent downloads
    let ino1 = ext4
        .generic_create(ROOT_INO, "pkg1.deb", file_mode)
        .expect("create pkg1 failed");
    let ino2 = ext4
        .generic_create(ROOT_INO, "pkg2.deb", file_mode)
        .expect("create pkg2 failed");
    let ino3 = ext4
        .generic_create(ROOT_INO, "pkg3.deb", file_mode)
        .expect("create pkg3 failed");

    let files = [ino1, ino2, ino3];
    let mut file_sizes = [0u64; 3];
    let chunk_size = 65536u64; // 64KB chunks, like apt download buffers

    // Simulate interleaved growth: grow each file by 64KB in round-robin,
    // up to ~1.6MB each (400 blocks). This forces non-contiguous allocation
    // because different files interleave their alloc_block calls.
    let target_size = 1_600_000u64;

    for round in 0u64.. {
        let mut all_done = true;
        for (i, &ino) in files.iter().enumerate() {
            if file_sizes[i] >= target_size {
                continue;
            }
            all_done = false;
            let new_size = core::cmp::min(file_sizes[i] + chunk_size, target_size);

            // Grow visible size without allocating holes.
            ext4.setattr(
                ino,
                SetAttr {
                    mode: None,
                    uid: None,
                    gid: None,
                    size: Some(new_size),
                    atime: None,
                    mtime: None,
                    ctime: None,
                    crtime: None,
                },
            )
            .unwrap_or_else(|e| {
                panic!(
                    "setattr FAILED: ino={} round={} old_size={} new_size={} err={:?}",
                    ino, round, file_sizes[i], new_size, e
                )
            });

            // Simulate buffered-write preparation followed by writeback for
            // the newly dirtied region.
            let write_offset = file_sizes[i] as usize;
            let write_len = (new_size - file_sizes[i]) as usize;
            ext4.prepare_buffered_write(ino, write_offset, write_len, new_size, None)
                .unwrap_or_else(|e| {
                    panic!(
                        "prepare_buffered_write FAILED: ino={} round={} old_size={} \
                         new_size={} err={:?}",
                        ino, round, file_sizes[i], new_size, e
                    )
                });
            let old_block = (file_sizes[i] as usize) / 4096;
            let new_block = ((new_size as usize) + 4095) / 4096;
            for blk in old_block..new_block {
                let offset = blk * 4096;
                let data = vec![0xABu8; 4096];
                let write_len = core::cmp::min(4096, new_size as usize - offset);
                ext4.write_data_only(ino, offset, &data[..write_len])
                    .unwrap_or_else(|e| {
                        panic!(
                            "write_data_only FAILED: ino={} round={} iblock={} offset={} \
                             old_size={} new_size={} err={:?}",
                            ino, round, blk, offset, file_sizes[i], new_size, e
                        )
                    });
            }

            file_sizes[i] = new_size;
        }
        if all_done {
            break;
        }
    }

    // Verify: read back all data
    for (i, &ino) in files.iter().enumerate() {
        let mut buf = vec![0u8; file_sizes[i] as usize];
        let n = ext4.read(ino, 0, &mut buf).expect("read failed");
        assert_eq!(n, file_sizes[i] as usize, "file {} size mismatch", i);
        // All written bytes should be 0xAB
        for (j, &b) in buf.iter().enumerate() {
            assert_eq!(b, 0xAB, "file {} byte {} mismatch: got {}", i, j, b);
        }
    }

    drop(ext4);

    // e2fsck validation
    let output = std::process::Command::new("e2fsck")
        .args(["-fn", "ext4.img"])
        .output()
        .expect("e2fsck failed");
    assert!(
        output.status.success(),
        "e2fsck FAILED:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    println!("interleaved setattr+writeback test done");
}

fn sparse_growth_and_range_writeback_test() {
    use another_ext4::SetAttr;

    make_ext4();
    let ext4 = load_ext4();
    let file_mode: InodeMode = InodeMode::FILE | InodeMode::ALL_RWX;
    let ino = ext4
        .generic_create(ROOT_INO, "sparse.dat", file_mode)
        .expect("create sparse.dat failed");

    let sparse_size = 2 * 1024 * 1024u64;
    ext4.setattr(
        ino,
        SetAttr {
            mode: None,
            uid: None,
            gid: None,
            size: Some(sparse_size),
            atime: None,
            mtime: None,
            ctime: None,
            crtime: None,
        },
    )
    .expect("sparse grow setattr failed");

    let attr = ext4.getattr(ino).expect("getattr sparse file failed");
    assert_eq!(attr.size, sparse_size);
    assert_eq!(
        attr.blocks, 0,
        "sparse growth must not allocate data blocks"
    );

    let mut hole = vec![0x5Au8; 8192];
    let n = ext4
        .read(ino, 123, &mut hole)
        .expect("read sparse hole failed");
    assert_eq!(n, hole.len());
    assert!(hole.iter().all(|&b| b == 0), "hole read must return zeros");

    let missing = ext4
        .write_data_only(ino, 4096, &[0x11; 512])
        .expect_err("writeback into unallocated hole must fail");
    assert_eq!(missing.code(), ErrCode::ENOENT);

    let write_offset = 1024 * 1024 + 123;
    let data = vec![0xC3u8; 7000];
    ext4.prepare_buffered_write(ino, write_offset, data.len(), sparse_size, None)
        .expect("prepare buffered sparse write failed");
    ext4.write_data_only(ino, write_offset, &data)
        .expect("write_data_only after prepare failed");

    let attr = ext4.getattr(ino).expect("getattr sparse file failed");
    assert_eq!(attr.size, sparse_size);
    assert!(attr.blocks > 0, "range write must allocate written blocks");
    assert!(
        attr.blocks < sparse_size / 512,
        "range write must not allocate the entire sparse file"
    );

    let mut read_back = vec![0u8; data.len()];
    let n = ext4
        .read(ino, write_offset, &mut read_back)
        .expect("read sparse written range failed");
    assert_eq!(n, data.len());
    assert_eq!(read_back, data);

    drop(ext4);
    let output = std::process::Command::new("e2fsck")
        .args(["-fn", "ext4.img"])
        .output()
        .expect("e2fsck failed");
    assert!(
        output.status.success(),
        "e2fsck FAILED:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    println!("sparse growth and range writeback test done");
}

fn prepare_buffered_write_does_not_commit_size_test() {
    make_ext4();
    let ext4 = load_ext4();
    let file_mode: InodeMode = InodeMode::FILE | InodeMode::ALL_RWX;
    let ino = ext4
        .generic_create(ROOT_INO, "prepared.dat", file_mode)
        .expect("create prepared.dat failed");

    let write_offset = 1024 * 1024;
    let data = vec![0x7Eu8; 4096];
    ext4.prepare_buffered_write(
        ino,
        write_offset,
        data.len(),
        (write_offset + data.len()) as u64,
        None,
    )
    .expect("prepare buffered write failed");

    let attr = ext4.getattr(ino).expect("getattr prepared file failed");
    assert_eq!(attr.size, 0, "prepare must not commit visible size");
    assert!(attr.blocks > 0, "prepare must allocate the written range");

    ext4.write_data_only(ino, write_offset, &data)
        .expect("write_data_only after prepare failed");
    let mut hidden = vec![0u8; data.len()];
    assert_eq!(
        ext4.read(ino, write_offset, &mut hidden)
            .expect("read beyond uncommitted size failed"),
        0,
        "uncommitted size must keep prepared data outside EOF hidden"
    );

    ext4.commit_inode_size(ino, (write_offset + data.len()) as u64, None)
        .expect("commit prepared size failed");
    let mut read_back = vec![0u8; data.len()];
    let n = ext4
        .read(ino, write_offset, &mut read_back)
        .expect("read committed prepared range failed");
    assert_eq!(n, data.len());
    assert_eq!(read_back, data);

    drop(ext4);
    let output = std::process::Command::new("e2fsck")
        .args(["-fn", "ext4.img"])
        .output()
        .expect("e2fsck failed");
    assert!(
        output.status.success(),
        "e2fsck FAILED:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    println!("prepare buffered write size boundary test done");
}

fn legacy_orphan_mount_cleanup_test() {
    make_ext4();
    let (free_inodes_before, free_blocks_before) = {
        let ext4 = load_ext4();
        let sb = ext4.super_block().expect("read baseline superblock failed");
        let counters = (sb.free_inodes_count(), sb.free_blocks_count());
        ext4.shutdown_writable()
            .expect("baseline clean writable shutdown failed");
        counters
    };
    let orphan = {
        let ext4 = load_ext4();
        let mode = InodeMode::FILE | InodeMode::ALL_RWX;
        let inode = ext4
            .generic_create(ROOT_INO, "crash_orphan", mode)
            .expect("create crash orphan failed");
        ext4.write(inode, 0, &vec![0x5a; 3 * 1024 * 1024])
            .expect("write crash orphan failed");

        // Model a crash after the final unlink transaction but before the VFS
        // lifetime owner invokes reclaim_inode(): persist the one-shot handle
        // only in volatile memory, then drop the mounted instance.
        let handle = ext4
            .unlink(ROOT_INO, "crash_orphan")
            .expect("final unlink transaction failed")
            .expect("final unlink did not produce reclaim handle");
        drop(handle);
        inode
    };

    let ext4 = load_ext4();
    ext4.generic_lookup(ROOT_INO, "crash_orphan")
        .expect_err("orphaned name reappeared after recovery");
    ext4.getattr(orphan)
        .expect_err("mount recovery did not reclaim orphan inode");

    // Recovery must leave both allocation bitmaps reusable.  Repeated
    // allocate/write/unlink/reclaim cycles catch a stale inode bit, leaked
    // data blocks, or an orphan record that is consumed more than once.
    let mode = InodeMode::FILE | InodeMode::ALL_RWX;
    for iteration in 0..16 {
        let name = format!("post_recovery_reuse_{iteration}");
        let inode = ext4
            .generic_create(ROOT_INO, &name, mode)
            .expect("post-recovery create failed");
        let payload = vec![iteration as u8; 2 * 1024 * 1024];
        ext4.write(inode, 0, &payload)
            .expect("post-recovery write failed");
        let handle = ext4
            .unlink(ROOT_INO, &name)
            .expect("post-recovery unlink failed")
            .expect("post-recovery final unlink returned no reclaim handle");
        ext4.reclaim_inode(handle)
            .expect("post-recovery reclaim failed");
    }
    let recovered = ext4
        .super_block()
        .expect("read recovered superblock failed");
    assert_eq!(
        recovered.free_inodes_count(),
        free_inodes_before,
        "orphan recovery leaked inode bitmap accounting"
    );
    assert_eq!(
        recovered.free_blocks_count(),
        free_blocks_before,
        "orphan recovery leaked block bitmap accounting"
    );
    ext4.shutdown_writable()
        .expect("clean writable shutdown failed");
    drop(ext4);

    let output = std::process::Command::new("e2fsck")
        .args(["-fn", "ext4.img"])
        .output()
        .expect("e2fsck failed");
    assert!(
        output.status.success(),
        "orphan recovery e2fsck FAILED:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!("legacy orphan mount cleanup test done");
}

fn rename_replace_orphan_mount_cleanup_test() {
    make_ext4();
    let replaced = {
        let ext4 = load_ext4();
        let mode = InodeMode::FILE | InodeMode::ALL_RWX;
        ext4.generic_create(ROOT_INO, "rename_source", mode)
            .expect("create rename source failed");
        let replaced = ext4
            .generic_create(ROOT_INO, "rename_target", mode)
            .expect("create rename target failed");
        ext4.write(replaced, 0, &vec![0xa5; 1024 * 1024])
            .expect("write rename target failed");

        // Model a crash after the atomic replace transaction and before the
        // VFS lifetime owner consumes the reclaim capability.  The replaced
        // inode must already be on the durable legacy orphan chain.
        let handle = ext4
            .rename(ROOT_INO, "rename_source", ROOT_INO, "rename_target")
            .expect("rename replace transaction failed")
            .expect("final target replacement did not return reclaim handle");
        drop(handle);
        replaced
    };

    let ext4 = load_ext4();
    ext4.getattr(replaced)
        .expect_err("mount recovery did not reclaim rename target orphan");
    ext4.generic_lookup(ROOT_INO, "rename_source")
        .expect_err("rename source name reappeared after recovery");
    ext4.generic_lookup(ROOT_INO, "rename_target")
        .expect("rename target disappeared after recovery");
    ext4.shutdown_writable()
        .expect("clean writable shutdown failed");
    drop(ext4);

    let output = std::process::Command::new("e2fsck")
        .args(["-fn", "ext4.img"])
        .output()
        .expect("e2fsck failed");
    assert!(
        output.status.success(),
        "rename orphan recovery e2fsck FAILED:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!("rename replace orphan mount cleanup test done");
}

fn cross_parent_directory_replace_orphan_test() {
    make_ext4();
    let replaced = {
        let ext4 = load_ext4();
        let mode = InodeMode::DIRECTORY | InodeMode::ALL_RWX;
        let old_parent = ext4
            .generic_create(ROOT_INO, "old_parent", mode)
            .expect("create old parent failed");
        let new_parent = ext4
            .generic_create(ROOT_INO, "new_parent", mode)
            .expect("create new parent failed");
        let source = ext4
            .generic_create(old_parent, "source_dir", mode)
            .expect("create source directory failed");
        let replaced = ext4
            .generic_create(new_parent, "target_dir", mode)
            .expect("create target directory failed");

        let handle = ext4
            .rename(old_parent, "source_dir", new_parent, "target_dir")
            .expect("cross-parent directory replace transaction failed")
            .expect("directory replacement did not return reclaim handle");
        assert_eq!(
            ext4.generic_lookup(new_parent, "target_dir")
                .expect("new directory entry missing"),
            source
        );
        ext4.generic_lookup(old_parent, "source_dir")
            .expect_err("old directory entry survived replace");
        assert_eq!(
            ext4.generic_lookup(source, "..").expect("new '..' missing"),
            new_parent
        );
        drop(handle);
        replaced
    };

    let ext4 = load_ext4();
    ext4.getattr(replaced)
        .expect_err("mount recovery did not reclaim replaced directory");
    ext4.shutdown_writable()
        .expect("clean writable shutdown failed");
    drop(ext4);
    let output = std::process::Command::new("e2fsck")
        .args(["-fn", "ext4.img"])
        .output()
        .expect("e2fsck failed");
    assert!(
        output.status.success(),
        "cross-parent directory rename e2fsck FAILED:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!("cross-parent directory replace orphan test done");
}

fn assert_linked_orphan_e2fsck(context: &str) {
    let output = std::process::Command::new("e2fsck")
        .args(["-fn", "ext4.img"])
        .output()
        .expect("e2fsck failed");
    assert!(
        output.status.success(),
        "{context} e2fsck FAILED:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Exercise the durable linked-tail protocol independently from a future
/// writeback mapper.  The test-only enrolment method creates exactly the
/// post-map/pre-size crash state which mount recovery must understand; normal
/// write paths never call it.
fn linked_orphan_tail_recovery_and_final_namespace_transition_test() {
    let mode = InodeMode::FILE | InodeMode::ALL_RWX;

    // A linked orphan with mappings beyond its durable EOF must retain its
    // name, remove only the tail, and then remove its orphan-list record.
    make_ext4();
    let (tail_inode, original_blocks) = {
        let ext4 = load_ext4();
        let inode = ext4
            .generic_create(ROOT_INO, "linked_tail", mode)
            .expect("create linked tail failed");
        ext4.write(inode, 0, &vec![0x6d; 3 * 4096])
            .expect("write linked tail failed");
        let original_blocks = ext4
            .getattr(inode)
            .expect("read linked tail attr failed")
            .blocks;
        ext4.setattr(
            inode,
            another_ext4::SetAttr {
                size: Some(4096),
                ..Default::default()
            },
        )
        .expect("shrink linked tail EOF failed");
        assert!(
            ext4.test_enroll_linked_tail_orphan(inode)
                .expect("enrol linked tail orphan failed"),
            "first linked-tail enrolment did not publish a chain node"
        );
        assert!(
            !ext4
                .test_enroll_linked_tail_orphan(inode)
                .expect("repeat linked-tail enrolment failed"),
            "repeat linked-tail enrolment inserted a duplicate chain node"
        );
        (inode, original_blocks)
    };
    let ext4 = load_ext4();
    let recovered = ext4
        .getattr(tail_inode)
        .expect("linked tail lost its namespace inode during recovery");
    assert_eq!(
        recovered.size, 4096,
        "linked-tail EOF changed during recovery"
    );
    assert!(
        recovered.blocks < original_blocks,
        "linked-tail recovery did not reclaim mappings beyond durable EOF"
    );
    let mut prefix = vec![0u8; 4096];
    assert_eq!(
        ext4.read(tail_inode, 0, &mut prefix)
            .expect("read recovered linked-tail prefix failed"),
        prefix.len()
    );
    assert!(prefix.iter().all(|byte| *byte == 0x6d));
    ext4.shutdown_writable()
        .expect("clean shutdown after linked-tail recovery failed");
    drop(ext4);
    assert_linked_orphan_e2fsck("linked-tail recovery");

    // Final unlink of a pre-existing linked-tail member must retain its chain
    // position and turn it into a zero-link orphan, not add it again.
    make_ext4();
    let unlinked_inode = {
        let ext4 = load_ext4();
        let inode = ext4
            .generic_create(ROOT_INO, "linked_unlink", mode)
            .expect("create linked-unlink target failed");
        ext4.write(inode, 0, &vec![0x29; 2 * 4096])
            .expect("write linked-unlink target failed");
        ext4.setattr(
            inode,
            another_ext4::SetAttr {
                size: Some(4096),
                ..Default::default()
            },
        )
        .expect("shrink linked-unlink EOF failed");
        assert!(ext4
            .test_enroll_linked_tail_orphan(inode)
            .expect("enrol linked-unlink orphan failed"));
        let handle = ext4
            .unlink(ROOT_INO, "linked_unlink")
            .expect("final unlink of linked tail failed")
            .expect("final unlink of linked tail returned no reclaim handle");
        drop(handle);
        inode
    };
    let ext4 = load_ext4();
    ext4.generic_lookup(ROOT_INO, "linked_unlink")
        .expect_err("final-unlinked linked tail regained its name after recovery");
    ext4.getattr(unlinked_inode)
        .expect_err("final-unlinked linked tail was not fully reclaimed");
    ext4.shutdown_writable()
        .expect("clean shutdown after linked-tail unlink recovery failed");
    drop(ext4);
    assert_linked_orphan_e2fsck("linked-tail final unlink");

    // Rename replacement has the same final-target transition, but exercises
    // the separate namespace transaction and its credit calculation.
    make_ext4();
    let replaced_inode = {
        let ext4 = load_ext4();
        let source = ext4
            .generic_create(ROOT_INO, "linked_rename_source", mode)
            .expect("create linked-rename source failed");
        let target = ext4
            .generic_create(ROOT_INO, "linked_rename_target", mode)
            .expect("create linked-rename target failed");
        ext4.write(target, 0, &vec![0xe4; 2 * 4096])
            .expect("write linked-rename target failed");
        ext4.setattr(
            target,
            another_ext4::SetAttr {
                size: Some(4096),
                ..Default::default()
            },
        )
        .expect("shrink linked-rename target EOF failed");
        assert!(ext4
            .test_enroll_linked_tail_orphan(target)
            .expect("enrol linked-rename target failed"));
        let handle = ext4
            .rename(
                ROOT_INO,
                "linked_rename_source",
                ROOT_INO,
                "linked_rename_target",
            )
            .expect("replace linked-tail target failed")
            .expect("replace linked-tail target returned no reclaim handle");
        drop(handle);
        assert_eq!(
            ext4.generic_lookup(ROOT_INO, "linked_rename_target")
                .expect("rename replacement name missing"),
            source
        );
        target
    };
    let ext4 = load_ext4();
    ext4.getattr(replaced_inode)
        .expect_err("replaced linked tail was not fully reclaimed");
    ext4.generic_lookup(ROOT_INO, "linked_rename_source")
        .expect_err("rename source reappeared after recovery");
    ext4.generic_lookup(ROOT_INO, "linked_rename_target")
        .expect("rename destination disappeared after recovery");
    ext4.shutdown_writable()
        .expect("clean shutdown after linked-tail rename recovery failed");
    drop(ext4);
    assert_linked_orphan_e2fsck("linked-tail rename replacement");
    println!("linked orphan tail recovery and namespace transition test done");
}

/// Exercise the first real reserved-delalloc mapper boundary without wiring it
/// into the VFS yet. The mapper must publish a zeroed extent and linked orphan
/// while on-disk EOF stays old; only exact full-block data submission may then
/// advance EOF and remove that orphan.
fn reserved_delalloc_block_mapper_test() {
    const BLOCK: usize = 4096;
    let mode = InodeMode::FILE | InodeMode::ALL_RWX;
    make_ext4();
    let ext4 = load_ext4();
    let inode = ext4
        .generic_create(ROOT_INO, "reserved_delalloc", mode)
        .expect("create reserved-delalloc target failed");

    let mut lease = ext4
        .reserve_delalloc_lease(1, 0)
        .expect("reserve delayed data block failed");
    let mut receipt = ext4
        .test_map_delalloc_reserved_block_append(inode, 0, &mut lease)
        .expect("map reserved delayed block failed");
    assert_eq!(
        ext4.getattr(inode)
            .expect("getattr after delayed map failed")
            .size,
        0,
        "map publication advanced durable EOF before data submission"
    );
    let mut pre_submit = [0u8; BLOCK];
    assert_eq!(
        ext4.read(inode, 0, &mut pre_submit)
            .expect("read before delayed data submit failed"),
        0,
        "mapped tail became visible before its EOF transaction"
    );

    // An existing linked receipt is a queue-head dependency. A second raw
    // mapper call must roll back its temporary bitmap debit and leave its
    // caller-owned lease releasable, rather than joining the same orphan with
    // an ambiguous second EOF lifetime.
    let mut blocked_lease = ext4
        .reserve_delalloc_lease(1, 0)
        .expect("reserve blocked delayed data block failed");
    assert_eq!(
        ext4.test_map_delalloc_reserved_block_append(inode, 0, &mut blocked_lease)
            .expect_err("second delayed mapper bypassed the linked-tail head")
            .code(),
        ErrCode::EAGAIN
    );
    ext4.release_delalloc_lease_batch(&mut [&mut blocked_lease])
        .expect("blocked mapper lease was not restored after abort");

    let payload = [0xa7; BLOCK];
    ext4.test_writeback_delalloc_mapped_block(&mut receipt, &payload, Some(123))
        .expect("submit delayed mapped block failed");
    let attr = ext4
        .getattr(inode)
        .expect("getattr after delayed writeback failed");
    assert_eq!(
        attr.size, BLOCK as u64,
        "delayed writeback did not commit EOF"
    );
    let mut read_back = [0u8; BLOCK];
    assert_eq!(
        ext4.read(inode, 0, &mut read_back)
            .expect("read delayed writeback payload failed"),
        BLOCK
    );
    assert_eq!(read_back, payload, "delayed writeback payload differs");

    ext4.shutdown_writable()
        .expect("clean shutdown after delayed mapper test failed");
    drop(ext4);
    assert_linked_orphan_e2fsck("reserved delayed mapper completion");
    println!("reserved delayed block mapper test done");
}

/// The mapper is intentionally exposed only through `test-api` until a VFS
/// token owns lifecycle/drain/queue state.  Exercise the capability boundary
/// here so a later refactor cannot accidentally restore the unsafe raw API.
fn reserved_delalloc_mapper_capability_boundary_test() {
    const BLOCK: usize = 4096;
    let mode = InodeMode::FILE | InodeMode::ALL_RWX;

    // A mapped linked tail is the only authority for its EOF and payload:
    // generic size/data publishers must not make its zeroed block visible or
    // overwrite it before the receipt completion transaction.
    make_ext4();
    let ext4 = load_ext4();
    let inode = ext4
        .generic_create(ROOT_INO, "tail_owner", mode)
        .expect("create linked-tail owner failed");
    ext4.write(inode, 0, &[0x4c; BLOCK])
        .expect("seed linked-tail owner failed");
    let mut lease = ext4
        .reserve_delalloc_lease(1, 0)
        .expect("reserve linked-tail owner failed");
    let mut receipt = ext4
        .test_map_delalloc_reserved_block_append(inode, BLOCK, &mut lease)
        .expect("map linked-tail owner failed");
    assert_eq!(
        ext4.setattr(
            inode,
            another_ext4::SetAttr {
                size: Some(0),
                ..Default::default()
            }
        )
        .expect_err("truncate bypassed linked-tail EOF owner")
        .code(),
        ErrCode::EAGAIN
    );
    assert_eq!(
        ext4.commit_inode_size(inode, (2 * BLOCK) as u64, None)
            .expect_err("metadata EOF publication bypassed linked-tail owner")
            .code(),
        ErrCode::EAGAIN
    );
    assert_eq!(
        ext4.write_data_only(inode, BLOCK, &[0x31; BLOCK])
            .expect_err("generic writeback overwrote mapped tail")
            .code(),
        ErrCode::EAGAIN
    );
    assert_eq!(
        ext4.write(inode, BLOCK, &[0x32; BLOCK])
            .expect_err("generic write bypassed mapped-tail owner")
            .code(),
        ErrCode::EAGAIN
    );
    // The temporary raw harness intentionally permits only one tail for the
    // whole mount until a verified orphan-role index exists. This must not be
    // weakened into a same-inode-only check.
    let follower = ext4
        .generic_create(ROOT_INO, "tail_follower", mode)
        .expect("create mapped-tail follower failed");
    let mut follower_lease = ext4
        .reserve_delalloc_lease(1, 0)
        .expect("reserve mapped-tail follower failed");
    assert_eq!(
        ext4.test_map_delalloc_reserved_block_append(follower, 0, &mut follower_lease)
            .expect_err("second inode bypassed mount-wide raw tail limit")
            .code(),
        ErrCode::EAGAIN
    );
    ext4.release_delalloc_lease_batch(&mut [&mut follower_lease])
        .expect("second-inode blocked lease was not releasable");
    ext4.test_writeback_delalloc_mapped_block(&mut receipt, &[0x33; BLOCK], Some(456))
        .expect("linked-tail owner could not complete after rejected interference");
    ext4.shutdown_writable()
        .expect("clean shutdown after linked-tail owner boundary test failed");

    // A receipt is mount-affine. Passing it to another filesystem must not
    // poison the accidental target or consume the source receipt.
    const A_IMAGE: &str = "ext4-delalloc-source.img";
    const B_IMAGE: &str = "ext4-delalloc-target.img";
    make_ext4_image(A_IMAGE);
    make_ext4_image(B_IMAGE);
    let source = load_ext4_image(A_IMAGE);
    let target = load_ext4_image(B_IMAGE);
    let source_inode = source
        .generic_create(ROOT_INO, "source", mode)
        .expect("create source receipt inode failed");
    let mut source_lease = source
        .reserve_delalloc_lease(1, 0)
        .expect("reserve source receipt failed");
    let mut source_receipt = source
        .test_map_delalloc_reserved_block_append(source_inode, 0, &mut source_lease)
        .expect("map source receipt failed");
    assert_eq!(
        target
            .test_writeback_delalloc_mapped_block(&mut source_receipt, &[0x61; BLOCK], None)
            .expect_err("foreign filesystem accepted a delayed receipt")
            .code(),
        ErrCode::EINVAL
    );
    let target_inode = target
        .generic_create(ROOT_INO, "target", mode)
        .expect("foreign receipt poisoned target mount");
    target
        .write(target_inode, 0, b"still writable")
        .expect("target mount was poisoned by foreign receipt");
    source
        .test_writeback_delalloc_mapped_block(&mut source_receipt, &[0x62; BLOCK], None)
        .expect("source receipt was consumed by foreign mount rejection");
    source
        .shutdown_writable()
        .expect("source clean shutdown failed");
    target
        .shutdown_writable()
        .expect("target clean shutdown failed");

    // After a fail-stop, terminalising an in-memory test capability must not
    // reopen capacity, but it must permit ordinary teardown without a second
    // Drop-time failure that hides the original I/O error.
    make_ext4();
    let ext4 = load_ext4();
    let mut lease = ext4
        .reserve_delalloc_lease(1, 0)
        .expect("reserve fail-stop lease failed");
    ext4.fail_stop_mutations();
    ext4.test_abandon_delalloc_lease_after_fail_stop(&mut lease)
        .expect("fail-stopped lease did not terminalise");
    drop(lease);
    drop(ext4);

    make_ext4();
    let ext4 = load_ext4();
    let inode = ext4
        .generic_create(ROOT_INO, "fail_stop_receipt", mode)
        .expect("create fail-stop receipt inode failed");
    let mut lease = ext4
        .reserve_delalloc_lease(1, 0)
        .expect("reserve fail-stop receipt failed");
    let mut receipt = ext4
        .test_map_delalloc_reserved_block_append(inode, 0, &mut lease)
        .expect("map fail-stop receipt failed");
    ext4.fail_stop_mutations();
    ext4.test_abandon_mapped_delalloc_after_fail_stop(&mut receipt)
        .expect("fail-stopped receipt did not terminalise");
    drop(receipt);
    drop(ext4);

    // If the caller reaches completion after another path has already
    // fail-stopped the matching mount, completion itself terminalises the
    // receipt without touching the ledger or attempting data I/O.
    make_ext4();
    let ext4 = load_ext4();
    let inode = ext4
        .generic_create(ROOT_INO, "fail_stop_auto_receipt", mode)
        .expect("create automatic fail-stop receipt inode failed");
    let mut lease = ext4
        .reserve_delalloc_lease(1, 0)
        .expect("reserve automatic fail-stop receipt failed");
    let mut receipt = ext4
        .test_map_delalloc_reserved_block_append(inode, 0, &mut lease)
        .expect("map automatic fail-stop receipt failed");
    ext4.fail_stop_mutations();
    assert_eq!(
        ext4.test_writeback_delalloc_mapped_block(&mut receipt, &[0x79; BLOCK], None)
            .expect_err("fail-stopped receipt completed")
            .code(),
        ErrCode::EIO
    );
    drop(receipt);
    drop(ext4);

    let _ = std::fs::remove_file(A_IMAGE);
    let _ = std::fs::remove_file(B_IMAGE);
    println!("reserved delayed mapper capability boundary test done");
}

fn main() {
    let _image_cleanup = TestImageCleanup("ext4.img");
    SimpleLogger::new().init().unwrap();
    log::set_max_level(log::LevelFilter::Off);
    nojournal_seeded_image_test();
    make_ext4();
    println!("ext4.img created");
    let mut ext4 = open_ext4();
    println!("ext4 opened");
    mkdir_test(&mut ext4);
    println!("mkdir test done");
    create_test(&mut ext4);
    println!("create test done");
    read_write_test(&mut ext4);
    println!("read write test done");
    large_read_write_test(&mut ext4);
    println!("large read write test done");
    remove_file_test(&mut ext4);
    println!("remove file test done");
    generic_rename_reclaims_replaced_inode_test(&mut ext4);
    println!("generic rename reclaim test done");
    xattr_test(&mut ext4);
    println!("xattr test done");
    rename_exchange_test::rename_exchange_test(&mut ext4);
    println!("rename_exchange test done");
    drop(ext4);
    extent_corruption_test();
    println!("extent corruption test done");

    rename_replace_orphan_mount_cleanup_test();
    cross_parent_directory_replace_orphan_test();

    // Interleaved setattr + writeback test
    interleaved_setattr_writeback_test();

    sparse_growth_and_range_writeback_test();

    prepare_buffered_write_does_not_commit_size_test();

    legacy_orphan_mount_cleanup_test();

    linked_orphan_tail_recovery_and_final_namespace_transition_test();

    reserved_delalloc_block_mapper_test();

    reserved_delalloc_mapper_capability_boundary_test();

    // Cache correctness tests — run on a fresh image
    // Use load_ext4 (not open_ext4) to avoid init() corrupting mkfs.ext4 checksums
    println!("\n--- Running cache correctness tests ---");
    make_ext4();
    let ext4 = load_ext4();
    cache_test::run_all_cache_tests(&ext4, "ext4.img");
    drop(ext4);
    // e2fsck validation after all writes are flushed
    cache_test::e2fsck_validation("ext4.img");
    println!("--- All cache correctness tests passed! ---");
}
