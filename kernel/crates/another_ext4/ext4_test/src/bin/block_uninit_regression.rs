//! Cross-group BLOCK_UNINIT allocation against an unmodified mkfs.ext4 image.
//! Run with `cargo run --bin block_uninit_regression` (requires e2fsprogs).
#[path = "../block_file.rs"]
#[allow(dead_code)]
mod block_file;

use another_ext4::{BlockDevice, Ext4, InodeMode, EXT4_ROOT_INO};
use block_file::{BlockFile, CrashBlockFile};
use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

const IMAGE_SIZE: u64 = 32 * 1024 * 1024;
const CHUNK: usize = 64 * 1024;
const PAYLOAD_SIZE: usize = 12 * 1024 * 1024;

struct Image(PathBuf);
impl Image {
    fn new(name: &str) -> Self {
        let path = std::env::current_dir()
            .unwrap()
            .join(format!("block-uninit-{}-{name}.img", std::process::id()));
        File::options()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap()
            .set_len(IMAGE_SIZE)
            .unwrap();
        let mkfs = |features: &str| {
            Command::new("mkfs.ext4")
                .args([
                    "-q", "-F", "-b", "4096", "-g", "1024", "-G", "2", "-I", "256", "-O", features,
                ])
                .arg(&path)
                .output()
                .expect("mkfs.ext4 is required")
        };
        let mut output = mkfs("metadata_csum,64bit,flex_bg,^orphan_file");
        if !output.status.success()
            && String::from_utf8_lossy(&output.stderr).contains("Invalid filesystem option set:")
            && String::from_utf8_lossy(&output.stderr).contains("orphan_file")
        {
            output = mkfs("metadata_csum,64bit,flex_bg");
        }
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let image = Self(path);
        assert!(
            image.group(1)[18] & 2 != 0,
            "fixture must preserve BLOCK_UNINIT"
        );
        image
    }
    fn path(&self) -> &str {
        self.0.to_str().unwrap()
    }
    fn group(&self, id: usize) -> [u8; 64] {
        let mut bytes = [0; 64];
        File::open(&self.0)
            .unwrap()
            .read_exact_at(&mut bytes, 4096 + id as u64 * 64)
            .unwrap();
        bytes
    }
    fn bitmap(&self, id: usize) -> u64 {
        u32::from_le_bytes(self.group(id)[0..4].try_into().unwrap()) as u64
    }
}
impl Drop for Image {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn create(fs: &Ext4) -> u32 {
    fs.generic_create(
        EXT4_ROOT_INO,
        "payload",
        InodeMode::FILE | InodeMode::ALL_RWX,
    )
    .expect("create payload")
}

fn write_chunks(fs: &Ext4, ino: u32, end: usize, transactional: bool) {
    for offset in (0..end).step_by(CHUNK) {
        let data = vec![(offset / CHUNK % 251 + 1) as u8; CHUNK];
        if transactional {
            fs.prepare_buffered_write(ino, offset, CHUNK, (offset + CHUNK) as u64, None)
                .unwrap_or_else(|e| panic!("transaction allocation at offset {offset}: {e:?}"));
        }
        assert_eq!(
            fs.write(ino, offset, &data)
                .unwrap_or_else(|e| panic!("write at offset {offset}: {e:?}")),
            CHUNK
        );
    }
}

fn fsck(path: &Path) {
    let out = Command::new("e2fsck")
        .arg("-fn")
        .arg(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "e2fsck:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn cross_group(transactional: bool) {
    let mode = if transactional {
        "transaction"
    } else {
        "direct"
    };
    let image = Image::new(mode);
    let original_flags = image.group(1)[18];
    let fs = Ext4::load_writable(Arc::new(BlockFile::new(image.path()))).unwrap();
    let ino = create(&fs);
    write_chunks(&fs, ino, PAYLOAD_SIZE, transactional);
    fs.shutdown_writable().unwrap();
    drop(fs);
    assert_eq!(
        image.group(1)[18],
        original_flags & !2,
        "only BLOCK_UNINIT may be cleared by data-block allocation"
    );
    let fs = Ext4::load_writable(Arc::new(BlockFile::new(image.path()))).unwrap();
    let mut data = vec![0; CHUNK];
    for offset in (0..PAYLOAD_SIZE).step_by(CHUNK) {
        assert_eq!(fs.read(ino, offset, &mut data).unwrap(), CHUNK);
        assert!(
            data.iter().all(|b| *b == (offset / CHUNK % 251 + 1) as u8),
            "remounted data mismatch at offset {offset}"
        );
    }
    fs.shutdown_writable().unwrap();
    drop(fs);
    fsck(&image.0);
    println!("PASS {mode}: 12 MiB crosses groups, flags preserved, remount and fsck clean");
}

fn corrupt_initialized_bitmap() {
    let image = Image::new("bad-checksum");
    let bitmap = image.bitmap(0);
    let file = File::options()
        .read(true)
        .write(true)
        .open(&image.0)
        .unwrap();
    let mut byte = [0];
    file.read_exact_at(&mut byte, bitmap * 4096).unwrap();
    byte[0] ^= 0x80;
    file.write_all_at(&byte, bitmap * 4096).unwrap();
    let fs = Ext4::load_writable(Arc::new(BlockFile::new(image.path()))).unwrap();
    let ino = create(&fs);
    let error = fs
        .prepare_buffered_write(ino, 0, CHUNK, CHUNK as u64, None)
        .unwrap_err();
    assert_eq!(error.code(), another_ext4::ErrCode::EIO);
    assert!(format!("{error:?}").contains("checksum"), "{error:?}");
    fs.shutdown_writable().unwrap();
    println!("PASS initialized bitmap checksum corruption is rejected");
}

fn aborted_first_initialization() {
    let image = Image::new("abort");
    let free = u16::from_le_bytes(image.group(0)[12..14].try_into().unwrap()) as usize;
    let prefix = free * 4096 / CHUNK * CHUNK;
    let device = Arc::new(CrashBlockFile::new(image.path()));
    let fs = Ext4::load_writable(device.clone()).unwrap();
    let ino = create(&fs);
    write_chunks(&fs, ino, prefix, true);
    assert_ne!(image.group(1)[18] & 2, 0);
    let group_before = image.group(1);
    let bitmap_before = device.read_block(image.bitmap(1)).unwrap();
    device.reset_operation_log();
    device.arm_io_error_at(0);
    let error = fs
        .prepare_buffered_write(ino, prefix, CHUNK, (prefix + CHUNK) as u64, None)
        .unwrap_err();
    assert_eq!(error.code(), another_ext4::ErrCode::EIO);
    device.crash();
    drop(fs);
    assert_eq!(
        image.group(1),
        group_before,
        "failed preparation published group initialization"
    );
    assert_eq!(
        device.read_block(image.bitmap(1)).unwrap().data,
        bitmap_before.data,
        "failed preparation published the new bitmap"
    );
    let fs = Ext4::load_writable(Arc::new(BlockFile::new(image.path()))).unwrap();
    fs.prepare_buffered_write(ino, prefix, CHUNK, (prefix + CHUNK) as u64, None)
        .unwrap();
    assert_eq!(fs.write(ino, prefix, &vec![0x73; CHUNK]).unwrap(), CHUNK);
    fs.shutdown_writable().unwrap();
    drop(fs);
    fsck(&image.0);
    println!("PASS first initialization preparation I/O failure leaves bitmap and descriptor unpublished; retry succeeds");
}

fn main() {
    cross_group(true);
    cross_group(false);
    corrupt_initialized_bitmap();
    aborted_first_initialization();
}
