use another_ext4::{Block, BlockDevice, BLOCK_SIZE};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::sync::Mutex;

/// A block device supporting state save and restore
pub trait StateBlockDevice<T>: BlockDevice
where
    T: Sized,
{
    fn checkpoint(&self) -> T;
    fn restore(&self, state: T);
}

/// An in-memory block device
#[derive(Debug)]
pub struct BlockMem(Mutex<Vec<[u8; BLOCK_SIZE]>>);

impl BlockMem {
    /// Create a new block device with the given number of blocks
    pub fn new(num_blocks: u64) -> Self {
        let mut blocks = Vec::with_capacity(num_blocks as usize);
        for _ in 0..num_blocks {
            blocks.push([0; BLOCK_SIZE]);
        }
        Self(Mutex::new(blocks))
    }
    /// Load a disk image from a file
    pub fn load(path: &str) -> Self {
        let mut file = OpenOptions::new().read(true).open(path).unwrap();
        let mut blocks = Vec::new();
        let mut buf = [0; BLOCK_SIZE];
        while file.read(&mut buf).unwrap() > 0 {
            blocks.push(buf);
        }
        Self(Mutex::new(blocks))
    }
    /// Save the disk image to a file
    pub fn save(&self, path: &str) {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .unwrap();
        let blocks = self.0.lock().unwrap();
        for block in blocks.iter() {
            file.write_all(block).unwrap();
        }
    }
    /// Make an ext4 filesystem on the block device
    pub fn mkfs(&self) {
        let path = "tmp.img";
        let mut mem = self.0.lock().unwrap();
        // Create a temp block file
        std::process::Command::new("dd")
            .args([
                "if=/dev/zero",
                &format!("of={}", path),
                &format!("bs={}", BLOCK_SIZE),
                &format!("count={}", mem.len()),
            ])
            .status()
            .expect("Failed to create temp file");
        // Make ext4 fs
        std::process::Command::new("mkfs.ext4")
            // Enable 64-bit feature and use 256-bit inode size
            .args([path, &format!("-b {}", BLOCK_SIZE), "-O 64bit", "-I 256"])
            .status()
            .expect("Failed to make ext4 fs");
        // Open the temp file and copy data to memory
        let mut file = OpenOptions::new().read(true).open(path).unwrap();
        for block in mem.iter_mut() {
            file.read(block).expect("Read failed");
        }
        // Remove the temp file
        std::process::Command::new("rm")
            .args(["-rf", path])
            .status()
            .expect("Failed to remove temp file");
    }
}

impl BlockDevice for BlockMem {
    fn read_block(&self, block_id: u64) -> Block {
        Block {
            id: block_id,
            data: self.0.lock().unwrap()[block_id as usize],
        }
    }
    fn write_block(&self, block: &Block) {
        self.0.lock().unwrap()[block.id as usize] = block.data;
    }
}

impl StateBlockDevice<Vec<[u8; BLOCK_SIZE]>> for BlockMem {
    fn checkpoint(&self) -> Vec<[u8; BLOCK_SIZE]> {
        self.0.lock().unwrap().clone()
    }
    fn restore(&self, state: Vec<[u8; BLOCK_SIZE]>) {
        self.0.lock().unwrap().clone_from(&state);
    }
}
