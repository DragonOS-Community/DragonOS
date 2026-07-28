use another_ext4::{Block, BlockDevice, BLOCK_SIZE};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::panic::panic_any;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

#[derive(Debug)]
pub struct BlockFile(File);

impl BlockFile {
    pub fn new(path: &str) -> Self {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        Self(file)
    }
}

impl BlockDevice for BlockFile {
    fn read_block(&self, block_id: u64) -> core::result::Result<Block, another_ext4::Ext4Error> {
        let mut file = &self.0;
        let mut buffer = [0u8; BLOCK_SIZE];
        // warn!("read_block {}", block_id);
        file.seek(SeekFrom::Start(block_id * BLOCK_SIZE as u64))
            .map_err(|_| another_ext4::Ext4Error::new(another_ext4::ErrCode::EIO))?;
        file.read_exact(&mut buffer)
            .map_err(|_| another_ext4::Ext4Error::new(another_ext4::ErrCode::EIO))?;
        Ok(Block::new(block_id, Box::new(buffer)))
    }

    fn write_block(&self, block: &Block) -> core::result::Result<(), another_ext4::Ext4Error> {
        let mut file = &self.0;
        // warn!("write_block {}", block.block_id);
        file.seek(SeekFrom::Start(block.id * BLOCK_SIZE as u64))
            .map_err(|_| another_ext4::Ext4Error::new(another_ext4::ErrCode::EIO))?;
        file.write_all(&*block.data)
            .map_err(|_| another_ext4::Ext4Error::new(another_ext4::ErrCode::EIO))?;
        Ok(())
    }

    fn flush(&self) -> core::result::Result<(), another_ext4::Ext4Error> {
        self.0
            .sync_all()
            .map_err(|_| another_ext4::Ext4Error::new(another_ext4::ErrCode::EIO))
    }

    fn supports_reliable_flush(&self) -> bool {
        true
    }
}

/// An operation on [`CrashBlockFile`] that was visible to the device model.
///
/// The model deliberately counts only writes and flushes. Reads have no
/// persistence effect and therefore must not move a crash boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CrashDeviceOperation {
    Write(u64),
    Flush,
}

/// Panic payload emitted by [`CrashBlockFile`] at an armed power-loss point.
///
/// A power loss is not an I/O error: callers must not be allowed to execute
/// their ordinary rollback or fail-stop path after it. Host recovery tests
/// catch this payload at the process boundary, discard the volatile epoch with
/// [`CrashBlockFile::crash`], and construct a new filesystem instance.
#[derive(Debug)]
pub struct SimulatedPowerLoss {
    pub operation: usize,
}

/// File-backed block device with an explicit volatile write epoch.
///
/// `write_block()` changes only the in-memory volatile map. A successful
/// `flush()` copies the complete map to the backing file and calls `sync_all`,
/// making it the model's power-loss durability boundary. `crash()` discards
/// the volatile map without invoking filesystem cleanup. This models the
/// contract DragonOS relies on when `supports_reliable_flush()` is true while
/// keeping all fault-injection behaviour outside production code.
pub struct CrashBlockFile {
    file: Mutex<File>,
    volatile: Mutex<BTreeMap<u64, Box<[u8; BLOCK_SIZE]>>>,
    operations: Mutex<Vec<CrashDeviceOperation>>,
    operation_index: AtomicUsize,
    crash_at: AtomicUsize,
    fail_at: AtomicUsize,
    write_through: std::sync::atomic::AtomicBool,
}

impl CrashBlockFile {
    pub fn new(path: &str) -> Self {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        Self {
            file: Mutex::new(file),
            volatile: Mutex::new(BTreeMap::new()),
            operations: Mutex::new(Vec::new()),
            operation_index: AtomicUsize::new(0),
            crash_at: AtomicUsize::new(usize::MAX),
            fail_at: AtomicUsize::new(usize::MAX),
            write_through: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Reset the operation sequence before the filesystem operation under
    /// test. Mount setup is intentionally excluded from the crash matrix.
    pub fn reset_operation_log(&self) {
        self.operations.lock().unwrap().clear();
        self.operation_index.store(0, Ordering::SeqCst);
        self.disarm_power_loss();
        self.disarm_io_error();
    }

    pub fn operations(&self) -> Vec<CrashDeviceOperation> {
        self.operations.lock().unwrap().clone()
    }

    /// Trigger a simulated power loss immediately before this zero-based
    /// persistence operation. The event does not return an Ext4 error.
    pub fn arm_power_loss_at(&self, operation: usize) {
        self.crash_at.store(operation, Ordering::SeqCst);
    }

    pub fn disarm_power_loss(&self) {
        self.crash_at.store(usize::MAX, Ordering::SeqCst);
    }

    /// Return `EIO` before this zero-based persistence operation. Unlike a
    /// power loss, this preserves normal stack unwinding so rollback and
    /// fail-stop paths can be tested independently.
    pub fn arm_io_error_at(&self, operation: usize) {
        self.fail_at.store(operation, Ordering::SeqCst);
    }

    pub fn disarm_io_error(&self) {
        self.fail_at.store(usize::MAX, Ordering::SeqCst);
    }

    /// Make completed writes durable before returning instead of retaining
    /// them in the volatile write epoch. Real block devices may persist a
    /// write before a later flush; recovery tests need this stricter model to
    /// validate ordering rather than assuming all writes remain cached.
    pub fn set_write_through(&self, enabled: bool) {
        self.write_through.store(enabled, Ordering::SeqCst);
    }

    /// Discard all writes since the last completed flush.
    pub fn crash(&self) {
        self.disarm_power_loss();
        self.disarm_io_error();
        self.volatile.lock().unwrap().clear();
        self.operation_index.store(0, Ordering::SeqCst);
    }

    fn before_persistence_operation(
        &self,
        operation: CrashDeviceOperation,
    ) -> Result<(), another_ext4::Ext4Error> {
        let index = self.operation_index.fetch_add(1, Ordering::SeqCst);
        if index == self.crash_at.load(Ordering::SeqCst) {
            panic_any(SimulatedPowerLoss { operation: index });
        }
        if index == self.fail_at.load(Ordering::SeqCst) {
            return Err(Self::io_error());
        }
        self.operations.lock().unwrap().push(operation);
        Ok(())
    }

    fn io_error() -> another_ext4::Ext4Error {
        another_ext4::Ext4Error::new(another_ext4::ErrCode::EIO)
    }

    fn write_stable(
        &self,
        block_id: u64,
        image: &[u8; BLOCK_SIZE],
    ) -> Result<(), another_ext4::Ext4Error> {
        let mut file = self.file.lock().unwrap();
        file.seek(SeekFrom::Start(block_id * BLOCK_SIZE as u64))
            .map_err(|_| Self::io_error())?;
        file.write_all(image).map_err(|_| Self::io_error())?;
        file.sync_data().map_err(|_| Self::io_error())
    }
}

impl BlockDevice for CrashBlockFile {
    fn read_block(&self, block_id: u64) -> core::result::Result<Block, another_ext4::Ext4Error> {
        if let Some(image) = self.volatile.lock().unwrap().get(&block_id).cloned() {
            return Ok(Block::new(block_id, image));
        }

        let mut image = [0u8; BLOCK_SIZE];
        let mut file = self.file.lock().unwrap();
        file.seek(SeekFrom::Start(block_id * BLOCK_SIZE as u64))
            .map_err(|_| Self::io_error())?;
        file.read_exact(&mut image).map_err(|_| Self::io_error())?;
        Ok(Block::new(block_id, Box::new(image)))
    }

    fn write_block(&self, block: &Block) -> core::result::Result<(), another_ext4::Ext4Error> {
        self.before_persistence_operation(CrashDeviceOperation::Write(block.id))?;
        if self.write_through.load(Ordering::SeqCst) {
            self.write_stable(block.id, &block.data)?;
        } else {
            self.volatile
                .lock()
                .unwrap()
                .insert(block.id, block.data.clone());
        }
        Ok(())
    }

    fn flush(&self) -> core::result::Result<(), another_ext4::Ext4Error> {
        self.before_persistence_operation(CrashDeviceOperation::Flush)?;

        // Keep the volatile epoch until the host has completed sync_all(). A
        // host I/O failure therefore cannot silently discard pending writes.
        let pending = self.volatile.lock().unwrap().clone();
        let mut file = self.file.lock().unwrap();
        for (block_id, image) in pending {
            file.seek(SeekFrom::Start(block_id * BLOCK_SIZE as u64))
                .map_err(|_| Self::io_error())?;
            file.write_all(&*image).map_err(|_| Self::io_error())?;
        }
        file.sync_all().map_err(|_| Self::io_error())?;
        self.volatile.lock().unwrap().clear();
        Ok(())
    }

    fn supports_reliable_flush(&self) -> bool {
        true
    }
}
