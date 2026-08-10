//! Host-only crash recovery coverage for the existing range-allocation path.
//!
//! This binary is deliberately separate from `ext4_test`'s broad smoke suite:
//! it makes many fresh images and injects a process-level power loss before
//! every persistence operation in `prepare_buffered_write()`. The block device
//! itself, rather than the filesystem, owns the fault model.

#[path = "../block_file.rs"]
#[allow(dead_code)]
mod block_file;

use another_ext4::{DelallocAppendBlockWriteback, Ext4, InodeMode, EXT4_ROOT_INO};
use block_file::{CrashBlockFile, CrashDeviceOperation, SimulatedPowerLoss};
use std::fs::{self, File};
use std::mem::ManuallyDrop;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::process::Command;
use std::sync::Arc;

const ROOT_INO: u32 = EXT4_ROOT_INO;
const IMAGE_SIZE: u64 = 64 * 1024 * 1024;
const WRITE_LEN: usize = 4 * 4096;
const PAYLOAD: &[u8; WRITE_LEN] = &[0xa5; WRITE_LEN];
const INLINE_EXTENT_CAPACITY: usize = 4;
// `ExtentHeader` (12 bytes) plus 340 extents (12 bytes each) leaves the
// four-byte metadata-checksum tail at the end of a 4 KiB extent block.
const EXTERNAL_LEAF_EXTENT_CAPACITY: usize = 340;
const DELALLOC_BLOCK_LEN: usize = 4096;
const DELALLOC_PAYLOAD: [u8; DELALLOC_BLOCK_LEN] = [0xb7; DELALLOC_BLOCK_LEN];
// A child which reaches an armed persistence operation terminates from its
// panic hook, before Rust can unwind any linear mapper capability.  Keeping
// this distinct from ordinary test failures lets the parent prove that the
// child really modelled a power cut, rather than merely accepting any abort.
const DELALLOC_POWER_LOSS_EXIT_CODE: i32 = 86;

#[derive(Clone, Copy, Debug)]
enum ImageKind {
    Journal,
    NoJournal,
}

#[derive(Clone, Copy, Debug)]
enum PersistenceModel {
    WriteBack,
    WriteThrough,
}

impl PersistenceModel {
    fn name(self) -> &'static str {
        match self {
            Self::WriteBack => "writeback",
            Self::WriteThrough => "writethrough",
        }
    }

    fn configure(self, device: &CrashBlockFile) {
        device.set_write_through(matches!(self, Self::WriteThrough));
    }

    fn parse(name: &str) -> Self {
        match name {
            "writeback" => Self::WriteBack,
            "writethrough" => Self::WriteThrough,
            _ => panic!("unknown persistence model for crash child: {name}"),
        }
    }
}

#[derive(Clone, Copy)]
enum DelallocCrashCase {
    RawMappedTail,
    ProductionAppend,
    ProductionProjectedSingle,
    ProductionProjectedMultiEntry,
    ProductionProjectedSparse,
}

impl DelallocCrashCase {
    fn name(self) -> &'static str {
        match self {
            Self::RawMappedTail => "raw-mapped-tail",
            Self::ProductionAppend => "production-append",
            Self::ProductionProjectedSingle => "production-projected-single",
            Self::ProductionProjectedMultiEntry => "production-projected-multi-entry",
            Self::ProductionProjectedSparse => "production-projected-sparse",
        }
    }

    fn parse(name: &str) -> Self {
        match name {
            "raw-mapped-tail" => Self::RawMappedTail,
            "production-append" => Self::ProductionAppend,
            "production-projected-single" => Self::ProductionProjectedSingle,
            "production-projected-multi-entry" => Self::ProductionProjectedMultiEntry,
            "production-projected-sparse" => Self::ProductionProjectedSparse,
            _ => panic!("unknown delayed-allocation crash child case: {name}"),
        }
    }
}

#[derive(Clone, Copy)]
struct ProductionProjectedStep {
    offset: usize,
    expected_durable_eof_before: u64,
    durable_eof_after: u64,
    fill: u8,
}

fn production_projected_steps(
    case: DelallocCrashCase,
    offset: usize,
    durable_eof: u64,
) -> Vec<ProductionProjectedStep> {
    match case {
        DelallocCrashCase::ProductionProjectedSingle => vec![ProductionProjectedStep {
            offset,
            expected_durable_eof_before: offset as u64,
            durable_eof_after: durable_eof,
            fill: DELALLOC_PAYLOAD[0],
        }],
        DelallocCrashCase::ProductionProjectedMultiEntry => vec![
            ProductionProjectedStep {
                offset: 0,
                expected_durable_eof_before: 0,
                durable_eof_after: DELALLOC_BLOCK_LEN as u64,
                fill: 0xb7,
            },
            ProductionProjectedStep {
                offset: DELALLOC_BLOCK_LEN,
                expected_durable_eof_before: DELALLOC_BLOCK_LEN as u64,
                durable_eof_after: (2 * DELALLOC_BLOCK_LEN) as u64,
                fill: 0x5c,
            },
        ],
        DelallocCrashCase::ProductionProjectedSparse => vec![ProductionProjectedStep {
            offset: 3 * DELALLOC_BLOCK_LEN,
            expected_durable_eof_before: 0,
            durable_eof_after: (4 * DELALLOC_BLOCK_LEN) as u64,
            fill: 0x6d,
        }],
        _ => panic!("non-projected delayed-allocation case has no projected steps"),
    }
}

fn run_production_projected_steps(
    ext4: &Ext4,
    inode: u32,
    steps: &[ProductionProjectedStep],
    before_submit: impl FnOnce(),
) {
    if steps.is_empty() {
        return;
    }
    let authority = ext4
        .delalloc_append_mapper_authority()
        .expect("projected production mapper authority issue failed");
    let mut pool = ext4
        .create_delalloc_extent_node_pool_authorized(&authority, inode)
        .expect("projected production pool creation failed");
    let mut reservations = Vec::with_capacity(steps.len());
    for step in steps {
        reservations.push(
            ext4.reserve_delalloc_append_block_projected_authorized(
                &authority,
                inode,
                step.offset,
                step.expected_durable_eof_before,
                &mut pool,
            )
            .expect("projected production reservation failed"),
        );
    }
    before_submit();
    let payloads: Vec<[u8; DELALLOC_BLOCK_LEN]> = steps
        .iter()
        .map(|step| [step.fill; DELALLOC_BLOCK_LEN])
        .collect();
    let publications: Vec<_> = steps
        .iter()
        .zip(payloads.iter())
        .map(
            |(step, payload)| another_ext4::DelallocAppendBlockPublication {
                payload,
                durable_eof: step.durable_eof_after,
                mtime: Some(96),
                ctime: Some(96),
            },
        )
        .collect();
    let mut reservation_refs: Vec<_> = reservations.iter_mut().collect();
    assert_eq!(
        ext4.submit_delalloc_append_batch_authorized_with_pool(
            &authority,
            &mut reservation_refs,
            &publications,
            &mut pool,
        ),
        another_ext4::DelallocAppendBlockSubmitOutcome::Completed,
        "projected production append batch did not complete"
    );
    ext4.release_delalloc_extent_node_pool_authorized(&authority, &mut pool)
        .expect("projected production pool release failed");
}

impl ImageKind {
    fn name(self) -> &'static str {
        match self {
            Self::Journal => "journal",
            Self::NoJournal => "nojournal",
        }
    }

    fn mkfs_features(self) -> &'static str {
        match self {
            // another_ext4 deliberately rejects orphan-file recovery, so use
            // the same feature subset as the existing host smoke tests.
            Self::Journal => "^orphan_file",
            Self::NoJournal => "^has_journal,^orphan_file,metadata_csum_seed",
        }
    }
}

fn remove_if_exists(path: &str) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("remove {path} failed: {error}"),
    }
}

fn make_image(path: &str, kind: ImageKind) {
    remove_if_exists(path);
    File::create(path)
        .and_then(|file| file.set_len(IMAGE_SIZE))
        .unwrap_or_else(|error| panic!("create {path} failed: {error}"));
    let output = Command::new("mkfs.ext4")
        .args([
            "-F",
            "-b",
            "4096",
            "-I",
            "256",
            "-O",
            kind.mkfs_features(),
            path,
        ])
        .output()
        .expect("mkfs.ext4 failed to start");
    assert!(
        output.status.success(),
        "mkfs.ext4 failed for {} image:\n{}\n{}",
        kind.name(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn create_clean_seed(path: &str, kind: ImageKind) -> u32 {
    make_image(path, kind);
    let device = Arc::new(CrashBlockFile::new(path));
    let ext4 = Ext4::load_writable(device).expect("seed image mount failed");
    let inode = ext4
        .generic_create(
            ROOT_INO,
            "range-target",
            InodeMode::FILE | InodeMode::ALL_RWX,
        )
        .expect("seed file create failed");
    ext4.shutdown_writable()
        .expect("seed image clean shutdown failed");
    inode
}

fn copy_seed(seed: &str, work: &str) {
    remove_if_exists(work);
    fs::copy(seed, work).unwrap_or_else(|error| panic!("copy {seed} to {work} failed: {error}"));
}

fn e2fsck(path: &str) -> std::process::Output {
    Command::new("e2fsck")
        .args(["-fn", path])
        .output()
        .expect("e2fsck failed to start")
}

fn assert_clean_e2fsck(path: &str, context: &str) {
    let output = e2fsck(path);
    assert!(
        output.status.success(),
        "{context}: e2fsck -fn failed for {path}:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn output_text(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn nojournal_e2fsck_requires_only_bitmap_repair(path: &str, crash_point: usize) -> bool {
    let output = e2fsck(path);
    if output.status.success() {
        return false;
    }
    let report = output_text(&output);
    assert!(
        report.contains("Block bitmap differences:"),
        "nojournal crash point {crash_point} has unexpected e2fsck failure:\n{report}"
    );
    for forbidden in [
        "Illegal block",
        "multiply-claimed",
        "Multiply-claimed",
        "bad blocks",
        "Inode bitmap differences",
        "Free inodes count wrong",
    ] {
        assert!(
            !report.contains(forbidden),
            "nojournal crash point {crash_point} is not a pure allocation leak ({forbidden}):\n{report}"
        );
    }
    true
}

fn count_prepare_operations(
    work: &str,
    inode: u32,
    persistence: PersistenceModel,
) -> Vec<CrashDeviceOperation> {
    let device = Arc::new(CrashBlockFile::new(work));
    let ext4 = Ext4::load_writable(device.clone()).expect("count image mount failed");
    persistence.configure(&device);
    device.reset_operation_log();
    ext4.prepare_buffered_write(inode, 0, WRITE_LEN, WRITE_LEN as u64, None)
        .expect("range preparation used to discover crash points failed");
    let operations = device.operations();
    ext4.shutdown_writable()
        .expect("count image clean shutdown failed");
    assert!(
        !operations.is_empty(),
        "range preparation issued no persistence operations"
    );
    operations
}

fn assert_simulated_power_loss(result: Box<dyn core::any::Any + Send>, expected: usize) {
    let power_loss = result
        .downcast::<SimulatedPowerLoss>()
        .unwrap_or_else(|_| panic!("unexpected panic payload at crash point {expected}"));
    assert_eq!(power_loss.operation, expected);
}

fn expect_power_loss<T>(crash_point: usize, operation: impl FnOnce() -> T) {
    // The injected panic represents a machine stop, not a Rust error path.
    // Suppress the otherwise misleading panic line because the caller checks
    // the payload and immediately discards the volatile device epoch.
    let saved_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = catch_unwind(AssertUnwindSafe(operation));
    std::panic::set_hook(saved_hook);
    let payload = match result {
        Ok(_) => panic!("armed crash point let filesystem operation return"),
        Err(payload) => payload,
    };
    assert_simulated_power_loss(payload, crash_point);
}

/// Run an operation in a fresh process and turn the injected panic into an
/// immediate exit.  `catch_unwind()` is intentionally unsuitable once an
/// operation owns a linear delayed-allocation lease or consumption: unwinding
/// that stack would execute its fail-stop Drop implementation, whereas a real
/// machine stop executes no destructors.  The file-backed device has no
/// volatile state shared with the parent, so process exit naturally discards
/// every write after the last completed flush.
fn run_delalloc_power_loss_subprocess(
    case: DelallocCrashCase,
    persistence: PersistenceModel,
    work: &str,
    inode: u32,
    offset: usize,
    durable_eof: u64,
    crash_point: usize,
) {
    let executable = std::env::current_exe().expect("locate recovery test executable failed");
    let output = Command::new(executable)
        .args([
            "--delalloc-power-loss-child",
            case.name(),
            persistence.name(),
            work,
            &inode.to_string(),
            &offset.to_string(),
            &durable_eof.to_string(),
            &crash_point.to_string(),
        ])
        .output()
        .expect("start delayed-allocation crash child failed");
    assert_eq!(
        output.status.code(),
        Some(DELALLOC_POWER_LOSS_EXIT_CODE),
        "delayed-allocation crash child did not reach persistence point {crash_point}: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Dispatch the isolated crash process before the ordinary matrix runner
/// starts.  The custom hook preserves the exact simulated-power-loss identity
/// across the process boundary without attempting to unwind a partially
/// materialised mapper transaction.
fn run_delalloc_power_loss_child_from_args() -> bool {
    let mut args = std::env::args().skip(1);
    let Some(flag) = args.next() else {
        return false;
    };
    if flag != "--delalloc-power-loss-child" {
        return false;
    }
    let case = DelallocCrashCase::parse(
        &args
            .next()
            .expect("delayed-allocation crash child missing case"),
    );
    let persistence = PersistenceModel::parse(
        &args
            .next()
            .expect("delayed-allocation crash child missing persistence model"),
    );
    let work = args
        .next()
        .expect("delayed-allocation crash child missing work image");
    let inode = args
        .next()
        .expect("delayed-allocation crash child missing inode")
        .parse::<u32>()
        .expect("delayed-allocation crash child inode is invalid");
    let offset = args
        .next()
        .expect("delayed-allocation crash child missing offset")
        .parse::<usize>()
        .expect("delayed-allocation crash child offset is invalid");
    let durable_eof = args
        .next()
        .expect("delayed-allocation crash child missing durable EOF")
        .parse::<u64>()
        .expect("delayed-allocation crash child durable EOF is invalid");
    let crash_point = args
        .next()
        .expect("delayed-allocation crash child missing crash point")
        .parse::<usize>()
        .expect("delayed-allocation crash child crash point is invalid");
    assert!(
        args.next().is_none(),
        "delayed-allocation crash child received unexpected arguments"
    );

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if info.payload().is::<SimulatedPowerLoss>() {
            std::process::exit(DELALLOC_POWER_LOSS_EXIT_CODE);
        }
        previous_hook(info);
    }));

    let device = Arc::new(CrashBlockFile::new(&work));
    persistence.configure(&device);
    let ext4 =
        Ext4::load_writable(device.clone()).expect("delayed-allocation crash child mount failed");
    match case {
        DelallocCrashCase::RawMappedTail => {
            assert_eq!(offset, 0, "raw mapper crash child received an offset");
            assert_eq!(durable_eof, 0, "raw mapper crash child received an EOF");
            device.reset_operation_log();
            device.arm_power_loss_at(crash_point);
            run_reserved_delalloc_lifecycle(&ext4, inode, 0, &DELALLOC_PAYLOAD);
        }
        DelallocCrashCase::ProductionAppend => {
            let mut lease = ext4
                .reserve_delalloc_append_block(inode, offset)
                .expect("production delayed crash child reservation failed");
            device.reset_operation_log();
            device.arm_power_loss_at(crash_point);
            production_delalloc_append_block(&ext4, inode, &mut lease, offset, durable_eof)
                .expect("production delayed crash child operation returned");
        }
        DelallocCrashCase::ProductionProjectedSingle
        | DelallocCrashCase::ProductionProjectedMultiEntry
        | DelallocCrashCase::ProductionProjectedSparse => {
            let steps = production_projected_steps(case, offset, durable_eof);
            run_production_projected_steps(&ext4, inode, &steps, || {
                device.reset_operation_log();
                device.arm_power_loss_at(crash_point);
            });
        }
    }
    panic!("armed delayed-allocation crash point {crash_point} let operation return");
}

fn recover_and_retry(work: &str, inode: u32, kind: ImageKind, persistence: PersistenceModel) {
    let device = Arc::new(CrashBlockFile::new(work));
    let ext4 = Ext4::load_writable(device.clone()).unwrap_or_else(|error| {
        panic!(
            "{} image failed to mount after simulated power loss: {error:?}",
            kind.name()
        )
    });
    persistence.configure(&device);

    let attr = ext4
        .getattr(inode)
        .expect("pre-existing inode disappeared after recovery");
    assert_eq!(
        attr.size, 0,
        "prepare_buffered_write must not publish visible EOF before data write"
    );
    let mut read = [0u8; 1];
    assert_eq!(
        ext4.read(inode, 0, &mut read)
            .expect("EOF read after recovery failed"),
        0,
        "crashed pre-write range became visible despite zero inode size"
    );

    // Retrying with the same logical range proves that the recovered bitmap
    // and extent state neither double-allocates nor leaves the inode unusable.
    ext4.prepare_buffered_write(inode, 0, WRITE_LEN, WRITE_LEN as u64, None)
        .expect("retry after recovery could not prepare the original range");
    assert_eq!(
        ext4.write_data_only(inode, 0, PAYLOAD)
            .expect("retry data write failed"),
        WRITE_LEN
    );
    ext4.commit_inode_size(inode, WRITE_LEN as u64, None)
        .expect("retry inode-size commit failed");
    let mut read = [0u8; WRITE_LEN];
    assert_eq!(
        ext4.read(inode, 0, &mut read)
            .expect("retry data read failed"),
        WRITE_LEN
    );
    assert_eq!(read, *PAYLOAD, "retry data differs after recovery");
    ext4.shutdown_writable()
        .expect("recovered image clean shutdown failed");
}

fn exercise_crash_point(
    seed: &str,
    work: &str,
    inode: u32,
    kind: ImageKind,
    persistence: PersistenceModel,
    crash_point: usize,
) -> bool {
    copy_seed(seed, work);
    let device = Arc::new(CrashBlockFile::new(work));
    let ext4 = Ext4::load_writable(device.clone()).expect("fault image mount failed");
    persistence.configure(&device);
    device.reset_operation_log();
    device.arm_power_loss_at(crash_point);
    expect_power_loss(crash_point, || {
        ext4.prepare_buffered_write(inode, 0, WRITE_LEN, WRITE_LEN as u64, None)
    });
    device.crash();
    drop(ext4);
    drop(device);

    recover_and_retry(work, inode, kind, persistence);
    match kind {
        ImageKind::Journal => {
            assert_clean_e2fsck(work, "journal replay/retry");
            false
        }
        ImageKind::NoJournal => nojournal_e2fsck_requires_only_bitmap_repair(work, crash_point),
    }
}

fn run_kind(kind: ImageKind, persistence: PersistenceModel) {
    let prefix = format!("ext4-range-recovery-{}-{}", kind.name(), persistence.name());
    let seed = format!("{prefix}-seed.img");
    let count_work = format!("{prefix}-count.img");
    let work = format!("{prefix}-fault.img");
    let inode = create_clean_seed(&seed, kind);

    copy_seed(&seed, &count_work);
    let operations = count_prepare_operations(&count_work, inode, persistence);
    println!(
        "{} {} range preparation has {} persistence points: {:?}",
        kind.name(),
        persistence.name(),
        operations.len(),
        operations
    );

    let mut repair_points = Vec::new();
    for crash_point in 0..operations.len() {
        if exercise_crash_point(&seed, &work, inode, kind, persistence, crash_point) {
            repair_points.push(crash_point);
        }
    }
    if matches!(kind, ImageKind::NoJournal) {
        // The direct range protocol intentionally persists allocation homes
        // before the inode extent. With no journal there is no replay record
        // to close that crash window, so the last point (before the inode
        // image) is an offline-repairable allocation leak. Treat this as an
        // explicit baseline, never as journal-equivalent recovery. Stage 2's
        // delayed-allocation design must either avoid this publication order
        // or define a separate nojournal contract.
        assert_eq!(repair_points, vec![operations.len() - 1]);
        println!(
            "nojournal direct-range baseline: crash point {:?} requires only offline bitmap repair",
            repair_points
        );
    } else {
        assert!(repair_points.is_empty());
    }

    remove_if_exists(&seed);
    remove_if_exists(&count_work);
    remove_if_exists(&work);
}

fn run_journal_io_error_matrix() {
    const SEED: &str = "ext4-range-io-error-seed.img";
    const COUNT_WORK: &str = "ext4-range-io-error-count.img";
    const WORK: &str = "ext4-range-io-error-fault.img";

    let inode = create_clean_seed(SEED, ImageKind::Journal);
    copy_seed(SEED, COUNT_WORK);
    let operations = count_prepare_operations(COUNT_WORK, inode, PersistenceModel::WriteBack);
    println!(
        "journal range I/O-error matrix has {} persistence points: {:?}",
        operations.len(),
        operations
    );

    for failure_point in 0..operations.len() {
        copy_seed(SEED, WORK);
        let device = Arc::new(CrashBlockFile::new(WORK));
        let ext4 = Ext4::load_writable(device.clone()).expect("I/O-error fault mount failed");
        device.reset_operation_log();
        device.arm_io_error_at(failure_point);
        let error = ext4
            .prepare_buffered_write(inode, 0, WRITE_LEN, WRITE_LEN as u64, None)
            .expect_err("armed I/O error let prepare_buffered_write succeed");
        assert_eq!(error.code(), another_ext4::ErrCode::EIO);
        device.crash();
        drop(ext4);
        drop(device);

        recover_and_retry(WORK, inode, ImageKind::Journal, PersistenceModel::WriteBack);
        assert_clean_e2fsck(WORK, "journal I/O error/retry");
    }

    remove_if_exists(SEED);
    remove_if_exists(COUNT_WORK);
    remove_if_exists(WORK);
}

fn run_nojournal_io_error_matrix() {
    const SEED: &str = "ext4-nojournal-range-io-error-seed.img";
    const COUNT_WORK: &str = "ext4-nojournal-range-io-error-count.img";
    const WORK: &str = "ext4-nojournal-range-io-error-fault.img";

    let inode = create_clean_seed(SEED, ImageKind::NoJournal);
    copy_seed(SEED, COUNT_WORK);
    let operations = count_prepare_operations(COUNT_WORK, inode, PersistenceModel::WriteBack);
    println!(
        "nojournal range I/O-error matrix has {} persistence points: {:?}",
        operations.len(),
        operations
    );

    let mut repair_points = Vec::new();
    for failure_point in 0..operations.len() {
        copy_seed(SEED, WORK);
        let device = Arc::new(CrashBlockFile::new(WORK));
        let ext4 =
            Ext4::load_writable(device.clone()).expect("nojournal I/O-error fault mount failed");
        device.reset_operation_log();
        device.arm_io_error_at(failure_point);
        let error = ext4
            .prepare_buffered_write(inode, 0, WRITE_LEN, WRITE_LEN as u64, None)
            .expect_err("armed nojournal I/O error let prepare_buffered_write succeed");
        assert_eq!(error.code(), another_ext4::ErrCode::EIO);

        // EIO is intentionally distinct from the crash matrix: the direct
        // path must roll back while publication is known absent, and poison
        // once the inode write is uncertain. Rebooting from this point tests
        // that neither outcome creates a dangling extent or double allocation.
        device.crash();
        drop(ext4);
        drop(device);

        recover_and_retry(
            WORK,
            inode,
            ImageKind::NoJournal,
            PersistenceModel::WriteBack,
        );
        if nojournal_e2fsck_requires_only_bitmap_repair(WORK, failure_point) {
            repair_points.push(failure_point);
        }
    }
    assert_eq!(repair_points, vec![operations.len() - 1]);
    println!(
        "nojournal direct-range I/O-error baseline: point {:?} requires only offline bitmap repair",
        repair_points
    );

    remove_if_exists(SEED);
    remove_if_exists(COUNT_WORK);
    remove_if_exists(WORK);
}

fn segment_payload(segment: usize) -> Vec<u8> {
    vec![0x20u8.wrapping_add(segment as u8); WRITE_LEN]
}

fn assert_fragmented_prefix(ext4: &Ext4, inode: u32) {
    let mut data = vec![0u8; INLINE_EXTENT_CAPACITY * WRITE_LEN];
    assert_eq!(
        ext4.read(inode, 0, &mut data)
            .expect("read fragmented prefix failed"),
        data.len()
    );
    for segment in 0..INLINE_EXTENT_CAPACITY {
        assert_eq!(
            &data[segment * WRITE_LEN..(segment + 1) * WRITE_LEN],
            segment_payload(segment).as_slice(),
            "fragmented prefix segment {segment} changed"
        );
    }
}

fn create_external_leaf_seed(path: &str) -> u32 {
    make_image(path, ImageKind::Journal);
    let device = Arc::new(CrashBlockFile::new(path));
    let ext4 = Ext4::load_writable(device).expect("external-leaf seed mount failed");
    let target = ext4
        .generic_create(
            ROOT_INO,
            "external-leaf-target",
            InodeMode::FILE | InodeMode::ALL_RWX,
        )
        .expect("external-leaf target create failed");
    let filler = ext4
        .generic_create(
            ROOT_INO,
            "external-leaf-filler",
            InodeMode::FILE | InodeMode::ALL_RWX,
        )
        .expect("external-leaf filler create failed");

    // Allocate target and filler alternately. The target's adjacent logical
    // ranges are therefore physically separated and consume the four inline
    // extent slots without relying on a filesystem-image byte offset.
    for segment in 0..INLINE_EXTENT_CAPACITY {
        let offset = segment * WRITE_LEN;
        ext4.write(target, offset, &segment_payload(segment))
            .expect("external-leaf target seed write failed");
        ext4.write(filler, offset, &[0x7c; WRITE_LEN])
            .expect("external-leaf filler seed write failed");
    }
    let attr = ext4
        .getattr(target)
        .expect("external-leaf target getattr failed");
    assert_eq!(attr.size, (INLINE_EXTENT_CAPACITY * WRITE_LEN) as u64);
    assert_eq!(
        attr.blocks,
        (INLINE_EXTENT_CAPACITY * WRITE_LEN / 512) as u64,
        "fragmented seed unexpectedly allocated an extent-tree block"
    );
    ext4.shutdown_writable()
        .expect("external-leaf seed clean shutdown failed");
    target
}

/// Cover the interaction between stale truncate-tail clearing and an extent
/// root promotion in the same delayed-allocation transaction.  The mapper
/// must query the old durable tree before the transaction-private root points
/// at newly staged (not yet readable from the device) extent nodes.
fn run_production_partial_eof_sparse_root_grow_test(persistence: PersistenceModel) {
    const IMAGE: &str = "ext4-production-partial-eof-sparse-root-grow.img";
    let inode = create_external_leaf_seed(IMAGE);
    let device = Arc::new(CrashBlockFile::new(IMAGE));
    persistence.configure(&device);
    let ext4 =
        Ext4::load_writable(device.clone()).expect("partial-EOF sparse root-grow mount failed");
    let partial_eof = INLINE_EXTENT_CAPACITY * WRITE_LEN - DELALLOC_BLOCK_LEN + 17;
    ext4.setattr(
        inode,
        another_ext4::SetAttr {
            size: Some(partial_eof as u64),
            ..Default::default()
        },
    )
    .expect("partial-EOF sparse root-grow truncate failed");
    let blocks_before = ext4
        .getattr(inode)
        .expect("partial-EOF sparse root-grow getattr failed")
        .blocks;
    let append_offset = INLINE_EXTENT_CAPACITY * WRITE_LEN + DELALLOC_BLOCK_LEN;
    let durable_eof = append_offset + DELALLOC_BLOCK_LEN;
    let steps = [ProductionProjectedStep {
        offset: append_offset,
        expected_durable_eof_before: partial_eof as u64,
        durable_eof_after: durable_eof as u64,
        fill: 0x6e,
    }];
    run_production_projected_steps(&ext4, inode, &steps, || {});

    let attr = ext4
        .getattr(inode)
        .expect("partial-EOF sparse root-grow final getattr failed");
    assert_eq!(attr.size, durable_eof as u64);
    assert!(
        attr.blocks > blocks_before + (DELALLOC_BLOCK_LEN / 512) as u64,
        "partial-EOF sparse append did not promote the extent root"
    );
    let mut data = vec![0u8; durable_eof];
    assert_eq!(
        ext4.read(inode, 0, &mut data)
            .expect("partial-EOF sparse root-grow read failed"),
        data.len()
    );
    for segment in 0..INLINE_EXTENT_CAPACITY {
        let start = segment * WRITE_LEN;
        let end = ((segment + 1) * WRITE_LEN).min(partial_eof);
        if start == end {
            break;
        }
        assert_eq!(
            &data[start..end],
            &segment_payload(segment)[..end - start],
            "partial-EOF sparse root-grow changed prefix segment {segment}"
        );
    }
    assert!(
        data[partial_eof..append_offset]
            .iter()
            .all(|byte| *byte == 0),
        "partial-EOF sparse root-grow exposed stale tail or logical-gap data"
    );
    assert_eq!(
        &data[append_offset..durable_eof],
        &[0x6e; DELALLOC_BLOCK_LEN]
    );
    ext4.shutdown_writable()
        .expect("partial-EOF sparse root-grow shutdown failed");
    drop(ext4);
    drop(device);
    assert_clean_e2fsck(IMAGE, "partial-EOF sparse root-grow");
    remove_if_exists(IMAGE);
}

fn count_external_leaf_append_operations(
    work: &str,
    inode: u32,
    persistence: PersistenceModel,
) -> Vec<CrashDeviceOperation> {
    let device = Arc::new(CrashBlockFile::new(work));
    let ext4 = Ext4::load_writable(device.clone()).expect("external-leaf count mount failed");
    persistence.configure(&device);
    let old_size = INLINE_EXTENT_CAPACITY * WRITE_LEN;
    assert_fragmented_prefix(&ext4, inode);
    device.reset_operation_log();
    ext4.prepare_buffered_write(
        inode,
        old_size,
        WRITE_LEN,
        (old_size + WRITE_LEN) as u64,
        None,
    )
    .expect("external-leaf append preparation failed");
    let attr = ext4
        .getattr(inode)
        .expect("external-leaf prepared getattr failed");
    assert_eq!(attr.size, old_size as u64);
    assert!(
        attr.blocks > ((old_size + WRITE_LEN) / 512) as u64,
        "fifth fragmented extent did not create an external extent-tree leaf"
    );
    let operations = device.operations();
    ext4.shutdown_writable()
        .expect("external-leaf count clean shutdown failed");
    operations
}

fn recover_and_retry_external_leaf(
    work: &str,
    inode: u32,
    persistence: PersistenceModel,
    crash_point: usize,
) {
    let device = Arc::new(CrashBlockFile::new(work));
    let ext4 = Ext4::load_writable(device.clone()).expect("external-leaf recovery mount failed");
    persistence.configure(&device);
    let old_size = INLINE_EXTENT_CAPACITY * WRITE_LEN;
    let attr = ext4
        .getattr(inode)
        .expect("external-leaf inode disappeared after recovery");
    assert_eq!(attr.size, old_size as u64);
    assert_fragmented_prefix(&ext4, inode);

    ext4.prepare_buffered_write(
        inode,
        old_size,
        WRITE_LEN,
        (old_size + WRITE_LEN) as u64,
        None,
    )
    .unwrap_or_else(|error| {
        panic!("external-leaf crash point {crash_point}: retry prepare failed: {error:?}")
    });
    assert_eq!(
        ext4.write_data_only(inode, old_size, PAYLOAD)
            .expect("external-leaf retry data write failed"),
        WRITE_LEN
    );
    ext4.commit_inode_size(inode, (old_size + WRITE_LEN) as u64, None)
        .expect("external-leaf retry inode-size commit failed");
    let attr = ext4
        .getattr(inode)
        .expect("external-leaf final getattr failed");
    assert_eq!(attr.size, (old_size + WRITE_LEN) as u64);
    assert!(
        attr.blocks > ((old_size + WRITE_LEN) / 512) as u64,
        "recovered external extent-tree leaf disappeared"
    );
    assert_fragmented_prefix(&ext4, inode);
    let mut tail = [0u8; WRITE_LEN];
    assert_eq!(
        ext4.read(inode, old_size, &mut tail)
            .expect("external-leaf tail read failed"),
        WRITE_LEN
    );
    assert_eq!(tail, *PAYLOAD);
    ext4.shutdown_writable()
        .expect("external-leaf recovered clean shutdown failed");
}

fn run_journal_external_leaf_matrix(persistence: PersistenceModel) {
    let prefix = format!("ext4-external-leaf-recovery-{}", persistence.name());
    let seed = format!("{prefix}-seed.img");
    let count_work = format!("{prefix}-count.img");
    let work = format!("{prefix}-fault.img");

    let inode = create_external_leaf_seed(&seed);
    assert_clean_e2fsck(&seed, "external-leaf seed");
    copy_seed(&seed, &count_work);
    let operations = count_external_leaf_append_operations(&count_work, inode, persistence);
    assert!(
        !operations.is_empty(),
        "external-leaf append issued no persistence operations"
    );
    println!(
        "journal {} external-leaf append has {} persistence points: {:?}",
        persistence.name(),
        operations.len(),
        operations
    );

    let old_size = INLINE_EXTENT_CAPACITY * WRITE_LEN;
    for crash_point in 0..operations.len() {
        copy_seed(&seed, &work);
        let device = Arc::new(CrashBlockFile::new(&work));
        let ext4 = Ext4::load_writable(device.clone()).expect("external-leaf fault mount failed");
        persistence.configure(&device);
        device.reset_operation_log();
        device.arm_power_loss_at(crash_point);
        expect_power_loss(crash_point, || {
            ext4.prepare_buffered_write(
                inode,
                old_size,
                WRITE_LEN,
                (old_size + WRITE_LEN) as u64,
                None,
            )
        });
        device.crash();
        drop(ext4);
        drop(device);

        recover_and_retry_external_leaf(&work, inode, persistence, crash_point);
        assert_clean_e2fsck(&work, "external-leaf journal replay/retry");
    }

    remove_if_exists(&seed);
    remove_if_exists(&count_work);
    remove_if_exists(&work);
}

fn create_existing_external_leaf_seed(path: &str) -> u32 {
    let inode = create_external_leaf_seed(path);
    let device = Arc::new(CrashBlockFile::new(path));
    let ext4 = Ext4::load_writable(device).expect("existing-leaf seed mount failed");
    let offset = INLINE_EXTENT_CAPACITY * WRITE_LEN;
    ext4.prepare_buffered_write(inode, offset, WRITE_LEN, (offset + WRITE_LEN) as u64, None)
        .expect("existing-leaf seed root split prepare failed");
    assert_eq!(
        ext4.write_data_only(inode, offset, PAYLOAD)
            .expect("existing-leaf seed data write failed"),
        WRITE_LEN
    );
    ext4.commit_inode_size(inode, (offset + WRITE_LEN) as u64, None)
        .expect("existing-leaf seed inode-size commit failed");
    let attr = ext4
        .getattr(inode)
        .expect("existing-leaf seed getattr failed");
    assert_eq!(attr.size, (offset + WRITE_LEN) as u64);
    assert!(
        attr.blocks > ((offset + WRITE_LEN) / 512) as u64,
        "existing-leaf seed has no external extent-tree leaf"
    );
    ext4.shutdown_writable()
        .expect("existing-leaf seed clean shutdown failed");
    inode
}

fn assert_existing_external_leaf_data(ext4: &Ext4, inode: u32) {
    assert_fragmented_prefix(ext4, inode);
    let offset = INLINE_EXTENT_CAPACITY * WRITE_LEN;
    let mut data = [0u8; WRITE_LEN];
    assert_eq!(
        ext4.read(inode, offset, &mut data)
            .expect("existing external-leaf data read failed"),
        WRITE_LEN
    );
    assert_eq!(data, *PAYLOAD);
}

fn count_existing_leaf_append_operations(
    work: &str,
    inode: u32,
    persistence: PersistenceModel,
) -> Vec<CrashDeviceOperation> {
    let device = Arc::new(CrashBlockFile::new(work));
    let ext4 = Ext4::load_writable(device.clone()).expect("existing-leaf count mount failed");
    persistence.configure(&device);
    let offset = (INLINE_EXTENT_CAPACITY + 1) * WRITE_LEN;
    assert_existing_external_leaf_data(&ext4, inode);
    device.reset_operation_log();
    ext4.prepare_buffered_write(inode, offset, WRITE_LEN, (offset + WRITE_LEN) as u64, None)
        .expect("existing-leaf append preparation failed");
    let operations = device.operations();
    assert!(
        operations
            .iter()
            .any(|operation| matches!(operation, CrashDeviceOperation::Flush)),
        "existing external-leaf append did not use the journal transaction path"
    );
    ext4.shutdown_writable()
        .expect("existing-leaf count clean shutdown failed");
    operations
}

fn recover_and_retry_existing_leaf(
    work: &str,
    inode: u32,
    persistence: PersistenceModel,
    crash_point: usize,
) {
    let device = Arc::new(CrashBlockFile::new(work));
    let ext4 = Ext4::load_writable(device.clone()).unwrap_or_else(|error| {
        panic!("existing-leaf crash point {crash_point}: recovery mount failed: {error:?}")
    });
    persistence.configure(&device);
    let offset = (INLINE_EXTENT_CAPACITY + 1) * WRITE_LEN;
    let attr = ext4
        .getattr(inode)
        .expect("existing external-leaf inode disappeared after recovery");
    assert_eq!(attr.size, offset as u64);
    assert_existing_external_leaf_data(&ext4, inode);

    ext4.prepare_buffered_write(inode, offset, WRITE_LEN, (offset + WRITE_LEN) as u64, None)
        .unwrap_or_else(|error| {
            panic!("existing-leaf crash point {crash_point}: retry prepare failed: {error:?}")
        });
    let tail = [0x5d; WRITE_LEN];
    assert_eq!(
        ext4.write_data_only(inode, offset, &tail)
            .expect("existing-leaf retry data write failed"),
        WRITE_LEN
    );
    ext4.commit_inode_size(inode, (offset + WRITE_LEN) as u64, None)
        .expect("existing-leaf retry inode-size commit failed");
    let mut read = [0u8; WRITE_LEN];
    assert_eq!(
        ext4.read(inode, offset, &mut read)
            .expect("existing-leaf retry data read failed"),
        WRITE_LEN
    );
    assert_eq!(read, tail);
    ext4.shutdown_writable()
        .expect("existing-leaf recovered clean shutdown failed");
}

fn run_journal_existing_leaf_matrix(persistence: PersistenceModel) {
    let prefix = format!("ext4-existing-leaf-recovery-{}", persistence.name());
    let seed = format!("{prefix}-seed.img");
    let count_work = format!("{prefix}-count.img");
    let work = format!("{prefix}-fault.img");

    let inode = create_existing_external_leaf_seed(&seed);
    assert_clean_e2fsck(&seed, "existing-leaf seed");
    copy_seed(&seed, &count_work);
    let operations = count_existing_leaf_append_operations(&count_work, inode, persistence);
    println!(
        "journal {} existing-leaf append has {} persistence points: {:?}",
        persistence.name(),
        operations.len(),
        operations
    );

    let offset = (INLINE_EXTENT_CAPACITY + 1) * WRITE_LEN;
    for crash_point in 0..operations.len() {
        copy_seed(&seed, &work);
        let device = Arc::new(CrashBlockFile::new(&work));
        let ext4 = Ext4::load_writable(device.clone()).expect("existing-leaf fault mount failed");
        persistence.configure(&device);
        device.reset_operation_log();
        device.arm_power_loss_at(crash_point);
        expect_power_loss(crash_point, || {
            ext4.prepare_buffered_write(inode, offset, WRITE_LEN, (offset + WRITE_LEN) as u64, None)
        });
        device.crash();
        drop(ext4);
        drop(device);

        recover_and_retry_existing_leaf(&work, inode, persistence, crash_point);
        assert_clean_e2fsck(&work, "existing-leaf journal replay/retry");
    }

    remove_if_exists(&seed);
    remove_if_exists(&count_work);
    remove_if_exists(&work);
}

fn assert_full_external_leaf_samples(ext4: &Ext4, inode: u32) {
    for segment in [
        0,
        1,
        INLINE_EXTENT_CAPACITY,
        EXTERNAL_LEAF_EXTENT_CAPACITY - 1,
    ] {
        let mut data = [0u8; WRITE_LEN];
        assert_eq!(
            ext4.read(inode, segment * WRITE_LEN, &mut data)
                .expect("full external-leaf sample read failed"),
            WRITE_LEN,
            "full external-leaf sample {segment} was truncated"
        );
        if segment == INLINE_EXTENT_CAPACITY {
            assert_eq!(
                data, *PAYLOAD,
                "full external-leaf sample {segment} changed"
            );
        } else {
            assert_eq!(
                data,
                segment_payload(segment).as_slice(),
                "full external-leaf sample {segment} changed"
            );
        }
    }
}

fn create_full_external_leaf_seed(path: &str) -> u32 {
    let target = create_existing_external_leaf_seed(path);
    let device = Arc::new(CrashBlockFile::new(path));
    let ext4 = Ext4::load_writable(device).expect("full external-leaf seed mount failed");
    let filler = ext4
        .generic_create(
            ROOT_INO,
            "full-external-leaf-filler",
            InodeMode::FILE | InodeMode::ALL_RWX,
        )
        .expect("full external-leaf filler create failed");

    // The seed already has five fragmented extents. Keep alternating the
    // target and filler so every remaining append consumes a distinct leaf
    // slot instead of merging with the previous physical extent.
    for segment in (INLINE_EXTENT_CAPACITY + 1)..EXTERNAL_LEAF_EXTENT_CAPACITY {
        let offset = segment * WRITE_LEN;
        ext4.write(target, offset, &segment_payload(segment))
            .expect("full external-leaf target seed write failed");
        ext4.write(filler, offset, &[0x7d; WRITE_LEN])
            .expect("full external-leaf filler seed write failed");
    }
    let attr = ext4
        .getattr(target)
        .expect("full external-leaf target getattr failed");
    assert_eq!(
        attr.size,
        (EXTERNAL_LEAF_EXTENT_CAPACITY * WRITE_LEN) as u64,
        "full external-leaf seed has an unexpected EOF"
    );
    assert_full_external_leaf_samples(&ext4, target);
    ext4.shutdown_writable()
        .expect("full external-leaf seed clean shutdown failed");
    target
}

fn count_full_leaf_split_operations(
    work: &str,
    inode: u32,
    persistence: PersistenceModel,
) -> Vec<CrashDeviceOperation> {
    let device = Arc::new(CrashBlockFile::new(work));
    let ext4 = Ext4::load_writable(device.clone()).expect("full leaf split count mount failed");
    persistence.configure(&device);
    let offset = EXTERNAL_LEAF_EXTENT_CAPACITY * WRITE_LEN;
    assert_full_external_leaf_samples(&ext4, inode);
    device.reset_operation_log();
    ext4.prepare_buffered_write(inode, offset, WRITE_LEN, (offset + WRITE_LEN) as u64, None)
        .expect("full external-leaf split preparation failed");
    let operations = device.operations();
    assert!(
        operations
            .iter()
            .any(|operation| matches!(operation, CrashDeviceOperation::Flush)),
        "full external-leaf split did not use the journal transaction path"
    );
    ext4.shutdown_writable()
        .expect("full leaf split count clean shutdown failed");
    operations
}

fn recover_and_retry_full_leaf_split(
    work: &str,
    inode: u32,
    persistence: PersistenceModel,
    crash_point: usize,
) {
    let device = Arc::new(CrashBlockFile::new(work));
    let ext4 = Ext4::load_writable(device.clone()).unwrap_or_else(|error| {
        panic!("full leaf split crash point {crash_point}: recovery mount failed: {error:?}")
    });
    persistence.configure(&device);
    let offset = EXTERNAL_LEAF_EXTENT_CAPACITY * WRITE_LEN;
    let attr = ext4
        .getattr(inode)
        .expect("full leaf split inode disappeared after recovery");
    assert_eq!(attr.size, offset as u64);
    assert_full_external_leaf_samples(&ext4, inode);

    ext4.prepare_buffered_write(inode, offset, WRITE_LEN, (offset + WRITE_LEN) as u64, None)
        .unwrap_or_else(|error| {
            panic!("full leaf split crash point {crash_point}: retry prepare failed: {error:?}")
        });
    let tail = [0x4c; WRITE_LEN];
    assert_eq!(
        ext4.write_data_only(inode, offset, &tail)
            .expect("full leaf split retry data write failed"),
        WRITE_LEN
    );
    ext4.commit_inode_size(inode, (offset + WRITE_LEN) as u64, None)
        .expect("full leaf split retry inode-size commit failed");
    let mut read = [0u8; WRITE_LEN];
    assert_eq!(
        ext4.read(inode, offset, &mut read)
            .expect("full leaf split retry data read failed"),
        WRITE_LEN
    );
    assert_eq!(read, tail);
    assert_full_external_leaf_samples(&ext4, inode);
    ext4.shutdown_writable()
        .expect("full leaf split recovered clean shutdown failed");
}

fn run_journal_full_leaf_split_matrix(persistence: PersistenceModel) {
    let prefix = format!("ext4-full-leaf-split-recovery-{}", persistence.name());
    let seed = format!("{prefix}-seed.img");
    let count_work = format!("{prefix}-count.img");
    let work = format!("{prefix}-fault.img");

    let inode = create_full_external_leaf_seed(&seed);
    assert_clean_e2fsck(&seed, "full external-leaf seed");
    copy_seed(&seed, &count_work);
    let operations = count_full_leaf_split_operations(&count_work, inode, persistence);
    println!(
        "journal {} full external-leaf split has {} persistence points: {:?}",
        persistence.name(),
        operations.len(),
        operations
    );

    let offset = EXTERNAL_LEAF_EXTENT_CAPACITY * WRITE_LEN;
    for crash_point in 0..operations.len() {
        copy_seed(&seed, &work);
        let device = Arc::new(CrashBlockFile::new(&work));
        let ext4 = Ext4::load_writable(device.clone()).expect("full leaf split fault mount failed");
        persistence.configure(&device);
        device.reset_operation_log();
        device.arm_power_loss_at(crash_point);
        expect_power_loss(crash_point, || {
            ext4.prepare_buffered_write(inode, offset, WRITE_LEN, (offset + WRITE_LEN) as u64, None)
        });
        device.crash();
        drop(ext4);
        drop(device);

        recover_and_retry_full_leaf_split(&work, inode, persistence, crash_point);
        assert_clean_e2fsck(&work, "full external-leaf journal replay/retry");
    }

    remove_if_exists(&seed);
    remove_if_exists(&count_work);
    remove_if_exists(&work);
}

fn count_reclaim_operations(
    work: &str,
    inode: u32,
    persistence: PersistenceModel,
) -> Vec<CrashDeviceOperation> {
    let device = Arc::new(CrashBlockFile::new(work));
    let ext4 = Ext4::load_writable(device.clone()).expect("reclaim count mount failed");
    persistence.configure(&device);
    assert_eq!(
        ext4.generic_lookup(ROOT_INO, "external-leaf-target")
            .expect("reclaim target name missing before unlink"),
        inode
    );
    let handle = ext4
        .unlink(ROOT_INO, "external-leaf-target")
        .expect("reclaim count unlink failed")
        .expect("reclaim count unlink returned no final-lifetime handle");
    device.reset_operation_log();
    ext4.reclaim_inode(handle)
        .expect("reclaim count operation failed");
    let operations = device.operations();
    ext4.shutdown_writable()
        .expect("reclaim count clean shutdown failed");
    operations
}

fn assert_reclaim_recovery_and_reuse(
    work: &str,
    inode: u32,
    persistence: PersistenceModel,
    crash_point: usize,
) {
    let device = Arc::new(CrashBlockFile::new(work));
    let ext4 = Ext4::load_writable(device.clone()).unwrap_or_else(|error| {
        panic!("reclaim crash point {crash_point}: recovery mount failed: {error:?}")
    });
    persistence.configure(&device);
    ext4.generic_lookup(ROOT_INO, "external-leaf-target")
        .expect_err("reclaimed orphan name reappeared after recovery");
    ext4.getattr(inode)
        .expect_err("reclaimed orphan inode remained reachable after recovery");

    // Reallocate and release after recovery so a stale bitmap bit or an
    // incorrectly replayed final inode image cannot hide behind the original
    // orphan's disappearance.
    let replacement = ext4
        .generic_create(
            ROOT_INO,
            "post-reclaim-reuse",
            InodeMode::FILE | InodeMode::ALL_RWX,
        )
        .expect("post-reclaim recovery allocation failed");
    ext4.write(replacement, 0, PAYLOAD)
        .expect("post-reclaim recovery write failed");
    let handle = ext4
        .unlink(ROOT_INO, "post-reclaim-reuse")
        .expect("post-reclaim recovery unlink failed")
        .expect("post-reclaim recovery unlink returned no handle");
    ext4.reclaim_inode(handle)
        .expect("post-reclaim recovery final reclaim failed");
    ext4.shutdown_writable()
        .expect("reclaim recovery clean shutdown failed");
}

fn run_journal_reclaim_matrix(persistence: PersistenceModel) {
    let prefix = format!("ext4-reclaim-recovery-{}", persistence.name());
    let seed = format!("{prefix}-seed.img");
    let count_work = format!("{prefix}-count.img");
    let work = format!("{prefix}-fault.img");

    let inode = create_full_external_leaf_seed(&seed);
    assert_clean_e2fsck(&seed, "reclaim seed");
    copy_seed(&seed, &count_work);
    let operations = count_reclaim_operations(&count_work, inode, persistence);
    assert!(
        !operations.is_empty(),
        "reclaim issued no persistence operations"
    );
    println!(
        "journal {} reclaim first batch has {} persistence points: {:?}",
        persistence.name(),
        operations.len(),
        operations
    );

    for crash_point in 0..operations.len() {
        copy_seed(&seed, &work);
        let device = Arc::new(CrashBlockFile::new(&work));
        let ext4 = Ext4::load_writable(device.clone()).expect("reclaim fault mount failed");
        persistence.configure(&device);
        let handle = ext4
            .unlink(ROOT_INO, "external-leaf-target")
            .expect("reclaim fault unlink failed")
            .expect("reclaim fault unlink returned no handle");
        device.reset_operation_log();
        device.arm_power_loss_at(crash_point);
        expect_power_loss(crash_point, || ext4.reclaim_inode(handle));
        device.crash();
        drop(ext4);
        drop(device);

        assert_reclaim_recovery_and_reuse(&work, inode, persistence, crash_point);
        assert_clean_e2fsck(&work, "journal reclaim replay/retry");
    }

    remove_if_exists(&seed);
    remove_if_exists(&count_work);
    remove_if_exists(&work);
}

const LINKED_TAIL_SIZE: usize = 2 * 4096;
const LINKED_TAIL_DURABLE_EOF: u64 = 4096;

/// Construct the state a delayed-allocation mapper will leave after publishing
/// a mapped tail but before it can commit the new EOF.  This is deliberately
/// a real journal transaction, not a raw image edit, so the namespace crash
/// matrices below exercise the actual legacy orphan protocol.
fn prepare_linked_tail(ext4: &Ext4, name: &str, byte: u8) -> u32 {
    let inode = ext4
        .generic_create(ROOT_INO, name, InodeMode::FILE | InodeMode::ALL_RWX)
        .unwrap_or_else(|error| panic!("create linked-tail {name} failed: {error:?}"));
    ext4.write(inode, 0, &vec![byte; LINKED_TAIL_SIZE])
        .unwrap_or_else(|error| panic!("write linked-tail {name} failed: {error:?}"));
    ext4.setattr(
        inode,
        another_ext4::SetAttr {
            size: Some(LINKED_TAIL_DURABLE_EOF),
            ..Default::default()
        },
    )
    .unwrap_or_else(|error| panic!("shrink linked-tail {name} failed: {error:?}"));
    assert!(
        ext4.test_enroll_linked_tail_orphan(inode)
            .unwrap_or_else(|error| panic!("enrol linked-tail {name} failed: {error:?}")),
        "linked-tail {name} was not newly enrolled"
    );
    inode
}

fn assert_linked_tail_file(ext4: &Ext4, name: &str, inode: u32, byte: u8) {
    assert_eq!(
        ext4.generic_lookup(ROOT_INO, name)
            .unwrap_or_else(|error| panic!("linked-tail {name} disappeared: {error:?}")),
        inode,
        "linked-tail {name} resolved to a different inode"
    );
    assert_eq!(
        ext4.getattr(inode)
            .unwrap_or_else(|error| panic!("read linked-tail {name} attr failed: {error:?}"))
            .size,
        LINKED_TAIL_DURABLE_EOF,
        "linked-tail {name} did not recover its durable EOF"
    );
    let mut prefix = [0u8; 4096];
    assert_eq!(
        ext4.read(inode, 0, &mut prefix)
            .unwrap_or_else(|error| panic!("read linked-tail {name} prefix failed: {error:?}")),
        prefix.len()
    );
    assert!(
        prefix.iter().all(|value| *value == byte),
        "linked-tail {name} prefix changed across recovery"
    );
}

fn assert_linked_tail_reuse(ext4: &Ext4, label: &str) {
    let name = format!("linked-tail-reuse-{label}");
    let inode = ext4
        .generic_create(ROOT_INO, &name, InodeMode::FILE | InodeMode::ALL_RWX)
        .unwrap_or_else(|error| panic!("{label}: post-recovery create failed: {error:?}"));
    ext4.write(inode, 0, PAYLOAD)
        .unwrap_or_else(|error| panic!("{label}: post-recovery write failed: {error:?}"));
    let handle = ext4
        .unlink(ROOT_INO, &name)
        .unwrap_or_else(|error| panic!("{label}: post-recovery unlink failed: {error:?}"))
        .expect("post-recovery final unlink returned no handle");
    ext4.reclaim_inode(handle)
        .unwrap_or_else(|error| panic!("{label}: post-recovery reclaim failed: {error:?}"));
}

fn count_linked_tail_final_unlink_operations(
    persistence: PersistenceModel,
) -> Vec<CrashDeviceOperation> {
    const WORK: &str = "ext4-linked-tail-unlink-count.img";
    make_image(WORK, ImageKind::Journal);
    let device = Arc::new(CrashBlockFile::new(WORK));
    persistence.configure(&device);
    let ext4 = Ext4::load_writable(device.clone()).expect("linked-tail unlink count mount failed");
    let target = prepare_linked_tail(&ext4, "linked-tail-unlink-target", 0x51);
    // Enrol the target first, then another tail so the target is a non-head
    // chain member when its final unlink transaction runs.
    let predecessor = prepare_linked_tail(&ext4, "linked-tail-unlink-head", 0x52);
    assert_ne!(target, predecessor);
    device.reset_operation_log();
    let handle = ext4
        .unlink(ROOT_INO, "linked-tail-unlink-target")
        .expect("linked-tail unlink count operation failed")
        .expect("linked-tail unlink count returned no handle");
    drop(handle);
    let operations = device.operations();
    drop(ext4);
    drop(device);
    remove_if_exists(WORK);
    operations
}

fn assert_linked_tail_final_unlink_recovery(
    work: &str,
    persistence: PersistenceModel,
    target: u32,
    predecessor: u32,
    crash_point: usize,
) {
    let device = Arc::new(CrashBlockFile::new(work));
    let ext4 = Ext4::load_writable(device.clone()).unwrap_or_else(|error| {
        panic!("linked-tail unlink crash point {crash_point}: mount failed: {error:?}")
    });
    persistence.configure(&device);
    assert_linked_tail_file(&ext4, "linked-tail-unlink-head", predecessor, 0x52);
    match ext4.generic_lookup(ROOT_INO, "linked-tail-unlink-target") {
        Ok(inode) => {
            assert_eq!(inode, target, "unlink rollback changed target identity");
            assert_linked_tail_file(&ext4, "linked-tail-unlink-target", target, 0x51);
        }
        Err(_) => {
            ext4.getattr(target)
                .expect_err("committed linked-tail unlink did not reclaim target inode");
        }
    }
    assert_linked_tail_reuse(&ext4, "unlink");
    ext4.shutdown_writable()
        .expect("linked-tail unlink recovery clean shutdown failed");
}

fn run_journal_linked_tail_final_unlink_matrix(persistence: PersistenceModel) {
    const WORK: &str = "ext4-linked-tail-unlink-fault.img";
    let operations = count_linked_tail_final_unlink_operations(persistence);
    assert!(
        !operations.is_empty(),
        "linked-tail final unlink issued no persistence operations"
    );
    println!(
        "journal {} linked-tail final unlink has {} persistence points: {:?}",
        persistence.name(),
        operations.len(),
        operations
    );

    for crash_point in 0..operations.len() {
        make_image(WORK, ImageKind::Journal);
        let device = Arc::new(CrashBlockFile::new(WORK));
        persistence.configure(&device);
        let ext4 =
            Ext4::load_writable(device.clone()).expect("linked-tail unlink fault mount failed");
        let target = prepare_linked_tail(&ext4, "linked-tail-unlink-target", 0x51);
        let predecessor = prepare_linked_tail(&ext4, "linked-tail-unlink-head", 0x52);
        device.reset_operation_log();
        device.arm_power_loss_at(crash_point);
        expect_power_loss(crash_point, || {
            let handle = ext4
                .unlink(ROOT_INO, "linked-tail-unlink-target")
                .expect("armed linked-tail unlink failed before power loss")
                .expect("armed linked-tail unlink returned no handle");
            drop(handle);
        });
        device.crash();
        drop(ext4);
        drop(device);

        assert_linked_tail_final_unlink_recovery(
            WORK,
            persistence,
            target,
            predecessor,
            crash_point,
        );
        assert_clean_e2fsck(WORK, "linked-tail final-unlink crash recovery");
    }
    remove_if_exists(WORK);
}

fn count_linked_tail_rename_operations(persistence: PersistenceModel) -> Vec<CrashDeviceOperation> {
    const WORK: &str = "ext4-linked-tail-rename-count.img";
    make_image(WORK, ImageKind::Journal);
    let device = Arc::new(CrashBlockFile::new(WORK));
    persistence.configure(&device);
    let ext4 = Ext4::load_writable(device.clone()).expect("linked-tail rename count mount failed");
    let source = ext4
        .generic_create(
            ROOT_INO,
            "linked-tail-rename-source",
            InodeMode::FILE | InodeMode::ALL_RWX,
        )
        .expect("create linked-tail rename source failed");
    let target = prepare_linked_tail(&ext4, "linked-tail-rename-target", 0x61);
    let predecessor = prepare_linked_tail(&ext4, "linked-tail-rename-head", 0x62);
    assert_ne!(source, target);
    assert_ne!(target, predecessor);
    device.reset_operation_log();
    let handle = ext4
        .rename(
            ROOT_INO,
            "linked-tail-rename-source",
            ROOT_INO,
            "linked-tail-rename-target",
        )
        .expect("linked-tail rename count operation failed")
        .expect("linked-tail rename count returned no handle");
    drop(handle);
    let operations = device.operations();
    drop(ext4);
    drop(device);
    remove_if_exists(WORK);
    operations
}

fn assert_linked_tail_rename_recovery(
    work: &str,
    persistence: PersistenceModel,
    source: u32,
    target: u32,
    predecessor: u32,
    crash_point: usize,
) {
    let device = Arc::new(CrashBlockFile::new(work));
    let ext4 = Ext4::load_writable(device.clone()).unwrap_or_else(|error| {
        panic!("linked-tail rename crash point {crash_point}: mount failed: {error:?}")
    });
    persistence.configure(&device);
    assert_linked_tail_file(&ext4, "linked-tail-rename-head", predecessor, 0x62);
    let destination = ext4
        .generic_lookup(ROOT_INO, "linked-tail-rename-target")
        .expect("rename destination missing after recovery");
    if destination == target {
        // The replace transaction was not durable.  Recovery must have kept
        // both names and truncated only the linked target's tail.
        assert_eq!(
            ext4.generic_lookup(ROOT_INO, "linked-tail-rename-source")
                .expect("rolled-back rename lost source name"),
            source
        );
        assert_linked_tail_file(&ext4, "linked-tail-rename-target", target, 0x61);
    } else {
        assert_eq!(
            destination, source,
            "rename destination has unexpected inode"
        );
        ext4.generic_lookup(ROOT_INO, "linked-tail-rename-source")
            .expect_err("committed rename retained old source name");
        ext4.getattr(target)
            .expect_err("committed rename did not reclaim linked target inode");
    }
    assert_linked_tail_reuse(&ext4, "rename");
    ext4.shutdown_writable()
        .expect("linked-tail rename recovery clean shutdown failed");
}

fn run_journal_linked_tail_rename_matrix(persistence: PersistenceModel) {
    const WORK: &str = "ext4-linked-tail-rename-fault.img";
    let operations = count_linked_tail_rename_operations(persistence);
    assert!(
        !operations.is_empty(),
        "linked-tail rename replacement issued no persistence operations"
    );
    println!(
        "journal {} linked-tail rename replacement has {} persistence points: {:?}",
        persistence.name(),
        operations.len(),
        operations
    );

    for crash_point in 0..operations.len() {
        make_image(WORK, ImageKind::Journal);
        let device = Arc::new(CrashBlockFile::new(WORK));
        persistence.configure(&device);
        let ext4 =
            Ext4::load_writable(device.clone()).expect("linked-tail rename fault mount failed");
        let source = ext4
            .generic_create(
                ROOT_INO,
                "linked-tail-rename-source",
                InodeMode::FILE | InodeMode::ALL_RWX,
            )
            .expect("create linked-tail rename source failed");
        let target = prepare_linked_tail(&ext4, "linked-tail-rename-target", 0x61);
        let predecessor = prepare_linked_tail(&ext4, "linked-tail-rename-head", 0x62);
        device.reset_operation_log();
        device.arm_power_loss_at(crash_point);
        expect_power_loss(crash_point, || {
            let handle = ext4
                .rename(
                    ROOT_INO,
                    "linked-tail-rename-source",
                    ROOT_INO,
                    "linked-tail-rename-target",
                )
                .expect("armed linked-tail rename failed before power loss")
                .expect("armed linked-tail rename returned no handle");
            drop(handle);
        });
        device.crash();
        drop(ext4);
        drop(device);

        assert_linked_tail_rename_recovery(
            WORK,
            persistence,
            source,
            target,
            predecessor,
            crash_point,
        );
        assert_clean_e2fsck(WORK, "linked-tail rename crash recovery");
    }
    remove_if_exists(WORK);
}

/// The simulated power-loss path intentionally does not run ordinary Rust
/// destructors: a real machine loses the in-memory lease/receipt together
/// with the process.  Keep these capabilities in `ManuallyDrop` so their
/// fail-stop Drop implementations do not turn the expected crash payload into
/// a destructor panic while the host harness is modelling that machine stop.
fn manually_drop_mut<T>(value: &mut ManuallyDrop<T>) -> &mut T {
    // SAFETY: `ManuallyDrop<T>` has the same layout as `T`; the caller keeps
    // the wrapper alive and this helper never moves or manually drops `T`.
    unsafe { &mut *(value as *mut ManuallyDrop<T> as *mut T) }
}

fn run_reserved_delalloc_lifecycle(ext4: &Ext4, inode: u32, offset: usize, payload: &[u8]) {
    assert_eq!(payload.len(), DELALLOC_BLOCK_LEN);
    let mut lease = ManuallyDrop::new(
        ext4.reserve_delalloc_lease(1, 0)
            .expect("reserve delayed mapper block failed"),
    );
    let receipt = ext4
        .test_map_delalloc_reserved_block_append(inode, offset, manually_drop_mut(&mut lease))
        .expect("map delayed mapper block failed");
    let mut receipt = ManuallyDrop::new(receipt);
    ext4.test_writeback_delalloc_mapped_block(manually_drop_mut(&mut receipt), payload, Some(77))
        .expect("submit delayed mapper block failed");
    // Both capabilities are now terminal. Drop them explicitly in the normal
    // path so their fail-stop guards remain exercised outside simulated crash.
    unsafe {
        ManuallyDrop::drop(&mut receipt);
        ManuallyDrop::drop(&mut lease);
    }
}

fn count_reserved_delalloc_operations(
    seed: &str,
    work: &str,
    inode: u32,
    persistence: PersistenceModel,
) -> Vec<CrashDeviceOperation> {
    copy_seed(seed, work);
    let device = Arc::new(CrashBlockFile::new(work));
    persistence.configure(&device);
    let ext4 = Ext4::load_writable(device.clone()).expect("delalloc count mount failed");
    device.reset_operation_log();
    run_reserved_delalloc_lifecycle(&ext4, inode, 0, &DELALLOC_PAYLOAD);
    let operations = device.operations();
    ext4.shutdown_writable()
        .expect("delalloc count clean shutdown failed");
    drop(ext4);
    drop(device);
    operations
}

fn assert_reserved_delalloc_recovery(
    work: &str,
    persistence: PersistenceModel,
    inode: u32,
    crash_point: usize,
) {
    let device = Arc::new(CrashBlockFile::new(work));
    let ext4 = Ext4::load_writable(device.clone()).unwrap_or_else(|error| {
        panic!("reserved-delalloc crash point {crash_point}: mount failed: {error:?}")
    });
    persistence.configure(&device);
    assert_eq!(
        ext4.generic_lookup(ROOT_INO, "range-target")
            .expect("reserved-delalloc target name disappeared"),
        inode
    );
    let size = ext4
        .getattr(inode)
        .expect("reserved-delalloc target inode disappeared")
        .size;
    assert!(
        size == 0 || size == DELALLOC_BLOCK_LEN as u64,
        "reserved-delalloc crash point {crash_point} published invalid EOF {size}"
    );
    if size == DELALLOC_BLOCK_LEN as u64 {
        let mut data = [0u8; DELALLOC_BLOCK_LEN];
        assert_eq!(
            ext4.read(inode, 0, &mut data)
                .expect("read durable delayed mapper data failed"),
            DELALLOC_BLOCK_LEN
        );
        assert_eq!(
            data, DELALLOC_PAYLOAD,
            "durable delayed mapper payload changed"
        );
    } else {
        // An incomplete journal map must recover to the old EOF and permit
        // the same logical block to be reserved and materialised again.
        run_reserved_delalloc_lifecycle(&ext4, inode, 0, &DELALLOC_PAYLOAD);
    }

    // Whether recovery selected the old or new transaction outcome, orphan
    // cleanup must leave the file usable for a fresh append and must not leak
    // the mapped block or its ledger debit.
    run_reserved_delalloc_lifecycle(
        &ext4,
        inode,
        DELALLOC_BLOCK_LEN,
        &[0x3d; DELALLOC_BLOCK_LEN],
    );
    let attr = ext4
        .getattr(inode)
        .expect("post-recovery delayed mapper inode disappeared");
    assert_eq!(attr.size, (2 * DELALLOC_BLOCK_LEN) as u64);
    ext4.shutdown_writable()
        .expect("reserved-delalloc recovery clean shutdown failed");
}

fn run_journal_reserved_delalloc_mapper_matrix(persistence: PersistenceModel) {
    const SEED: &str = "ext4-reserved-delalloc-seed.img";
    const WORK: &str = "ext4-reserved-delalloc-fault.img";
    let inode = create_clean_seed(SEED, ImageKind::Journal);
    let operations = count_reserved_delalloc_operations(SEED, WORK, inode, persistence);
    assert!(
        !operations.is_empty(),
        "reserved delayed mapper issued no persistence operations"
    );
    println!(
        "journal {} reserved delayed mapper has {} persistence points: {:?}",
        persistence.name(),
        operations.len(),
        operations
    );

    for crash_point in 0..operations.len() {
        copy_seed(SEED, WORK);
        run_delalloc_power_loss_subprocess(
            DelallocCrashCase::RawMappedTail,
            persistence,
            WORK,
            inode,
            0,
            0,
            crash_point,
        );

        assert_reserved_delalloc_recovery(WORK, persistence, inode, crash_point);
        assert_clean_e2fsck(WORK, "reserved delayed mapper crash recovery");
    }
    remove_if_exists(SEED);
    remove_if_exists(WORK);
}

/// The data phase is intentionally retryable: its validation completed while
/// holding the direct gate/shard, but the block device may reject the exact
/// payload write before it persists anything.  Do not infer this from an
/// `EIO` code alone—validation failures use the same errno and must instead
/// fail-stop.  This focused test proves the typed mapper outcome preserves the
/// receipt for the former case.
fn run_reserved_delalloc_payload_retry_test(persistence: PersistenceModel) {
    const IMAGE: &str = "ext4-reserved-delalloc-payload-retry.img";
    let inode = create_clean_seed(IMAGE, ImageKind::Journal);
    let device = Arc::new(CrashBlockFile::new(IMAGE));
    persistence.configure(&device);
    let ext4 = Ext4::load_writable(device.clone()).expect("payload retry mount failed");
    let mut lease = ext4
        .reserve_delalloc_lease(1, 0)
        .expect("payload retry reserve failed");
    let mut receipt = ext4
        .test_map_delalloc_reserved_block_append(inode, 0, &mut lease)
        .expect("payload retry map failed");

    device.reset_operation_log();
    device.arm_io_error_at(0);
    assert_eq!(
        ext4.test_writeback_delalloc_mapped_block(&mut receipt, &DELALLOC_PAYLOAD, Some(88))
            .expect_err("armed payload I/O error completed the receipt")
            .code(),
        another_ext4::ErrCode::EIO
    );
    device.disarm_io_error();
    ext4.test_writeback_delalloc_mapped_block(&mut receipt, &DELALLOC_PAYLOAD, Some(88))
        .expect("payload receipt was not retryable after device failure");
    ext4.shutdown_writable()
        .expect("payload retry clean shutdown failed");
    drop(ext4);
    drop(device);
    assert_clean_e2fsck(IMAGE, "reserved delayed mapper payload retry");
    remove_if_exists(IMAGE);
}

/// The narrow raw mapper intentionally declines every full-leaf append before
/// it consumes a data lease.  Its reservation contains no metadata credit or
/// exact-adjacent-block promise, so accepting this shape and discovering a
/// split/merge requirement after zeroing would be a late writeback failure.
fn run_reserved_delalloc_ineligible_shape_test() {
    const IMAGE: &str = "ext4-reserved-delalloc-ineligible-shape.img";
    let inode = create_full_external_leaf_seed(IMAGE);
    let device = Arc::new(CrashBlockFile::new(IMAGE));
    let ext4 = Ext4::load_writable(device.clone()).expect("ineligible-shape mount failed");
    let before = ext4
        .getattr(inode)
        .expect("ineligible-shape getattr before map failed");
    let offset = usize::try_from(before.size).expect("ineligible-shape EOF does not fit usize");
    let mut lease = ext4
        .reserve_delalloc_lease(1, 0)
        .expect("ineligible-shape reserve failed");

    device.reset_operation_log();
    assert_eq!(
        ext4.test_map_delalloc_reserved_block_append(inode, offset, &mut lease)
            .expect_err("full external leaf entered the raw mapper")
            .code(),
        another_ext4::ErrCode::ENOTSUP
    );
    assert!(
        device.operations().is_empty(),
        "ineligible raw mapper shape reached persistence before rejecting"
    );
    ext4.release_delalloc_lease_batch(&mut [&mut lease])
        .expect("ineligible-shape lease was consumed before rejection");
    let after = ext4
        .getattr(inode)
        .expect("ineligible-shape getattr after map failed");
    assert_eq!(
        (after.size, after.blocks),
        (before.size, before.blocks),
        "ineligible raw mapper shape changed inode metadata"
    );
    ext4.shutdown_writable()
        .expect("ineligible-shape clean shutdown failed");
    drop(ext4);
    drop(device);
    assert_clean_e2fsck(IMAGE, "reserved delayed mapper ineligible shape");
    remove_if_exists(IMAGE);
}

/// The production append primitive deliberately has no mapped-tail receipt:
/// one call owns reservation consumption, data durability, extent publication
/// and durable EOF.  Exercise a partial visible EOF, an append beyond that
/// EOF (but immediately after the mapped extent), and a retryable payload
/// device error with the same live lease.
fn run_production_delalloc_append_block_test(persistence: PersistenceModel) {
    const IMAGE: &str = "ext4-production-delalloc-append-block.img";
    make_image(IMAGE, ImageKind::Journal);
    let device = Arc::new(CrashBlockFile::new(IMAGE));
    persistence.configure(&device);
    let ext4 = Ext4::load_writable(device.clone()).expect("production delalloc mount failed");
    let inode = ext4
        .generic_create(
            ROOT_INO,
            "production-delalloc-target",
            InodeMode::FILE | InodeMode::ALL_RWX,
        )
        .expect("production delalloc target create failed");

    // Exercise the exact normal-build authority and strict aligned mapper,
    // rather than relying only on the partial-EOF test facade below.
    let authority = ext4
        .delalloc_append_mapper_authority()
        .expect("strict production mapper authority issue failed");
    assert_eq!(
        ext4.delalloc_append_mapper_authority()
            .expect_err("mapper authority must be issued once")
            .code(),
        another_ext4::ErrCode::EEXIST
    );
    let strict_inode = ext4
        .generic_create(
            ROOT_INO,
            "production-delalloc-strict-target",
            InodeMode::FILE | InodeMode::ALL_RWX,
        )
        .expect("strict production target create failed");
    let mut strict = ext4
        .reserve_delalloc_append_block_authorized(&authority, strict_inode, 0)
        .expect("strict production reservation failed");
    device.reset_operation_log();
    device.arm_io_error_at(0);
    assert_eq!(
        ext4.submit_delalloc_append_block_authorized(
            &authority,
            &mut strict,
            &DELALLOC_PAYLOAD,
            DELALLOC_BLOCK_LEN as u64,
            Some(90),
        ),
        another_ext4::DelallocAppendBlockSubmitOutcome::RetryableNotPublished(
            another_ext4::ErrCode::EIO
        )
    );
    device.disarm_io_error();
    assert_eq!(
        ext4.submit_delalloc_append_block_authorized(
            &authority,
            &mut strict,
            &DELALLOC_PAYLOAD,
            DELALLOC_BLOCK_LEN as u64,
            Some(90),
        ),
        another_ext4::DelallocAppendBlockSubmitOutcome::Completed
    );
    let strict_attr = ext4
        .getattr(strict_inode)
        .expect("strict production getattr failed");
    assert_eq!(strict_attr.size, DELALLOC_BLOCK_LEN as u64);
    let mut strict_data = [0u8; DELALLOC_BLOCK_LEN];
    assert_eq!(
        ext4.read(strict_inode, 0, &mut strict_data)
            .expect("strict production read failed"),
        strict_data.len()
    );
    assert_eq!(strict_data, DELALLOC_PAYLOAD);

    let mut first = ext4
        .reserve_delalloc_append_block_capability(inode, 0)
        .expect("production delalloc first reservation failed");
    // The production mapper persists the complete zero-padded payload block
    // exactly once. A failure of that unpublished data write retains the same
    // reservation for retry.
    device.reset_operation_log();
    device.arm_io_error_at(0);
    let outcome =
        ext4.submit_delalloc_append_block_capability(&mut first, &DELALLOC_PAYLOAD, 17, Some(91));
    assert_eq!(
        outcome,
        another_ext4::DelallocAppendBlockSubmitOutcome::RetryableNotPublished(
            another_ext4::ErrCode::EIO
        )
    );
    device.disarm_io_error();
    assert_eq!(
        ext4.submit_delalloc_append_block_capability(&mut first, &DELALLOC_PAYLOAD, 17, Some(91),),
        another_ext4::DelallocAppendBlockSubmitOutcome::Completed
    );

    // `i_size` is intentionally only 17 here. The second reservation proves
    // append geometry comes from the extent tail, not from a rounded-up EOF.
    let mut second = ext4
        .reserve_delalloc_append_block(inode, DELALLOC_BLOCK_LEN)
        .expect("production delayed second reservation failed");
    let second_payload = [0x4e; DELALLOC_BLOCK_LEN];
    ext4.writeback_delalloc_append_block(
        inode,
        DelallocAppendBlockWriteback {
            offset: DELALLOC_BLOCK_LEN,
            payload: &second_payload,
            durable_eof: (2 * DELALLOC_BLOCK_LEN) as u64,
            mtime: Some(92),
            ctime: None,
        },
        &mut second,
    )
    .expect("production delayed second append failed");

    // Cancellation is an ownership transition too: exercise the typed
    // release path with an otherwise eligible next append, then prove it did
    // not publish an extent or EOF merely by reserving space.
    let mut cancelled = ext4
        .reserve_delalloc_append_block_capability(inode, 2 * DELALLOC_BLOCK_LEN)
        .expect("typed delayed reservation for release test failed");
    ext4.release_delalloc_append_block_capability(&mut cancelled)
        .expect("typed delayed reservation release failed");

    let attr = ext4
        .getattr(inode)
        .expect("production delayed final getattr failed");
    assert_eq!(attr.size, (2 * DELALLOC_BLOCK_LEN) as u64);
    let mut data = vec![0u8; 2 * DELALLOC_BLOCK_LEN];
    assert_eq!(
        ext4.read(inode, 0, &mut data)
            .expect("production delayed final read failed"),
        data.len()
    );
    assert_eq!(&data[..17], &DELALLOC_PAYLOAD[..17]);
    assert!(
        data[17..DELALLOC_BLOCK_LEN].iter().all(|byte| *byte == 0),
        "partial durable EOF exposed non-zero stale bytes"
    );
    assert_eq!(&data[DELALLOC_BLOCK_LEN..], &second_payload);
    ext4.shutdown_writable()
        .expect("production delayed append clean shutdown failed");
    drop(ext4);
    drop(device);
    assert_clean_e2fsck(IMAGE, "production delayed append");
    remove_if_exists(IMAGE);
}

/// Exercise both bounded extent-tree growth forms accepted by the production
/// mapper.  The root-split case also injects a payload I/O failure, proving
/// that data and metadata debits return to the same live lease before retry.
fn run_production_delalloc_extent_split_test(persistence: PersistenceModel) {
    const ROOT_IMAGE: &str = "ext4-production-delalloc-root-split.img";
    const LEAF_IMAGE: &str = "ext4-production-delalloc-leaf-split.img";

    let root_inode = create_external_leaf_seed(ROOT_IMAGE);
    let root_device = Arc::new(CrashBlockFile::new(ROOT_IMAGE));
    persistence.configure(&root_device);
    let root_ext4 =
        Ext4::load_writable(root_device.clone()).expect("production root-split mount failed");
    let root_offset = INLINE_EXTENT_CAPACITY * WRITE_LEN;
    let root_payload = [0x8d; DELALLOC_BLOCK_LEN];
    let mut root_lease = root_ext4
        .reserve_delalloc_append_block(root_inode, root_offset)
        .expect("production root-split reservation failed");
    root_device.reset_operation_log();
    // The unpublished zero-padded payload block is a single write; failure
    // must roll allocation consumption back into the same lease.
    root_device.arm_io_error_at(0);
    assert_eq!(
        root_ext4
            .writeback_delalloc_append_block(
                root_inode,
                DelallocAppendBlockWriteback {
                    offset: root_offset,
                    payload: &root_payload,
                    durable_eof: (root_offset + DELALLOC_BLOCK_LEN) as u64,
                    mtime: Some(94),
                    ctime: None,
                },
                &mut root_lease,
            )
            .expect_err("production root-split payload error unexpectedly succeeded")
            .code(),
        another_ext4::ErrCode::EIO
    );
    root_device.disarm_io_error();
    root_ext4
        .writeback_delalloc_append_block(
            root_inode,
            DelallocAppendBlockWriteback {
                offset: root_offset,
                payload: &root_payload,
                durable_eof: (root_offset + DELALLOC_BLOCK_LEN) as u64,
                mtime: Some(94),
                ctime: None,
            },
            &mut root_lease,
        )
        .expect("production root-split retry failed");
    assert_fragmented_prefix(&root_ext4, root_inode);
    let mut root_data = [0u8; DELALLOC_BLOCK_LEN];
    assert_eq!(
        root_ext4
            .read(root_inode, root_offset, &mut root_data)
            .expect("production root-split read failed"),
        root_data.len()
    );
    assert_eq!(root_data, root_payload);
    root_ext4
        .shutdown_writable()
        .expect("production root-split clean shutdown failed");
    drop(root_ext4);
    drop(root_device);
    assert_clean_e2fsck(ROOT_IMAGE, "production delayed root split");
    remove_if_exists(ROOT_IMAGE);

    let leaf_inode = create_full_external_leaf_seed(LEAF_IMAGE);
    let leaf_device = Arc::new(CrashBlockFile::new(LEAF_IMAGE));
    persistence.configure(&leaf_device);
    let leaf_ext4 =
        Ext4::load_writable(leaf_device.clone()).expect("production leaf-split mount failed");
    let leaf_offset = EXTERNAL_LEAF_EXTENT_CAPACITY * WRITE_LEN;
    let leaf_payload = [0x8e; DELALLOC_BLOCK_LEN];
    let mut leaf_lease = leaf_ext4
        .reserve_delalloc_append_block(leaf_inode, leaf_offset)
        .expect("production leaf-split reservation failed");
    leaf_ext4
        .writeback_delalloc_append_block(
            leaf_inode,
            DelallocAppendBlockWriteback {
                offset: leaf_offset,
                payload: &leaf_payload,
                durable_eof: (leaf_offset + DELALLOC_BLOCK_LEN) as u64,
                mtime: Some(95),
                ctime: None,
            },
            &mut leaf_lease,
        )
        .expect("production leaf-split append failed");
    assert_full_external_leaf_samples(&leaf_ext4, leaf_inode);
    let mut leaf_data = [0u8; DELALLOC_BLOCK_LEN];
    assert_eq!(
        leaf_ext4
            .read(leaf_inode, leaf_offset, &mut leaf_data)
            .expect("production leaf-split read failed"),
        leaf_data.len()
    );
    assert_eq!(leaf_data, leaf_payload);
    leaf_ext4
        .shutdown_writable()
        .expect("production leaf-split clean shutdown failed");
    drop(leaf_ext4);
    drop(leaf_device);
    assert_clean_e2fsck(LEAF_IMAGE, "production delayed leaf split");
    remove_if_exists(LEAF_IMAGE);
}

/// A delayed append after a partial truncate must not expose bytes that were
/// previously hidden past EOF.  The same test also proves a reservation is
/// invalidated when a later truncate changes the observed EOF before its
/// mapper runs.
fn run_production_delalloc_partial_eof_test(persistence: PersistenceModel) {
    const IMAGE: &str = "ext4-production-delalloc-partial-eof.img";
    make_image(IMAGE, ImageKind::Journal);
    let device = Arc::new(CrashBlockFile::new(IMAGE));
    persistence.configure(&device);
    let ext4 = Ext4::load_writable(device.clone()).expect("production partial-EOF mount failed");
    let inode = ext4
        .generic_create(
            ROOT_INO,
            "production-partial-eof-target",
            InodeMode::FILE | InodeMode::ALL_RWX,
        )
        .expect("production partial-EOF target create failed");
    let old_block = [0x6a; DELALLOC_BLOCK_LEN];
    ext4.write(inode, 0, &old_block)
        .expect("production partial-EOF seed write failed");
    ext4.setattr(
        inode,
        another_ext4::SetAttr {
            size: Some(17),
            ..Default::default()
        },
    )
    .expect("production partial-EOF truncate failed");

    let mut lease = ext4
        .reserve_delalloc_append_block(inode, DELALLOC_BLOCK_LEN)
        .expect("production partial-EOF reservation failed");
    let payload = [0x6b; DELALLOC_BLOCK_LEN];
    ext4.writeback_delalloc_append_block(
        inode,
        DelallocAppendBlockWriteback {
            offset: DELALLOC_BLOCK_LEN,
            payload: &payload,
            durable_eof: (2 * DELALLOC_BLOCK_LEN) as u64,
            mtime: Some(96),
            ctime: None,
        },
        &mut lease,
    )
    .expect("production partial-EOF append failed");
    let mut data = vec![0u8; 2 * DELALLOC_BLOCK_LEN];
    assert_eq!(
        ext4.read(inode, 0, &mut data)
            .expect("production partial-EOF read failed"),
        data.len()
    );
    assert_eq!(&data[..17], &old_block[..17]);
    assert!(
        data[17..DELALLOC_BLOCK_LEN].iter().all(|byte| *byte == 0),
        "partial truncate tail became visible during delayed append"
    );
    assert_eq!(&data[DELALLOC_BLOCK_LEN..], &payload);

    let mut stale = ext4
        .reserve_delalloc_append_block(inode, 2 * DELALLOC_BLOCK_LEN)
        .expect("production stale-certificate reservation failed");
    ext4.setattr(
        inode,
        another_ext4::SetAttr {
            size: Some(19),
            ..Default::default()
        },
    )
    .expect("production stale-certificate truncate failed");
    assert_eq!(
        ext4.writeback_delalloc_append_block(
            inode,
            DelallocAppendBlockWriteback {
                offset: 2 * DELALLOC_BLOCK_LEN,
                payload: &payload,
                durable_eof: (3 * DELALLOC_BLOCK_LEN) as u64,
                mtime: Some(97),
                ctime: None,
            },
            &mut stale,
        )
        .expect_err("stale delayed reservation recreated a truncated tail")
        .code(),
        another_ext4::ErrCode::EAGAIN
    );
    ext4.release_delalloc_lease_batch(&mut [&mut stale])
        .expect("stale delayed reservation was not releasable");
    // `setattr(size < allocated extent tail)` is intentionally covered by the
    // separate linked-orphan/truncate work. Restore this fixture's old EOF so
    // this mapper-focused test does not leave that known independent state for
    // `e2fsck` to diagnose.
    ext4.setattr(
        inode,
        another_ext4::SetAttr {
            size: Some((2 * DELALLOC_BLOCK_LEN) as u64),
            ..Default::default()
        },
    )
    .expect("production partial-EOF fixture restoration failed");
    ext4.shutdown_writable()
        .expect("production partial-EOF clean shutdown failed");
    drop(ext4);
    drop(device);
    assert_clean_e2fsck(IMAGE, "production delayed partial EOF");
    remove_if_exists(IMAGE);
}

/// A journal write failure before its commit record fail-stops the mount, but
/// must not hand an active linear lease back to the caller.  The mapper owns
/// the abort-only release before poisoning, so ordinary scope teardown is
/// safe and the next mount observes the old EOF.
fn run_production_delalloc_before_commit_fail_stop_test(persistence: PersistenceModel) {
    const IMAGE: &str = "ext4-production-delalloc-before-commit.img";
    make_image(IMAGE, ImageKind::Journal);
    let device = Arc::new(CrashBlockFile::new(IMAGE));
    persistence.configure(&device);
    let ext4 = Ext4::load_writable(device.clone()).expect("production before-commit mount failed");
    let inode = ext4
        .generic_create(
            ROOT_INO,
            "production-before-commit-target",
            InodeMode::FILE | InodeMode::ALL_RWX,
        )
        .expect("production before-commit target create failed");
    let mut reservation = ext4
        .reserve_delalloc_append_block_capability(inode, 0)
        .expect("production before-commit reservation failed");
    device.reset_operation_log();
    // The zero-padded payload write/flush complete first. The third operation
    // is the first journal I/O, before a commit record.
    device.arm_io_error_at(2);
    assert_eq!(
        ext4.submit_delalloc_append_block_capability(
            &mut reservation,
            &DELALLOC_PAYLOAD,
            17,
            Some(98),
        ),
        another_ext4::DelallocAppendBlockSubmitOutcome::Terminal(another_ext4::ErrCode::EIO),
        "before-commit fail-stop must terminalise the typed capability"
    );
    device.disarm_io_error();
    // This drop is the assertion: the before-commit branch must already have
    // terminalised the typed capability before returning the fail-stop result.
    drop(reservation);
    drop(ext4);
    drop(device);

    let recovered_device = Arc::new(CrashBlockFile::new(IMAGE));
    persistence.configure(&recovered_device);
    let recovered = Ext4::load_writable(recovered_device.clone())
        .expect("production before-commit recovery mount failed");
    assert_eq!(
        recovered
            .getattr(inode)
            .expect("production before-commit inode disappeared")
            .size,
        0,
        "before-commit failure published an EOF"
    );
    recovered
        .shutdown_writable()
        .expect("production before-commit recovery shutdown failed");
    drop(recovered);
    drop(recovered_device);
    assert_clean_e2fsck(IMAGE, "production delayed before-commit failure");
    remove_if_exists(IMAGE);
}

/// A delayed lease can outlive another operation that fail-stops the mount.
/// The publication-aware mapper must terminalise that lease before returning:
/// reporting it as retryable would eventually trigger the live-lease Drop
/// fail-stop, even though no mapper I/O was attempted.
fn run_production_delalloc_preexisting_fail_stop_test(persistence: PersistenceModel) {
    const IMAGE: &str = "ext4-production-delalloc-preexisting-fail-stop.img";
    const FOREIGN_IMAGE: &str = "ext4-production-delalloc-foreign-lease.img";
    make_image(IMAGE, ImageKind::Journal);
    make_image(FOREIGN_IMAGE, ImageKind::Journal);
    let device = Arc::new(CrashBlockFile::new(IMAGE));
    let foreign_device = Arc::new(CrashBlockFile::new(FOREIGN_IMAGE));
    persistence.configure(&device);
    persistence.configure(&foreign_device);
    let ext4 =
        Ext4::load_writable(device.clone()).expect("production preexisting-fail-stop mount failed");
    let foreign =
        Ext4::load_writable(foreign_device.clone()).expect("production foreign-lease mount failed");
    let inode = ext4
        .generic_create(
            ROOT_INO,
            "production-preexisting-fail-stop-target",
            InodeMode::FILE | InodeMode::ALL_RWX,
        )
        .expect("production preexisting-fail-stop target create failed");
    let mut reservation = ext4
        .reserve_delalloc_append_block_capability(inode, 0)
        .expect("production preexisting-fail-stop reservation failed");
    let foreign_inode = foreign
        .generic_create(
            ROOT_INO,
            "production-foreign-lease-target",
            InodeMode::FILE | InodeMode::ALL_RWX,
        )
        .expect("production foreign-lease target create failed");
    let mut foreign_reservation = foreign
        .reserve_delalloc_append_block_capability(foreign_inode, 0)
        .expect("production foreign-lease reservation failed");

    device.reset_operation_log();
    ext4.fail_stop_mutations();
    assert_eq!(
        ext4.submit_delalloc_append_block_capability(
            &mut reservation,
            &DELALLOC_PAYLOAD,
            17,
            Some(99),
        ),
        another_ext4::DelallocAppendBlockSubmitOutcome::Terminal(another_ext4::ErrCode::EIO),
        "an already fail-stopped mount must not return a live retryable lease"
    );
    assert!(
        device.operations().is_empty(),
        "preexisting fail-stop must reject before any mapper persistence I/O"
    );
    assert_eq!(
        ext4.submit_delalloc_append_block_capability(
            &mut foreign_reservation,
            &DELALLOC_PAYLOAD,
            17,
            Some(100),
        ),
        another_ext4::DelallocAppendBlockSubmitOutcome::RetryableNotPublished(
            another_ext4::ErrCode::EINVAL
        ),
        "a poisoned foreign mount must not terminalise a lease it did not issue"
    );
    foreign
        .release_delalloc_append_block_capability(&mut foreign_reservation)
        .expect("foreign lease must remain releasable by its source mount");
    assert_eq!(
        ext4.submit_delalloc_append_block_capability(
            &mut foreign_reservation,
            &DELALLOC_PAYLOAD,
            17,
            Some(101),
        ),
        another_ext4::DelallocAppendBlockSubmitOutcome::Terminal(another_ext4::ErrCode::EINVAL),
        "an inactive foreign lease must not be reported as retryable"
    );
    // This drop is the ownership assertion: `Terminal` must have deactivated
    // the capability even though no transaction was started by this call.
    drop(reservation);
    drop(ext4);
    drop(device);
    drop(foreign_reservation);
    foreign
        .shutdown_writable()
        .expect("foreign-lease clean shutdown failed");
    drop(foreign);
    drop(foreign_device);
    remove_if_exists(IMAGE);
    assert_clean_e2fsck(FOREIGN_IMAGE, "foreign delayed lease provenance");
    remove_if_exists(FOREIGN_IMAGE);
}

fn production_delalloc_append_block(
    ext4: &Ext4,
    inode: u32,
    lease: &mut another_ext4::DelallocLease,
    offset: usize,
    durable_eof: u64,
) -> Result<(), another_ext4::Ext4Error> {
    ext4.writeback_delalloc_append_block(
        inode,
        DelallocAppendBlockWriteback {
            offset,
            payload: &DELALLOC_PAYLOAD,
            durable_eof,
            mtime: Some(93),
            ctime: None,
        },
        lease,
    )
}

fn count_production_delalloc_append_operations(
    seed: &str,
    work: &str,
    inode: u32,
    persistence: PersistenceModel,
) -> Vec<CrashDeviceOperation> {
    copy_seed(seed, work);
    let device = Arc::new(CrashBlockFile::new(work));
    persistence.configure(&device);
    let ext4 = Ext4::load_writable(device.clone()).expect("production delayed count mount failed");
    let mut lease = ext4
        .reserve_delalloc_append_block(inode, 0)
        .expect("production delayed count reservation failed");
    device.reset_operation_log();
    production_delalloc_append_block(&ext4, inode, &mut lease, 0, 17)
        .expect("production delayed count operation failed");
    let operations = device.operations();
    ext4.shutdown_writable()
        .expect("production delayed count clean shutdown failed");
    operations
}

fn recover_production_delalloc_append_block(
    work: &str,
    inode: u32,
    persistence: PersistenceModel,
    crash_point: usize,
) {
    let device = Arc::new(CrashBlockFile::new(work));
    persistence.configure(&device);
    let ext4 = Ext4::load_writable(device.clone()).unwrap_or_else(|error| {
        panic!("production delayed crash point {crash_point}: mount failed: {error:?}")
    });
    let attr = ext4
        .getattr(inode)
        .expect("production delayed target disappeared after recovery");
    assert!(
        attr.size == 0 || attr.size == 17,
        "production delayed crash point {crash_point} published invalid EOF {}",
        attr.size
    );
    if attr.size == 0 {
        let mut lease = ext4
            .reserve_delalloc_append_block(inode, 0)
            .expect("production delayed recovery reservation failed");
        production_delalloc_append_block(&ext4, inode, &mut lease, 0, 17)
            .expect("production delayed recovery retry failed");
    }
    let mut data = [0u8; 17];
    assert_eq!(
        ext4.read(inode, 0, &mut data)
            .expect("production delayed recovery read failed"),
        data.len()
    );
    assert_eq!(&data, &DELALLOC_PAYLOAD[..17]);
    ext4.shutdown_writable()
        .expect("production delayed recovery clean shutdown failed");
}

fn run_journal_production_delalloc_append_matrix(persistence: PersistenceModel) {
    let prefix = format!("ext4-production-delalloc-append-{}", persistence.name());
    let seed = format!("{prefix}-seed.img");
    let count_work = format!("{prefix}-count.img");
    let work = format!("{prefix}-fault.img");
    let inode = create_clean_seed(&seed, ImageKind::Journal);
    let operations =
        count_production_delalloc_append_operations(&seed, &count_work, inode, persistence);
    assert!(
        !operations.is_empty(),
        "production delayed append issued no persistence operations"
    );
    println!(
        "journal {} production delayed append has {} persistence points: {:?}",
        persistence.name(),
        operations.len(),
        operations
    );
    for crash_point in 0..operations.len() {
        copy_seed(&seed, &work);
        run_delalloc_power_loss_subprocess(
            DelallocCrashCase::ProductionAppend,
            persistence,
            &work,
            inode,
            0,
            17,
            crash_point,
        );

        recover_production_delalloc_append_block(&work, inode, persistence, crash_point);
        assert_clean_e2fsck(&work, "production delayed append crash recovery");
    }
    remove_if_exists(&seed);
    remove_if_exists(&count_work);
    remove_if_exists(&work);
}

#[derive(Clone, Copy)]
enum ProductionProjectedShape {
    Plain,
    RootGrow,
    FullLeafSplit,
    FullLeafMerge,
}

fn create_full_external_leaf_merge_seed(path: &str) -> u32 {
    let inode = create_full_external_leaf_seed(path);
    let device = Arc::new(CrashBlockFile::new(path));
    let ext4 = Ext4::load_writable(device).expect("full-leaf merge seed mount failed");
    let filler = ext4
        .unlink(ROOT_INO, "full-external-leaf-filler")
        .expect("full-leaf merge filler unlink failed")
        .expect("full-leaf merge filler had no reclaim handle");
    ext4.reclaim_inode(filler)
        .expect("full-leaf merge filler reclaim failed");
    ext4.shutdown_writable()
        .expect("full-leaf merge seed shutdown failed");
    inode
}

fn read_inode_blocks(path: &str, inode: u32) -> u64 {
    let device = Arc::new(CrashBlockFile::new(path));
    let ext4 = Ext4::load_writable(device).expect("projected seed inspection mount failed");
    let blocks = ext4
        .getattr(inode)
        .expect("projected seed inspection getattr failed")
        .blocks;
    ext4.shutdown_writable()
        .expect("projected seed inspection shutdown failed");
    blocks
}

fn count_production_projected_operations(
    seed: &str,
    work: &str,
    inode: u32,
    persistence: PersistenceModel,
    case: DelallocCrashCase,
    offset: usize,
    durable_eof: u64,
) -> Vec<CrashDeviceOperation> {
    copy_seed(seed, work);
    let device = Arc::new(CrashBlockFile::new(work));
    persistence.configure(&device);
    let ext4 = Ext4::load_writable(device.clone()).expect("projected count mount failed");
    let steps = production_projected_steps(case, offset, durable_eof);
    run_production_projected_steps(&ext4, inode, &steps, || device.reset_operation_log());
    let operations = device.operations();
    ext4.shutdown_writable()
        .expect("projected count shutdown failed");
    assert!(
        !operations.is_empty(),
        "projected production scenario issued no persistence operations"
    );
    operations
}

fn recover_production_projected_scenario(
    work: &str,
    inode: u32,
    persistence: PersistenceModel,
    crash_point: usize,
    case: DelallocCrashCase,
    offset: usize,
    durable_eof: u64,
    shape: ProductionProjectedShape,
    blocks_before: u64,
) {
    let device = Arc::new(CrashBlockFile::new(work));
    persistence.configure(&device);
    let ext4 = Ext4::load_writable(device.clone()).unwrap_or_else(|error| {
        panic!("projected crash point {crash_point}: recovery mount failed: {error:?}")
    });
    let steps = production_projected_steps(case, offset, durable_eof);
    let size = ext4
        .getattr(inode)
        .expect("projected target disappeared after recovery")
        .size;
    let initial_eof = steps[0].expected_durable_eof_before;
    let valid_prefix =
        size == initial_eof || steps.iter().any(|step| size == step.durable_eof_after);
    assert!(
        valid_prefix,
        "projected crash point {crash_point} published non-prefix EOF {size}"
    );
    let remaining: Vec<_> = steps
        .iter()
        .copied()
        .filter(|step| step.durable_eof_after > size)
        .collect();
    run_production_projected_steps(&ext4, inode, &remaining, || {});

    let final_attr = ext4
        .getattr(inode)
        .expect("projected target disappeared after retry");
    assert_eq!(
        final_attr.size,
        steps.last().unwrap().durable_eof_after,
        "projected recovery did not reach the final EOF"
    );
    match case {
        DelallocCrashCase::ProductionProjectedMultiEntry => {
            let mut data = vec![0u8; 2 * DELALLOC_BLOCK_LEN];
            assert_eq!(
                ext4.read(inode, 0, &mut data)
                    .expect("projected multi-entry read failed"),
                data.len()
            );
            assert_eq!(&data[..DELALLOC_BLOCK_LEN], &[0xb7; DELALLOC_BLOCK_LEN]);
            assert_eq!(&data[DELALLOC_BLOCK_LEN..], &[0x5c; DELALLOC_BLOCK_LEN]);
        }
        DelallocCrashCase::ProductionProjectedSparse => {
            let mut data = vec![0u8; 4 * DELALLOC_BLOCK_LEN];
            assert_eq!(
                ext4.read(inode, 0, &mut data)
                    .expect("projected sparse read failed"),
                data.len()
            );
            assert!(
                data[..3 * DELALLOC_BLOCK_LEN].iter().all(|byte| *byte == 0),
                "projected sparse recovery allocated or exposed the logical gap"
            );
            assert_eq!(&data[3 * DELALLOC_BLOCK_LEN..], &[0x6d; DELALLOC_BLOCK_LEN]);
        }
        DelallocCrashCase::ProductionProjectedSingle => {
            let mut tail = [0u8; DELALLOC_BLOCK_LEN];
            assert_eq!(
                ext4.read(inode, offset, &mut tail)
                    .expect("projected right-spine tail read failed"),
                tail.len()
            );
            assert_eq!(tail, DELALLOC_PAYLOAD);
        }
        _ => unreachable!(),
    }

    match shape {
        ProductionProjectedShape::Plain => {}
        ProductionProjectedShape::RootGrow => {
            assert_fragmented_prefix(&ext4, inode);
            assert!(
                final_attr.blocks > blocks_before + (DELALLOC_BLOCK_LEN / 512) as u64,
                "production root grow did not allocate an extent-tree node"
            );
        }
        ProductionProjectedShape::FullLeafSplit => {
            assert_full_external_leaf_samples(&ext4, inode);
            assert!(
                final_attr.blocks > blocks_before + (DELALLOC_BLOCK_LEN / 512) as u64,
                "production full-leaf nonmerge did not allocate a split node"
            );
        }
        ProductionProjectedShape::FullLeafMerge => {
            assert_full_external_leaf_samples(&ext4, inode);
            assert_eq!(
                final_attr.blocks,
                blocks_before + (DELALLOC_BLOCK_LEN / 512) as u64,
                "production full-leaf adjacent merge consumed a reserved split node"
            );
        }
    }
    ext4.shutdown_writable()
        .expect("projected recovery shutdown failed");
}

fn run_journal_production_projected_matrix(
    persistence: PersistenceModel,
    label: &str,
    create_seed: impl FnOnce(&str) -> u32,
    case: DelallocCrashCase,
    offset: usize,
    durable_eof: u64,
    shape: ProductionProjectedShape,
) {
    let prefix = format!("ext4-production-{label}-{}", persistence.name());
    let seed = format!("{prefix}-seed.img");
    let count_work = format!("{prefix}-count.img");
    let work = format!("{prefix}-fault.img");
    let inode = create_seed(&seed);
    let blocks_before = read_inode_blocks(&seed, inode);
    assert_clean_e2fsck(&seed, "projected production seed");
    let operations = count_production_projected_operations(
        &seed,
        &count_work,
        inode,
        persistence,
        case,
        offset,
        durable_eof,
    );
    println!(
        "journal {} production {label} has {} persistence points: {:?}",
        persistence.name(),
        operations.len(),
        operations
    );
    for crash_point in 0..operations.len() {
        copy_seed(&seed, &work);
        run_delalloc_power_loss_subprocess(
            case,
            persistence,
            &work,
            inode,
            offset,
            durable_eof,
            crash_point,
        );
        recover_production_projected_scenario(
            &work,
            inode,
            persistence,
            crash_point,
            case,
            offset,
            durable_eof,
            shape,
            blocks_before,
        );
        assert_clean_e2fsck(&work, "projected production crash recovery");
    }
    remove_if_exists(&seed);
    remove_if_exists(&count_work);
    remove_if_exists(&work);
}

/// Build a seed whose block past EOF deliberately contains non-zero bytes.
/// Extending it through the delayed mapper must make that former tail zero,
/// including after every simulated crash boundary.
fn create_production_partial_eof_seed(path: &str) -> u32 {
    make_image(path, ImageKind::Journal);
    let device = Arc::new(CrashBlockFile::new(path));
    let ext4 = Ext4::load_writable(device.clone()).expect("partial-EOF matrix seed mount failed");
    let inode = ext4
        .generic_create(
            ROOT_INO,
            "production-partial-eof-target",
            InodeMode::FILE | InodeMode::ALL_RWX,
        )
        .expect("partial-EOF matrix seed create failed");
    ext4.write(inode, 0, &[0x6a; DELALLOC_BLOCK_LEN])
        .expect("partial-EOF matrix seed write failed");
    ext4.setattr(
        inode,
        another_ext4::SetAttr {
            size: Some(17),
            ..Default::default()
        },
    )
    .expect("partial-EOF matrix seed truncate failed");
    ext4.shutdown_writable()
        .expect("partial-EOF matrix seed shutdown failed");
    drop(ext4);
    drop(device);
    inode
}

fn count_production_partial_eof_operations(
    seed: &str,
    work: &str,
    inode: u32,
    persistence: PersistenceModel,
) -> Vec<CrashDeviceOperation> {
    copy_seed(seed, work);
    let device = Arc::new(CrashBlockFile::new(work));
    persistence.configure(&device);
    let ext4 = Ext4::load_writable(device.clone()).expect("partial-EOF count mount failed");
    let mut lease = ext4
        .reserve_delalloc_append_block(inode, DELALLOC_BLOCK_LEN)
        .expect("partial-EOF count reservation failed");
    device.reset_operation_log();
    production_delalloc_append_block(
        &ext4,
        inode,
        &mut lease,
        DELALLOC_BLOCK_LEN,
        (2 * DELALLOC_BLOCK_LEN) as u64,
    )
    .expect("partial-EOF count append failed");
    let operations = device.operations();
    ext4.shutdown_writable()
        .expect("partial-EOF count shutdown failed");
    operations
}

fn recover_production_partial_eof(
    work: &str,
    inode: u32,
    persistence: PersistenceModel,
    crash_point: usize,
) {
    let device = Arc::new(CrashBlockFile::new(work));
    persistence.configure(&device);
    let ext4 = Ext4::load_writable(device.clone()).unwrap_or_else(|error| {
        panic!("partial-EOF crash point {crash_point}: mount failed: {error:?}")
    });
    let size = ext4
        .getattr(inode)
        .expect("partial-EOF target disappeared after recovery")
        .size;
    assert!(
        size == 17 || size == (2 * DELALLOC_BLOCK_LEN) as u64,
        "partial-EOF crash point {crash_point} published invalid EOF {size}"
    );
    if size == 17 {
        let mut lease = ext4
            .reserve_delalloc_append_block(inode, DELALLOC_BLOCK_LEN)
            .expect("partial-EOF recovery reservation failed");
        production_delalloc_append_block(
            &ext4,
            inode,
            &mut lease,
            DELALLOC_BLOCK_LEN,
            (2 * DELALLOC_BLOCK_LEN) as u64,
        )
        .expect("partial-EOF recovery retry failed");
    }
    let mut data = vec![0u8; 2 * DELALLOC_BLOCK_LEN];
    assert_eq!(
        ext4.read(inode, 0, &mut data)
            .expect("partial-EOF recovery read failed"),
        data.len()
    );
    assert_eq!(&data[..17], &[0x6a; 17]);
    assert!(
        data[17..DELALLOC_BLOCK_LEN].iter().all(|byte| *byte == 0),
        "partial-EOF crash point {crash_point} exposed a stale truncate tail"
    );
    assert_eq!(&data[DELALLOC_BLOCK_LEN..], &DELALLOC_PAYLOAD);
    ext4.shutdown_writable()
        .expect("partial-EOF recovery shutdown failed");
}

fn run_journal_production_partial_eof_matrix(persistence: PersistenceModel) {
    let prefix = format!(
        "ext4-production-delalloc-partial-eof-{}",
        persistence.name()
    );
    let seed = format!("{prefix}-seed.img");
    let count_work = format!("{prefix}-count.img");
    let work = format!("{prefix}-fault.img");
    let inode = create_production_partial_eof_seed(&seed);
    let operations =
        count_production_partial_eof_operations(&seed, &count_work, inode, persistence);
    assert!(
        !operations.is_empty(),
        "partial-EOF delayed append issued no persistence operations"
    );
    println!(
        "journal {} production partial-EOF delayed append has {} persistence points: {:?}",
        persistence.name(),
        operations.len(),
        operations
    );
    for crash_point in 0..operations.len() {
        copy_seed(&seed, &work);
        run_delalloc_power_loss_subprocess(
            DelallocCrashCase::ProductionAppend,
            persistence,
            &work,
            inode,
            DELALLOC_BLOCK_LEN,
            (2 * DELALLOC_BLOCK_LEN) as u64,
            crash_point,
        );
        recover_production_partial_eof(&work, inode, persistence, crash_point);
        assert_clean_e2fsck(
            &work,
            "production partial-EOF delayed append crash recovery",
        );
    }
    remove_if_exists(&seed);
    remove_if_exists(&count_work);
    remove_if_exists(&work);
}

fn run_production_delalloc_contract_failure_test(persistence: PersistenceModel) {
    const IMAGE: &str = "ext4-production-delalloc-contract-failure.img";
    make_image(IMAGE, ImageKind::Journal);
    let device = Arc::new(CrashBlockFile::new(IMAGE));
    persistence.configure(&device);
    let ext4 = Ext4::load_writable(device.clone()).expect("contract-failure mount failed");
    let inode = ext4
        .generic_create(
            ROOT_INO,
            "contract-failure-target",
            InodeMode::FILE | InodeMode::ALL_RWX,
        )
        .expect("contract-failure target create failed");
    let authority = ext4
        .delalloc_append_mapper_authority()
        .expect("contract-failure authority issue failed");
    let mut pool = ext4
        .create_delalloc_extent_node_pool_authorized(&authority, inode)
        .expect("contract-failure pool creation failed");
    let mut reservation = ext4
        .reserve_delalloc_append_block_projected_authorized(&authority, inode, 0, 0, &mut pool)
        .expect("contract-failure reservation failed");

    ext4.setattr(
        inode,
        another_ext4::SetAttr {
            size: Some(1),
            ..Default::default()
        },
    )
    .expect("contract-failure stale EOF setup failed");
    assert_eq!(
        ext4.submit_delalloc_append_block_authorized_with_pool(
            &authority,
            &mut reservation,
            another_ext4::DelallocAppendBlockPublication {
                payload: &DELALLOC_PAYLOAD,
                durable_eof: DELALLOC_BLOCK_LEN as u64,
                mtime: Some(101),
                ctime: Some(102),
            },
            Some(&mut pool),
        ),
        another_ext4::DelallocAppendBlockSubmitOutcome::Terminal(another_ext4::ErrCode::EIO),
        "a stale production certificate must fail-stop once, not become EAGAIN"
    );
    ext4.terminalize_delalloc_extent_node_pool_authorized_after_fail_stop(&authority, &mut pool)
        .expect("contract-failure pool terminalisation failed");
    drop(ext4);
    drop(device);
    assert_clean_e2fsck(IMAGE, "production delayed contract failure");
    remove_if_exists(IMAGE);
}

fn main() {
    if run_delalloc_power_loss_child_from_args() {
        return;
    }
    if let Ok(case) = std::env::var("DRAGONOS_EXT4_RECOVERY_CASE") {
        let persistence = PersistenceModel::WriteBack;
        match case.as_str() {
            "reserved-payload-retry" => run_reserved_delalloc_payload_retry_test(persistence),
            "production-append" => run_production_delalloc_append_block_test(persistence),
            "production-split" => run_production_delalloc_extent_split_test(persistence),
            _ => panic!("unknown DRAGONOS_EXT4_RECOVERY_CASE: {case}"),
        }
        return;
    }
    for persistence in [PersistenceModel::WriteBack, PersistenceModel::WriteThrough] {
        run_kind(ImageKind::Journal, persistence);
        run_journal_external_leaf_matrix(persistence);
        run_journal_existing_leaf_matrix(persistence);
        run_journal_full_leaf_split_matrix(persistence);
        run_journal_reclaim_matrix(persistence);
        run_journal_linked_tail_final_unlink_matrix(persistence);
        run_journal_linked_tail_rename_matrix(persistence);
        run_journal_reserved_delalloc_mapper_matrix(persistence);
        run_reserved_delalloc_payload_retry_test(persistence);
        run_production_delalloc_append_block_test(persistence);
        run_production_delalloc_extent_split_test(persistence);
        run_production_delalloc_partial_eof_test(persistence);
        run_production_partial_eof_sparse_root_grow_test(persistence);
        run_production_delalloc_before_commit_fail_stop_test(persistence);
        run_production_delalloc_preexisting_fail_stop_test(persistence);
        run_production_delalloc_contract_failure_test(persistence);
        run_journal_production_delalloc_append_matrix(persistence);
        run_journal_production_partial_eof_matrix(persistence);
        run_journal_production_projected_matrix(
            persistence,
            "projected-multi-entry",
            |path| create_clean_seed(path, ImageKind::Journal),
            DelallocCrashCase::ProductionProjectedMultiEntry,
            0,
            (2 * DELALLOC_BLOCK_LEN) as u64,
            ProductionProjectedShape::Plain,
        );
        run_journal_production_projected_matrix(
            persistence,
            "projected-sparse-forward",
            |path| create_clean_seed(path, ImageKind::Journal),
            DelallocCrashCase::ProductionProjectedSparse,
            3 * DELALLOC_BLOCK_LEN,
            (4 * DELALLOC_BLOCK_LEN) as u64,
            ProductionProjectedShape::Plain,
        );
        run_journal_production_projected_matrix(
            persistence,
            "projected-root-grow",
            create_external_leaf_seed,
            DelallocCrashCase::ProductionProjectedSingle,
            INLINE_EXTENT_CAPACITY * WRITE_LEN,
            (INLINE_EXTENT_CAPACITY * WRITE_LEN + DELALLOC_BLOCK_LEN) as u64,
            ProductionProjectedShape::RootGrow,
        );
        run_journal_production_projected_matrix(
            persistence,
            "projected-full-leaf-split",
            create_full_external_leaf_seed,
            DelallocCrashCase::ProductionProjectedSingle,
            EXTERNAL_LEAF_EXTENT_CAPACITY * WRITE_LEN,
            (EXTERNAL_LEAF_EXTENT_CAPACITY * WRITE_LEN + DELALLOC_BLOCK_LEN) as u64,
            ProductionProjectedShape::FullLeafSplit,
        );
        run_journal_production_projected_matrix(
            persistence,
            "projected-full-leaf-merge",
            create_full_external_leaf_merge_seed,
            DelallocCrashCase::ProductionProjectedSingle,
            EXTERNAL_LEAF_EXTENT_CAPACITY * WRITE_LEN,
            (EXTERNAL_LEAF_EXTENT_CAPACITY * WRITE_LEN + DELALLOC_BLOCK_LEN) as u64,
            ProductionProjectedShape::FullLeafMerge,
        );
    }
    run_reserved_delalloc_ineligible_shape_test();
    run_journal_io_error_matrix();
    run_nojournal_io_error_matrix();
    run_kind(ImageKind::NoJournal, PersistenceModel::WriteBack);
    println!("range allocation crash-recovery matrix completed");
}
