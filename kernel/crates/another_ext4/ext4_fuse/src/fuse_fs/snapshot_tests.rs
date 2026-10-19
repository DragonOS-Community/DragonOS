use super::*;
use crate::block_dev::BlockMem;
use another_ext4::{Block, BlockDevice, BLOCK_SIZE};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static NEXT_IMAGE: AtomicUsize = AtomicUsize::new(0);
type Snapshot = Vec<[u8; BLOCK_SIZE]>;

fn image() -> BlockMem {
    image_with_features("64bit")
}

fn image_with_features(features: &str) -> BlockMem {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../bin/mm037-073-075/fuse-unit");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(format!(
        "{}-{}.ext4",
        std::process::id(),
        NEXT_IMAGE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::File::create(&path)
        .unwrap()
        .set_len(32 * 1024 * 1024)
        .unwrap();
    assert!(std::process::Command::new("mkfs.ext4")
        .args(["-q", "-F", "-b", "4096", "-I", "256", "-O", features])
        .arg(&path)
        .status()
        .unwrap()
        .success());
    let device = BlockMem::load(path.to_str().unwrap());
    std::fs::remove_file(path).unwrap();
    device
}

fn owner() -> StateExt4FuseFs<Snapshot> {
    StateExt4FuseFs::new(Arc::new(image()), true)
}

fn file(owner: &StateExt4FuseFs<Snapshot>, name: &str, data: &[u8]) -> u32 {
    let inode = owner
        .fs
        .create(1, name, InodeMode::FILE | InodeMode::ALL_RWX)
        .unwrap();
    owner.fs.write(inode, 0, data).unwrap();
    inode
}

#[test]
fn restores_inode_namespace_counters_and_continues_allocating() {
    let mut owner = owner();
    let inode = file(&owner, "before", b"checkpoint");
    let free = owner.fs.super_block().unwrap().free_blocks_count();
    owner.checkpoint(7).unwrap();
    owner.fs.write(inode, 10, b"-later-data").unwrap();
    file(&owner, "discarded", &vec![0x55; 32 * BLOCK_SIZE]);
    assert_eq!(owner.fs.getattr(inode).unwrap().size, 21);
    owner.restore(7).unwrap();
    assert_eq!(owner.fs.getattr(inode).unwrap().size, 10);
    assert_eq!(owner.fs.super_block().unwrap().free_blocks_count(), free);
    assert!(owner.fs.lookup(1, "discarded").is_err());
    let mut data = [0; 64];
    assert_eq!(owner.fs.read(inode, 0, &mut data).unwrap(), 10);
    assert_eq!(&data[..10], b"checkpoint");
    let next = file(&owner, "continued", &vec![0x77; 32 * BLOCK_SIZE]);
    assert_eq!(
        owner.fs.getattr(next).unwrap().size,
        (32 * BLOCK_SIZE) as u64
    );
    owner.fs.shutdown_writable().unwrap();
    let remount = Ext4::load_read_only_checked(owner.block_dev.clone()).unwrap();
    assert_eq!(remount.lookup(1, "continued").unwrap(), next);
    assert_eq!(remount.getattr(inode).unwrap().size, 10);
}

#[test]
fn duplicate_and_missing_keys_do_not_change_the_timeline() {
    let mut owner = owner();
    let inode = file(&owner, "before", b"checkpoint");
    owner.checkpoint(7).unwrap();
    owner.fs.write(inode, 10, b"later").unwrap();
    assert_eq!(owner.checkpoint(7), Err(libc::EEXIST));
    assert_eq!(owner.restore(8), Err(libc::ENOENT));
    assert_eq!(owner.nodes.0, 0);
    owner.restore(7).unwrap();
    assert_eq!(owner.fs.getattr(inode).unwrap().size, 10);
    assert_eq!(owner.restore(7), Err(libc::ENOENT));
}

#[test]
fn stale_handles_never_retarget_or_block_current_reclaim() {
    let mut owner = owner();
    let inode = file(&owner, "before", b"old");
    let old = owner.nodes.encode(inode);
    owner.open_handles.insert(12, owner.handle(old));
    owner.open_dir_handles.insert(10, owner.handle(1));
    owner.retain_lookup(inode);
    owner.checkpoint(7).unwrap();
    owner.restore(7).unwrap();
    let new = owner.nodes.encode(inode);
    assert_ne!(new, old);
    assert_eq!(owner.nodes.decode(old), Err(libc::ESTALE));
    assert_eq!(
        owner.check_handle(&owner.open_handles, 12, old),
        Err(libc::ESTALE)
    );
    assert_eq!(
        owner.check_handle(&owner.open_handles, 12, new),
        Err(libc::ESTALE)
    );
    assert_eq!(
        owner.check_handle(&owner.open_dir_handles, 10, 1),
        Err(libc::ESTALE)
    );
    // Old live handles must not prevent a current inode lifetime's reclaim.
    let reclaim = owner.fs.unlink(1, "before").unwrap().unwrap();
    owner.queue_reclaim(reclaim).unwrap();
    assert!(owner.pending_reclaims.is_empty());
    owner.release_handle(12, old, false).unwrap();
    owner.release_handle(10, 1, true).unwrap();
    assert!(owner.open_handles.is_empty());
}

#[test]
fn checkpoint_refuses_pending_linear_reclaim_capabilities() {
    let mut owner = owner();
    let inode = file(&owner, "before", b"old");
    owner.retain_lookup(inode);
    let reclaim = owner.fs.unlink(1, "before").unwrap().unwrap();
    owner.queue_reclaim(reclaim).unwrap();
    assert_eq!(owner.checkpoint(7), Err(libc::EBUSY));
    assert!(!owner.states.contains_key(&7));
}

struct FaultDevice {
    memory: BlockMem,
    read_error: AtomicBool,
    flush_error: AtomicBool,
}
impl BlockDevice for FaultDevice {
    fn read_block(&self, id: u64) -> Result<Block, Ext4Error> {
        if self.read_error.swap(false, Ordering::SeqCst) {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        self.memory.read_block(id)
    }
    fn write_block(&self, block: &Block) -> Result<(), Ext4Error> {
        self.memory.write_block(block)
    }
    fn flush(&self) -> Result<(), Ext4Error> {
        if self.flush_error.swap(false, Ordering::SeqCst) {
            return Err(Ext4Error::new(ErrCode::EIO));
        }
        Ok(())
    }
    fn supports_reliable_flush(&self) -> bool {
        true
    }
}
impl StateBlockDevice<Snapshot> for FaultDevice {
    fn checkpoint(&self) -> Snapshot {
        self.memory.checkpoint()
    }
    fn restore(&self, state: &Snapshot) {
        self.memory.restore(state);
    }
}

#[test]
fn failed_load_rolls_back_bytes_and_preserves_owner_key_and_handles() {
    let device = Arc::new(FaultDevice {
        memory: image_with_features("64bit,^has_journal"),
        read_error: AtomicBool::new(false),
        flush_error: AtomicBool::new(false),
    });
    let mut owner = StateExt4FuseFs::new(device.clone(), true);
    let inode = file(&owner, "before", b"checkpoint");
    owner.checkpoint(7).unwrap();
    owner.fs.write(inode, 10, b"later").unwrap();
    owner.retain_lookup(inode);
    owner
        .open_handles
        .insert(12, owner.handle(owner.nodes.encode(inode)));
    let before = device.checkpoint();
    device.read_error.store(true, Ordering::SeqCst);
    assert_eq!(owner.restore(7), Err(libc::EIO));
    assert_eq!(device.checkpoint(), before);
    assert!(owner.states.contains_key(&7));
    assert_eq!(owner.nodes.0, 0);
    assert_eq!(owner.lookup_counts.get(&inode), Some(&1));
    assert_eq!(
        owner.check_handle(&owner.open_handles, 12, u64::from(inode)),
        Ok(inode)
    );
    assert_eq!(owner.fs.getattr(inode).unwrap().size, 15);
    device.flush_error.store(true, Ordering::SeqCst);
    assert_eq!(owner.checkpoint(8), Err(libc::EIO));
    assert!(!owner.states.contains_key(&8));
    // Fail after writable mount has begun; any mount-time disk changes must
    // also roll back, rather than only handling a failed initial read.
    device.flush_error.store(true, Ordering::SeqCst);
    assert_eq!(owner.restore(7), Err(libc::EIO));
    assert_eq!(device.checkpoint(), before);
    assert!(owner.states.contains_key(&7));
    assert_eq!(owner.nodes.0, 0);
    owner.restore(7).unwrap();
    assert_eq!(owner.fs.getattr(inode).unwrap().size, 10);
}

#[test]
fn old_forget_cannot_drop_a_current_lookup_or_reclaim_a_new_inode() {
    let mut owner = owner();
    let inode = file(&owner, "before", b"old");
    let old = owner.nodes.encode(inode);
    owner.checkpoint(7).unwrap();
    owner.restore(7).unwrap();
    let current = owner.nodes.encode(inode);
    owner.retain_lookup(inode);
    owner.open_handles.insert(9, owner.handle(current));
    assert_eq!(
        owner.check_handle(&owner.open_handles, 9, current),
        Ok(inode)
    );
    assert_eq!(
        owner.check_handle(&owner.open_handles, 9, current + 1),
        Err(libc::ESTALE)
    );
    let reclaim = owner.fs.unlink(1, "before").unwrap().unwrap();
    owner.queue_reclaim(reclaim).unwrap();
    owner.forget_node(old, u64::MAX).unwrap();
    assert_eq!(owner.lookup_counts.get(&inode), Some(&1));
    owner.release_handle(9, current, false).unwrap();
    assert!(!owner.pending_reclaims.is_empty());
    owner.forget_node(current, 1).unwrap();
    assert!(owner.pending_reclaims.is_empty());
}

#[test]
fn ioctl_keys_are_sized_and_epochs_cannot_wrap() {
    type Owner = StateExt4FuseFs<Snapshot>;
    assert_eq!(Owner::snapshot_key(1, &[]), Err(libc::EINVAL));
    assert_eq!(
        Owner::snapshot_key(Owner::CHECKPOINT_IOW, &[0; 7]),
        Err(libc::EINVAL)
    );
    assert_eq!(
        Owner::snapshot_key(Owner::RESTORE_IOW, &42u64.to_ne_bytes()),
        Ok((true, 42))
    );
    assert_eq!(Owner::snapshot_key(0, &[]), Err(libc::ENOTTY));
    let mut owner = owner();
    owner.checkpoint(7).unwrap();
    owner.nodes.0 = u32::MAX;
    assert_eq!(owner.restore(7), Err(libc::EOVERFLOW));
    assert!(owner.states.contains_key(&7));
    assert_eq!(owner.nodes.decode(0), Err(libc::ESTALE));
    assert_eq!(owner.nodes.decode(1), Ok(1));
    owner.next_fid = u64::MAX;
    owner.next_did = u64::MAX;
    assert_eq!(owner.allocate_handle(false), Err(libc::EOVERFLOW));
    assert_eq!(owner.allocate_handle(true), Err(libc::EOVERFLOW));
    for entry in owner.fs.listdir(1).unwrap() {
        if entry.name() == "." || entry.name() == ".." {
            assert_eq!(entry.inode(), 1);
        }
    }
}
