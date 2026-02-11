use another_ext4::{Ext4, InodeMode};

use super::ROOT_INO;

pub fn rename_exchange_test(ext4: &mut Ext4) {
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