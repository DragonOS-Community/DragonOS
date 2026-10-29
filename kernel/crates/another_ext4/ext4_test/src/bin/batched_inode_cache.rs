//! Cold inode reads retain their metadata view through cache insertion.
#[path = "../block_file.rs"]
#[allow(dead_code)]
mod block_file;
use another_ext4::{Block, BlockDevice, ErrCode, Ext4, Ext4Error, SetAttr, EXT4_ROOT_INO};
use block_file::BlockFile;
use std::{
    fs::File,
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Duration,
};

type Pause = (mpsc::SyncSender<()>, mpsc::Receiver<()>);
struct PausingDevice {
    file: Mutex<BlockFile>,
    reads: AtomicUsize,
    pause: Mutex<Option<Pause>>,
}
impl BlockDevice for PausingDevice {
    fn read_block(&self, id: u64) -> Result<Block, Ext4Error> {
        let block = self.file.lock().unwrap().read_block(id)?;
        self.reads.fetch_add(1, Ordering::Relaxed);
        let pause = self.pause.lock().unwrap().take();
        if let Some((entered, resume)) = pause {
            entered.send(()).unwrap();
            resume.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        Ok(block)
    }
    fn write_block(&self, block: &Block) -> Result<(), Ext4Error> {
        self.file.lock().unwrap().write_block(block)
    }
    fn flush(&self) -> Result<(), Ext4Error> {
        self.file.lock().unwrap().flush()
    }
    fn supports_reliable_flush(&self) -> bool {
        true
    }
}

fn main() {
    let path = format!("/tmp/batched-inode-cache-{}.img", std::process::id());
    File::create(&path)
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
            &path
        ])
        .status()
        .unwrap()
        .success());
    let device = Arc::new(PausingDevice {
        file: Mutex::new(BlockFile::new(&path)),
        reads: AtomicUsize::new(0),
        pause: Mutex::new(None),
    });
    let mut fs = Ext4::load_writable(device.clone()).unwrap();
    fs.enable_batching(256).unwrap();
    let fs = Arc::new(fs);
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = mpsc::sync_channel(1);
    *device.pause.lock().unwrap() = Some((entered_tx, resume_rx));
    let reader_fs = fs.clone();
    let reader = std::thread::spawn(move || reader_fs.getattr(EXT4_ROOT_INO).unwrap());
    entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let attr = SetAttr {
        uid: Some(1234),
        ..SetAttr::default()
    };
    // The cold value has been fetched but not inserted. Publication must not
    // pass it and then be overwritten by that old insertion.
    assert_eq!(
        fs.setattr(EXT4_ROOT_INO, attr).unwrap_err().code(),
        ErrCode::EAGAIN
    );
    resume_tx.send(()).unwrap();
    assert_eq!(reader.join().unwrap().uid, 0);
    fs.setattr(
        EXT4_ROOT_INO,
        SetAttr {
            uid: Some(1234),
            ..SetAttr::default()
        },
    )
    .unwrap();
    assert_eq!(fs.getattr(EXT4_ROOT_INO).unwrap().uid, 1234);
    let before = device.reads.load(Ordering::Relaxed);
    for _ in 0..100 {
        assert_eq!(fs.getattr(EXT4_ROOT_INO).unwrap().uid, 1234);
    }
    assert_eq!(device.reads.load(Ordering::Relaxed), before);
    fs.request_batch_commit();
    fs.commit_pending_batch().unwrap();
    let before = device.reads.load(Ordering::Relaxed);
    assert_eq!(fs.getattr(EXT4_ROOT_INO).unwrap().uid, 1234);
    assert_eq!(
        device.reads.load(Ordering::Relaxed),
        before,
        "Frozen checkpoint must not republish or invalidate live cache"
    );
    fs.shutdown_writable().unwrap();
    drop(fs);
    drop(device);
    let check = Command::new("e2fsck")
        .args(["-fn", &path])
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stdout)
    );
    std::fs::remove_file(path).unwrap();
    println!("cold read excludes publication; live inode cache survives checkpoint; e2fsck clean");
}
