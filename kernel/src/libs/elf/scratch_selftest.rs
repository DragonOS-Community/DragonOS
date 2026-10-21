//! Exercise the eager population consumer without committing an exec image.
use super::*;
use crate::{
    cgroup::memory::{object::ObjectAccount, MemoryAccount},
    filesystem::{
        ramfs::RamFS,
        vfs::{
            file::{File, FileFlags},
            FileSystem, FileType, InodeMode,
        },
    },
    mm::kernel_buffer::KernelBuffer,
};

pub(crate) fn run() -> bool {
    let result = (|| -> Result<bool, SystemError> {
        let account = MemoryAccount::new(u64::MAX - 209, None)?;
        let prepared = ObjectAccount::prepare(&account)?;
        let fs = RamFS::new();
        let inode = File::recover_io(|| {
            fs.root_inode()
                .create("eager", FileType::File, InodeMode::S_IRWXU)
        })?;
        File::recover_io(|| inode.resize(2 * ElfLoader::FILE_READ_BUF_SIZE))?;
        let file = File::recover_io(|| File::new(inode.clone(), FileFlags::O_RDONLY))?;
        file.lseek(SeekFrom::SeekSet(7))?;
        let mut backing_attempts = 0;
        let mut requested = 0;
        let result = ElfLoader::do_load_file_with(
            VirtAddr::new(0x1000),
            ElfLoader::FILE_READ_BUF_SIZE + 1,
            0,
            &file,
            |size| {
                requested = size;
                File::recover_io(|| {
                    KernelBuffer::try_new_zeroed_in_with(
                        size,
                        prepared.object_account(),
                        &mut || {
                            backing_attempts += 1;
                            Err(SystemError::ENOMEM)
                        },
                    )
                })
            },
        );
        Ok(matches!(result, Err(SystemError::ENOMEM))
            && backing_attempts == 1
            && requested == ElfLoader::FILE_READ_BUF_SIZE
            && file.pos() == 7
            && account
                .snapshot()
                .kernel_bytes
                .iter()
                .all(|bytes| *bytes == 0))
    })();
    matches!(result, Ok(true))
}
