//! Real ext4 delayed-allocation acceptance and durability ownership regression.
#[path = "../block_file.rs"]
#[allow(dead_code)]
mod block_file;
use another_ext4::{
    DelallocAppendBlockPublication, DelallocAppendBlockSubmitOutcome as Outcome, ErrCode, Ext4,
    InodeMode, EXT4_ROOT_INO,
};
use block_file::{BlockFile, CrashBlockFile, CrashDeviceOperation};
use std::{fs::File, process::Command, sync::Arc};

fn submit(
    fs: &Ext4,
    authority: &another_ext4::DelallocAppendMapperAuthority,
    reservation: &mut another_ext4::DelallocAppendBlockReservation,
    publication: DelallocAppendBlockPublication<'_>,
    pool: &mut another_ext4::DelallocExtentNodePool,
    single: bool,
) -> Outcome {
    if single {
        fs.submit_delalloc_append_block_authorized_with_pool(
            authority,
            reservation,
            publication,
            Some(pool),
        )
    } else {
        fs.submit_delalloc_append_batch_authorized_with_pool(
            authority,
            &mut [reservation],
            &[publication],
            pool,
        )
    }
}

fn run(background_error: bool, single: bool, partial: bool) {
    let path = if background_error {
        "delalloc-batch-failed.img"
    } else {
        "delalloc-batch-ok.img"
    };
    File::create(path)
        .unwrap()
        .set_len(32 * 1024 * 1024)
        .unwrap();
    assert!(Command::new("mkfs.ext4")
        .args([
            "-q",
            "-F",
            "-b",
            "4096",
            "-I",
            "256",
            "-O",
            "^orphan_file",
            path
        ])
        .status()
        .unwrap()
        .success());
    let device = Arc::new(CrashBlockFile::new(path));
    let mut fs = Ext4::load_writable(device.clone()).unwrap();
    let inode = fs
        .create(EXT4_ROOT_INO, "file", InodeMode::FILE | InodeMode::ALL_RW)
        .unwrap();
    let old_eof = if partial { 17 } else { 0 };
    let offset = if partial { 4096 } else { 0 };
    let length = if partial { 17 } else { 4096 };
    let new_eof = (offset + length) as u64;
    if partial {
        // Dirty bytes beyond the shortened EOF must never be exposed by the
        // next sparse append, even though its metadata is only accepted.
        fs.write(inode, 0, &[0x66; 4096]).unwrap();
        fs.setattr(
            inode,
            another_ext4::SetAttr {
                size: Some(old_eof),
                ..Default::default()
            },
        )
        .unwrap();
    }
    fs.enable_batching(256).unwrap();
    let authority = fs.delalloc_append_mapper_authority().unwrap();
    let mut pool = fs
        .create_delalloc_extent_node_pool_authorized(&authority, inode)
        .unwrap();
    let mut reservation = fs
        .reserve_delalloc_append_block_projected_authorized(
            &authority, inode, offset, old_eof, &mut pool,
        )
        .unwrap();
    let bytes = vec![0xa7; length];
    let publication = DelallocAppendBlockPublication {
        payload: &bytes,
        durable_eof: new_eof,
        mtime: Some(111),
        ctime: Some(112),
    };
    device.reset_operation_log();
    device.arm_io_error_at(0);
    let first = submit(
        &fs,
        &authority,
        &mut reservation,
        publication,
        &mut pool,
        single,
    );
    assert_eq!(
        first,
        Outcome::RetryableNotPublished(ErrCode::EIO),
        "partial={partial} single={single} attrs={:?} progress={:?}",
        fs.getattr(inode),
        fs.batch_progress()
    );
    assert_eq!(fs.batch_progress().unwrap().accepted, 0);
    assert_eq!(fs.getattr(inode).unwrap().size, old_eof);
    device.reset_operation_log();
    let publication = DelallocAppendBlockPublication {
        payload: &bytes,
        durable_eof: new_eof,
        mtime: Some(111),
        ctime: Some(112),
    };
    let sequence = match submit(
        &fs,
        &authority,
        &mut reservation,
        publication,
        &mut pool,
        single,
    ) {
        Outcome::Published(sequence) => sequence,
        other => panic!("unexpected submission outcome: {other:?}"),
    };
    assert_eq!(
        fs.getattr(inode).unwrap().size,
        new_eof,
        "accepted mapping is immediately visible"
    );
    assert_eq!(fs.batch_progress().unwrap().durable, 0);
    assert!(
        !device.operations().contains(&CrashDeviceOperation::Flush),
        "accepted descriptor must not force durability"
    );
    fs.release_delalloc_extent_node_pool_authorized(&authority, &mut pool)
        .unwrap();
    device.reset_operation_log();
    if background_error {
        device.arm_io_error_at(0);
    }
    fs.request_batch_commit();
    let commit = fs.commit_pending_batch();
    if background_error {
        assert_eq!(commit.unwrap_err().code(), ErrCode::EIO);
        assert_eq!(fs.batch_progress().unwrap().accepted, sequence);
        assert_eq!(fs.batch_progress().unwrap().durable, 0);
        // The old lease remains terminal even when the *disk* failure happened
        // before a commit record. It must never be restored and retried.
        let publication = DelallocAppendBlockPublication {
            payload: &bytes,
            durable_eof: new_eof,
            mtime: None,
            ctime: None,
        };
        assert!(matches!(
            submit(
                &fs,
                &authority,
                &mut reservation,
                publication,
                &mut pool,
                single
            ),
            Outcome::Terminal(_)
        ));
        device.crash();
    } else {
        assert_eq!(commit.unwrap(), Some(sequence));
        assert_eq!(fs.batch_progress().unwrap().durable, sequence);
        fs.shutdown_writable().unwrap();
    }
    drop(reservation);
    drop(pool);
    drop(fs);
    drop(device);
    let fs = Ext4::load_writable(Arc::new(BlockFile::new(path))).unwrap();
    assert_eq!(
        fs.getattr(inode).unwrap().size,
        if background_error { old_eof } else { new_eof }
    );
    if !background_error {
        let mut read = vec![0; length];
        assert_eq!(fs.read(inode, offset, &mut read).unwrap(), length);
        assert_eq!(read, bytes);
        if partial {
            let mut gap = vec![0x88; offset - old_eof as usize];
            assert_eq!(
                fs.read(inode, old_eof as usize, &mut gap).unwrap(),
                gap.len()
            );
            assert!(gap.iter().all(|byte| *byte == 0));
        }
    }
    fs.shutdown_writable().unwrap();
    drop(fs);
    let check = Command::new("e2fsck").args(["-fn", path]).output().unwrap();
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stdout)
    );
    std::fs::remove_file(path).unwrap();
    println!("batched delalloc background_error={background_error} single={single} partial={partial} passed");
}
fn main() {
    for single in [false, true] {
        for partial in [false, true] {
            run(false, single, partial);
            run(true, single, partial);
        }
    }
}
