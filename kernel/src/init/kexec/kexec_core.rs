use super::{
    kexec_segment_buf, KexecFlags, KexecSegment, Kimage, KimageEntry, IND_DESTINATION, IND_DONE,
    IND_INDIRECTION, IND_SOURCE, KEXEC_IMAGE,
};
use crate::arch::mm::LockedFrameAllocator;
use crate::arch::CurrentIrqArch;
use crate::arch::KexecArch;
use crate::arch::MMArch;
use crate::exception::InterruptArch;
use crate::libs::spinlock::SpinLock;
use crate::mm::page::{page_manager_lock, Page, PageFlags, PageType};
use crate::mm::MemoryManagementArch;
use crate::syscall::user_access::UserBufferReader;
use alloc::rc::Rc;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cmp::min;
use core::mem::size_of;
use system_error::SystemError;

pub fn do_kexec_load(
    entry: usize,
    nr_segments: usize,
    ksegments: &[KexecSegment],
    flags: usize,
) -> Result<usize, SystemError> {
    let _flags = KexecFlags::from_bits_truncate(flags as u64);

    if nr_segments == 0 {
        /* Uninstall image */
        log::warn!("kexec: nr_segments == 0, should Uninstall, not impled!");
        return Ok(0);
    }

    let image = kimage_alloc_init(entry, nr_segments, ksegments, flags)?;

    // load segment 的解析: https://zhuanlan.zhihu.com/p/105284305
    for i in 0..nr_segments {
        kimage_load_normal_segment(image.clone(), i)?;
    }

    kimage_terminate(image.clone());

    KexecArch::init_pgtable(image.clone())?;

    KexecArch::machine_kexec_prepare(image.clone())?;

    unsafe {
        KEXEC_IMAGE = Some(image.clone());
    }

    Ok(0)
}

pub fn kimage_alloc_init(
    entry: usize,
    nr_segments: usize,
    ksegments: &[KexecSegment],
    _flags: usize,
) -> Result<Rc<SpinLock<Kimage>>, SystemError> {
    let image = Rc::new(SpinLock::new(Kimage {
        head: 0,
        entry: core::ptr::null_mut(),
        last_entry: core::ptr::null_mut(),
        start: 0,
        control_code_page: None,
        stack_page: None,
        nr_segments: 0,
        segment: [KexecSegment {
            buffer: kexec_segment_buf {
                buf: core::ptr::null_mut(),
            },
            bufsz: 0,
            mem: 0,
            memsz: 0,
        }; super::KEXEC_SEGMENT_MAX],
        pages: Vec::new(),
        pgd: 0,
    }));

    image.lock().start = entry;
    image.lock().nr_segments = nr_segments;

    {
        let mut locked_image = image.lock();
        locked_image.entry = &mut locked_image.head as *mut KimageEntry;
        locked_image.last_entry = &mut locked_image.head as *mut KimageEntry;
    }

    image.lock().segment[..ksegments.len()].copy_from_slice(ksegments);

    let temp_c = kimage_alloc_pages(image.clone(), 0, false)?;
    image.lock().control_code_page = temp_c.clone();

    let temp_st = kimage_alloc_pages(image.clone(), 0, true)?;
    image.lock().stack_page = temp_st.clone();

    Ok(image)
}

pub fn kimage_alloc_pages(
    kimage: Rc<SpinLock<Kimage>>,
    order: usize,
    store: bool,
) -> Result<Option<Arc<Page>>, SystemError> {
    let mut _page = None;
    let mut extra_pages: Vec<Arc<Page>> = Vec::new();
    let mut alloc = page_manager_lock();

    let _count = 1 << order;

    // 目前只分配一个页面, 后面改成多个, 使用 order 控制
    loop {
        let p = alloc.create_one_page(
            PageType::Normal,
            PageFlags::PG_RESERVED | PageFlags::PG_PRIVATE,
            &mut LockedFrameAllocator,
        )?;

        if check_isdst(kimage.clone(), p.clone()) {
            extra_pages.push(p);
            continue;
        }
        if store {
            kimage.lock().pages.push(p.clone());
        }
        _page = Some(p.clone());
        break;
    }

    for p in extra_pages {
        alloc.remove_page(&p.phys_address());
    }

    Ok(_page)
}

pub fn check_isdst(kimage: Rc<SpinLock<Kimage>>, page: Arc<Page>) -> bool {
    let nr_segments = kimage.lock().nr_segments;
    let segments = kimage.lock().segment;
    let paddr = page.phys_address().data();

    for seg in segments.iter().take(nr_segments) {
        let mem = seg.mem - MMArch::PAGE_SIZE;
        let memend = mem + seg.memsz;
        if paddr >= mem && paddr <= memend {
            return true;
        }
    }

    false
}

pub fn kernel_kexec() {
    unsafe {
        if KEXEC_IMAGE.is_none() {
            return;
        }
        CurrentIrqArch::interrupt_disable();

        let kimage = KEXEC_IMAGE.clone().unwrap().clone();

        // TODO:像 linux 一样添加更多的设置

        KexecArch::machine_kexec(kimage);
    }
}

pub fn kimage_add_entry(
    kimage: Rc<SpinLock<Kimage>>,
    entry: KimageEntry,
) -> Result<(), SystemError> {
    unsafe {
        if *kimage.lock().entry != 0 {
            let t = kimage.lock().entry.add(1);
            kimage.lock().entry = t;
        }

        let k_entry = kimage.lock().entry;
        let k_last_entry = kimage.lock().last_entry;
        if k_entry == k_last_entry {
            let page = kimage_alloc_pages(kimage.clone(), 0, true)?.unwrap();

            let ind_page =
                MMArch::phys_2_virt(page.phys_address()).unwrap().data() as *mut KimageEntry;

            let page_phys_usize = page.phys_address().data() | IND_INDIRECTION;

            *kimage.lock().entry = page_phys_usize;
            kimage.lock().entry = ind_page;
            let ind_page = ind_page.add((MMArch::PAGE_SIZE / size_of::<KimageEntry>()) - 1);
            kimage.lock().last_entry = ind_page;
        }

        *kimage.lock().entry = entry;
        let t = kimage.lock().entry.add(1);
        kimage.lock().entry = t;
        *kimage.lock().entry = 0;
    }
    Ok(())
}

pub fn kimage_set_destination(
    kimage: Rc<SpinLock<Kimage>>,
    destination: usize,
) -> Result<(), SystemError> {
    let d = destination & MMArch::PAGE_MASK;
    kimage_add_entry(kimage.clone(), d | IND_DESTINATION)
}

pub fn kimage_add_page(kimage: Rc<SpinLock<Kimage>>, page: usize) -> Result<(), SystemError> {
    let p = page & MMArch::PAGE_MASK;
    kimage_add_entry(kimage.clone(), p | IND_SOURCE)
}

pub fn kimage_load_normal_segment(
    kimage: Rc<SpinLock<Kimage>>,
    index: usize,
) -> Result<(), SystemError> {
    let segment = kimage.lock().segment[index];

    let mut maddr = segment.mem;
    let mut mbytes: isize = segment.memsz as isize;
    let mut buf = unsafe { segment.buffer.buf } as *mut u8;
    let mut ubytes = segment.bufsz;

    kimage_set_destination(kimage.clone(), maddr)?;

    loop {
        let page = (kimage_alloc_pages(kimage.clone(), 0, true)?).unwrap();
        kimage_add_page(kimage.clone(), page.phys_address().data())?;

        let mut virt_data = unsafe { MMArch::phys_2_virt(page.phys_address()).unwrap().data() };
        virt_data += maddr & !(MMArch::PAGE_MASK);
        let mchunk = min(
            mbytes as usize,
            MMArch::PAGE_SIZE - (maddr & !MMArch::PAGE_MASK),
        );
        let uchunk = min(ubytes, mchunk);

        if uchunk != 0 {
            let usegments_buf = UserBufferReader::new::<u8>(buf, uchunk, true)?;
            let ksegment: &[u8] = usegments_buf.read_from_user(0)?;
            unsafe { core::ptr::copy(ksegment.as_ptr(), virt_data as *mut u8, uchunk) };

            ubytes -= uchunk;
            unsafe { buf = buf.add(uchunk) };
        }

        maddr += mchunk;
        mbytes -= mchunk as isize;

        if mbytes <= 0 {
            return Ok(());
        }
    }
}

pub fn kimage_terminate(kimage: Rc<SpinLock<Kimage>>) {
    unsafe {
        if *kimage.lock().entry != 0 {
            let t = kimage.lock().entry.add(1);
            kimage.lock().entry = t;
        }
        *kimage.lock().entry = IND_DONE;
    }
}
