use super::{PerfEventOps, Result};
use crate::arch::mm::LockedFrameAllocator;
use crate::arch::MMArch;
use crate::filesystem::page_cache::PageCache;
use crate::filesystem::vfs::{FilePrivateData, FileSystem, IndexNode};
use crate::include::bindings::linux_bpf::{
    perf_event_header, perf_event_mmap_page, perf_event_type,
};
use crate::libs::align::page_align_up;
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::mm::allocator::page_frame::{PageFrameCount, PhysPageFrame};
use crate::mm::page::{page_manager_lock_irqsave, PageFlags, PageType};
use crate::mm::{MemoryManagementArch, PhysAddr};
use crate::perf::util::{LostSamples, PerfProbeArgs, PerfSample, SampleHeader};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt::Debug;
use system_error::SystemError;
const PAGE_SIZE: usize = MMArch::PAGE_SIZE;
#[derive(Debug)]
pub struct BpfPerfEvent {
    _args: PerfProbeArgs,
    data: SpinLock<BpfPerfEventData>,
}

#[derive(Debug)]
pub struct BpfPerfEventData {
    enabled: bool,
    mmap_page: RingPage,
    page_cache: Arc<PageCache>,
    offset: usize,
}

#[derive(Debug)]
pub struct RingPage {
    size: usize,
    ptr: usize,
    data_region_size: usize,
    lost: usize,
    phys_addr: PhysAddr,
}

impl RingPage {
    pub fn empty() -> Self {
        RingPage {
            ptr: 0,
            size: 0,
            data_region_size: 0,
            lost: 0,
            phys_addr: PhysAddr::new(0),
        }
    }

    pub fn new_init(start: usize, len: usize, phys_addr: PhysAddr) -> Self {
        Self::init(start as _, len, phys_addr)
    }

    fn init(ptr: *mut u8, size: usize, phys_addr: PhysAddr) -> Self {
        assert_eq!(size % PAGE_SIZE, 0);
        assert!(size / PAGE_SIZE >= 2);
        // The first page will be filled with perf_event_mmap_page
        unsafe {
            let perf_event_mmap_page = &mut *(ptr as *mut perf_event_mmap_page);
            perf_event_mmap_page.data_offset = PAGE_SIZE as u64;
            perf_event_mmap_page.data_size = (size - PAGE_SIZE) as u64;
            // user will read sample or lost record from data_tail
            perf_event_mmap_page.data_tail = 0;
            // kernel will write sample or lost record from data_head
            perf_event_mmap_page.data_head = 0;
            // It is a ring buffer.
        }
        RingPage {
            ptr: ptr as usize,
            size,
            data_region_size: size - PAGE_SIZE,
            lost: 0,
            phys_addr,
        }
    }

    #[inline]
    fn can_write(&self, data_size: usize, data_tail: usize, data_head: usize) -> bool {
        let capacity = self.data_region_size - data_head + data_tail;
        data_size <= capacity
    }

    pub fn write_event(&mut self, data: &[u8]) -> Result<()> {
        let data_tail = unsafe { &mut (*(self.ptr as *mut perf_event_mmap_page)).data_tail };
        let data_head = unsafe { &mut (*(self.ptr as *mut perf_event_mmap_page)).data_head };

        // user lib will update the tail after read the data,but it will not % data_region_size
        let perf_header_size = size_of::<perf_event_header>();
        let can_write_perf_header =
            self.can_write(perf_header_size, *data_tail as usize, *data_head as usize);

        if can_write_perf_header {
            let can_write_lost_record = self.can_write(
                size_of::<LostSamples>(),
                *data_tail as usize,
                *data_head as usize,
            );
            // if there is lost record, we need to write the lost record first
            if self.lost > 0 && can_write_lost_record {
                let new_data_head = self.write_lost(*data_head as usize)?;
                *data_head = new_data_head as u64;
                // log::info!(
                //     "Write lost record: {}, data_tail: {}, new_data_head: {}",
                //     self.lost,
                //     *data_tail,
                //     *data_head
                // );
                self.lost = 0;
                // try to write the event again
                return self.write_event(data);
            }
            let sample_size = PerfSample::calculate_size(data.len());
            let can_write_sample =
                self.can_write(sample_size, *data_tail as usize, *data_head as usize);
            // log::error!(
            //     "can_write_sample: {}, data_tail: {}, data_head: {}, data.len(): {}, region_size: {}",
            //     can_write_sample,
            //     *data_tail,
            //     *data_head,
            //     data.len(),
            //     self.data_region_size
            // );
            if can_write_sample {
                let new_data_head = self.write_sample(data, *data_head as usize)?;
                *data_head = new_data_head as u64;
                // log::info!(
                //     "Write sample record, data_tail: {}, new_data_head: {}",
                //     *data_tail,
                //     *data_head
                // );
            } else {
                self.lost += 1;
            }
        } else {
            self.lost += 1;
        }
        Ok(())
    }

    /// Write any data to the page.
    ///
    /// Return the new data_head
    fn write_any(&mut self, data: &[u8], data_head: usize) -> Result<()> {
        let data_region_len = self.data_region_size;
        let data_region = self.as_mut_slice()[PAGE_SIZE..].as_mut();
        let data_len = data.len();
        let start = data_head % data_region_len;
        let end = (data_head + data_len) % data_region_len;
        if start < end {
            data_region[start..end].copy_from_slice(data);
        } else {
            let first_len = data_region_len - start;
            data_region[start..start + first_len].copy_from_slice(&data[..first_len]);
            data_region[0..end].copy_from_slice(&data[first_len..]);
        }
        Ok(())
    }
    #[inline]
    fn fill_size(&self, data_head_mod: usize) -> usize {
        if self.data_region_size - data_head_mod < size_of::<perf_event_header>() {
            // The remaining space is not enough to write the perf_event_header
            // We need to fill the remaining space with 0
            self.data_region_size - data_head_mod
        } else {
            0
        }
    }

    /// Write a sample to the page.
    fn write_sample(&mut self, data: &[u8], data_head: usize) -> Result<usize> {
        let sample_size = PerfSample::calculate_size(data.len());
        let maybe_end = (data_head + sample_size) % self.data_region_size;
        let fill_size = self.fill_size(maybe_end);
        let perf_sample = PerfSample {
            s_hdr: SampleHeader {
                header: perf_event_header {
                    type_: perf_event_type::PERF_RECORD_SAMPLE as u32,
                    misc: 0,
                    size: size_of::<SampleHeader>() as u16 + data.len() as u16 + fill_size as u16,
                },
                size: data.len() as u32,
            },
            value: data,
        };
        self.write_any(perf_sample.s_hdr.as_bytes(), data_head)?;
        self.write_any(perf_sample.value, data_head + size_of::<SampleHeader>())?;
        Ok(data_head + sample_size + fill_size)
    }

    /// Write a lost record to the page.
    ///
    /// Return the new data_head
    fn write_lost(&mut self, data_head: usize) -> Result<usize> {
        let maybe_end = (data_head + size_of::<LostSamples>()) % self.data_region_size;
        let fill_size = self.fill_size(maybe_end);
        let lost = LostSamples {
            header: perf_event_header {
                type_: perf_event_type::PERF_RECORD_LOST as u32,
                misc: 0,
                size: size_of::<LostSamples>() as u16 + fill_size as u16,
            },
            id: 0,
            count: self.lost as u64,
        };
        self.write_any(lost.as_bytes(), data_head)?;
        Ok(data_head + size_of::<LostSamples>() + fill_size)
    }

    pub fn readable(&self) -> bool {
        let data_tail = unsafe { &(*(self.ptr as *mut perf_event_mmap_page)).data_tail };
        let data_head = unsafe { &(*(self.ptr as *mut perf_event_mmap_page)).data_head };
        data_tail != data_head
    }

    #[allow(dead_code)]
    pub fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.ptr as *const u8, self.size) }
    }
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.ptr as *mut u8, self.size) }
    }
}

impl BpfPerfEvent {
    pub fn new(args: PerfProbeArgs) -> Self {
        BpfPerfEvent {
            _args: args,
            data: SpinLock::new(BpfPerfEventData {
                enabled: false,
                mmap_page: RingPage::empty(),
                page_cache: PageCache::new(None),
                offset: 0,
            }),
        }
    }
    pub fn do_mmap(&self, _start: usize, len: usize, offset: usize) -> Result<()> {
        let mut data = self.data.lock();
        let mut page_manager_guard = page_manager_lock_irqsave();
        let (phy_addr, pages) = page_manager_guard.create_pages(
            PageType::Normal,
            PageFlags::PG_UNEVICTABLE,
            &mut LockedFrameAllocator,
            PageFrameCount::new(page_align_up(len) / PAGE_SIZE),
        )?;
        for i in 0..pages.len() {
            data.page_cache
                .lock_irqsave()
                .add_page(i, pages.get(i).unwrap());
        }
        let virt_addr = unsafe { MMArch::phys_2_virt(phy_addr) }.ok_or(SystemError::EFAULT)?;
        // create mmap page
        let mmap_page = RingPage::new_init(virt_addr.data(), len, phy_addr);
        data.mmap_page = mmap_page;
        data.offset = offset;
        Ok(())
    }

    pub fn write_event(&self, data: &[u8]) -> Result<()> {
        let mut inner_data = self.data.lock();
        inner_data.mmap_page.write_event(data)?;
        Ok(())
    }
}

impl Drop for BpfPerfEvent {
    fn drop(&mut self) {
        let mut page_manager_guard = page_manager_lock_irqsave();
        let data = self.data.lock();
        let phy_addr = data.mmap_page.phys_addr;
        let len = data.mmap_page.size;
        let page_count = PageFrameCount::new(len / PAGE_SIZE);
        let mut cur_phys = PhysPageFrame::new(phy_addr);
        for _ in 0..page_count.data() {
            page_manager_guard.remove_page(&cur_phys.phys_address());
            cur_phys = cur_phys.next();
        }
    }
}

impl IndexNode for BpfPerfEvent {
    fn mmap(&self, start: usize, len: usize, offset: usize) -> Result<()> {
        self.do_mmap(start, len, offset)
    }

    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize> {
        panic!("PerfEventInode does not support read")
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize> {
        panic!("PerfEventInode does not support write")
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        panic!("PerfEventInode does not have a filesystem")
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn list(&self) -> Result<Vec<String>> {
        Err(SystemError::ENOSYS)
    }

    fn page_cache(&self) -> Option<Arc<PageCache>> {
        Some(self.data.lock().page_cache.clone())
    }
}

impl PerfEventOps for BpfPerfEvent {
    fn enable(&self) -> Result<()> {
        self.data.lock().enabled = true;
        Ok(())
    }
    fn disable(&self) -> Result<()> {
        self.data.lock().enabled = false;
        Ok(())
    }
    fn readable(&self) -> bool {
        self.data.lock().mmap_page.readable()
    }
}

pub fn perf_event_open_bpf(args: PerfProbeArgs) -> BpfPerfEvent {
    BpfPerfEvent::new(args)
}
