//! Independent e2fsprogs seed and nojournal shared external-xattr regression.
//! Run in a disposable directory, or pass `--seed IMAGE` for guest fixtures.
#[path = "../block_file.rs"]
#[allow(dead_code)]
mod block_file;

use another_ext4::{ErrCode, Ext4, InodeMode, EXT4_ROOT_INO};
use block_file::BlockFile;
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    process::Command,
    sync::Arc,
};

fn debugfs(path: &str, command: &str) -> String {
    let out = Command::new("debugfs")
        .args(["-w", "-R", command, path])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}
fn fsck(path: &str) -> bool {
    let out = Command::new("e2fsck").args(["-fn", path]).output().unwrap();
    if !out.status.success() {
        eprintln!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    out.status.success()
}
fn crc32c(mut crc: u32, bytes: &[u8]) -> u32 {
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f63b78u32.wrapping_mul(crc & 1));
        }
    }
    crc
}
fn value(path: &str, file: &str, name: &str) -> Vec<u8> {
    let output = format!("{path}.readback");
    let _ = fs::remove_file(&output);
    debugfs(path, &format!("ea_get -f {output} /{file} {name}"));
    let bytes = fs::read(&output).unwrap();
    fs::remove_file(output).unwrap();
    bytes
}
fn read_at(path: &str, offset: u64, length: usize) -> Vec<u8> {
    let mut image = fs::File::open(path).unwrap();
    let mut bytes = vec![0; length];
    image.seek(SeekFrom::Start(offset)).unwrap();
    image.read_exact(&mut bytes).unwrap();
    bytes
}
fn free_blocks(path: &str) -> u32 {
    u32::from_le_bytes(read_at(path, 1024 + 12, 4).try_into().unwrap())
}
fn acl_block(path: &str, name: &str) -> u64 {
    debugfs(path, &format!("stat /{name}"))
        .lines()
        .find_map(|line| line.strip_prefix("File ACL: "))
        .unwrap()
        .parse()
        .unwrap()
}
fn refcount(path: &str, block: u64) -> u32 {
    u32::from_le_bytes(read_at(path, block * 4096 + 4, 4).try_into().unwrap())
}
fn seed(path: &str, metadata_csum: bool, single_attribute: bool) {
    let features = if metadata_csum {
        "^has_journal,^orphan_file,^metadata_csum_seed,metadata_csum"
    } else {
        "^has_journal,^orphan_file,^metadata_csum_seed,^metadata_csum"
    };
    fs::File::create(path)
        .unwrap()
        .set_len(16 * 1024 * 1024)
        .unwrap();
    assert!(Command::new("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-I", "256", "-O", features, path])
        .status()
        .unwrap()
        .success());
    for name in ["a", "b", "c"] {
        debugfs(path, &format!("write /dev/null /{name}"));
    }
    let input = format!("{path}.input");
    for (name, byte) in [("user.alpha", b'a'), ("user.beta", b'b')] {
        if single_attribute && name == "user.beta" {
            continue;
        }
        fs::write(&input, vec![byte; 1000]).unwrap();
        debugfs(path, &format!("ea_set -f {input} /a {name}"));
    }
    fs::remove_file(input).unwrap();
    let stat = debugfs(path, "stat /a");
    let block: u64 = stat
        .lines()
        .find_map(|line| line.strip_prefix("File ACL: "))
        .unwrap()
        .parse()
        .unwrap();
    assert_ne!(block, 0);
    // debugfs updates inode checksums, block accounting and pointers. No
    // library code under test creates or encodes any seed xattr entry.
    for name in ["b", "c"] {
        debugfs(path, &format!("sif /{name} file_acl {block}"));
        debugfs(path, &format!("sif /{name} blocks 8"));
    }
    // debugfs does not deduplicate blocks. The only raw edits implement the
    // documented shared-block reference count and Linux's xattr CRC32C.
    let mut image = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let mut uuid = [0; 16];
    image.seek(SeekFrom::Start(1024 + 0x68)).unwrap();
    image.read_exact(&mut uuid).unwrap();
    let mut bytes = [0; 4096];
    image.seek(SeekFrom::Start(block * 4096)).unwrap();
    image.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes[..4], &0xea020000u32.to_le_bytes());
    assert_eq!(&bytes[4..8], &1u32.to_le_bytes());
    bytes[4..8].copy_from_slice(&3u32.to_le_bytes());
    bytes[16..20].fill(0);
    let checksum = crc32c(crc32c(crc32c(!0, &uuid), &block.to_le_bytes()), &bytes);
    if metadata_csum {
        bytes[16..20].copy_from_slice(&checksum.to_le_bytes());
    }
    image.seek(SeekFrom::Start(block * 4096)).unwrap();
    image.write_all(&bytes).unwrap();
    image.sync_all().unwrap();
    drop(image);
    assert!(fsck(path), "independent shared seed must be e2fsck clean");
    for file in ["a", "b", "c"] {
        assert_eq!(value(path, file, "user.alpha"), vec![b'a'; 1000]);
        if !single_attribute {
            assert_eq!(value(path, file, "user.beta"), vec![b'b'; 1000]);
        }
    }
    // An independent writer must also accept and correctly split this seed.
    let oracle = format!("{path}.oracle");
    fs::copy(path, &oracle).unwrap();
    debugfs(&oracle, "ea_set /a user.alpha oracle");
    assert_eq!(value(&oracle, "a", "user.alpha"), b"oracle");
    for peer in ["b", "c"] {
        assert_eq!(value(&oracle, peer, "user.alpha"), vec![b'a'; 1000]);
    }
    assert_ne!(acl_block(&oracle, "a"), block);
    assert_eq!(refcount(&oracle, block), 2);
    assert!(fsck(&oracle));
    fs::remove_file(oracle).unwrap();
    println!(
        "seed {path}: three inodes, shared block {block}, refcount=3, metadata_csum={metadata_csum}; e2fsck clean"
    );
}
fn get(fs: &Ext4, inode: u32, name: &[u8]) -> Result<Vec<u8>, ErrCode> {
    let size = fs.getxattr(inode, name, &mut []).map_err(|e| e.code())?;
    let mut out = vec![0; size];
    fs.getxattr(inode, name, &mut out).map_err(|e| e.code())?;
    Ok(out)
}
// The library reports metadata gate contention as EAGAIN; its guest VFS
// caller retries it. Keep the same contract without requiring a kernel waker.
fn retry_contention(
    mut operation: impl FnMut() -> Result<(), another_ext4::Ext4Error>,
) -> Result<(), another_ext4::Ext4Error> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match operation() {
            Err(error)
                if error.code() == ErrCode::EAGAIN && std::time::Instant::now() < deadline =>
            {
                std::thread::yield_now()
            }
            result => return result,
        }
    }
}
fn run(metadata_csum: bool) -> usize {
    let seed_path = format!("shared-xattr-seed-{metadata_csum}.img");
    seed(&seed_path, metadata_csum, false);
    let seed_free = free_blocks(&seed_path);
    let seed_block = acl_block(&seed_path, "a");
    let seed_image = read_at(&seed_path, seed_block * 4096, 4096);
    let mut failures = 0;
    for case in [
        "set",
        "replace",
        "create",
        "remove",
        "invalid-flags",
        "oversized",
        "release",
        "concurrent",
    ] {
        let path = format!("shared-xattr-{case}-{metadata_csum}.img");
        fs::copy(&seed_path, &path).unwrap();
        let fs = Arc::new(Ext4::load_writable(Arc::new(BlockFile::new(&path))).unwrap());
        let a = fs.lookup(EXT4_ROOT_INO, "a").unwrap();
        let b = fs.lookup(EXT4_ROOT_INO, "b").unwrap();
        let c = fs.lookup(EXT4_ROOT_INO, "c").unwrap();
        let mut operation_ok = true;
        let expected_alpha = match case {
            "set" => {
                fs.setxattr(a, b"user.alpha", &vec![b'x'; 1100]).unwrap();
                Ok(vec![b'x'; 1100])
            }
            "replace" => {
                fs.setxattr_with_flags(a, b"user.alpha", &vec![b'x'; 900], false, true)
                    .unwrap();
                Ok(vec![b'x'; 900])
            }
            "create" => {
                fs.setxattr_with_flags(a, b"user.gamma", &vec![b'g'; 500], true, false)
                    .unwrap();
                Ok(vec![b'a'; 1000])
            }
            "remove" => {
                fs.removexattr(a, b"user.alpha").unwrap();
                Err(ErrCode::ENODATA)
            }
            "invalid-flags" => {
                assert_eq!(
                    fs.setxattr_with_flags(a, b"user.alpha", b"bad", true, false)
                        .unwrap_err()
                        .code(),
                    ErrCode::EEXIST
                );
                assert_eq!(
                    fs.setxattr_with_flags(a, b"user.missing", b"bad", false, true)
                        .unwrap_err()
                        .code(),
                    ErrCode::ENODATA
                );
                Ok(vec![b'a'; 1000])
            }
            "oversized" => {
                assert!(fs.setxattr(a, b"user.alpha", &vec![0; 4096]).is_err());
                Ok(vec![b'a'; 1000])
            }
            "concurrent" => {
                let barrier = Arc::new(std::sync::Barrier::new(3));
                let threads: Vec<_> = [(a, b'x'), (b, b'y')]
                    .into_iter()
                    .map(|(inode, byte)| {
                        let fs = fs.clone();
                        let barrier = barrier.clone();
                        std::thread::spawn(move || {
                            barrier.wait();
                            let value = vec![byte; 1100];
                            retry_contention(|| {
                                fs.setxattr_with_flags(inode, b"user.alpha", &value, false, true)
                            })?;
                            retry_contention(|| fs.removexattr(inode, b"user.beta"))
                        })
                    })
                    .collect();
                barrier.wait();
                for thread in threads {
                    operation_ok &= thread.join().unwrap().is_ok();
                }
                Ok(vec![b'x'; 1100])
            }
            "release" => {
                for inode in [a, b, c] {
                    let alpha = fs.removexattr(inode, b"user.alpha");
                    let beta = fs.removexattr(inode, b"user.beta");
                    if alpha.is_err() || beta.is_err() {
                        operation_ok = false;
                        eprintln!("release inode {inode}: alpha={alpha:?} beta={beta:?}");
                    }
                }
                Err(ErrCode::ENODATA)
            }
            _ => unreachable!(),
        };
        let mut good = operation_ok && get(&fs, a, b"user.alpha") == expected_alpha;
        if case != "release" && case != "concurrent" {
            good &= get(&fs, a, b"user.beta") == Ok(vec![b'b'; 1000]);
            for inode in [b, c] {
                good &= get(&fs, inode, b"user.alpha") == Ok(vec![b'a'; 1000]);
                good &= get(&fs, inode, b"user.beta") == Ok(vec![b'b'; 1000]);
                good &= get(&fs, inode, b"user.gamma") == Err(ErrCode::ENODATA);
            }
        }
        fs.shutdown_writable().unwrap();
        drop(fs);
        let remounted = Ext4::load_writable(Arc::new(BlockFile::new(&path))).unwrap();
        good &= get(&remounted, a, b"user.alpha") == expected_alpha;
        if case == "release" {
            for inode in [a, b, c] {
                good &= get(&remounted, inode, b"user.alpha") == Err(ErrCode::ENODATA);
                good &= get(&remounted, inode, b"user.beta") == Err(ErrCode::ENODATA);
                good &= remounted.getattr(inode).unwrap().blocks == 0;
            }
        } else if case == "concurrent" {
            good &= get(&remounted, b, b"user.alpha") == Ok(vec![b'y'; 1100]);
            good &= get(&remounted, c, b"user.alpha") == Ok(vec![b'a'; 1000]);
            good &= get(&remounted, c, b"user.beta") == Ok(vec![b'b'; 1000]);
            for inode in [a, b] {
                good &= get(&remounted, inode, b"user.beta") == Err(ErrCode::ENODATA);
            }
        } else {
            for inode in [b, c] {
                good &= get(&remounted, inode, b"user.alpha") == Ok(vec![b'a'; 1000]);
                good &= get(&remounted, inode, b"user.beta") == Ok(vec![b'b'; 1000]);
                good &= remounted.getattr(inode).unwrap().blocks == 8;
            }
            if case == "create" {
                good &= get(&remounted, a, b"user.gamma") == Ok(vec![b'g'; 500]);
            }
        }
        remounted.shutdown_writable().unwrap();
        drop(remounted);
        let expected_free = match case {
            "release" => seed_free + 1,
            "concurrent" => seed_free - 2,
            "invalid-flags" | "oversized" => seed_free,
            _ => seed_free - 1,
        };
        good &= free_blocks(&path) == expected_free;
        if case == "release" {
            for name in ["a", "b", "c"] {
                good &= acl_block(&path, name) == 0;
            }
        } else if case == "invalid-flags" || case == "oversized" {
            good &= acl_block(&path, "a") == seed_block;
            good &= read_at(&path, seed_block * 4096, 4096) == seed_image;
        } else {
            let new_block = acl_block(&path, "a");
            good &= new_block != seed_block && refcount(&path, new_block) == 1;
            good &= refcount(&path, seed_block) == if case == "concurrent" { 1 } else { 2 };
            good &= acl_block(&path, "c") == seed_block;
            if case == "concurrent" {
                let peer_block = acl_block(&path, "b");
                good &= peer_block != seed_block
                    && peer_block != new_block
                    && refcount(&path, peer_block) == 1;
            }
        }
        good &= fsck(&path);
        println!(
            "shared-xattr metadata_csum={metadata_csum} {case}: {}",
            if good {
                "PASS"
            } else {
                "FAIL: peer isolation or persistent structure"
            }
        );
        failures += usize::from(!good);
        fs::remove_file(&path).unwrap();
    }
    failures
}
/// Keep ownership transitions in one concrete scenario: no generic scenario
/// interpreter, production test hooks or requirements on allocator placement.
fn boundary_cases(metadata_csum: bool) {
    let seed_path = format!("shared-xattr-seed-{metadata_csum}.img");
    let path = format!("shared-xattr-lifetime-{metadata_csum}.img");
    fs::copy(&seed_path, &path).unwrap();
    let baseline_free = free_blocks(&path);
    let old_home = acl_block(&path, "a");
    let fs = Ext4::load_writable(Arc::new(BlockFile::new(&path))).unwrap();
    let baseline_inodes = fs.super_block().unwrap().free_inodes_count();
    let a = fs.lookup(EXT4_ROOT_INO, "a").unwrap();
    let b = fs.lookup(EXT4_ROOT_INO, "b").unwrap();
    let c = fs.lookup(EXT4_ROOT_INO, "c").unwrap();

    fs.setxattr_with_flags(a, b"user.alpha", &vec![b'a'; 1000], false, true)
        .unwrap();
    assert_eq!(get(&fs, a, b"user.alpha"), Ok(vec![b'a'; 1000]));
    // A same-value update may keep the shared home or perform a safe COW.
    let same_home = acl_block(&path, "a");
    assert_eq!(
        free_blocks(&path),
        baseline_free - u32::from(same_home != old_home)
    );
    assert_eq!(
        refcount(&path, old_home),
        if same_home == old_home { 3 } else { 2 }
    );

    fs.setxattr(a, b"user.alpha", b"").unwrap();
    assert_eq!(get(&fs, a, b"user.alpha"), Ok(Vec::new()));
    let private_home = acl_block(&path, "a");
    assert_ne!(private_home, old_home);
    assert_eq!(refcount(&path, private_home), 1);
    assert_eq!(refcount(&path, old_home), 2);
    assert_eq!(free_blocks(&path), baseline_free - 1);
    fs.setxattr(a, b"user.alpha", &vec![b'p'; 1500]).unwrap();
    assert_eq!(get(&fs, a, b"user.alpha"), Ok(vec![b'p'; 1500]));
    assert_eq!(
        acl_block(&path, "a"),
        private_home,
        "private replacement must stay in place"
    );
    assert_eq!(free_blocks(&path), baseline_free - 1);
    for name in [b"user.alpha".as_slice(), b"user.beta"] {
        fs.removexattr(a, name).unwrap();
    }
    assert_eq!(fs.getattr(a).unwrap().blocks, 0);
    assert_eq!(acl_block(&path, "a"), 0);
    assert_eq!(free_blocks(&path), baseline_free);

    // First attach, then last-item removal. The old shared home must not be
    // charged to this inode after its earlier detach.
    fs.setxattr(a, b"user.gamma", &vec![b'g'; 1000]).unwrap();
    assert_eq!(fs.getattr(a).unwrap().blocks, 8);
    assert_eq!(refcount(&path, acl_block(&path, "a")), 1);
    assert_eq!(free_blocks(&path), baseline_free - 1);
    fs.removexattr(a, b"user.gamma").unwrap();
    assert_eq!(fs.getattr(a).unwrap().blocks, 0);
    assert_eq!(free_blocks(&path), baseline_free);
    for peer in [b, c] {
        assert_eq!(get(&fs, peer, b"user.alpha"), Ok(vec![b'a'; 1000]));
        assert_eq!(get(&fs, peer, b"user.beta"), Ok(vec![b'b'; 1000]));
    }

    // An unlinked inode retains its xattrs until the VFS consumes the final
    // close capability. A different owner mutates while that capability lives.
    let held = fs.unlink(EXT4_ROOT_INO, "b").unwrap().unwrap();
    assert_eq!(held.inode_id(), b);
    assert_eq!(get(&fs, b, b"user.alpha"), Ok(vec![b'a'; 1000]));
    fs.removexattr(c, b"user.beta").unwrap();
    assert_eq!(get(&fs, b, b"user.beta"), Ok(vec![b'b'; 1000]));
    assert_eq!(refcount(&path, old_home), 1);
    assert_eq!(free_blocks(&path), baseline_free - 1);
    fs.reclaim_inode(held).unwrap();
    assert_eq!(
        free_blocks(&path),
        baseline_free,
        "last shared reference retired exactly once"
    );
    for name in ["a", "c"] {
        let handle = fs.unlink(EXT4_ROOT_INO, name).unwrap().unwrap();
        fs.reclaim_inode(handle).unwrap();
    }
    assert_eq!(free_blocks(&path), baseline_free + 1);
    assert_eq!(
        fs.super_block().unwrap().free_inodes_count(),
        baseline_inodes + 3
    );
    let fresh = fs
        .create(EXT4_ROOT_INO, "reused", InodeMode::FILE | InodeMode::ALL_RW)
        .unwrap();
    fs.setxattr(fresh, b"user.reused", &vec![b'r'; 1000])
        .unwrap();
    assert_eq!(get(&fs, fresh, b"user.reused"), Ok(vec![b'r'; 1000]));
    assert_eq!(fs.getattr(fresh).unwrap().blocks, 8);
    assert_eq!(free_blocks(&path), baseline_free);
    fs.shutdown_writable().unwrap();
    drop(fs);
    assert!(fsck(&path));
    let fs = Ext4::load_writable(Arc::new(BlockFile::new(&path))).unwrap();
    let fresh = fs.lookup(EXT4_ROOT_INO, "reused").unwrap();
    assert_eq!(get(&fs, fresh, b"user.reused"), Ok(vec![b'r'; 1000]));
    let handle = fs.unlink(EXT4_ROOT_INO, "reused").unwrap().unwrap();
    fs.reclaim_inode(handle).unwrap();
    assert_eq!(free_blocks(&path), baseline_free + 1);
    assert_eq!(
        fs.super_block().unwrap().free_inodes_count(),
        baseline_inodes + 3
    );
    fs.shutdown_writable().unwrap();
    drop(fs);
    assert!(fsck(&path));
    fs::remove_file(path).unwrap();
    println!("shared-xattr metadata_csum={metadata_csum} samevalue/empty/private/firstattach/remove-last/unlink-lifetime/reuse: PASS");
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "--seed" {
        assert!(args[3..]
            .iter()
            .all(|arg| arg == "--no-metadata-csum" || arg == "--single-attribute"));
        seed(
            &args[2],
            !args[3..].iter().any(|arg| arg == "--no-metadata-csum"),
            args[3..].iter().any(|arg| arg == "--single-attribute"),
        );
    } else {
        assert_eq!(args.len(), 1);
        let failures = run(true) + run(false);
        assert_eq!(failures, 0, "shared xattr regression failures");
        boundary_cases(true);
        boundary_cases(false);
    }
}
