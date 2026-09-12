//! Focused namespace transaction regression coverage, including durable
//! validation with e2fsck. Run in a disposable working directory.
#[path = "../block_file.rs"]
#[allow(dead_code)]
mod block_file;

use another_ext4::{ErrCode, Ext4, InodeMode, InodeOwner, EXT4_ROOT_INO};
use block_file::BlockFile;
use std::{fs::File, process::Command, sync::Arc};

fn fsck(path: &str) {
    let output = Command::new("e2fsck").args(["-fn", path]).output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run(journal: bool) {
    let path = if journal {
        "namespace-journal.img"
    } else {
        "namespace-direct.img"
    };
    File::create(path)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let features = if journal {
        "^orphan_file"
    } else {
        "^orphan_file,^has_journal"
    };
    assert!(Command::new("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-I", "256", "-O", features, path])
        .status()
        .unwrap()
        .success());
    let fs = Ext4::load_writable(Arc::new(BlockFile::new(path))).unwrap();
    let directory = fs
        .mkdir(EXT4_ROOT_INO, "namespace", InodeMode::ALL_RWX)
        .unwrap();
    // Long names force repeated directory block allocation, exercising the
    // append helper's single ownership of i_blocks accounting.
    for index in 0..96 {
        let name = format!("{index:03}-{}", "x".repeat(180));
        fs.create(directory, &name, InodeMode::FILE | InodeMode::ALL_RW)
            .unwrap();
    }
    let owner = InodeOwner { uid: 123, gid: 456 };
    let target = "a/".repeat(700);
    let symlink = fs
        .symlink_with_owner_and_attr(directory, "long-link", &target, owner)
        .unwrap();
    assert_eq!((symlink.uid, symlink.gid), (123, 456));
    assert_eq!(
        symlink.blocks, 8,
        "one 4 KiB target must count once in 512-byte units"
    );
    let short = fs
        .symlink_with_owner_and_attr(directory, "short-link", "abc", owner)
        .unwrap();
    assert_eq!(short.blocks, 0);
    let device = fs
        .mknod_with_owner_and_attr(
            directory,
            "device",
            InodeMode::CHARDEV | InodeMode::ALL_RW,
            259,
            1234,
            owner,
        )
        .unwrap();
    assert_eq!(device.rdev, (259, 1234));
    assert_eq!(device.blocks, 0);

    // Failure happens after inode staging; abort must not leak an allocated
    // zero-link inode or any directory/bitmap image to disk.
    let error = fs
        .create(
            directory,
            &"z".repeat(256),
            InodeMode::FILE | InodeMode::ALL_RW,
        )
        .unwrap_err();
    assert_eq!(error.code(), ErrCode::ENAMETOOLONG);
    assert!(fs.lookup(directory, &"z".repeat(256)).is_err());

    let file = fs
        .create(directory, "xattrs", InodeMode::FILE | InodeMode::ALL_RW)
        .unwrap();
    fs.setxattr(file, "user.payload", &vec![0x31; 1000])
        .unwrap();
    fs.setxattr(file, "user.payload", &vec![0x32; 1700])
        .unwrap();
    assert_eq!(fs.getxattr(file, "user.payload").unwrap(), vec![0x32; 1700]);
    fs.setxattr(file, "user.other", b"retained").unwrap();
    fs.removexattr(file, "user.payload").unwrap();
    assert_eq!(fs.getxattr(file, "user.other").unwrap(), b"retained");
    // Allocation after xattr creation must retain the external xattr block
    // in i_blocks, including the direct backend's extent-count recomputation.
    assert_eq!(fs.write(file, 0, b"payload").unwrap(), 7);
    assert_eq!(fs.getattr(file).unwrap().blocks, 16);
    fs.shutdown_writable().unwrap();
    drop(fs);
    fsck(path);

    let fs = Ext4::load_writable(Arc::new(BlockFile::new(path))).unwrap();
    let mut bytes = vec![0; target.len()];
    assert_eq!(
        fs.readlink(symlink.ino, 0, &mut bytes).unwrap(),
        target.len()
    );
    assert_eq!(bytes, target.as_bytes());
    assert_eq!(fs.getattr(device.ino).unwrap().rdev, (259, 1234));
    assert_eq!(fs.getxattr(file, "user.other").unwrap(), b"retained");
    fs.removexattr(file, "user.other").unwrap();
    assert_eq!(fs.getattr(file).unwrap().blocks, 8);
    fs.shutdown_writable().unwrap();
    drop(fs);
    fsck(path);
    std::fs::remove_file(path).unwrap();
    println!(
        "namespace transactions {} passed",
        if journal { "journal" } else { "nojournal" }
    );
}

fn crash_matrix() {
    use block_file::{CrashBlockFile, SimulatedPowerLoss};
    use std::panic::{catch_unwind, AssertUnwindSafe};
    const SEED: &str = "namespace-crash-seed.img";
    const WORK: &str = "namespace-crash-work.img";
    File::create(SEED)
        .unwrap()
        .set_len(32 * 1024 * 1024)
        .unwrap();
    assert!(Command::new("mkfs.ext4")
        .args([
            "-q",
            "-F",
            "-b",
            "4096",
            "-I",
            "256",
            "-O",
            "^orphan_file",
            SEED
        ])
        .status()
        .unwrap()
        .success());
    let fs = Ext4::load_writable(Arc::new(BlockFile::new(SEED))).unwrap();
    let a = fs.mkdir(EXT4_ROOT_INO, "a", InodeMode::ALL_RWX).unwrap();
    let b = fs.mkdir(EXT4_ROOT_INO, "b", InodeMode::ALL_RWX).unwrap();
    let left = fs.mkdir(a, "entry", InodeMode::ALL_RWX).unwrap();
    let right = fs
        .create(b, "entry", InodeMode::FILE | InodeMode::ALL_RW)
        .unwrap();
    fs.shutdown_writable().unwrap();
    drop(fs);
    let perform = |fs: &Ext4, case| match case {
        0 => {
            fs.mkdir(a, "new", InodeMode::ALL_RWX).unwrap();
        }
        1 => {
            fs.symlink_with_owner_and_attr(
                a,
                "new",
                &"target/".repeat(100),
                InodeOwner { uid: 0, gid: 0 },
            )
            .unwrap();
        }
        2 => {
            fs.rename_exchange(a, "entry", b, "entry").unwrap();
        }
        _ => unreachable!(),
    };
    for write_through in [false, true] {
        for case in 0..3 {
            std::fs::copy(SEED, WORK).unwrap();
            let device = Arc::new(CrashBlockFile::new(WORK));
            let fs = Ext4::load_writable(device.clone()).unwrap();
            device.set_write_through(write_through);
            device.reset_operation_log();
            perform(&fs, case);
            let points = device.operations().len();
            drop(fs);
            drop(device);
            for point in 0..points {
                std::fs::copy(SEED, WORK).unwrap();
                let device = Arc::new(CrashBlockFile::new(WORK));
                let fs = Ext4::load_writable(device.clone()).unwrap();
                device.set_write_through(write_through);
                device.reset_operation_log();
                device.arm_power_loss_at(point);
                // Namespace operations own no delayed-allocation linear lease;
                // unwinding only discards private images and releases guards.
                let hook = std::panic::take_hook();
                std::panic::set_hook(Box::new(|_| {}));
                let result = catch_unwind(AssertUnwindSafe(|| perform(&fs, case)));
                std::panic::set_hook(hook);
                let payload = result.expect_err("did not reach armed persistence point");
                assert!(payload.downcast_ref::<SimulatedPowerLoss>().is_some());
                device.crash();
                drop(fs);
                drop(device);
                let recovered = Ext4::load_writable(Arc::new(BlockFile::new(WORK))).unwrap();
                if case == 2 {
                    let x = recovered.lookup(a, "entry").unwrap();
                    let y = recovered.lookup(b, "entry").unwrap();
                    assert!(
                        (x == left && y == right) || (x == right && y == left),
                        "partial exchange at {point}"
                    );
                    let moved = x == right;
                    assert_eq!(
                        recovered.lookup(left, "..").unwrap(),
                        if moved { b } else { a }
                    );
                    assert_eq!(
                        recovered.getattr(a).unwrap().links,
                        if moved { 2 } else { 3 }
                    );
                    assert_eq!(
                        recovered.getattr(b).unwrap().links,
                        if moved { 3 } else { 2 }
                    );
                } else {
                    match recovered.lookup(a, "new") {
                        Err(error) => assert_eq!(error.code(), ErrCode::ENOENT),
                        Ok(inode) if case == 0 => {
                            assert_eq!(recovered.lookup(inode, ".").unwrap(), inode);
                            assert_eq!(recovered.lookup(inode, "..").unwrap(), a);
                        }
                        Ok(inode) => {
                            let expected = "target/".repeat(100);
                            let mut bytes = vec![0; expected.len()];
                            assert_eq!(
                                recovered.readlink(inode, 0, &mut bytes).unwrap(),
                                bytes.len()
                            );
                            assert_eq!(bytes, expected.as_bytes());
                        }
                    }
                }
                recovered.shutdown_writable().unwrap();
                drop(recovered);
                fsck(WORK);
            }
            println!(
                "namespace crash case {case} write_through={write_through}: {points} points passed"
            );
        }
    }
    std::fs::remove_file(SEED).unwrap();
    std::fs::remove_file(WORK).unwrap();
}

fn main() {
    if std::env::args().any(|arg| arg == "--crash-only") {
        crash_matrix();
        return;
    }
    run(true);
    run(false);
    crash_matrix();
}
