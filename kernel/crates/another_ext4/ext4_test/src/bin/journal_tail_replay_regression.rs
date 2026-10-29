//! Clean ext4 state with a stale nonempty JBD2 tail must discard old
//! transactions, preserving newer home metadata. Dirty ext4 recovery remains
//! covered by the journal fault-injection tests.
//! Run with --craft <private image> to create the state and verify readonly
//! access, writable discard, permissions and e2fsck consistency.

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
            args.get(2)
                .map(String::as_str)
                .unwrap_or("journal-tail-regression.img"),
        ),
        Some(path) => mount_dirty_image(path),
        _ => {
            eprintln!("usage: {} [--craft] <image>", args[0]);
            std::process::exit(64);
        }
    }
}

/// Mount an explicitly supplied image writable using its ext4 recovery state.
fn mount_dirty_image(path: &str) {
    let device = Arc::new(CrashBlockFile::new(path));
    match Ext4::load_writable(device) {
        Ok(fs) => {
            let attr = fs
                .getattr(ROOT_INO)
                .expect("root getattr failed after recovery mount");
            println!(
                "DIRTY-TAIL MOUNT OK: root ino {ROOT_INO} size {} (journal initialized)",
                attr.size
            );
        }
        Err(error) => {
            println!("DIRTY-TAIL MOUNT FAILED: {:?}", error.code());
            std::process::exit(1);
        }
    }
}

/// Checkpoint a transaction while failing the clean-tail write, then model a
/// clean ext4/offline update by clearing RECOVER and changing home permissions.
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
    let probe_path = format!("{path}.probe");
    let probe = probe_path.as_str();
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

    // An offline metadata update is newer than this clean filesystem's stale
    // journal. Wiping must preserve it; replaying would restore the old mode.
    let output = Command::new("debugfs")
        .args(["-w", "-R", "set_inode_field / mode 040700", path])
        .output()
        .expect("debugfs mode update failed");
    assert!(output.status.success());
    let readonly_device = Arc::new(CrashBlockFile::new(path));
    let readonly = Ext4::load_read_only_checked(readonly_device.clone())
        .expect("clean ext4 with stale tail must permit readonly access");
    assert_eq!(
        readonly.getattr(ROOT_INO).unwrap().perm.bits() & 0o7777,
        0o700
    );
    drop(readonly);
    assert!(
        readonly_device.operations().is_empty(),
        "readonly mount must not write or flush"
    );
    let device = Arc::new(CrashBlockFile::new(path));
    let fs = Ext4::load_writable(device)
        .expect("writable mount must discard a stale journal tail without RECOVER");
    let attr = fs
        .getattr(ROOT_INO)
        .expect("root getattr failed after replay mount");
    assert_eq!(
        attr.perm.bits() & 0o7777,
        0o700,
        "stale journal must not overwrite newer home inode permissions"
    );
    println!("STALE-TAIL WIPE OK: root size {}", attr.size);
    fs.shutdown_writable()
        .expect("clean shutdown after stale journal discard");
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
    println!("journal tail discard regression done");
}
