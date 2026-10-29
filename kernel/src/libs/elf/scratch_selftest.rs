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
    matches!(result, Ok(true)) && metadata_birth()
}

/// Exercise the actual parser allocation and cached-table consumer. The
/// existing scratch test entry covers this metadata boundary as well.
fn metadata_birth() -> bool {
    use crate::mm::oom::denial;
    let result = (|| -> Result<bool, SystemError> {
        let fs = RamFS::new();
        let inode = File::recover_io(|| {
            fs.root_inode()
                .create("metadata", FileType::File, InodeMode::S_IRWXU)
        })?;
        File::recover_io(|| inode.resize(128))?;
        let file = File::recover_io(|| File::new(inode.clone(), FileFlags::O_RDONLY))?;
        file.lseek(SeekFrom::SeekSet(7))?;
        let mut bytes = [0; 64];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2; // ELFCLASS64
        bytes[5] = 1; // ELFDATA2LSB
        bytes[6] = 1; // EV_CURRENT
        bytes[32] = 64; // e_phoff
        bytes[54] = 56; // e_phentsize
        bytes[56] = 1; // e_phnum
        let header = ElfLoader::parse_ehdr(&bytes).map_err(|_| SystemError::ENOEXEC)?;
        let denied = {
            let _scope = denial::Scope::enter(true);
            let before = denial::rejected_count();
            let table = matches!(
                PreparedElf::prepare(&file, header),
                Err(SystemError::ENOMEM)
            );
            let path = matches!(ElfLoader::zeroed_metadata(32), Err(SystemError::ENOMEM));
            table && path && denial::rejected_count() >= before + 2
        };
        let header = ElfLoader::parse_ehdr(&bytes).map_err(|_| SystemError::ENOEXEC)?;
        let prepared = PreparedElf::prepare(&file, header)?;
        let cached = {
            let _scope = denial::Scope::enter(true);
            let before = denial::rejected_count();
            prepared.segments().into_iter().count() == 1 && before == denial::rejected_count()
        };
        bytes[56] = 0xff;
        bytes[57] = 0xff;
        let extended = ElfLoader::parse_ehdr(&bytes).map_err(|_| SystemError::ENOEXEC)?;
        let bounded = {
            let _scope = denial::Scope::enter(true);
            let before = denial::rejected_count();
            ElfLoader::program_headers_size(&extended) == Err(SystemError::ENOEXEC)
                && denial::rejected_count() == before
        };
        Ok(denied && cached && bounded && file.pos() == 7)
    })();
    matches!(result, Ok(true))
}
