//! Ordered nojournal xattr ownership: I/O errors are not power loss, and an
//! error may be returned after the device applied a write or flush.
#[path = "../block_file.rs"]
#[allow(dead_code)]
mod block_file;

use another_ext4::{
    Block, BlockDevice, ErrCode, Ext4, Ext4Error, InodeReclaimHandle, BLOCK_SIZE, EXT4_ROOT_INO,
};
use block_file::{CrashBlockFile, CrashDeviceOperation, SimulatedPowerLoss};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Seek, SeekFrom},
    panic::{catch_unwind, AssertUnwindSafe},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Barrier, Mutex, OnceLock,
    },
};

type Result<T> = std::result::Result<T, Ext4Error>;
#[derive(Clone, Copy, Debug)]
enum Timing {
    Before,
    After,
}
#[derive(Default)]
struct Fault {
    index: usize,
    points: Vec<usize>,
    after: bool,
    log: Vec<CrashDeviceOperation>,
}
struct PausedRead {
    home: u64,
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}
struct Device {
    inner: CrashBlockFile,
    fault: Mutex<Fault>,
    reliable: AtomicBool,
    paused_read: Mutex<Option<PausedRead>>,
}
impl Device {
    fn new(path: &str, through: bool) -> Self {
        let inner = CrashBlockFile::new(path);
        inner.set_write_through(through);
        Self {
            inner,
            fault: Mutex::new(Fault::default()),
            reliable: AtomicBool::new(true),
            paused_read: Mutex::new(None),
        }
    }
    fn reset(&self, points: Vec<usize>, timing: Timing) {
        *self.fault.lock().unwrap() = Fault {
            points,
            after: matches!(timing, Timing::After),
            ..Fault::default()
        };
        self.inner.reset_operation_log();
    }
    fn operation(
        &self,
        op: CrashDeviceOperation,
        apply: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let (fail, after) = {
            let mut fault = self.fault.lock().unwrap();
            let index = fault.index;
            fault.index += 1;
            fault.log.push(op);
            (fault.points.contains(&index), fault.after)
        };
        if fail && !after {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        apply()?;
        if fail {
            Err(Ext4Error::new(ErrCode::EIO))
        } else {
            Ok(())
        }
    }
    fn operations(&self) -> Vec<CrashDeviceOperation> {
        self.fault.lock().unwrap().log.clone()
    }
}
impl BlockDevice for Device {
    fn read_block(&self, home: u64) -> Result<Block> {
        let pause = {
            let mut pending = self.paused_read.lock().unwrap();
            if pending.as_ref().is_some_and(|pause| pause.home == home) {
                pending.take()
            } else {
                None
            }
        };
        if let Some(pause) = pause {
            pause.entered.wait();
            pause.release.wait();
        }
        self.inner.read_block(home)
    }
    fn write_block(&self, block: &Block) -> Result<()> {
        self.operation(CrashDeviceOperation::Write(block.id), || {
            self.inner.write_block(block)
        })
    }
    fn flush(&self) -> Result<()> {
        self.operation(CrashDeviceOperation::Flush, || self.inner.flush())
    }
    fn supports_reliable_flush(&self) -> bool {
        self.reliable.load(Ordering::SeqCst)
    }
}
#[derive(Clone, Copy, Debug)]
enum Edit {
    Set,
    Remove,
    Detach,
    Reclaim,
    Attach,
    PrivateDetach,
    PrivateReclaim,
}
impl Edit {
    fn apply(self, fs: &Ext4, inode: u32, handle: Option<InodeReclaimHandle>) -> Result<()> {
        match self {
            Self::Set => fs.setxattr_with_flags(inode, b"user.alpha", &[b'x'; 1100], false, true),
            Self::Remove | Self::Detach => fs.removexattr(inode, b"user.alpha"),
            Self::PrivateDetach => fs.removexattr(inode, b"user.beta"),
            Self::Attach => fs.setxattr(inode, b"user.alpha", &[b'x'; 1100]),
            Self::Reclaim | Self::PrivateReclaim => {
                fs.reclaim_inode(handle.unwrap()).map_err(Ext4Error::from)
            }
        }
    }
}
fn bytes(path: &str, offset: u64, count: usize) -> Vec<u8> {
    let mut file = fs::File::open(path).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    let mut result = vec![0; count];
    file.read_exact(&mut result).unwrap();
    result
}
fn u32_at(image: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(image[at..at + 4].try_into().unwrap())
}
struct Geometry {
    inode_table: u64,
    bitmap: u64,
    inode_size: u64,
    first_block: u64,
}
impl Geometry {
    fn read(path: &str) -> Self {
        let sb = bytes(path, 1024, 1024);
        assert_eq!(u32_at(&sb, 24), 2, "the independent seed uses 4KiB blocks");
        assert_eq!(u32_at(&sb, 4), 4096, "one-group 16MiB seed");
        let group = bytes(path, BLOCK_SIZE as u64, 64);
        Self {
            inode_table: u32_at(&group, 8) as u64,
            bitmap: u32_at(&group, 0) as u64,
            inode_size: u16::from_le_bytes(sb[88..90].try_into().unwrap()) as u64,
            first_block: u32_at(&sb, 20) as u64,
        }
    }
    fn inode(&self, path: &str, inode: u32) -> Vec<u8> {
        bytes(
            path,
            self.inode_table * BLOCK_SIZE as u64 + u64::from(inode - 1) * self.inode_size,
            self.inode_size as usize,
        )
    }
    fn inode_home(&self, inode: u32) -> u64 {
        self.inode_table + u64::from(inode - 1) * self.inode_size / BLOCK_SIZE as u64
    }
    fn home(&self, path: &str, inode: u32) -> u64 {
        u32_at(&self.inode(path, inode), 104) as u64
    }
    fn allocated(&self, path: &str, home: u64) -> bool {
        let bit = home.checked_sub(self.first_block).unwrap() as usize;
        let bitmap = bytes(path, self.bitmap * BLOCK_SIZE as u64, BLOCK_SIZE);
        bitmap[bit / 8] & (1 << (bit % 8)) != 0
    }
}
static REPAIR_DIAGNOSTICS: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
fn fsck(path: &str) -> bool {
    let output = Command::new("e2fsck").args(["-fn", path]).output().unwrap();
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .all(|line| line.is_empty() || line.starts_with("e2fsck ")),
        "unexpected fsck diagnostic: {output:?}"
    );
    assert!(
        matches!(output.status.code(), Some(0 | 4)),
        "unexpected fsck failure: {output:?}"
    );
    if !output.status.success() {
        let mut diagnostics = REPAIR_DIAGNOSTICS
            .get_or_init(Default::default)
            .lock()
            .unwrap();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            // The fixed one-group seed permits only leaked claim 10,
            // conservative shared refcount 3->2, and the existing final
            // inode-12 retirement's interrupted bitmap/counter updates.
            // In particular, arbitrary checksum/entry/directory errors are
            // not accepted merely because e2fsck returns its repair status.
            let allowed = line.is_empty()
                || line.starts_with("Pass ")
                || line == "Fix? no"
                || line == "IGNORED."
                || line == "Block bitmap differences:  -10"
                || line
                    == "Block bitmap differences: Group 0 block bitmap does not match checksum."
                || line == "Inode bitmap differences:  -12"
                || line
                    == "Inode bitmap differences: Group 0 inode bitmap does not match checksum."
                || line == "Deleted inode 12 has zero dtime.  Fix? no"
                || line
                    == "Extended attribute block 9 has reference count 3, should be 2.  Fix? no"
                || line.starts_with("Free blocks count wrong ")
                || line.starts_with("Free inodes count wrong ")
                || line
                    == format!(
                        "{path}: ********** WARNING: Filesystem still has errors **********"
                    )
                || (line.starts_with(&format!("{path}: "))
                    && line.contains("/4096 files (0.0% non-contiguous), ")
                    && line.ends_with("/4096 blocks"));
            assert!(allowed, "unexpected interrupted-filesystem damage: {line}");
            diagnostics.insert(line.to_owned());
        }
    }
    output.status.success()
}
fn prepare(
    path: &str,
    through: bool,
    edit: Edit,
) -> (Arc<Device>, Ext4, [u32; 3], Option<InodeReclaimHandle>) {
    let device = Arc::new(Device::new(path, through));
    let fs = Ext4::load_writable(device.clone()).unwrap();
    let ids = ["a", "b", "c"].map(|name| fs.lookup(EXT4_ROOT_INO, name).unwrap());
    if matches!(
        edit,
        Edit::PrivateDetach | Edit::PrivateReclaim | Edit::Attach
    ) {
        fs.removexattr(ids[0], b"user.alpha").unwrap();
        if matches!(edit, Edit::Attach) {
            fs.removexattr(ids[0], b"user.beta").unwrap();
        }
    }
    let handle = if matches!(edit, Edit::Reclaim | Edit::PrivateReclaim) {
        let handle = fs
            .unlink(EXT4_ROOT_INO, "a")
            .unwrap()
            .expect("final unlink must issue ownership handle");
        device.flush().unwrap();
        Some(handle)
    } else {
        None
    };
    device.reset(vec![], Timing::Before);
    (device, fs, ids, handle)
}
/// Inspect the durable raw image, not a possibly stale inode cache. The two
/// untouched peer inodes always retain their original block and entry bytes.
/// Unreachable claims and reference overcounts are allowed only at interrupted
/// boundaries; a free referenced block or reference undercount is never safe.
fn crc32c(mut checksum: u32, bytes: &[u8]) -> u32 {
    for &byte in bytes {
        checksum ^= u32::from(byte);
        for _ in 0..8 {
            checksum = (checksum >> 1) ^ (0x82f63b78u32.wrapping_mul(checksum & 1));
        }
    }
    checksum
}
fn assert_safe(path: &str, seed: &str, ids: [u32; 3]) {
    let geometry = Geometry::read(path);
    let sb = bytes(path, 1024, 1024);
    let old_home = geometry.home(seed, ids[2]);
    for inode in &ids[1..] {
        assert_eq!(
            geometry.home(path, *inode),
            old_home,
            "peer pointer changed"
        );
        assert_eq!(
            u32_at(&geometry.inode(path, *inode), 28),
            8,
            "peer block charge changed"
        );
    }
    let old = bytes(seed, old_home * BLOCK_SIZE as u64, BLOCK_SIZE);
    let current = bytes(path, old_home * BLOCK_SIZE as u64, BLOCK_SIZE);
    assert_eq!(&current[32..], &old[32..], "peer entry or value changed");
    let mut owners = BTreeMap::<u64, u32>::new();
    for inode in ids {
        let home = geometry.home(path, inode);
        if home != 0 {
            *owners.entry(home).or_default() += 1;
        }
    }
    for (home, owners) in owners {
        assert!(
            geometry.allocated(path, home),
            "live inode references free block {home}"
        );
        let mut block = bytes(path, home * BLOCK_SIZE as u64, BLOCK_SIZE);
        if u32_at(&sb, 100) & 0x400 != 0 {
            let actual = u32_at(&block, 16);
            block[16..20].fill(0);
            let expected = crc32c(
                crc32c(crc32c(!0, &sb[104..120]), &home.to_le_bytes()),
                &block,
            );
            assert_eq!(actual, expected, "reachable xattr checksum invalid");
        }
        assert_eq!(
            u32_at(&block, 0),
            0xea020000,
            "reachable image not initialized"
        );
        assert!(
            u32_at(&block, 4) >= owners,
            "reference undercount at {home}"
        );
        assert_eq!(u32_at(&block, 8), 1);
        if home != old_home {
            assert_eq!(u32_at(&block, 4), 1, "new COW block must be private");
        }
    }
}
fn clean_success(path: &str, edit: Edit, ids: [u32; 3]) {
    assert!(fsck(path), "successful {edit:?} must be fsck clean");
    let geometry = Geometry::read(path);
    let old_home = geometry.home(path, ids[2]);
    let old = bytes(path, old_home * BLOCK_SIZE as u64, BLOCK_SIZE);
    assert_eq!(u32_at(&old, 4), 2);
    let home = geometry.home(path, ids[0]);
    assert_eq!(
        home == 0,
        matches!(
            edit,
            Edit::Detach | Edit::Reclaim | Edit::PrivateDetach | Edit::PrivateReclaim
        )
    );
    if home != 0 {
        assert_ne!(home, old_home);
    }
}
fn seed(path: &str, single: bool, checksum: bool) {
    let binary = std::env::current_exe()
        .unwrap()
        .with_file_name("shared_xattr");
    let mut command = Command::new(binary);
    command.args(["--seed", path]);
    if single {
        command.arg("--single-attribute");
    }
    if !checksum {
        command.arg("--no-metadata-csum");
    }
    assert!(command.status().unwrap().success());
}
fn matrix(seed: &str, edit: Edit, through: bool) -> usize {
    const WORK: &str = "shared-xattr-fault-work.img";
    fs::copy(seed, WORK).unwrap();
    let (device, filesystem, ids, handle) = prepare(WORK, through, edit);
    edit.apply(&filesystem, ids[0], handle).unwrap();
    let operations = device.operations();
    assert!(!operations.is_empty());
    filesystem.shutdown_writable().unwrap();
    drop(filesystem);
    drop(device);
    assert_safe(WORK, seed, ids);
    clean_success(WORK, edit, ids);
    let new_home = Geometry::read(WORK).home(WORK, ids[0]);
    let image_at = (new_home != 0).then(|| {
        operations
            .iter()
            .position(|op| *op == CrashDeviceOperation::Write(new_home))
            .unwrap()
    });
    let inode_at = operations
        .iter()
        .position(|op| *op == CrashDeviceOperation::Write(Geometry::read(WORK).inode_home(ids[0])))
        .unwrap();
    println!(
        "{edit:?} through={through}: {} persistence points {operations:?}",
        operations.len()
    );
    let mut repair = 0;
    for timing in [Timing::Before, Timing::After] {
        for point in 0..operations.len() {
            fs::copy(seed, WORK).unwrap();
            let (device, filesystem, ids, handle) = prepare(WORK, through, edit);
            let retired_home = Geometry::read(WORK).home(WORK, ids[0]);
            device.reset(vec![point], timing);
            let error = edit.apply(&filesystem, ids[0], handle).unwrap_err();
            assert_eq!(
                error.code(),
                ErrCode::EIO,
                "{edit:?} {timing:?} point={point}"
            );
            // An invalid CREATE is a side-effect-free probe: a healthy mount
            // returns EEXIST, while fail-stop must reject even that mutation.
            let probe = filesystem
                .setxattr_with_flags(ids[1], b"user.alpha", b"probe", true, false)
                .unwrap_err()
                .code();
            let before_inode = image_at.is_some_and(|at| point >= at && point < inode_at);
            assert_eq!(
                probe,
                if before_inode {
                    ErrCode::EEXIST
                } else {
                    ErrCode::EIO
                },
                "wrong fail-stop boundary: {edit:?} {timing:?} point={point}"
            );
            device.inner.crash();
            drop(filesystem);
            drop(device);
            assert_safe(WORK, seed, ids);
            if matches!(edit, Edit::PrivateDetach | Edit::PrivateReclaim) && point <= inode_at + 1 {
                assert!(
                    Geometry::read(WORK).allocated(WORK, retired_home),
                    "uncertain detach released private home"
                );
            }
            if new_home != 0 {
                let allocated = Geometry::read(WORK).allocated(WORK, new_home);
                if before_inode {
                    assert!(!allocated, "unpublished claim rollback leaked");
                }
                if point >= inode_at {
                    assert!(allocated, "uncertain inode publication released new claim");
                }
            }
            repair += usize::from(!fsck(WORK));
        }
    }
    // A B-image failure followed by a rollback write failure must poison the
    // public facade, including when both writes reached the device first.
    if let Some(image_at) = image_at {
        for timing in [Timing::Before, Timing::After] {
            fs::copy(seed, WORK).unwrap();
            let (device, filesystem, ids, handle) = prepare(WORK, through, edit);
            device.reset(vec![image_at, image_at + 1], timing);
            assert_eq!(
                edit.apply(&filesystem, ids[0], handle).unwrap_err().code(),
                ErrCode::EIO
            );
            assert_eq!(
                filesystem
                    .setxattr_with_flags(ids[1], b"user.alpha", b"probe", true, false)
                    .unwrap_err()
                    .code(),
                ErrCode::EIO
            );
            device.inner.crash();
            drop(filesystem);
            drop(device);
            assert_safe(WORK, seed, ids);
        }
    }
    for point in 0..operations.len() {
        fs::copy(seed, WORK).unwrap();
        let (device, filesystem, ids, handle) = prepare(WORK, through, edit);
        device.inner.arm_power_loss_at(point);
        let outcome = catch_unwind(AssertUnwindSafe(|| edit.apply(&filesystem, ids[0], handle)));
        let payload = outcome.expect_err("armed power loss must not return normally");
        assert!(payload.is::<SimulatedPowerLoss>());
        device.inner.crash();
        drop(filesystem);
        drop(device);
        assert_safe(WORK, seed, ids);
        repair += usize::from(!fsck(WORK));
    }
    fs::remove_file(WORK).unwrap();
    println!("{edit:?} through={through}: EIO before/after + crash PASS; {repair} interrupted images need offline repair (not crash atomic)");
    3 * operations.len() + usize::from(image_at.is_some()) * 2
}
/// Stop a reader after selecting the inode's old home, before reading that
/// home. Retirement must be rejected until the entire get/list snapshot ends.
fn unsupported_retirement(seed: &str, single: bool) {
    const WORK: &str = "shared-xattr-no-flush.img";
    fs::copy(seed, WORK).unwrap();
    let device = Arc::new(Device::new(WORK, false));
    device.reliable.store(false, Ordering::SeqCst);
    let filesystem = Ext4::load_writable_with_options(device.clone(), false).unwrap();
    let inode = filesystem.lookup(EXT4_ROOT_INO, "a").unwrap();
    let handle = if single {
        None
    } else {
        filesystem.unlink(EXT4_ROOT_INO, "a").unwrap()
    };
    device.reset(vec![], Timing::Before);
    let edit = if single { Edit::Detach } else { Edit::Reclaim };
    assert_eq!(
        edit.apply(&filesystem, inode, handle).unwrap_err().code(),
        ErrCode::ENOTSUP
    );
    assert!(
        device.operations().is_empty(),
        "unsupported retirement must not write"
    );
    drop(filesystem);
    drop(device);
    fs::remove_file(WORK).unwrap();
    println!("{edit:?} without reliable flush rejects before writes PASS");
}
fn reader_lifetime(seed: &str) {
    const WORK: &str = "shared-xattr-reader.img";
    for list in [false, true] {
        fs::copy(seed, WORK).unwrap();
        let (device, filesystem, ids, _) = prepare(WORK, false, Edit::Set);
        let filesystem = Arc::new(filesystem);
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        *device.paused_read.lock().unwrap() = Some(PausedRead {
            home: Geometry::read(WORK).home(WORK, ids[0]),
            entered: entered.clone(),
            release: release.clone(),
        });
        let reader_fs = filesystem.clone();
        let reader = std::thread::spawn(move || {
            if list {
                let mut names = [0; 128];
                let length = reader_fs.listxattr(ids[0], &mut names).unwrap();
                let mut names: Vec<_> = names[..length]
                    .split(|byte| *byte == 0)
                    .filter(|name| !name.is_empty())
                    .collect();
                names.sort();
                assert_eq!(names, [b"user.alpha".as_slice(), b"user.beta".as_slice()]);
            } else {
                let mut value = [0; 1000];
                assert_eq!(
                    reader_fs
                        .getxattr(ids[0], b"user.alpha", &mut value)
                        .unwrap(),
                    1000
                );
                assert_eq!(value, [b'a'; 1000]);
            }
        });
        entered.wait();
        let blocked = filesystem.setxattr(ids[0], b"user.alpha", b"replacement");
        // Always let the reader exit, even when the assertion fails.
        release.wait();
        reader.join().unwrap();
        assert_eq!(blocked.unwrap_err().code(), ErrCode::EAGAIN);
        filesystem
            .setxattr(ids[0], b"user.alpha", b"replacement")
            .unwrap();
        filesystem.shutdown_writable().unwrap();
        drop(filesystem);
        drop(device);
        assert_safe(WORK, seed, ids);
        clean_success(WORK, Edit::Set, ids);
    }
    fs::remove_file(WORK).unwrap();
    println!("get/list selected-old-home reader lifetime PASS");
}
fn capability(seed: &str) {
    const WORK: &str = "shared-xattr-capability.img";
    for reliable in [false, true] {
        fs::copy(seed, WORK).unwrap();
        let device = Arc::new(Device::new(WORK, false));
        device.reliable.store(reliable, Ordering::SeqCst);
        let filesystem = Ext4::load_writable_with_options(device.clone(), false).unwrap();
        let inode = filesystem.lookup(EXT4_ROOT_INO, "a").unwrap();
        device.reset(vec![], Timing::Before);
        let result = filesystem.setxattr(inode, b"user.alpha", &[b'x'; 1100]);
        if reliable {
            result.unwrap();
            assert!(
                device.operations().contains(&CrashDeviceOperation::Flush),
                "barrier=0 must not bypass COW safety"
            );
        } else {
            assert_eq!(result.unwrap_err().code(), ErrCode::ENOTSUP);
            assert!(
                device.operations().is_empty(),
                "capability rejection must precede physical writes"
            );
        }
        device.reliable.store(true, Ordering::SeqCst);
        if !reliable {
            filesystem
                .setxattr(inode, b"user.alpha", &[b'x'; 1100])
                .unwrap();
        }
        device.reset(vec![], Timing::Before);
        device.reliable.store(false, Ordering::SeqCst);
        filesystem
            .setxattr(inode, b"user.alpha", b"private")
            .unwrap();
        assert!(
            !device.operations().contains(&CrashDeviceOperation::Flush),
            "private nonempty update must preserve no-flush compatibility"
        );
        device.reliable.store(true, Ordering::SeqCst);
        filesystem.shutdown_writable().unwrap();
        device.flush().unwrap();
        drop(filesystem);
        drop(device);
        assert!(fsck(WORK));
    }
    fs::remove_file(WORK).unwrap();
    println!("no reliable flush / barrier=0 capability PASS");
}
fn main() {
    // Expected power-loss panics are test control flow, not failed assertions.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if !info.payload().is::<SimulatedPowerLoss>() {
            previous(info);
        }
    }));
    let mut points = 0;
    for checksum in [true, false] {
        for (single, edits) in [
            (
                false,
                &[
                    Edit::Set,
                    Edit::Remove,
                    Edit::Reclaim,
                    Edit::Attach,
                    Edit::PrivateDetach,
                    Edit::PrivateReclaim,
                ][..],
            ),
            (true, &[Edit::Detach][..]),
        ] {
            let path = format!("shared-xattr-fault-seed-{checksum}-{single}.img");
            seed(&path, single, checksum);
            for edit in edits {
                for through in [false, true] {
                    points += matrix(&path, *edit, through);
                }
            }
            if !single {
                capability(&path);
                reader_lifetime(&path);
            }
            unsupported_retirement(&path, single);
            fs::remove_file(path).unwrap();
        }
    }
    if let Some(diagnostics) = REPAIR_DIAGNOSTICS.get() {
        for line in diagnostics.lock().unwrap().iter() {
            println!("repair diagnostic: {line}");
        }
    }
    println!("shared xattr fault matrices PASS: {points} injected boundaries");
}
