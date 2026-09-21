//! Real-image coverage for bounded multi-page append credit accounting.
#[path = "../block_file.rs"]
#[allow(dead_code)]
mod block_file;

use another_ext4::{
    DelallocAppendBlockPublication, DelallocAppendBlockSubmitOutcome as Outcome, ErrCode, Ext4,
    InodeMode, EXT4_ROOT_INO,
};
use block_file::{CrashBlockFile, CrashDeviceOperation, SimulatedPowerLoss};
use std::{fs::File, process::Command, sync::Arc};

const PAGE: usize = 4096;
const PAYLOAD: [u8; PAGE] = [0xa7; PAGE];

fn seed(path: &str, extents: usize) -> u32 {
    File::create(path)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    assert!(Command::new("mkfs.ext4")
        .args([
            "-q",
            "-F",
            "-b",
            "4096",
            "-g",
            "1024",
            "-I",
            "256",
            "-O",
            "^orphan_file",
            path
        ])
        .status()
        .unwrap()
        .success());
    let fs = Ext4::load_writable(Arc::new(CrashBlockFile::new(path))).unwrap();
    let inode = fs
        .create(EXT4_ROOT_INO, "target", InodeMode::FILE | InodeMode::ALL_RW)
        .unwrap();
    let filler = fs
        .create(EXT4_ROOT_INO, "filler", InodeMode::FILE | InodeMode::ALL_RW)
        .unwrap();
    for i in 0..extents {
        fs.write(inode, i * PAGE, &[0x61; PAGE]).unwrap();
        fs.write(filler, i * PAGE, &[0x62; PAGE]).unwrap();
    }
    let attr = fs.getattr(inode).unwrap();
    assert_eq!(attr.size, (extents * PAGE) as u64);
    if extents == 4 {
        assert_eq!(attr.blocks, 4 * 8);
    }
    // The ordinary write path promotes the inline root into two leaves,
    // retaining two entries in the left leaf. 342 extents fill the right
    // leaf's 340 slots, so the following nonmerge append really splits it.
    if extents == 342 {
        assert_eq!(attr.blocks, 344 * 8);
    }
    fs.shutdown_writable().unwrap();
    inode
}

// Deliberately retain the production projected reservation API: the new bound
// must remain compatible with independently admitted page-cache entries.
fn append(
    fs: &Ext4,
    authority: &another_ext4::DelallocAppendMapperAuthority,
    device: &CrashBlockFile,
    inode: u32,
    budget: usize,
    retry: bool,
) -> u64 {
    let count = fs
        .max_delalloc_append_batch_blocks_authorized(authority)
        .unwrap();
    assert_eq!(count, 64.min((budget - 25) / 2));
    let mut pool = fs
        .create_delalloc_extent_node_pool_authorized(authority, inode)
        .unwrap();
    let start = fs.getattr(inode).unwrap().size as usize;
    let mut reservations = (0..count)
        .map(|i| {
            let offset = start + i * PAGE;
            fs.reserve_delalloc_append_block_projected_authorized(
                authority,
                inode,
                offset,
                offset as u64,
                &mut pool,
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    let publications = (0..count)
        .map(|i| DelallocAppendBlockPublication {
            payload: &PAYLOAD,
            durable_eof: (start + (i + 1) * PAGE) as u64,
            mtime: Some(111),
            ctime: Some(112),
        })
        .collect::<Vec<_>>();
    if retry {
        device.reset_operation_log();
        device.arm_io_error_at(0);
        let result = fs.submit_delalloc_append_batch_authorized_with_pool(
            authority,
            &mut reservations.iter_mut().collect::<Vec<_>>(),
            &publications,
            &mut pool,
        );
        assert_eq!(result, Outcome::RetryableNotPublished(ErrCode::EIO));
        assert_eq!(fs.getattr(inode).unwrap().size, start as u64);
        device.reset_operation_log();
    }
    let result = fs.submit_delalloc_append_batch_authorized_with_pool(
        authority,
        &mut reservations.iter_mut().collect::<Vec<_>>(),
        &publications,
        &mut pool,
    );
    let Outcome::Published(sequence) = result else {
        panic!("{result:?}");
    };
    fs.release_delalloc_extent_node_pool_authorized(authority, &mut pool)
        .unwrap();
    sequence
}

fn verify(path: &str, inode: u32, prefix: usize, appended: Option<usize>) {
    let fs = Ext4::load_writable(Arc::new(CrashBlockFile::new(path))).unwrap();
    let size = fs.getattr(inode).unwrap().size as usize;
    if let Some(pages) = appended {
        assert_eq!(size, (prefix + pages) * PAGE);
    } else {
        assert!(size == prefix * PAGE || size == (prefix + 64) * PAGE);
    }
    if prefix == 342 && size > prefix * PAGE {
        assert_eq!(
            fs.getattr(inode).unwrap().blocks,
            (size / 512 + 3 * 8) as u64,
            "the full right leaf must split into a third external leaf"
        );
    }
    let mut bytes = vec![0; size];
    assert_eq!(fs.read(inode, 0, &mut bytes).unwrap(), size);
    assert!(bytes[..prefix * PAGE].iter().all(|b| *b == 0x61));
    assert!(bytes[prefix * PAGE..].iter().all(|b| *b == 0xa7));
    fs.shutdown_writable().unwrap();
    drop(fs);
    let check = Command::new("e2fsck").args(["-fn", path]).output().unwrap();
    assert!(
        check.status.success(),
        "{}{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );
}

fn child(path: &str, inode: u32, operation: usize, io_error: bool, write_through: bool) {
    std::panic::set_hook(Box::new(|info| {
        if info.payload().is::<SimulatedPowerLoss>() {
            std::process::exit(86);
        }
        eprintln!("{info}");
    }));
    let device = Arc::new(CrashBlockFile::new(path));
    device.set_write_through(write_through);
    let mut fs = Ext4::load_writable(device.clone()).unwrap();
    fs.enable_batching(256).unwrap();
    let authority = fs.delalloc_append_mapper_authority().unwrap();
    append(&fs, &authority, &device, inode, 256, false);
    device.reset_operation_log();
    if io_error {
        device.arm_io_error_at(operation);
    } else {
        device.arm_power_loss_at(operation);
    }
    fs.request_batch_commit();
    let result = fs.commit_pending_batch();
    if io_error {
        assert_eq!(result.unwrap_err().code(), ErrCode::EIO);
        assert_eq!(fs.batch_progress().unwrap().durable, 0);
        // Exit without cleanup, exactly as after a failed mount is abandoned.
        std::process::exit(87);
    }
    panic!("armed power loss was not reached");
}

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    if args.len() > 1 {
        child(
            &args[1],
            args[2].parse().unwrap(),
            args[3].parse().unwrap(),
            args[4] == "io",
            args[5] == "through",
        );
        return;
    }
    for (prefix, budget, batches) in [(0, 256, 20), (4, 256, 1), (342, 256, 1), (0, 64, 1)] {
        let path = "batch-credits.img";
        let inode = seed(path, prefix);
        let device = Arc::new(CrashBlockFile::new(path));
        let mut fs = Ext4::load_writable(device.clone()).unwrap();
        fs.enable_batching(budget).unwrap();
        let authority = fs.delalloc_append_mapper_authority().unwrap();
        for _ in 0..batches {
            let sequence = append(&fs, &authority, &device, inode, budget, true);
            fs.request_batch_commit();
            assert_eq!(fs.commit_pending_batch().unwrap(), Some(sequence));
        }
        fs.shutdown_writable().unwrap();
        drop(fs);
        verify(
            path,
            inode,
            prefix,
            Some(batches * 64.min((budget - 25) / 2)),
        );
        std::fs::remove_file(path).unwrap();
        println!("PASS prefix={prefix} budget={budget} batches={batches}");
    }

    // Record the actual commit operation sequence, then cut at every write
    // and flush under both persistence models. Reuse one clean seed image.
    let seed_path = "batch-credits-seed.img";
    let inode = seed(seed_path, 342);
    let path = "batch-credits-fault.img";
    std::fs::copy(seed_path, path).unwrap();
    let device = Arc::new(CrashBlockFile::new(path));
    let mut fs = Ext4::load_writable(device.clone()).unwrap();
    fs.enable_batching(256).unwrap();
    let authority = fs.delalloc_append_mapper_authority().unwrap();
    append(&fs, &authority, &device, inode, 256, false);
    device.reset_operation_log();
    fs.request_batch_commit();
    fs.commit_pending_batch().unwrap();
    let operations = device.operations();
    assert!(operations.contains(&CrashDeviceOperation::Flush));
    fs.shutdown_writable().unwrap();
    drop(fs);
    for model in ["back", "through"] {
        for kind in ["power", "io"] {
            for index in 0..operations.len() {
                std::fs::copy(seed_path, path).unwrap();
                let status = Command::new(std::env::current_exe().unwrap())
                    .args([path, &inode.to_string(), &index.to_string(), kind, model])
                    .status()
                    .unwrap();
                assert_eq!(
                    status.code(),
                    Some(if kind == "io" { 87 } else { 86 }),
                    "{kind} {model} operation {index}"
                );
                verify(path, inode, 342, None);
            }
            println!(
                "PASS {kind} {model}: {} commit boundaries",
                operations.len()
            );
        }
    }
    std::fs::remove_file(path).unwrap();
    std::fs::remove_file(seed_path).unwrap();
}
