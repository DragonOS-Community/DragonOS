//! Disposable nojournal images: real range operations plus every write/flush
//! I/O-error boundary. Nonjournal leaks after failure are permitted; reachable
//! blocks marked free, lost neighbors and early cache publication are not.
#[path = "../block_file.rs"]
#[allow(dead_code)]
mod block_file;
use another_ext4::{BlockDevice, Ext4, InodeMode, SetAttr, BLOCK_SIZE, EXT4_ROOT_INO};
use block_file::{CrashBlockFile, SimulatedPowerLoss};
use std::{fs, path::Path, process::Command, sync::Arc};

fn mkfs(path: &str) {
    fs::File::create(path)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    assert!(Command::new("mkfs.ext4")
        .args([
            "-q",
            "-F",
            "-b",
            "4096",
            "-g",
            "1024",
            "-I",
            "256",
            "-O",
            "^has_journal,^orphan_file",
            path
        ])
        .status()
        .unwrap()
        .success());
}

fn allocate(fs: &Ext4, file: u32, offset: usize, len: usize) {
    let end = offset + len;
    let mut cursor = offset;
    while cursor < end {
        let next = fs
            .preallocate_range_batch(file, cursor, end - cursor)
            .unwrap()
            .next_offset;
        assert!(next > cursor && next <= end);
        cursor = next;
    }
}

fn fsck(path: &str, clean: bool) {
    let output = Command::new("e2fsck").args(["-fn", path]).output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if clean {
        assert!(output.status.success(), "{text}");
    }
    assert!(matches!(output.status.code(), Some(0 | 4)), "{text}");
    // A failure may leak owned blocks or leave accounting needing repair. It
    // must never expose a reachable block as free or corrupt the COW tree.
    for line in text.lines() {
        assert!(
            !(line.contains("Block bitmap differences:") && line.contains('+')),
            "{text}"
        );
        assert!(
            !(line.contains("Multiply-claimed")
                || line.contains("multiply-claimed")
                || line.contains("invalid extent")
                || line.contains("extent tree could be")
                || line.contains("size is") && line.contains("should be")),
            "{text}"
        );
    }
}

fn assert_reachable_allocated(path: &str) {
    // Inspect the on-disk tree before mounting or repairing it. In particular,
    // e2fsck's EOF cleanup must not hide a reachable-but-free data/tree block.
    let output = Command::new("debugfs")
        .args(["-n", "-R", "blocks /fault", path])
        .output()
        .unwrap();
    assert!(output.status.success());
    let blocks = String::from_utf8(output.stdout).unwrap();
    assert!(
        !blocks.trim().is_empty(),
        "failed to inspect the live fault inode"
    );
    for number in blocks.split_whitespace() {
        let block: u64 = number.parse().unwrap();
        let output = Command::new("debugfs")
            .args(["-n", "-R", &format!("testb {block}"), path])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            format!("Block {block} marked in use")
        );
    }
    fsck(path, false);
}

fn functional(path: &str) {
    mkfs(path);
    let device = Arc::new(CrashBlockFile::new(path));
    let fs = Ext4::load_writable(device.clone()).unwrap();
    let file = fs
        .create(EXT4_ROOT_INO, "range", InodeMode::FILE | InodeMode::ALL_RW)
        .unwrap();
    allocate(&fs, file, 17, 130 * BLOCK_SIZE);
    assert_eq!(fs.getattr(file).unwrap().size, 0);
    assert_eq!(fs.getattr(file).unwrap().blocks, 131 * 8);
    allocate(&fs, file, 17, 130 * BLOCK_SIZE);
    assert_eq!(fs.getattr(file).unwrap().blocks, 131 * 8);
    fs.setattr(
        file,
        SetAttr {
            size: Some((131 * BLOCK_SIZE) as u64),
            ..Default::default()
        },
    )
    .unwrap();
    fs.write(file, 65 * BLOCK_SIZE + 13, b"middle").unwrap();
    let mut contents = vec![0xff; 131 * BLOCK_SIZE];
    fs.read(file, 0, &mut contents).unwrap();
    assert!(contents[..65 * BLOCK_SIZE + 13]
        .iter()
        .all(|byte| *byte == 0));
    assert_eq!(
        &contents[65 * BLOCK_SIZE + 13..65 * BLOCK_SIZE + 19],
        b"middle"
    );
    assert!(contents[65 * BLOCK_SIZE + 19..]
        .iter()
        .all(|byte| *byte == 0));
    fs.punch_block_range(file, 1, 130).unwrap();
    fs.punch_block_range(file, 1, 130).unwrap();
    // Ext4 may retain a sparse external tree after a middle removal; its
    // metadata blocks remain accounted until their last leaf is removed.
    assert!(fs.getattr(file).unwrap().blocks >= 16);
    contents.fill(0xff);
    fs.read(file, 0, &mut contents).unwrap();
    assert!(contents.iter().all(|byte| *byte == 0));
    fs.punch_block_range(file, 0, 131).unwrap();
    assert_eq!(fs.getattr(file).unwrap().blocks, 0);
    let tree = fs
        .create(EXT4_ROOT_INO, "tree", InodeMode::FILE | InodeMode::ALL_RW)
        .unwrap();
    // More than 4 * 340 extents forces a second external level with 4K blocks.
    for logical in (0..3000).step_by(2) {
        allocate(&fs, tree, logical * BLOCK_SIZE, BLOCK_SIZE);
    }
    assert!(fs.getattr(tree).unwrap().blocks > 1500 * 8);
    fs.punch_block_range(tree, 103, 2499).unwrap();
    fs.punch_block_range(tree, 0, 3000).unwrap();
    assert_eq!(fs.getattr(tree).unwrap().blocks, 0);
    // Same-size truncate also discards KEEP_SIZE reservations. Two overlapping
    // short receipts exercise a non-head removal where adjacent inode records
    // share their containing inode-table block.
    let left = fs
        .create(EXT4_ROOT_INO, "left", InodeMode::FILE | InodeMode::ALL_RW)
        .unwrap();
    let right = fs
        .create(EXT4_ROOT_INO, "right", InodeMode::FILE | InodeMode::ALL_RW)
        .unwrap();
    for id in [left, right] {
        fs.write(id, 0, b"prefix").unwrap();
        allocate(&fs, id, 2 * BLOCK_SIZE, 3 * BLOCK_SIZE);
    }
    let attr = SetAttr {
        size: Some(6),
        ..Default::default()
    };
    let mut first = fs.begin_size_change(left, 6, &attr).unwrap();
    let mut second = fs.begin_size_change(right, 6, &attr).unwrap();
    fs.finish_size_change(&mut first, None).unwrap();
    fs.finish_size_change(&mut second, None).unwrap();
    drop((first, second));
    for id in [left, right] {
        assert_eq!(fs.getattr(id).unwrap().blocks, 8);
        let mut data = [0; 6];
        assert_eq!(fs.read(id, 0, &mut data).unwrap(), 6);
        assert_eq!(&data, b"prefix");
        fs.setattr(
            id,
            SetAttr {
                size: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(fs.getattr(id).unwrap().blocks, 0);
    }
    let (temporary, reclaim) = fs
        .tmpfile_with_owner_and_attr(
            EXT4_ROOT_INO,
            InodeMode::FILE | InodeMode::ALL_RW,
            another_ext4::InodeOwner { uid: 0, gid: 0 },
        )
        .unwrap();
    fs.write(temporary.ino, 0, b"temporary").unwrap();
    fs.setattr(
        temporary.ino,
        SetAttr {
            size: Some(0),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(fs.getattr(temporary.ino).unwrap().blocks, 0);
    fs.reclaim_inode(reclaim).unwrap();
    device.flush().unwrap();
    fs.shutdown_writable().unwrap();
    drop(fs);
    fsck(path, true);
    println!("Direct KEEP/unwritten conversion/repeated punch/1500 fragmented extents/multilevel tree collapse: PASS");
}

fn operation(fs: &Ext4, file: u32, kind: &str) -> Result<(), another_ext4::Ext4Error> {
    match kind {
        "allocate" => fs
            .preallocate_range_batch(file, 3 * BLOCK_SIZE, BLOCK_SIZE)
            .map(|_| ()),
        "punch" => fs.punch_block_range(file, 4, 5),
        "convert" | "extend" => fs
            .write(file, 4 * BLOCK_SIZE + 17, b"converted")
            .map(|_| ()),
        "tailgrow" => fs.write(file, 123, b"converted").map(|_| ()),
        "truncate" => fs.setattr(
            file,
            SetAttr {
                size: Some((4 * BLOCK_SIZE + 17) as u64),
                ..Default::default()
            },
        ),
        _ => unreachable!(),
    }
}

fn fault_matrix(base: &str, working: &str, kind: &str) {
    let mut needs_offline_repair = 0;
    mkfs(base);
    let device = Arc::new(CrashBlockFile::new(base));
    let fs = Ext4::load_writable(device.clone()).unwrap();
    let file = fs
        .create(EXT4_ROOT_INO, "fault", InodeMode::FILE | InodeMode::ALL_RW)
        .unwrap();
    for logical in (0..16).step_by(2) {
        allocate(&fs, file, logical * BLOCK_SIZE, BLOCK_SIZE);
    }
    if kind != "extend" {
        fs.setattr(
            file,
            SetAttr {
                size: Some(if kind == "tailgrow" {
                    17
                } else {
                    (16 * BLOCK_SIZE) as u64
                }),
                ..Default::default()
            },
        )
        .unwrap();
    }
    if kind == "tailgrow" {
        fs.write(file, 0, &[0x51; 17]).unwrap();
    } else if kind != "convert" && kind != "extend" {
        for logical in (0..16).step_by(2) {
            fs.write(file, logical * BLOCK_SIZE, &vec![0x51; BLOCK_SIZE])
                .unwrap();
        }
    }
    device.flush().unwrap();
    fs.shutdown_writable().unwrap();
    drop(fs);
    fsck(base, true);
    fs::copy(base, working).unwrap();
    let device = Arc::new(CrashBlockFile::new(working));
    device.set_write_through(true);
    let fs = Ext4::load_writable(device.clone()).unwrap();
    device.reset_operation_log();
    operation(&fs, file, kind).unwrap();
    let operations = device.operations().len();
    if kind == "truncate" {
        eprintln!("truncate persistence sequence: {:?}", device.operations());
    }
    device.flush().unwrap();
    fs.shutdown_writable().unwrap();
    drop(fs);
    fsck(working, true);
    for (point, power_loss) in (0..operations).flat_map(|point| [(point, false), (point, true)]) {
        fs::copy(base, working).unwrap();
        if power_loss {
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--crash",
                    working,
                    kind,
                    &file.to_string(),
                    &point.to_string(),
                ])
                .status()
                .unwrap();
            assert_eq!(
                status.code(),
                Some(86),
                "{kind} point {point} did not crash"
            );
        } else {
            let device = Arc::new(CrashBlockFile::new(working));
            device.set_write_through(true);
            let fs = Ext4::load_writable(device.clone()).unwrap();
            device.reset_operation_log();
            device.arm_io_error_at(point);
            assert!(
                operation(&fs, file, kind).is_err(),
                "{kind} point {point} did not fail"
            );
            device.crash();
            drop(fs);
        }
        assert_reachable_allocated(working);
        let mounted = match Ext4::load_writable(Arc::new(CrashBlockFile::new(working))) {
            Ok(mounted) => mounted,
            Err(error) if kind == "truncate"
                && format!("{error:?}") == "Ext4Error { code: EIO, message: \"Corrupt block bitmap checksum\" }" => {
                // Nojournal bitmap and descriptor homes cannot be atomically
                // persisted. Preserve the failed source, then repair only the
                // disposable working copy; never weaken the kernel check.
                let evidence = format!("{working}.cut-{point}-power-{power_loss}.img");
                fs::copy(working, &evidence).unwrap();
                let repair = Command::new("e2fsck").args(["-fy", working]).output().unwrap();
                assert!(matches!(repair.status.code(), Some(0 | 1)), "{}", String::from_utf8_lossy(&repair.stdout));
                fsck(working, true);
                needs_offline_repair += 1;
                Ext4::load_writable(Arc::new(CrashBlockFile::new(working))).unwrap()
            }
            Err(error) => panic!("{kind} point {point} power_loss={power_loss}: {error:?}"),
        };
        if kind == "truncate" {
            let size = mounted.getattr(file).unwrap().size as usize;
            assert!(size == 16 * BLOCK_SIZE || size == 4 * BLOCK_SIZE + 17);
            let mut bytes = vec![0xff; size];
            assert_eq!(mounted.read(file, 0, &mut bytes).unwrap(), size);
            for (offset, byte) in bytes.iter().enumerate() {
                let expected = if (offset / BLOCK_SIZE).is_multiple_of(2) {
                    0x51
                } else {
                    0
                };
                assert_eq!(*byte, expected, "truncate {point} lost byte {offset}");
            }
            if size < 16 * BLOCK_SIZE {
                // Recovery must clear the initialized partial EOF block too,
                // not merely retire complete blocks above it.
                mounted
                    .setattr(
                        file,
                        SetAttr {
                            size: Some((16 * BLOCK_SIZE) as u64),
                            ..Default::default()
                        },
                    )
                    .unwrap();
                let mut tail = vec![0xff; 16 * BLOCK_SIZE - size];
                assert_eq!(mounted.read(file, size, &mut tail).unwrap(), tail.len());
                assert!(tail.iter().all(|byte| *byte == 0));
            }
            mounted.shutdown_writable().unwrap();
            drop(mounted);
            fsck(working, false);
            continue;
        }
        if kind == "tailgrow" {
            let size = mounted.getattr(file).unwrap().size as usize;
            assert!(
                size == 17 || size == 132,
                "incorrect tail EOF {size} at {point}"
            );
            let mut bytes = vec![0xff; size];
            assert_eq!(mounted.read(file, 0, &mut bytes).unwrap(), size);
            assert!(bytes[..17].iter().all(|byte| *byte == 0x51));
            if size > 17 {
                assert!(bytes[17..123].iter().all(|byte| *byte == 0));
                assert_eq!(&bytes[123..], b"converted");
            }
            drop(mounted);
            fsck(working, false);
            continue;
        }
        if kind == "extend" {
            let size = mounted.getattr(file).unwrap().size as usize;
            assert!(
                size == 0 || size == 4 * BLOCK_SIZE + 26,
                "incorrect completed EOF {size} at {point}"
            );
            let mut bytes = vec![0xff; size];
            assert_eq!(mounted.read(file, 0, &mut bytes).unwrap(), size);
            if size != 0 {
                assert!(bytes[..4 * BLOCK_SIZE + 17].iter().all(|byte| *byte == 0));
                assert_eq!(&bytes[4 * BLOCK_SIZE + 17..], b"converted");
            }
            drop(mounted);
            fsck(working, false);
            continue;
        }
        let mut bytes = vec![0xff; 16 * BLOCK_SIZE];
        mounted.read(file, 0, &mut bytes).unwrap();
        for logical in 0..16 {
            let block = &bytes[logical * BLOCK_SIZE..(logical + 1) * BLOCK_SIZE];
            if logical == 4 && kind == "punch" {
                assert!(block.iter().all(|v| *v == 0) || block.iter().all(|v| *v == 0x51));
            } else if logical == 4 && kind == "convert" {
                assert!(block[..17].iter().all(|v| *v == 0));
                assert!(block[26..].iter().all(|v| *v == 0));
                assert!(block[17..26].iter().all(|v| *v == 0) || &block[17..26] == b"converted");
            } else {
                let expected = if kind != "convert" && logical % 2 == 0 {
                    0x51
                } else {
                    0
                };
                assert!(
                    block.iter().all(|v| *v == expected),
                    "{kind} {point} lost neighbor {logical}"
                );
            }
        }
        drop(mounted);
        fsck(working, false);
    }
    println!("Direct {kind}: {operations} write/flush boundaries x I/O-error and power-loss preserve neighbors and reachable ownership");
    println!("Direct {kind}: {needs_offline_repair} checksum-rejected cuts required offline fsck; failed source images retained");
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).is_some_and(|arg| arg == "--crash") {
        std::panic::set_hook(Box::new(|information| {
            if information.payload().is::<SimulatedPowerLoss>() {
                std::process::exit(86);
            }
            eprintln!("{information}");
        }));
        let device = Arc::new(CrashBlockFile::new(&args[2]));
        device.set_write_through(true);
        let fs = Ext4::load_writable(device.clone()).unwrap();
        device.reset_operation_log();
        device.arm_power_loss_at(args[5].parse().unwrap());
        operation(&fs, args[4].parse().unwrap(), &args[3]).unwrap();
        panic!("armed persistence boundary was not reached");
    }
    // A unique private directory, never an input filesystem or user image.
    let directory = std::env::temp_dir().join(format!(
        "ext4-direct-range-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&directory).unwrap();
    let functional_image = directory.join("functional.img");
    let base = directory.join("base.img");
    let working = directory.join("working.img");
    if args.get(1).is_some_and(|arg| arg == "--matrix") {
        fault_matrix(base.to_str().unwrap(), working.to_str().unwrap(), &args[2]);
    } else {
        functional(functional_image.to_str().unwrap());
        for kind in [
            "allocate", "punch", "convert", "extend", "tailgrow", "truncate",
        ] {
            fault_matrix(base.to_str().unwrap(), working.to_str().unwrap(), kind);
        }
    }
    for path in [&functional_image, &base, &working] {
        assert!(Path::new(path).starts_with(&directory));
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
    if fs::read_dir(&directory).unwrap().next().is_none() {
        fs::remove_dir(directory).unwrap();
    } else {
        println!("Preserved fault evidence: {}", directory.display());
    }
}
