//! End-to-end real-image coverage for the bounded live metadata view.
#[path = "../block_file.rs"]
#[allow(dead_code)]
mod block_file;
use another_ext4::{ErrCode, Ext4, Ext4Error, InodeMode, InodeReclaimHandle, EXT4_ROOT_INO};
use block_file::{BlockFile, CrashBlockFile, CrashDeviceOperation};
use std::{fs::File, process::Command, sync::Arc};

fn progress(fs: &Ext4) {
    fs.request_batch_commit();
    assert!(
        fs.commit_pending_batch().unwrap().is_some(),
        "EAGAIN must have an independently committable batch"
    );
}
fn retry<T>(fs: &Ext4, mut operation: impl FnMut() -> Result<T, Ext4Error>) -> T {
    for _ in 0..1024 {
        match operation() {
            Ok(value) => return value,
            Err(error) if error.code() == ErrCode::EAGAIN => progress(fs),
            Err(error) => panic!("operation failed: {error:?}"),
        }
    }
    panic!("operation cannot make bounded progress");
}
fn reclaim(fs: &Ext4, mut handle: InodeReclaimHandle) {
    for _ in 0..1024 {
        match fs.reclaim_inode(handle) {
            Ok(()) => return,
            Err(failure) => {
                let (error, returned) = failure.into_parts();
                handle = returned;
                assert_eq!(error.code(), ErrCode::EAGAIN);
                progress(fs);
            }
        }
    }
    panic!("reclaim cannot make bounded progress");
}
fn main() {
    const PATH: &str = "batched-filesystem.img";
    File::create(PATH)
        .unwrap()
        .set_len(64 * 1024 * 1024)
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
            PATH
        ])
        .status()
        .unwrap()
        .success());
    let device = Arc::new(CrashBlockFile::new(PATH));
    let mut fs = Ext4::load_writable(device.clone()).unwrap();
    fs.enable_batching(256).unwrap();
    device.reset_operation_log();
    let directory = retry(&fs, || {
        fs.mkdir(EXT4_ROOT_INO, "working", InodeMode::ALL_RWX)
    });
    let other = retry(&fs, || {
        fs.mkdir(EXT4_ROOT_INO, "destination", InodeMode::ALL_RWX)
    });
    // Reclaim must detach the xattr reference and its i_blocks charge in the
    // same transaction, including inode formats without an extent tree.
    for kind in 0..3 {
        let name = format!("xattr-reclaim-{kind}");
        let inode = match kind {
            0 => retry(&fs, || {
                fs.create(directory, &name, InodeMode::FILE | InodeMode::ALL_RW)
            }),
            1 => {
                retry(&fs, || {
                    fs.symlink_with_owner_and_attr(
                        directory,
                        &name,
                        "abc",
                        another_ext4::InodeOwner { uid: 0, gid: 0 },
                    )
                })
                .ino
            }
            _ => {
                retry(&fs, || {
                    fs.mknod_with_owner_and_attr(
                        directory,
                        &name,
                        InodeMode::CHARDEV | InodeMode::ALL_RW,
                        1,
                        3,
                        another_ext4::InodeOwner { uid: 0, gid: 0 },
                    )
                })
                .ino
            }
        };
        retry(&fs, || fs.setxattr(inode, "user.retained", b"value"));
        assert_eq!(retry(&fs, || fs.getattr(inode)).blocks, 8);
        let handle = retry(&fs, || fs.unlink(directory, &name)).unwrap();
        reclaim(&fs, handle);
        println!("xattr reclaim kind {kind} passed");
    }
    let mut files = Vec::new();
    for index in 0..400 {
        let name = format!("{index:04}-{}", "name".repeat(32));
        let file = retry(&fs, || {
            fs.create(directory, &name, InodeMode::FILE | InodeMode::ALL_RW)
        });
        let payload = format!("file {index}").into_bytes();
        assert_eq!(retry(&fs, || fs.write(file, 0, &payload)), payload.len());
        if index % 32 == 0 {
            assert_eq!(retry(&fs, || fs.write(file, 3 * 4096, b"tail")), 4);
            retry(&fs, || fs.setxattr(file, "user.tag", b"stored"));
            assert_eq!(retry(&fs, || fs.getxattr(file, "user.tag")), b"stored");
            retry(&fs, || fs.removexattr(file, "user.tag"));
        }
        assert_eq!(
            retry(&fs, || fs.lookup(directory, &name)),
            file,
            "read-your-writes before checkpoint"
        );
        files.push((name, file, payload));
    }
    retry(&fs, || fs.rename(directory, &files[0].0, other, "moved"));
    retry(&fs, || {
        fs.rename_exchange(directory, &files[1].0, other, "moved")
    });
    for index in (2..400).step_by(2) {
        let handle = retry(&fs, || fs.unlink(directory, &files[index].0)).unwrap();
        reclaim(&fs, handle);
    }
    // Reuse retired capacity in later operations. No explicit fsync is issued;
    // bounded admission and retirement must request the necessary checkpoint.
    for index in 0..220 {
        let name = format!("reused-{index}");
        let file = retry(&fs, || {
            fs.create(other, &name, InodeMode::FILE | InodeMode::ALL_RW)
        });
        retry(&fs, || fs.write(file, 0, &[0x71; 4096]));
    }
    while fs.batch_progress().unwrap().durable < fs.batch_progress().unwrap().accepted {
        progress(&fs);
    }
    let count = device
        .operations()
        .iter()
        .filter(|operation| **operation == CrashDeviceOperation::Flush)
        .count();
    fs.shutdown_writable().unwrap();
    drop(fs);
    drop(device);
    let fs = Ext4::load_writable(Arc::new(BlockFile::new(PATH))).unwrap();
    assert_eq!(fs.lookup(other, "moved").unwrap(), files[1].1);
    assert_eq!(fs.lookup(directory, &files[1].0).unwrap(), files[0].1);
    for index in (3..400).step_by(2) {
        let mut bytes = vec![0; files[index].2.len()];
        assert_eq!(fs.read(files[index].1, 0, &mut bytes).unwrap(), bytes.len());
        assert_eq!(bytes, files[index].2);
    }
    let mut gap = vec![0xff; 3 * 4096 - files[0].2.len()];
    assert_eq!(
        fs.read(files[0].1, files[0].2.len(), &mut gap).unwrap(),
        gap.len()
    );
    assert!(gap.iter().all(|byte| *byte == 0));
    fs.shutdown_writable().unwrap();
    drop(fs);
    let output = Command::new("e2fsck").args(["-fn", PATH]).output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::remove_file(PATH).unwrap();
    println!("real batched namespace/eager/xattr/reclaim passed; observed {count} flushes");
}
