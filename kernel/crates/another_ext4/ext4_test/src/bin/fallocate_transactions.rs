//! Real-image Journal/Batched physical-range regression coverage.
//! Run in a disposable directory; never mutates a user filesystem image.
#[path = "../block_file.rs"]
#[allow(dead_code)]
mod block_file;
use another_ext4::{
    BlockDevice, ErrCode, ExistingBlockImageOutcome, Ext4, Ext4Error, InodeMode, SetAttr,
    BLOCK_SIZE, EXT4_ROOT_INO,
};
use block_file::{CrashBlockFile, CrashDeviceOperation, SimulatedPowerLoss};
use std::{fs::File, process::Command, sync::Arc};

fn retry<T>(fs: &Ext4, mut operation: impl FnMut() -> Result<T, Ext4Error>) -> T {
    for _ in 0..4096 {
        match operation() {
            Ok(result) => return result,
            Err(error) if error.code() == ErrCode::EAGAIN => {
                fs.request_batch_commit();
                assert!(fs.commit_pending_batch().unwrap().is_some());
            }
            Err(error) => panic!("range operation failed: {error:?}"),
        }
    }
    panic!("range operation failed to make bounded progress");
}

fn run(batch: bool) {
    let path = if batch {
        "fallocate-batched.img"
    } else {
        "fallocate-journal.img"
    };
    File::create(path)
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
            path
        ])
        .status()
        .unwrap()
        .success());
    let device = Arc::new(CrashBlockFile::new(path));
    let mut fs = Ext4::load_writable(device.clone()).unwrap();
    if batch {
        fs.enable_batching(256).unwrap();
    }
    let file = retry(&fs, || {
        fs.create(
            EXT4_ROOT_INO,
            "allocated",
            InodeMode::FILE | InodeMode::ALL_RW,
        )
    });
    let end = 4 * BLOCK_SIZE + 17;
    let mut cursor = 11;
    while cursor < end {
        let next = retry(&fs, || {
            fs.preallocate_range_batch(file, cursor, end - cursor)
        })
        .next_offset;
        assert!(cursor < next && next <= end);
        cursor = next;
    }
    let attr = fs.getattr(file).unwrap();
    assert_eq!(attr.size, 0, "physical preparation never owns i_size");
    assert_eq!(attr.blocks, 5 * 8, "real blocks, not sparse resize");
    let before = attr.blocks;
    assert_eq!(
        retry(&fs, || fs.preallocate_range_batch(file, 11, end - 11)).next_offset,
        end
    );
    assert_eq!(fs.getattr(file).unwrap().blocks, before);
    retry(&fs, || {
        fs.setattr(
            file,
            SetAttr {
                size: Some(end as u64),
                ..Default::default()
            },
        )
    });
    let mut contents = vec![0xff; end];
    assert_eq!(fs.read(file, 0, &mut contents).unwrap(), end);
    assert!(contents.iter().all(|byte| *byte == 0));
    device.reset_operation_log();
    assert_eq!(
        retry(&fs, || fs.write_existing_block_image(
            file,
            2,
            &[0xff; BLOCK_SIZE]
        )),
        ExistingBlockImageOutcome::LogicallyZero
    );
    assert!(
        device.operations().is_empty(),
        "unwritten image bridge must perform no physical I/O"
    );
    // Convert only the middle block of a merged unwritten allocation. Both
    // neighbors must stay logically zero and keep their physical allocation.
    retry(&fs, || fs.write(file, 2 * BLOCK_SIZE + 123, b"middle"));
    contents.fill(0xff);
    fs.read(file, 0, &mut contents).unwrap();
    assert!(contents[..2 * BLOCK_SIZE + 123]
        .iter()
        .all(|byte| *byte == 0));
    assert_eq!(
        &contents[2 * BLOCK_SIZE + 123..2 * BLOCK_SIZE + 129],
        b"middle"
    );
    assert!(contents[2 * BLOCK_SIZE + 129..]
        .iter()
        .all(|byte| *byte == 0));
    assert_eq!(fs.getattr(file).unwrap().blocks, before);
    assert_eq!(
        retry(&fs, || fs.write_existing_block_image(
            file,
            1,
            &[0xff; BLOCK_SIZE]
        )),
        ExistingBlockImageOutcome::LogicallyZero
    );
    assert_eq!(
        retry(&fs, || fs.write_existing_block_image(
            file,
            3,
            &[0xff; BLOCK_SIZE]
        )),
        ExistingBlockImageOutcome::LogicallyZero
    );
    retry(&fs, || fs.write(file, 0, &vec![0x51; end]));
    let metadata = fs.getattr(file).unwrap();
    assert_eq!(
        retry(&fs, || fs.write_existing_block_image(
            file,
            1,
            &[0x63; BLOCK_SIZE]
        )),
        ExistingBlockImageOutcome::Written
    );
    assert_eq!(fs.getattr(file).unwrap().size, metadata.size);
    assert_eq!(fs.getattr(file).unwrap().blocks, metadata.blocks);
    device.reset_operation_log();
    device.arm_io_error_at(1);
    assert_eq!(
        fs.write_existing_block_image(file, 1, &[0x63; BLOCK_SIZE])
            .unwrap_err()
            .code(),
        ErrCode::EIO
    );
    device.disarm_io_error();
    assert_eq!(
        fs.getattr(file).unwrap().size,
        metadata.size,
        "failed payload flush cannot publish SIZE"
    );
    assert_eq!(fs.getattr(file).unwrap().blocks, metadata.blocks);
    device.flush().unwrap();
    device.reset_operation_log();
    assert_eq!(
        retry(&fs, || fs.write_existing_block_image(
            file,
            99,
            &[0xff; BLOCK_SIZE]
        )),
        ExistingBlockImageOutcome::LogicallyZero
    );
    assert!(
        device.operations().is_empty(),
        "hole image bridge must perform no physical I/O"
    );
    device.reset_operation_log();
    retry(&fs, || {
        fs.write_existing_block_image(file, 2, &[0x51; BLOCK_SIZE])
    });
    let retired_home = device
        .operations()
        .into_iter()
        .find_map(|operation| match operation {
            CrashDeviceOperation::Write(home) => Some(home),
            CrashDeviceOperation::Flush => None,
        })
        .unwrap();
    retry(&fs, || fs.punch_block_range(file, 1, 4));
    assert_eq!(fs.getattr(file).unwrap().size, end as u64);
    assert_eq!(fs.getattr(file).unwrap().blocks, 2 * 8);
    contents.fill(0xff);
    fs.read(file, 0, &mut contents).unwrap();
    assert!(contents[..BLOCK_SIZE].iter().all(|byte| *byte == 0x51));
    assert!(contents[BLOCK_SIZE..4 * BLOCK_SIZE]
        .iter()
        .all(|byte| *byte == 0));
    assert!(contents[4 * BLOCK_SIZE..].iter().all(|byte| *byte == 0x51));
    if batch {
        assert!(fs.batch_progress().unwrap().running_retirements > 0);
        // An accepted free is visible in the bitmap, but cannot be reused
        // before checkpoint/tail completion. Observe real data home writes.
        let probe = retry(&fs, || {
            fs.create(
                EXT4_ROOT_INO,
                "quarantine",
                InodeMode::FILE | InodeMode::ALL_RW,
            )
        });
        device.reset_operation_log();
        retry(&fs, || fs.preallocate_range_batch(probe, 0, BLOCK_SIZE));
        let allocated_home = device
            .operations()
            .into_iter()
            .find_map(|operation| match operation {
                CrashDeviceOperation::Write(home) => Some(home),
                CrashDeviceOperation::Flush => None,
            })
            .unwrap();
        assert_ne!(
            allocated_home, retired_home,
            "accepted retirement cannot be reused early"
        );
    }
    retry(&fs, || fs.punch_block_range(file, 1, 4));
    assert_eq!(fs.getattr(file).unwrap().blocks, 2 * 8);

    // Logical gaps prevent extent merging and force root/leaf splitting.
    let tree = retry(&fs, || {
        fs.create(EXT4_ROOT_INO, "deep", InodeMode::FILE | InodeMode::ALL_RW)
    });
    for logical in (0..3000usize).step_by(2) {
        retry(&fs, || {
            fs.preallocate_range_batch(tree, logical * BLOCK_SIZE, BLOCK_SIZE)
        });
    }
    assert!(
        fs.getattr(tree).unwrap().blocks > 1500 * 8,
        "external tree nodes are accounted"
    );
    retry(&fs, || fs.punch_block_range(tree, 101, 2499));
    retry(&fs, || fs.punch_block_range(tree, 0, 3000));
    assert_eq!(
        fs.getattr(tree).unwrap().blocks,
        0,
        "all empty subtrees are detached"
    );
    retry(&fs, || {
        fs.preallocate_range_batch(tree, 19 * BLOCK_SIZE, BLOCK_SIZE)
    });
    assert_eq!(
        fs.getattr(tree).unwrap().blocks,
        8,
        "empty root returns to inline leaf"
    );

    // Payload failure before publication cannot leak allocation metadata.
    if batch {
        fs.request_batch_commit();
        while fs.commit_pending_batch().unwrap().is_some() {}
    }
    let failed = retry(&fs, || {
        fs.create(EXT4_ROOT_INO, "failed", InodeMode::FILE | InodeMode::ALL_RW)
    });
    if batch {
        fs.request_batch_commit();
        while fs.commit_pending_batch().unwrap().is_some() {}
    }
    device.reset_operation_log();
    device.arm_io_error_at(0);
    assert_eq!(
        fs.preallocate_range_batch(failed, 0, BLOCK_SIZE)
            .unwrap_err()
            .code(),
        ErrCode::EIO
    );
    device.disarm_io_error();
    assert_eq!(fs.getattr(failed).unwrap().blocks, 0);
    retry(&fs, || fs.preallocate_range_batch(failed, 0, BLOCK_SIZE));
    if batch {
        fs.request_batch_commit();
        while fs.commit_pending_batch().unwrap().is_some() {}
    }
    fs.shutdown_writable().unwrap();
    drop(fs);
    let check = Command::new("e2fsck").args(["-fn", path]).output().unwrap();
    assert!(
        check.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );
    println!(
        "{} physical-range regression passed",
        if batch { "Batched" } else { "Journal" }
    );
}
fn drain(fs: &Ext4) {
    fs.request_batch_commit();
    while fs.commit_pending_batch().unwrap().is_some() {}
}

fn truncate_functional(batch: bool) {
    let mode = if batch { "batched" } else { "journal" };
    let path = format!("truncate-functional-{mode}.img");
    File::create(&path)
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
            &path
        ])
        .status()
        .unwrap()
        .success());
    let mut fs = Ext4::load_writable(Arc::new(CrashBlockFile::new(&path))).unwrap();
    if batch {
        fs.enable_batching(256).unwrap();
    }
    let file = retry(&fs, || {
        fs.create(
            EXT4_ROOT_INO,
            "truncate",
            InodeMode::FILE | InodeMode::ALL_RW,
        )
    });
    retry(&fs, || fs.write(file, 0, &vec![0x51; 3 * BLOCK_SIZE]));
    for index in 3..9 {
        retry(&fs, || {
            fs.preallocate_range_batch(file, index * BLOCK_SIZE, BLOCK_SIZE)
        });
    }
    retry(&fs, || {
        fs.setattr(
            file,
            SetAttr {
                size: Some((3 * BLOCK_SIZE) as u64),
                ..Default::default()
            },
        )
    });
    assert_eq!(
        fs.getattr(file).unwrap().blocks,
        24,
        "same SIZE releases KEEP beyond EOF"
    );
    retry(&fs, || {
        fs.setattr(
            file,
            SetAttr {
                size: Some(37),
                ..Default::default()
            },
        )
    });
    assert_eq!(fs.getattr(file).unwrap().blocks, 8);
    retry(&fs, || {
        fs.setattr(
            file,
            SetAttr {
                size: Some((3 * BLOCK_SIZE) as u64),
                ..Default::default()
            },
        )
    });
    let mut contents = vec![0xff; 3 * BLOCK_SIZE];
    fs.read(file, 0, &mut contents).unwrap();
    assert!(contents[..37].iter().all(|byte| *byte == 0x51));
    assert!(contents[37..].iter().all(|byte| *byte == 0));
    retry(&fs, || {
        fs.setattr(
            file,
            SetAttr {
                size: Some(0),
                ..Default::default()
            },
        )
    });
    assert_eq!(fs.getattr(file).unwrap().blocks, 0);
    // An upper dirty append has stable visible EOF100 while durable SIZE37.
    // Its image bridge must not be undone by re-zeroing from durable EOF37.
    retry(&fs, || fs.write(file, 0, &[0x51; 37]));
    let mut image = [0; BLOCK_SIZE];
    image[..37].fill(0x51);
    image[37..100].fill(0x63);
    retry(&fs, || fs.write_existing_block_image(file, 0, &image));
    let mut growth = retry(&fs, || {
        fs.begin_size_change(
            file,
            100,
            &SetAttr {
                size: Some((2 * BLOCK_SIZE + 17) as u64),
                ..Default::default()
            },
        )
    });
    retry(&fs, || fs.finish_size_change(&mut growth, None));
    drop(growth);
    let mut contents = vec![0xff; 2 * BLOCK_SIZE + 17];
    fs.read(file, 0, &mut contents).unwrap();
    assert_eq!(&contents[..100], &image[..100]);
    assert!(contents[100..].iter().all(|byte| *byte == 0));
    let (temporary, _reclaim) = retry(&fs, || {
        fs.tmpfile_with_owner_and_attr(
            EXT4_ROOT_INO,
            InodeMode::FILE | InodeMode::ALL_RW,
            another_ext4::InodeOwner { uid: 0, gid: 0 },
        )
    });
    retry(&fs, || {
        fs.write(temporary.ino, 0, &vec![0x71; 3 * BLOCK_SIZE])
    });
    retry(&fs, || {
        fs.setattr(
            temporary.ino,
            SetAttr {
                size: Some(37),
                ..Default::default()
            },
        )
    });
    retry(&fs, || fs.link(temporary.ino, EXT4_ROOT_INO, "relinked"));
    assert_eq!(fs.getattr(temporary.ino).unwrap().links, 1);
    retry(&fs, || {
        fs.setattr(
            temporary.ino,
            SetAttr {
                size: Some((3 * BLOCK_SIZE) as u64),
                ..Default::default()
            },
        )
    });
    contents.resize(3 * BLOCK_SIZE, 0xff);
    fs.read(temporary.ino, 0, &mut contents).unwrap();
    assert!(contents[..37].iter().all(|byte| *byte == 0x71));
    assert!(contents[37..].iter().all(|byte| *byte == 0));
    // Two enrolled lifetimes exercise non-head incoming-link merge.
    let second = retry(&fs, || {
        fs.create(EXT4_ROOT_INO, "second", InodeMode::FILE | InodeMode::ALL_RW)
    });
    retry(&fs, || fs.write(second, 0, &[0x31; 100]));
    let mut first_receipt = retry(&fs, || {
        fs.begin_size_change(
            file,
            (2 * BLOCK_SIZE + 17) as u64,
            &SetAttr {
                size: Some(37),
                ..Default::default()
            },
        )
    });
    let mut second_receipt = retry(&fs, || {
        fs.begin_size_change(
            second,
            100,
            &SetAttr {
                size: Some(19),
                ..Default::default()
            },
        )
    });
    retry(&fs, || fs.finish_size_change(&mut first_receipt, None));
    retry(&fs, || fs.finish_size_change(&mut second_receipt, None));
    drop(first_receipt);
    drop(second_receipt);
    drain(&fs);
    fs.shutdown_writable().unwrap();
    drop(fs);
    // Public split phases require upper lifecycle exclusion. Even a raw
    // caller violating that contract must be rejected before tail/free I/O.
    let mut fs = Ext4::load_writable(Arc::new(CrashBlockFile::new(&path))).unwrap();
    if batch {
        fs.enable_batching(256).unwrap();
    }
    let file = fs.lookup(EXT4_ROOT_INO, "truncate").unwrap();
    let previous = fs.getattr(file).unwrap().size;
    let mut abandoned = retry(&fs, || {
        fs.begin_size_change(
            file,
            previous,
            &SetAttr {
                size: Some(17),
                ..Default::default()
            },
        )
    });
    retry(&fs, || fs.link(file, EXT4_ROOT_INO, "changed-role"));
    let before = fs.getattr(file).unwrap().blocks;
    assert_eq!(
        fs.finish_size_change(&mut abandoned, None)
            .unwrap_err()
            .code(),
        ErrCode::EIO
    );
    assert_eq!(
        fs.getattr(file).unwrap().blocks,
        before,
        "role mismatch must precede range release"
    );
    drop(abandoned);
    assert_eq!(
        fs.write(file, 0, b"x").unwrap_err().code(),
        ErrCode::EIO,
        "unfinished receipt uses production fail-stop"
    );
    drop(fs);
    let fs = Ext4::load_writable(Arc::new(CrashBlockFile::new(&path))).unwrap();
    let file = fs.lookup(EXT4_ROOT_INO, "truncate").unwrap();
    let mut prefix = [0; 17];
    fs.read(file, 0, &mut prefix).unwrap();
    assert_eq!(prefix, [0x51; 17]);
    fs.shutdown_writable().unwrap();
    drop(fs);
    assert!(Command::new("e2fsck")
        .args(["-fn", &path])
        .output()
        .unwrap()
        .status
        .success());
    println!("{mode} truncate zero/same SIZE/dirty EOF bridge/zero-link/nonhead passed");
}

fn fault_operation(fs: &Ext4, file: u32, operation: &str) {
    match operation {
        "preallocate" => {
            retry(fs, || fs.preallocate_range_batch(file, 0, BLOCK_SIZE));
        }
        "punch" => {
            retry(fs, || fs.punch_block_range(file, 1, 2));
        }
        "convert" | "extend" => {
            retry(fs, || fs.write(file, BLOCK_SIZE + 123, b"conversion"));
        }
        "truncate" | "truncate_zero" | "same_size" => {
            let size = match operation {
                "truncate" => 37,
                "truncate_zero" => 0,
                _ => 3 * BLOCK_SIZE,
            };
            retry(fs, || {
                fs.setattr(
                    file,
                    SetAttr {
                        size: Some(size as u64),
                        ..Default::default()
                    },
                )
            });
        }
        _ => panic!("unknown operation"),
    }
    drain(fs);
}

fn space_pressure(batch: bool) {
    let mode = if batch { "batched" } else { "journal" };
    let path = format!("space-{mode}.img");
    File::create(&path)
        .unwrap()
        .set_len(8 * 1024 * 1024)
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
            &path
        ])
        .status()
        .unwrap()
        .success());
    let mut fs = Ext4::load_writable(Arc::new(CrashBlockFile::new(&path))).unwrap();
    if batch {
        fs.enable_batching(256).unwrap();
    }
    let file = retry(&fs, || {
        fs.create(EXT4_ROOT_INO, "full", InodeMode::FILE | InodeMode::ALL_RW)
    });
    let mut cursor = 0;
    loop {
        assert!(
            cursor < 2048 * BLOCK_SIZE,
            "bounded fixture must reach real ENOSPC"
        );
        match fs.preallocate_range_batch(file, cursor, BLOCK_SIZE) {
            Ok(progress) => {
                assert_eq!(progress.next_offset, cursor + BLOCK_SIZE);
                cursor = progress.next_offset;
            }
            Err(error) if error.code() == ErrCode::EAGAIN => drain(&fs),
            Err(error) if error.code() == ErrCode::ENOSPC => break,
            Err(error) => panic!("unexpected space pressure result: {error:?}"),
        }
    }
    assert!(cursor > 0);
    let before = fs.getattr(file).unwrap();
    assert_eq!(before.size, 0);
    assert!(before.blocks >= (cursor / BLOCK_SIZE * 8) as u64);
    drain(&fs);
    assert_eq!(
        fs.preallocate_range_batch(file, cursor, BLOCK_SIZE)
            .unwrap_err()
            .code(),
        ErrCode::ENOSPC
    );
    assert_eq!(
        fs.getattr(file).unwrap().blocks,
        before.blocks,
        "failed allocation cannot leak staged claims"
    );
    retry(&fs, || {
        fs.punch_block_range(
            file,
            (cursor / BLOCK_SIZE - 1) as u32,
            (cursor / BLOCK_SIZE) as u32,
        )
    });
    drain(&fs);
    assert_eq!(
        retry(&fs, || fs.preallocate_range_batch(file, cursor, BLOCK_SIZE)).next_offset,
        cursor + BLOCK_SIZE
    );
    drain(&fs);
    fs.shutdown_writable().unwrap();
    drop(fs);
    let check = Command::new("e2fsck")
        .args(["-fn", &path])
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );
    println!(
        "{mode} bounded ENOSPC/progress/reclaim passed after {} allocated blocks",
        cursor / BLOCK_SIZE
    );
}

fn check_fault_image(path: &str, operation: &str) {
    let fs = Ext4::load_writable(Arc::new(CrashBlockFile::new(path))).unwrap();
    let file = fs.lookup(EXT4_ROOT_INO, "fault").unwrap();
    let attr = fs.getattr(file).unwrap();
    match operation {
        "truncate" | "truncate_zero" | "same_size" => {
            let size = match operation {
                "truncate" => 37,
                "truncate_zero" => 0,
                _ => 3 * BLOCK_SIZE,
            };
            assert!(attr.size == (3 * BLOCK_SIZE) as u64 || attr.size == size as u64);
            if attr.size == size as u64 && (size != 3 * BLOCK_SIZE || attr.blocks == 24) {
                assert_eq!(attr.blocks, size.div_ceil(BLOCK_SIZE) as u64 * 8);
                let mut prefix = vec![0xff; size];
                fs.read(file, 0, &mut prefix).unwrap();
                assert!(prefix.iter().all(|byte| *byte == 0x51));
                fs.setattr(
                    file,
                    SetAttr {
                        size: Some((3 * BLOCK_SIZE) as u64),
                        ..Default::default()
                    },
                )
                .unwrap();
                let mut contents = vec![0xff; 3 * BLOCK_SIZE];
                fs.read(file, 0, &mut contents).unwrap();
                assert!(contents[..size].iter().all(|byte| *byte == 0x51));
                assert!(contents[size..].iter().all(|byte| *byte == 0));
            } else {
                assert_eq!(
                    attr.blocks, 48,
                    "unpublished SIZE/cleanup preserves old allocation"
                );
                let mut contents = vec![0xff; 3 * BLOCK_SIZE];
                fs.read(file, 0, &mut contents).unwrap();
                if operation == "truncate" {
                    // Linux journal_stop does not force SIZE durability.
                    // An un-fsynced shrink may zero its deleted suffix before
                    // the accepted SIZE/orphan images reach the journal.
                    assert!(contents[..37].iter().all(|byte| *byte == 0x51));
                    assert!(contents[BLOCK_SIZE..].iter().all(|byte| *byte == 0x51));
                    let deleted = &contents[37..BLOCK_SIZE];
                    assert!(
                        deleted.iter().all(|byte| *byte == 0x51)
                            || deleted.iter().all(|byte| *byte == 0)
                    );
                } else {
                    assert!(contents.iter().all(|byte| *byte == 0x51));
                }
            }
        }
        "preallocate" => {
            assert_eq!(attr.size, 0);
            assert!(attr.blocks == 0 || attr.blocks == 8);
        }
        "extend" => {
            assert!(attr.size == 0 || attr.size == (BLOCK_SIZE + 133) as u64);
            // Interrupted orphan cleanup may release all allocation beyond
            // the recovered EOF; allocated unreachable data must never leak.
            if attr.size != 0 {
                let mut contents = vec![0xff; attr.size as usize];
                fs.read(file, 0, &mut contents).unwrap();
                assert!(contents[..BLOCK_SIZE + 123].iter().all(|byte| *byte == 0));
                assert_eq!(&contents[BLOCK_SIZE + 123..], b"conversion");
            }
        }
        "punch" | "convert" => {
            assert_eq!(attr.size, (3 * BLOCK_SIZE) as u64);
            let mut contents = vec![0xff; 3 * BLOCK_SIZE];
            assert_eq!(fs.read(file, 0, &mut contents).unwrap(), contents.len());
            if operation == "punch" {
                assert!(attr.blocks == 16 || attr.blocks == 24);
                assert!(contents[..BLOCK_SIZE].iter().all(|byte| *byte == 0x51));
                assert!(contents[2 * BLOCK_SIZE..].iter().all(|byte| *byte == 0x51));
                let middle = &contents[BLOCK_SIZE..2 * BLOCK_SIZE];
                assert!(
                    middle.iter().all(|byte| *byte == 0) || middle.iter().all(|byte| *byte == 0x51)
                );
            } else {
                assert_eq!(attr.blocks, 24);
                assert!(contents[..BLOCK_SIZE + 123].iter().all(|byte| *byte == 0));
                assert!(contents[BLOCK_SIZE + 133..].iter().all(|byte| *byte == 0));
                let middle = &contents[BLOCK_SIZE + 123..BLOCK_SIZE + 133];
                assert!(middle == b"conversion" || middle.iter().all(|byte| *byte == 0));
            }
        }
        _ => unreachable!(),
    }
    fs.shutdown_writable().unwrap();
    let check = Command::new("e2fsck").args(["-fn", path]).output().unwrap();
    assert!(
        check.status.success(),
        "{path}: {}\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );
}

fn fault_matrix(batch: bool, operation: &str) {
    let mode = if batch { "batched" } else { "journal" };
    let baseline = format!("fault-{mode}-{operation}-base.img");
    File::create(&baseline)
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
            &baseline
        ])
        .status()
        .unwrap()
        .success());
    let fs = Ext4::load_writable(Arc::new(CrashBlockFile::new(&baseline))).unwrap();
    let file = fs
        .create(EXT4_ROOT_INO, "fault", InodeMode::FILE | InodeMode::ALL_RW)
        .unwrap();
    if operation != "preallocate" {
        let mut cursor = 0;
        let allocation_end = if matches!(operation, "truncate" | "truncate_zero" | "same_size") {
            6 * BLOCK_SIZE
        } else {
            3 * BLOCK_SIZE
        };
        while cursor < allocation_end {
            cursor = fs
                .preallocate_range_batch(file, cursor, allocation_end - cursor)
                .unwrap()
                .next_offset;
        }
        if operation != "extend" {
            fs.setattr(
                file,
                SetAttr {
                    size: Some((3 * BLOCK_SIZE) as u64),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        if matches!(
            operation,
            "punch" | "truncate" | "truncate_zero" | "same_size"
        ) {
            fs.write(file, 0, &vec![0x51; 3 * BLOCK_SIZE]).unwrap();
        }
    }
    fs.shutdown_writable().unwrap();
    drop(fs);
    let success = format!("fault-{mode}-{operation}-success.img");
    std::fs::copy(&baseline, &success).unwrap();
    let device = Arc::new(CrashBlockFile::new(&success));
    device.set_write_through(true);
    let mut fs = Ext4::load_writable(device.clone()).unwrap();
    if batch {
        fs.enable_batching(256).unwrap();
    }
    device.reset_operation_log();
    fault_operation(&fs, file, operation);
    let count = device.operations().len();
    assert!(count > 0);
    fs.shutdown_writable().unwrap();
    drop(fs);
    check_fault_image(&success, operation);
    for point in 0..count {
        let image = format!("fault-{mode}-{operation}-{point}.img");
        std::fs::copy(&baseline, &image).unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--crash", &image, mode, operation, &point.to_string()])
            .status()
            .unwrap();
        assert_eq!(
            status.code(),
            Some(86),
            "fault point {point} did not terminate at the armed persistence operation"
        );
        check_fault_image(&image, operation);
        // Only verified disposable crash images are removed. A failed image
        // is left intact by the preceding assertion for independent recovery.
        std::fs::remove_file(&image).unwrap();
    }
    println!("{mode} {operation}: {count} write/flush crash boundaries passed");
}

fn main() {
    let arguments: Vec<_> = std::env::args().collect();
    if arguments
        .get(1)
        .is_some_and(|argument| argument == "--truncate-matrix")
    {
        for batch in [false, true] {
            truncate_functional(batch);
            for operation in ["truncate", "truncate_zero", "same_size"] {
                fault_matrix(batch, operation);
            }
        }
        return;
    }
    if arguments
        .get(1)
        .is_some_and(|argument| argument == "--extend-matrix")
    {
        for batch in [false, true] {
            fault_matrix(batch, "extend");
        }
        return;
    }
    if arguments
        .get(1)
        .is_some_and(|argument| argument == "--crash")
    {
        std::panic::set_hook(Box::new(|information| {
            if information.payload().is::<SimulatedPowerLoss>() {
                std::process::exit(86);
            }
            eprintln!("{information}");
        }));
        let device = Arc::new(CrashBlockFile::new(&arguments[2]));
        device.set_write_through(true);
        let mut fs = Ext4::load_writable(device.clone()).unwrap();
        if arguments[3] == "batched" {
            fs.enable_batching(256).unwrap();
        }
        let file = fs.lookup(EXT4_ROOT_INO, "fault").unwrap();
        device.reset_operation_log();
        device.arm_power_loss_at(arguments[5].parse().unwrap());
        fault_operation(&fs, file, &arguments[4]);
        panic!("armed crash boundary was not reached");
    }
    run(false);
    run(true);
    for batch in [false, true] {
        space_pressure(batch);
        for operation in ["preallocate", "punch", "convert"] {
            fault_matrix(batch, operation);
        }
    }
}
