//! Bounded, page-backed exec strings. Bytes are packed, including each NUL;
//! neither string count nor string length requires a contiguous heap array.
//! Immutable snapshots may outlive exec (cmdline/fork) without task-local maps.
use alloc::{boxed::Box, ffi::CString, sync::Arc};
use core::{mem::size_of, ops::Range};
use system_error::SystemError;

use crate::{
    arch::ipc::signal::Signal,
    libs::paged_vec::{PagedVec, CHUNK_BYTES},
    mm::VirtAddr,
    process::{resource::RLimitID, ProcessManager},
    syscall::user_access::copy_from_user_protected,
};

const MAX_BYTES: usize = 6 * 1024 * 1024;
const MAX_STRING: usize = 32 * CHUNK_BYTES;
const MIN_BYTES: usize = 32 * CHUNK_BYTES;
const MAX_PAGES: usize = MAX_BYTES / CHUNK_BYTES;
const _: () = assert!(PagedVec::<Box<[u8]>, MAX_PAGES>::layout_supported());

#[derive(Debug, Default)]
pub struct ExecStrings {
    pages: PagedVec<Box<[u8]>, MAX_PAGES>,
    bytes: usize,
    count: usize,
}

impl ExecStrings {
    pub fn len(&self) -> usize {
        self.count
    }
    pub fn byte_len(&self) -> usize {
        self.bytes
    }

    fn byte(&self, index: usize) -> u8 {
        self.pages[index / CHUNK_BYTES][index % CHUNK_BYTES]
    }

    pub(crate) fn ranges(&self) -> StringRanges<'_> {
        StringRanges {
            strings: self,
            front: 0,
            back: self.bytes,
        }
    }

    pub(crate) fn chunks(&self, range: Range<usize>) -> StringChunks<'_> {
        assert!(range.start <= range.end && range.end <= self.bytes);
        StringChunks {
            strings: self,
            front: range.start,
            back: range.end,
        }
    }

    pub(crate) fn copy_range(&self, offset: usize, output: &mut [u8]) -> usize {
        let count = output.len().min(self.bytes.saturating_sub(offset));
        let mut done = 0;
        for part in self.chunks(offset.min(self.bytes)..offset.min(self.bytes) + count) {
            output[done..done + part.len()].copy_from_slice(part);
            done += part.len();
        }
        count
    }

    fn room(&mut self) -> Result<&mut [u8], SystemError> {
        if self.bytes >= MAX_BYTES {
            return Err(SystemError::E2BIG);
        }
        let page = self.bytes / CHUNK_BYTES;
        if page == self.pages.len() {
            self.pages.try_reserve(1)?;
            let mut storage =
                Box::<[u8]>::try_new_uninit_slice(CHUNK_BYTES).map_err(|_| SystemError::ENOMEM)?;
            // Initialize in place; never materialize a page-sized stack value.
            unsafe {
                storage
                    .as_mut_ptr()
                    .cast::<u8>()
                    .write_bytes(0, CHUNK_BYTES);
            }
            self.pages.push(unsafe { storage.assume_init() });
        }
        Ok(&mut self.pages[page][self.bytes % CHUNK_BYTES..])
    }

    fn append_bytes(&mut self, mut bytes: &[u8]) -> Result<(), SystemError> {
        while !bytes.is_empty() {
            let room = self.room()?;
            let take = room.len().min(bytes.len());
            room[..take].copy_from_slice(&bytes[..take]);
            self.bytes += take;
            bytes = &bytes[take..];
        }
        Ok(())
    }

    fn append_kernel(&mut self, string: &[u8], budget: &mut usize) -> Result<(), SystemError> {
        let size = string.len().checked_add(1).ok_or(SystemError::E2BIG)?;
        if size > MAX_STRING || size > *budget || string.contains(&0) {
            return Err(SystemError::E2BIG);
        }
        self.append_bytes(string)?;
        self.append_bytes(&[0])?;
        self.count += 1;
        *budget -= size;
        Ok(())
    }

    fn append_user(&mut self, address: usize, budget: &mut usize) -> Result<(), SystemError> {
        if address == 0 {
            return Err(SystemError::EFAULT);
        }
        let start = self.bytes;
        loop {
            let consumed = self.bytes - start;
            if consumed != 0 {
                checkpoint()?;
            }
            let address = address.checked_add(consumed).ok_or(SystemError::EFAULT)?;
            let mut maximum = (MAX_STRING - consumed).min(*budget);
            // Most argv/env entries are short. A small first probe avoids
            // copying almost a whole user page for every empty/tiny string.
            if consumed == 0 {
                maximum = maximum.min(64);
            }
            if maximum == 0 {
                return Err(SystemError::E2BIG);
            }
            let room = self.room()?;
            // A NUL at the end of an accessible page must not require the next
            // user page to be readable. Copy only the current page's remainder.
            let take = room
                .len()
                .min(CHUNK_BYTES - address % CHUNK_BYTES)
                .min(maximum);
            unsafe {
                copy_from_user_protected(&mut room[..take], VirtAddr::new(address))?;
            }
            let nul = room[..take].iter().position(|&byte| byte == 0);
            let copied = nul.map_or(take, |index| index + 1);
            self.bytes += copied;
            *budget -= copied;
            if nul.is_some() {
                self.count += 1;
                return Ok(());
            }
        }
    }

    fn append_tail(
        &mut self,
        source: &Self,
        start: usize,
        budget: &mut usize,
    ) -> Result<(), SystemError> {
        let length = source.bytes - start;
        if length > *budget {
            return Err(SystemError::E2BIG);
        }
        for part in source.chunks(start..source.bytes) {
            self.append_bytes(part)?;
        }
        self.count += source.count.saturating_sub(1);
        *budget -= length;
        Ok(())
    }
}

pub(crate) struct StringRanges<'a> {
    strings: &'a ExecStrings,
    front: usize,
    back: usize,
}
impl Iterator for StringRanges<'_> {
    type Item = Range<usize>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.front == self.back {
            return None;
        }
        let start = self.front;
        while self.strings.byte(self.front) != 0 {
            self.front += 1;
        }
        self.front += 1;
        Some(start..self.front)
    }
}
impl DoubleEndedIterator for StringRanges<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.front == self.back {
            return None;
        }
        let end = self.back;
        self.back -= 1; // trailing NUL
        while self.back > self.front && self.strings.byte(self.back - 1) != 0 {
            self.back -= 1;
        }
        Some(self.back..end)
    }
}

pub(crate) struct StringChunks<'a> {
    strings: &'a ExecStrings,
    front: usize,
    back: usize,
}
impl<'a> Iterator for StringChunks<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<Self::Item> {
        if self.front == self.back {
            return None;
        }
        let start = self.front;
        self.front = ((start / CHUNK_BYTES + 1) * CHUNK_BYTES).min(self.back);
        Some(
            &self.strings.pages[start / CHUNK_BYTES]
                [start % CHUNK_BYTES..start % CHUNK_BYTES + self.front - start],
        )
    }
}
impl DoubleEndedIterator for StringChunks<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.front == self.back {
            return None;
        }
        let end = self.back;
        self.back = ((end - 1) / CHUNK_BYTES * CHUNK_BYTES).max(self.front);
        let page = self.back / CHUNK_BYTES;
        Some(
            &self.strings.pages[page]
                [self.back % CHUNK_BYTES..self.back % CHUNK_BYTES + end - self.back],
        )
    }
}

fn checkpoint() -> Result<(), SystemError> {
    if Signal::fatal_signal_pending(&ProcessManager::current_pcb()) {
        return Err(SystemError::EINTR);
    }
    crate::sched::cond_resched();
    Ok(())
}

fn pointer_at(base: usize, index: usize) -> Result<usize, SystemError> {
    let address = index
        .checked_mul(size_of::<usize>())
        .and_then(|n| base.checked_add(n))
        .ok_or(SystemError::EFAULT)?;
    let mut bytes = [0u8; size_of::<usize>()];
    unsafe {
        copy_from_user_protected(&mut bytes, VirtAddr::new(address))?;
    }
    Ok(usize::from_ne_bytes(bytes))
}

fn count_user(base: usize, maximum: usize) -> Result<usize, SystemError> {
    if base == 0 {
        return Ok(0);
    }
    for index in 0..maximum {
        if index % 256 == 0 {
            checkpoint()?;
        }
        if pointer_at(base, index)? == 0 {
            return Ok(index);
        }
    }
    Err(SystemError::E2BIG)
}

/// Initial Linux string budget is retained through interpreter rewrites. The
/// original pointer reservation is not charged again for each shebang round.
pub(crate) struct ExecArguments {
    pub(crate) args: Arc<ExecStrings>,
    pub(crate) envs: Arc<ExecStrings>,
    string_limit: usize,
}

impl ExecArguments {
    fn limit() -> usize {
        let stack = ProcessManager::current_pcb()
            .get_rlimit(RLimitID::Stack)
            .rlim_cur;
        (stack / 4).min(MAX_BYTES as u64).max(MIN_BYTES as u64) as usize
    }

    fn string_limit(
        limit: usize,
        argc: usize,
        envc: usize,
        filename: &str,
    ) -> Result<usize, SystemError> {
        let pointers = argc
            .max(1)
            .checked_add(envc)
            .and_then(|n| n.checked_mul(size_of::<usize>()))
            .ok_or(SystemError::E2BIG)?;
        let strings = limit.checked_sub(pointers).ok_or(SystemError::E2BIG)?;
        let filename_bytes = filename.len().checked_add(1).ok_or(SystemError::E2BIG)?;
        if filename_bytes > MAX_STRING {
            return Err(SystemError::E2BIG);
        }
        strings
            .checked_sub(filename_bytes)
            .ok_or(SystemError::E2BIG)
    }

    pub(crate) fn from_user(filename: &str, argv: usize, envp: usize) -> Result<Self, SystemError> {
        let limit = Self::limit();
        let argc = count_user(argv, limit / size_of::<usize>())?;
        let envc = count_user(envp, limit / size_of::<usize>())?;
        let string_limit = Self::string_limit(limit, argc, envc, filename)?;
        let mut budget = string_limit;
        let mut envs = ExecStrings::default();
        let mut args = ExecStrings::default();
        for index in 0..envc {
            if index % 256 == 0 {
                checkpoint()?;
            }
            envs.append_user(pointer_at(envp, index)?, &mut budget)?;
        }
        for index in 0..argc {
            if index % 256 == 0 {
                checkpoint()?;
            }
            args.append_user(pointer_at(argv, index)?, &mut budget)?;
        }
        if argc == 0 {
            args.append_kernel(&[], &mut budget)?;
        }
        Self::freeze(args, envs, string_limit)
    }

    pub(crate) fn from_kernel(
        filename: &str,
        argv: &[CString],
        envp: &[CString],
    ) -> Result<Self, SystemError> {
        let string_limit = Self::string_limit(Self::limit(), argv.len(), envp.len(), filename)?;
        let mut budget = string_limit;
        let mut envs = ExecStrings::default();
        let mut args = ExecStrings::default();
        for string in envp {
            envs.append_kernel(string.as_bytes(), &mut budget)?;
        }
        for string in argv {
            args.append_kernel(string.as_bytes(), &mut budget)?;
        }
        if argv.is_empty() {
            args.append_kernel(&[], &mut budget)?;
        }
        Self::freeze(args, envs, string_limit)
    }

    fn freeze(
        args: ExecStrings,
        envs: ExecStrings,
        string_limit: usize,
    ) -> Result<Self, SystemError> {
        Ok(Self {
            args: Arc::try_new(args).map_err(|_| SystemError::ENOMEM)?,
            envs: Arc::try_new(envs).map_err(|_| SystemError::ENOMEM)?,
            string_limit,
        })
    }

    pub(crate) fn rewrite(&self, prefix: &[&[u8]]) -> Result<Self, SystemError> {
        let mut budget = self
            .string_limit
            .checked_sub(self.envs.byte_len())
            .ok_or(SystemError::E2BIG)?;
        let mut args = ExecStrings::default();
        for value in prefix {
            args.append_kernel(value, &mut budget)?;
        }
        let first = self.args.ranges().next().expect("exec argv normalized");
        args.append_tail(&self.args, first.end, &mut budget)?;
        Ok(Self {
            args: Arc::try_new(args).map_err(|_| SystemError::ENOMEM)?,
            envs: self.envs.clone(),
            string_limit: self.string_limit,
        })
    }

    /// Retain DragonOS's argv[0] symlink adjustment for actual bounded UTF-8
    /// paths. Arbitrary long/non-UTF8 argv entries remain ordinary byte strings.
    pub(crate) fn resolve_arg0(self) -> Result<Self, SystemError> {
        use crate::filesystem::vfs::{MAX_PATHLEN, VFS_MAX_FOLLOW_SYMLINK_TIMES};
        let first = self.args.ranges().next().expect("exec argv normalized");
        if first.len() <= 1 || first.len() > MAX_PATHLEN {
            return Ok(self);
        }
        let mut bytes = alloc::vec::Vec::new();
        bytes
            .try_reserve_exact(first.len())
            .map_err(|_| SystemError::ENOMEM)?;
        bytes.resize(first.len(), 0);
        self.args.copy_range(0, &mut bytes);
        let cstr = core::ffi::CStr::from_bytes_with_nul(&bytes).map_err(|_| SystemError::EINVAL)?;
        let Ok(path) = cstr.to_str() else {
            return Ok(self);
        };
        let root = ProcessManager::current_mntns().root_inode();
        if let Ok(inode) = root.lookup_follow_symlink2(path, VFS_MAX_FOLLOW_SYMLINK_TIMES, false) {
            if let Ok(real) = inode.absolute_path() {
                if real.as_bytes() != path.as_bytes() {
                    return self.rewrite(&[real.as_bytes()]);
                }
            }
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_ranges_and_chunks_preserve_cross_page_strings() {
        let mut bytes = ExecStrings::default();
        let mut budget = MAX_BYTES;
        let long = alloc::vec![b'a'; CHUNK_BYTES + 7];
        bytes.append_kernel(&long, &mut budget).unwrap();
        bytes.append_kernel(b"", &mut budget).unwrap();
        bytes.append_kernel(b"\xff\x80", &mut budget).unwrap();
        let forward: alloc::vec::Vec<_> = bytes.ranges().collect();
        let reverse: alloc::vec::Vec<_> = bytes.ranges().rev().collect();
        assert_eq!(forward.len(), 3);
        assert_eq!(
            reverse,
            forward
                .iter()
                .rev()
                .cloned()
                .collect::<alloc::vec::Vec<_>>()
        );
        for range in forward {
            let normal: alloc::vec::Vec<_> =
                bytes.chunks(range.clone()).flatten().copied().collect();
            let reverse: alloc::vec::Vec<_> = bytes
                .chunks(range)
                .rev()
                .flat_map(|part| part.iter().rev())
                .copied()
                .collect();
            assert_eq!(
                normal.iter().rev().copied().collect::<alloc::vec::Vec<_>>(),
                reverse
            );
            assert_eq!(normal.last(), Some(&0));
        }
        let mut partial = [0u8; 15];
        assert_eq!(bytes.copy_range(CHUNK_BYTES - 3, &mut partial), 15);
        assert_eq!(&partial[..3], b"aaa");
        assert_eq!(bytes.copy_range(usize::MAX, &mut partial), 0);
    }

    #[test]
    fn maximum_linux_byte_budget_has_no_directory_or_string_count_ceiling() {
        let mut bytes = ExecStrings::default();
        let mut budget = MAX_BYTES;
        let page_string = alloc::vec![b'x'; CHUNK_BYTES - 1];
        for _ in 0..MAX_PAGES {
            bytes.append_kernel(&page_string, &mut budget).unwrap();
        }
        assert_eq!(budget, 0);
        assert_eq!(bytes.byte_len(), MAX_BYTES);
        assert_eq!(bytes.pages.len(), MAX_PAGES);
        assert_eq!(
            bytes.append_kernel(b"", &mut budget),
            Err(SystemError::E2BIG)
        );
    }

    #[test]
    fn linux_budget_charges_empty_argv_pointer_and_filename() {
        assert_eq!(
            ExecArguments::string_limit(MIN_BYTES, 0, 0, "x").unwrap(),
            MIN_BYTES - size_of::<usize>() - 2
        );
        assert_eq!(
            ExecArguments::string_limit(MIN_BYTES, MIN_BYTES / size_of::<usize>(), 0, "x"),
            Err(SystemError::E2BIG)
        );
        assert_eq!(
            ExecArguments::string_limit(MIN_BYTES, usize::MAX, 1, "x"),
            Err(SystemError::E2BIG)
        );
    }

    #[test]
    fn single_string_limit_includes_nul_and_rewrite_refunds_arg_zero() {
        let mut bytes = ExecStrings::default();
        let mut budget = MAX_BYTES;
        bytes
            .append_kernel(&alloc::vec![b'a'; MAX_STRING - 1], &mut budget)
            .unwrap();
        assert_eq!(
            bytes.append_kernel(&alloc::vec![b'a'; MAX_STRING], &mut budget),
            Err(SystemError::E2BIG)
        );
        let mut args = ExecStrings::default();
        let mut budget = 20;
        args.append_kernel(b"old", &mut budget).unwrap();
        args.append_kernel(b"tail", &mut budget).unwrap();
        let args = ExecArguments::freeze(args, ExecStrings::default(), 20).unwrap();
        let rewritten = args.rewrite(&[b"new", b"script"]).unwrap();
        let mut copied = [0u8; 20];
        let length = rewritten.args.copy_range(0, &mut copied);
        assert_eq!(&copied[..length], b"new\0script\0tail\0");
        assert_eq!(rewritten.args.len(), 3);
    }
}
