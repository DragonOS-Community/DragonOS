//! O_TMPFILE namespace/orphan regression on journaled and direct ext4.
//! Run from a disposable working directory.
#[path = "../block_file.rs"]
#[allow(dead_code)]
mod block_file;

use another_ext4::{Ext4, InodeMode, InodeOwner, EXT4_ROOT_INO};
use block_file::BlockFile;
use std::{fs::File, process::Command, sync::Arc};

fn run(journal: bool) {
    let path = if journal {
        "tmpfile-journal.img"
    } else {
        "tmpfile-direct.img"
    };
    File::create(path).unwrap().set_len(32 * 1024 * 1024).unwrap();
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

    let owner = InodeOwner { uid: 123, gid: 456 };
    let fs = Ext4::load_writable(Arc::new(BlockFile::new(path))).unwrap();
    let (attr, _reclaim) = fs
        .tmpfile_with_owner_and_attr(EXT4_ROOT_INO, InodeMode::FILE | InodeMode::ALL_RW, owner)
        .unwrap();
    assert_eq!(attr.links, 0);
    assert_eq!((attr.uid, attr.gid), (123, 456));
    fs.link(attr.ino, EXT4_ROOT_INO, "published").unwrap();
    assert_eq!(fs.lookup(EXT4_ROOT_INO, "published").unwrap(), attr.ino);
    assert_eq!(fs.getattr(attr.ino).unwrap().links, 1);

    let (unpublished, reclaim) = fs
        .tmpfile_with_owner_and_attr(EXT4_ROOT_INO, InodeMode::FILE | InodeMode::ALL_RW, owner)
        .unwrap();
    assert_eq!(unpublished.links, 0);
    fs.reclaim_inode(reclaim).unwrap();
    fs.shutdown_writable().unwrap();
    drop(fs);

    // A successful open can outlive the process; remount recovery must find
    // its zero-link inode without a directory entry and reclaim it.
    let fs = Ext4::load_writable(Arc::new(BlockFile::new(path))).unwrap();
    let (orphan, _reclaim) = fs
        .tmpfile_with_owner_and_attr(EXT4_ROOT_INO, InodeMode::FILE | InodeMode::ALL_RW, owner)
        .unwrap();
    drop(fs);  // simulate process/kernel loss before the final close
    let fs = Ext4::load_writable(Arc::new(BlockFile::new(path))).unwrap();
    assert!(fs.getattr(orphan.ino).is_err());
    fs.shutdown_writable().unwrap();
    drop(fs);

    let check = Command::new("e2fsck").args(["-fn", path]).output().unwrap();
    assert!(
        check.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );
    std::fs::remove_file(path).unwrap();
}

fn main() {
    run(true);
    run(false);
    println!("O_TMPFILE journal/direct transactions passed");
}
