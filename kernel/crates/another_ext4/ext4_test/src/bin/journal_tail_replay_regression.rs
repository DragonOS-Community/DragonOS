//! Host regression: a disk whose JBD2 log tail is non-empty while the ext4
//! superblock no longer carries `EXT4_FEATURE_INCOMPAT_RECOVER`.
//!
//! Such a state is legal on real hardware: the write that clears RECOVER at
//! clean shutdown and the write that empties the journal tail of the final
//! transaction are independent, and a power cut (or host page-cache loss for
//! a file-backed virtio disk) may persist either one without the other.
//! Linux jbd2 replays a non-empty log tail regardless of the RECOVER bit;
//! DragonOS used to fail the mount with EIO, bricking the rootfs.
//!
//! Usage:
//!   journal_tail_replay_regression <image>   mount an existing image
//!                                           writable and report whether
//!                                           journal replay succeeded
//!   journal_tail_replay_regression --craft [image]
//!                                           build the poisoned state on a
//!                                           fresh mkfs image, then prove
//!                                           that a writable mount repairs
//!                                           it and e2fsck accepts it

#[path = "../block_file.rs"]
#[allow(dead_code)]
mod block_file;

use another_ext4::{Ext4, InodeMode, EXT4_ROOT_INO};
use block_file::{CrashBlockFile, CrashDeviceOperation};
use std::process::Command;
use std::sync::Arc;

const ROOT_INO: u32 = EXT4_ROOT_INO;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--craft") => craft_and_repair_regression(
            args.get(2).map(String::as_str).unwrap_or("journal-tail-regression.img"),
        ),
        Some(path) => mount_dirty_image(path),
        _ => {
            eprintln!("usage: {} [--craft] <image>", args[0]);
            std::process::exit(64);
        }
    }
}

/// Mount an existing image writable and report whether journal replay made
/// the mount succeed. This is the direct check against a real dirty disk
/// captured from a crashed system.
fn mount_dirty_image(path: &str) {
    let device = Arc::new(CrashBlockFile::new(path));
    match Ext4::load_writable(device) {
        Ok(fs) => {
            let attr = fs
                .getattr(ROOT_INO)
                .expect("root getattr failed after recovery mount");
            println!(
                "DIRTY-TAIL MOUNT OK: root ino {ROOT_INO} size {} (journal replayed)",
                attr.size
            );
        }
        Err(error) => {
            println!("DIRTY-TAIL MOUNT FAILED: {:?}", error.code());
            std::process::exit(1);
        }
    }
}

/// Deterministically construct the poisoned on-disk state on a fresh image:
///
/// 1. A normal writable session commits a metadata transaction.
/// 2. The commit's final "empty journal tail" superblock write is turned
///    into an I/O error, so the durable journal superblock keeps the active
///    (non-empty) tail while the transaction itself is fully checkpointed.
/// 3. The ext4 RECOVER feature bit is then cleared with debugfs, modelling
///    the real-world subset write loss in which the shutdown-time RECOVER
///    clear survived while the journal tail update did not.
///
/// The regression expectation is that a following writable mount replays
/// the tail (idempotently) instead of failing with EIO, and that e2fsck
/// accepts the repaired image.
fn craft_and_repair_regression(path: &str) {
    let status = Command::new("mkfs.ext4")
        .args([
            "-F",
            "-b",
            "4096",
            "-I",
            "256",
            "-O",
            "^orphan_file",
            path,
            "64M",
        ])
        .status()
        .expect("mkfs.ext4 failed");
    assert!(status.success(), "mkfs.ext4 did not succeed");

    // The persistence-operation index of the commit's clean-tail write is
    // found on an identical probe copy, so the armed run replays the same
    // deterministic operation sequence.
    let probe = "journal-tail-regression-probe.img";
    std::fs::copy(path, probe).expect("probe copy failed");
    let tail_write_index = {
        let device = Arc::new(CrashBlockFile::new(probe));
        let mut fs = Ext4::load_writable(device.clone()).expect("probe mount failed");
        fs.generic_create(ROOT_INO, "payload", InodeMode::FILE | InodeMode::ALL_RWX)
            .expect("probe create failed");
        // The journal superblock is the only block besides block 0 that is
        // written more than once: active tail, then clean tail.
        let ops = device.operations();
        let mut counts = std::collections::BTreeMap::new();
        for op in &ops {
            if let CrashDeviceOperation::Write(id) = op {
                *counts.entry(*id).or_insert(0usize) += 1;
            }
        }
        let journal_block = *counts
            .iter()
            .filter(|(id, count)| **count >= 2 && **id != 0)
            .map(|(id, _)| id)
            .max()
            .expect("no repeatedly-written journal block found");
        let mut last_journal_write = None;
        for (index, op) in ops.iter().enumerate() {
            if matches!(op, CrashDeviceOperation::Write(id) if *id == journal_block) {
                last_journal_write = Some(index);
            }
        }
        last_journal_write.expect("journal superblock was never written")
    };

    // Re-run the workload with the clean-tail write turned into an I/O
    // error: the durable tail stays active and the mount fail-stops.
    {
        let device = Arc::new(CrashBlockFile::new(path));
        let fs = Ext4::load_writable(device.clone()).expect("mount failed");
        device.arm_io_error_at(tail_write_index);
        let commit = fs.generic_create(ROOT_INO, "payload", InodeMode::FILE | InodeMode::ALL_RWX);
        assert!(
            commit.is_err(),
            "the poisoned-state workload expected the tail write to fail"
        );
        // Nothing else is written: drop the poisoned mount like a crash.
    }

    // Clear RECOVER like the surviving shutdown-time superblock write.
    let output = Command::new("debugfs")
        .args(["-w", "-R", "feature ^needs_recovery", path])
        .output()
        .expect("debugfs failed");
    assert!(
        output.status.success(),
        "debugfs could not clear needs_recovery:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Regression expectation: the mount must replay the non-empty tail.
    let device = Arc::new(CrashBlockFile::new(path));
    let fs = Ext4::load_writable(device)
        .expect("writable mount must replay a non-empty journal tail without RECOVER");
    let attr = fs
        .getattr(ROOT_INO)
        .expect("root getattr failed after replay mount");
    println!("POISONED-TAIL REPLAY OK: root size {}", attr.size);
    drop(fs);

    let output = Command::new("e2fsck")
        .args(["-fn", path])
        .output()
        .expect("e2fsck failed");
    assert!(
        output.status.success(),
        "e2fsck rejected the replayed image:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!("journal tail replay regression done");
}
