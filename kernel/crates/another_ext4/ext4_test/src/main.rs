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

fn make_ext4() {
    let _ = std::process::Command::new("rm")
        .args(["-rf", "ext4.img"])
        .status();
    let _ = std::process::Command::new("dd")
        .args(["if=/dev/zero", "of=ext4.img", "bs=1M", "count=512"])
        .status();
    let _ = std::process::Command::new("mkfs.ext4")
        .args(["ext4.img"])
        .output();
}

fn open_ext4() -> Ext4 {
    let file = BlockFile::new("ext4.img");
    println!("creating ext4");
    let mut ext4 = Ext4::load(Arc::new(file)).expect("open ext4 failed");
    ext4.init().expect("init ext4 failed");
    ext4
}

fn load_ext4() -> Ext4 {
    let file = BlockFile::new("ext4.img");
    Ext4::load(Arc::new(file)).expect("open ext4 failed")
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
    ext4.generic_remove(ROOT_INO, "d3/f0")
        .expect("remove file failed");
    ext4.generic_lookup(ROOT_INO, "d3/f0")
        .expect_err("file not removed");
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

/// Simulate the apt update scenario: multiple files being grown via setattr
/// in an interleaved pattern, then verified via extent_query (write_data_only).
/// This reproduces the ENOENT bug where extent_query cannot find blocks that
/// setattr already allocated.
fn interleaved_setattr_writeback_test() {
    use another_ext4::{FileAttr, SetAttr};

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

            // setattr to grow file (allocate blocks)
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

            // Simulate writeback: write_data_only for all blocks in the
            // newly allocated region
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

fn main() {
    SimpleLogger::new().init().unwrap();
    log::set_max_level(log::LevelFilter::Off);
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
    xattr_test(&mut ext4);
    println!("xattr test done");
    rename_exchange_test::rename_exchange_test(&mut ext4);
    println!("rename_exchange test done");
    drop(ext4);
    extent_corruption_test();
    println!("extent corruption test done");

    // Interleaved setattr + writeback test
    interleaved_setattr_writeback_test();

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
