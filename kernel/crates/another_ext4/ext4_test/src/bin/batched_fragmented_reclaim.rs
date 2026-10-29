//! Real-image regression: many nonadjacent extents share a few bitmap homes,
//! so retirement capacity, not metadata image capacity, bounds each reclaim.
#[path = "../block_file.rs"]
#[allow(dead_code)]
mod block_file;
use another_ext4::{ErrCode, Ext4, Ext4Error, InodeMode, EXT4_ROOT_INO};
use block_file::BlockFile;
use std::{fs::File, process::Command, sync::Arc};

fn advance(fs: &Ext4) {
    fs.request_batch_commit();
    assert!(fs.commit_pending_batch().unwrap().is_some());
}

fn retry<T>(fs: &Ext4, mut operation: impl FnMut() -> Result<T, Ext4Error>) -> T {
    for _ in 0..1024 {
        match operation() {
            Ok(value) => return value,
            Err(error) if error.code() == ErrCode::EAGAIN => advance(fs),
            Err(error) => panic!("unexpected error: {error:?}"),
        }
    }
    panic!("operation failed to make bounded progress");
}

fn main() {
    const PATH: &str = "batched-fragmented-reclaim.img";
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
    let mut fs = Ext4::load_writable(Arc::new(BlockFile::new(PATH))).unwrap();
    fs.enable_batching(256).unwrap();
    let victim = retry(&fs, || {
        fs.create(EXT4_ROOT_INO, "victim", InodeMode::FILE | InodeMode::ALL_RW)
    });
    let survivor = retry(&fs, || {
        fs.create(
            EXT4_ROOT_INO,
            "survivor",
            InodeMode::FILE | InodeMode::ALL_RW,
        )
    });
    for index in 0..64 {
        // Alternating allocation prevents the victim's retired ranges from
        // merging even though their bitmap/GDT/SB images are shared.
        retry(&fs, || fs.write(victim, index * 4096, &[0x31; 4096]));
        retry(&fs, || fs.write(survivor, index * 4096, &[0x72; 4096]));
    }
    while fs.batch_progress().unwrap().accepted != fs.batch_progress().unwrap().durable {
        advance(&fs);
    }
    let mut handle = retry(&fs, || fs.unlink(EXT4_ROOT_INO, "victim")).unwrap();
    let mut completed = false;
    for _ in 0..1024 {
        match fs.reclaim_inode(handle) {
            Ok(()) => {
                completed = true;
                break;
            }
            Err(failure) => {
                let (error, returned) = failure.into_parts();
                assert_eq!(
                    error.code(),
                    ErrCode::EAGAIN,
                    "fragmented reclaim must restart before E2BIG"
                );
                handle = returned;
                advance(&fs);
            }
        }
    }
    assert!(completed, "reclaim did not make bounded progress");
    while fs.batch_progress().unwrap().accepted != fs.batch_progress().unwrap().durable {
        advance(&fs);
    }
    let mut bytes = vec![0; 64 * 4096];
    assert_eq!(fs.read(survivor, 0, &mut bytes).unwrap(), bytes.len());
    assert!(bytes.iter().all(|byte| *byte == 0x72));
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
    println!("64 fragmented extents reclaimed without E2BIG; adjacent file and e2fsck clean");
}
