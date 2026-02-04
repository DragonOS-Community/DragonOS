use another_ext4::{Ext4, InodeMode, EXT4_ROOT_INO};
use block_file::BlockFile;
use simple_logger::SimpleLogger;
use std::sync::Arc;

mod block_file;

const ROOT_INO: u32 = EXT4_ROOT_INO;

fn make_ext4() {
    let _ = std::process::Command::new("rm")
        .args(["-rf", "ext4.img"])
        .status();
    let _ = std::process::Command::new("dd")
        .args(["if=/dev/zero", "of=ext4.img", "bs=1M", "count=512"])
        .status();
    let _ = std::process::Command::new("mkfs.ext4")
        .args(["ext4.img"])
        .output();
}

fn open_ext4() -> Ext4 {
    let file = BlockFile::new("ext4.img");
    println!("creating ext4");
    let mut ext4 = Ext4::load(Arc::new(file)).expect("open ext4 failed");
    ext4.init().expect("init ext4 failed");
    ext4
}

fn mkdir_test(ext4: &mut Ext4) {
    let dir_mode: InodeMode = InodeMode::DIRECTORY | InodeMode::ALL_RWX;
    ext4.generic_create(ROOT_INO, "d1", dir_mode)
        .expect("mkdir failed");
    ext4.generic_create(ROOT_INO, "d1/d2", dir_mode)
        .expect("mkdir failed");
    ext4.generic_create(ROOT_INO, "d1/d2/d3", dir_mode)
        .expect("mkdir failed");
    ext4.generic_create(ROOT_INO, "d1/d2/d3/d4", dir_mode)
        .expect("mkdir failed");
    ext4.generic_create(ROOT_INO, "d2", dir_mode)
        .expect("mkdir failed");
    ext4.generic_create(ROOT_INO, "d2/d3", dir_mode)
        .expect("mkdir failed");
    ext4.generic_create(ROOT_INO, "d2/d3/d4", dir_mode)
        .expect("mkdir failed");
    ext4.generic_create(ROOT_INO, "d3", dir_mode)
        .expect("mkdir failed");
}

fn create_test(ext4: &mut Ext4) {
    let file_mode: InodeMode = InodeMode::FILE | InodeMode::ALL_RWX;
    ext4.generic_create(ROOT_INO, "d1/d2/d3/d4/f1", file_mode)
        .expect("open failed");
    ext4.generic_create(ROOT_INO, "d3/f0", file_mode)
        .expect("open failed");
    ext4.generic_create(ROOT_INO, "d3/f1", file_mode)
        .expect("open failed");
    ext4.generic_create(ROOT_INO, "f1", file_mode)
        .expect("open failed");
}

fn read_write_test(ext4: &mut Ext4) {
    let wbuffer = "hello world".as_bytes();
    let file = ext4.generic_lookup(ROOT_INO, "d3/f0").expect("open failed");
    ext4.write(file, 0, wbuffer).expect("write failed");
    let mut rbuffer = vec![0u8; wbuffer.len() + 100]; // Test end of file
    let rcount = ext4.read(file, 0, &mut rbuffer).expect("read failed");
    assert_eq!(wbuffer, &rbuffer[..rcount]);
}

fn large_read_write_test(ext4: &mut Ext4) {
    let wbuffer = vec![99u8; 1024 * 1024 * 16];
    let file = ext4.generic_lookup(ROOT_INO, "d3/f1").expect("open failed");
    ext4.write(file, 0, &wbuffer).expect("write failed");
    let mut rbuffer = vec![0u8; wbuffer.len()];
    let rcount = ext4.read(file, 0, &mut rbuffer).expect("read failed");
    assert_eq!(wbuffer, &rbuffer[..rcount]);
}

fn remove_file_test(ext4: &mut Ext4) {
    ext4.generic_remove(ROOT_INO, "d3/f0")
        .expect("remove file failed");
    ext4.generic_lookup(ROOT_INO, "d3/f0")
        .expect_err("file not removed");
    ext4.generic_remove(ROOT_INO, "d3/f1")
        .expect("remove file failed");
    ext4.generic_lookup(ROOT_INO, "d3/f1")
        .expect_err("file not removed");
    ext4.generic_remove(ROOT_INO, "f1")
        .expect("remove file failed");
    ext4.generic_lookup(ROOT_INO, "f1")
        .expect_err("file not removed");
    ext4.generic_remove(ROOT_INO, "d1/not_exist")
        .expect_err("remove file failed");
}

fn xattr_test(ext4: &mut Ext4) {
    let file_mode: InodeMode = InodeMode::FILE | InodeMode::ALL_RWX;
    let file = ext4
        .generic_create(ROOT_INO, "f2", file_mode)
        .expect("Create failed");
    ext4.setxattr(file, "user.testone", "hello world".as_bytes())
        .expect("setxattr failed");
    ext4.setxattr(file, "user.testtwo", "world hello".as_bytes())
        .expect("setxattr failed");

    let names = ext4.listxattr(file).expect("listxattr failed");
    assert_eq!(names, vec!["user.testone", "user.testtwo"]);

    let value = ext4
        .getxattr(file, "user.testone")
        .expect("getxattr failed");
    assert_eq!(value, "hello world".as_bytes());
    let value = ext4
        .getxattr(file, "user.testtwo")
        .expect("getxattr failed");
    assert_eq!(value, "world hello".as_bytes());

    let names = ext4.listxattr(file).expect("listxattr failed");
    assert_eq!(names, vec!["user.testone", "user.testtwo"]);

    ext4.removexattr(file, "user.testone")
        .expect("removexattr failed");
    ext4.getxattr(file, "user.testone")
        .expect_err("getxattr failed");
    let names = ext4.listxattr(file).expect("listxattr failed");
    assert_eq!(names, vec!["user.testtwo"]);
}

fn rename_exchange_test(ext4: &mut Ext4) {
    let dir_mode: InodeMode = InodeMode::DIRECTORY | InodeMode::ALL_RWX;
    let file_mode: InodeMode = InodeMode::FILE | InodeMode::ALL_RWX;

    // Setup: create test directories and files
    let exchange_dir = ext4
        .generic_create(ROOT_INO, "exchange_test", dir_mode)
        .expect("mkdir exchange_test failed");
    let sub_dir_a = ext4
        .generic_create(exchange_dir, "dir_a", dir_mode)
        .expect("mkdir dir_a failed");
    let sub_dir_b = ext4
        .generic_create(exchange_dir, "dir_b", dir_mode)
        .expect("mkdir dir_b failed");

    // Create files with different content
    let file_a = ext4
        .generic_create(exchange_dir, "file_a", file_mode)
        .expect("create file_a failed");
    ext4.write(file_a, 0, b"content_a")
        .expect("write file_a failed");

    let file_b = ext4
        .generic_create(exchange_dir, "file_b", file_mode)
        .expect("create file_b failed");
    ext4.write(file_b, 0, b"content_b")
        .expect("write file_b failed");

    // Test 1: Same-directory file exchange
    ext4.rename_exchange(exchange_dir, "file_a", exchange_dir, "file_b")
        .expect("same-dir file exchange failed");

    // Verify: file_a should now have content_b, file_b should have content_a
    let new_file_a = ext4
        .generic_lookup(exchange_dir, "file_a")
        .expect("lookup file_a failed");
    let new_file_b = ext4
        .generic_lookup(exchange_dir, "file_b")
        .expect("lookup file_b failed");
    assert_eq!(new_file_a, file_b, "file_a should point to old file_b inode");
    assert_eq!(new_file_b, file_a, "file_b should point to old file_a inode");

    // Test 2: Cross-directory file exchange
    let file_in_a = ext4
        .generic_create(sub_dir_a, "file_in_a", file_mode)
        .expect("create file_in_a failed");
    let file_in_b = ext4
        .generic_create(sub_dir_b, "file_in_b", file_mode)
        .expect("create file_in_b failed");

    ext4.rename_exchange(sub_dir_a, "file_in_a", sub_dir_b, "file_in_b")
        .expect("cross-dir file exchange failed");

    let new_file_in_a = ext4
        .generic_lookup(sub_dir_a, "file_in_a")
        .expect("lookup file_in_a failed");
    let new_file_in_b = ext4
        .generic_lookup(sub_dir_b, "file_in_b")
        .expect("lookup file_in_b failed");
    assert_eq!(
        new_file_in_a, file_in_b,
        "file_in_a should point to old file_in_b inode"
    );
    assert_eq!(
        new_file_in_b, file_in_a,
        "file_in_b should point to old file_in_a inode"
    );

    // Test 3: Same-directory directory exchange
    let inner_dir_1 = ext4
        .generic_create(exchange_dir, "inner_1", dir_mode)
        .expect("mkdir inner_1 failed");
    let inner_dir_2 = ext4
        .generic_create(exchange_dir, "inner_2", dir_mode)
        .expect("mkdir inner_2 failed");

    ext4.rename_exchange(exchange_dir, "inner_1", exchange_dir, "inner_2")
        .expect("same-dir directory exchange failed");

    let new_inner_1 = ext4
        .generic_lookup(exchange_dir, "inner_1")
        .expect("lookup inner_1 failed");
    let new_inner_2 = ext4
        .generic_lookup(exchange_dir, "inner_2")
        .expect("lookup inner_2 failed");
    assert_eq!(
        new_inner_1, inner_dir_2,
        "inner_1 should point to old inner_2 inode"
    );
    assert_eq!(
        new_inner_2, inner_dir_1,
        "inner_2 should point to old inner_1 inode"
    );

    // Test 4: Cross-directory directory exchange
    let nested_in_a = ext4
        .generic_create(sub_dir_a, "nested_a", dir_mode)
        .expect("mkdir nested_a failed");
    let nested_in_b = ext4
        .generic_create(sub_dir_b, "nested_b", dir_mode)
        .expect("mkdir nested_b failed");

    ext4.rename_exchange(sub_dir_a, "nested_a", sub_dir_b, "nested_b")
        .expect("cross-dir directory exchange failed");

    let new_nested_a = ext4
        .generic_lookup(sub_dir_a, "nested_a")
        .expect("lookup nested_a failed");
    let new_nested_b = ext4
        .generic_lookup(sub_dir_b, "nested_b")
        .expect("lookup nested_b failed");
    assert_eq!(
        new_nested_a, nested_in_b,
        "nested_a should point to old nested_b inode"
    );
    assert_eq!(
        new_nested_b, nested_in_a,
        "nested_b should point to old nested_a inode"
    );

    // Test 5: Exchange with non-existent source should fail
    let result = ext4.rename_exchange(exchange_dir, "nonexistent", exchange_dir, "file_a");
    assert!(
        result.is_err(),
        "exchange with non-existent source should fail"
    );

    // Test 6: Exchange with non-existent target should fail
    let result = ext4.rename_exchange(exchange_dir, "file_a", exchange_dir, "nonexistent");
    assert!(
        result.is_err(),
        "exchange with non-existent target should fail"
    );

    // Test 7: Same inode exchange (no-op, should succeed)
    let same_file = ext4
        .generic_create(exchange_dir, "same_file", file_mode)
        .expect("create same_file failed");
    // Create a hardlink to same_file (link signature: child, parent, name)
    ext4.link(same_file, exchange_dir, "same_file_link")
        .expect("link failed");
    ext4.rename_exchange(exchange_dir, "same_file", exchange_dir, "same_file_link")
        .expect("same inode exchange should succeed as no-op");
    // Both should still point to the same inode
    let check_same = ext4
        .generic_lookup(exchange_dir, "same_file")
        .expect("lookup same_file failed");
    let check_link = ext4
        .generic_lookup(exchange_dir, "same_file_link")
        .expect("lookup same_file_link failed");
    assert_eq!(check_same, check_link, "both should still be same inode");

    // Test 8: Cycle detection - cannot exchange parent with its child
    let parent_dir = ext4
        .generic_create(exchange_dir, "parent_dir", dir_mode)
        .expect("mkdir parent_dir failed");
    let _child_dir = ext4
        .generic_create(parent_dir, "child_dir", dir_mode)
        .expect("mkdir child_dir failed");

    // Try to exchange parent_dir with child_dir (would create cycle)
    let result = ext4.rename_exchange(exchange_dir, "parent_dir", parent_dir, "child_dir");
    assert!(
        result.is_err(),
        "exchange that would create cycle should fail"
    );
}

fn main() {
    SimpleLogger::new().init().unwrap();
    log::set_max_level(log::LevelFilter::Off);
    make_ext4();
    println!("ext4.img created");
    let mut ext4 = open_ext4();
    println!("ext4 opened");
    mkdir_test(&mut ext4);
    println!("mkdir test done");
    create_test(&mut ext4);
    println!("create test done");
    read_write_test(&mut ext4);
    println!("read write test done");
    large_read_write_test(&mut ext4);
    println!("large read write test done");
    remove_file_test(&mut ext4);
    println!("remove file test done");
    xattr_test(&mut ext4);
    println!("xattr test done");
    rename_exchange_test(&mut ext4);
    println!("rename_exchange test done");
}
