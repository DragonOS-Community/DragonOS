//! 动态位图的集成测试

use bitmap::{traits::BitMapOps, AllocBitmap};

/// 测试空的位图
///
/// 这是一个测试空的位图的例子
///

/// 测试空的位图
#[test]
fn test_empty_bitmap_32() {
    let mut bitmap = AllocBitmap::new(32);
    assert_eq!(bitmap.len(), 32);
    assert_eq!(bitmap.size(), 8);
    assert_eq!(bitmap.first_index(), None);
    assert_eq!(bitmap.first_false_index(), Some(0));
    assert_eq!(bitmap.last_index(), None);
    assert_eq!(bitmap.last_false_index(), Some(31));
    assert_eq!(bitmap.next_index(0), None);
    assert_eq!(bitmap.next_false_index(0), Some(1));
    assert_eq!(bitmap.prev_index(0), None);
    assert_eq!(bitmap.prev_false_index(0), None);
    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), true);
    bitmap.invert();
    assert_eq!(bitmap.len(), 32);
    assert_eq!(bitmap.size(), 8);
    assert_eq!(bitmap.first_index(), Some(0));
    assert_eq!(bitmap.first_false_index(), None);
    assert_eq!(bitmap.last_index(), Some(31));
    assert_eq!(bitmap.last_false_index(), None);
    assert_eq!(bitmap.next_index(0), Some(1));
    assert_eq!(bitmap.next_false_index(0), None);
    assert_eq!(bitmap.prev_index(0), None);
    assert_eq!(bitmap.prev_false_index(0), None);
    assert_eq!(bitmap.is_full(), true);
    assert_eq!(bitmap.is_empty(), false);
}

#[test]
fn test_empty_bitmap_64() {
    let mut bitmap = AllocBitmap::new(64);
    assert_eq!(bitmap.len(), 64);
    assert_eq!(bitmap.size(), 8);
    assert_eq!(bitmap.first_index(), None);
    assert_eq!(bitmap.first_false_index(), Some(0));
    assert_eq!(bitmap.last_index(), None);
    assert_eq!(bitmap.last_false_index(), Some(63));
    assert_eq!(bitmap.next_index(0), None);
    assert_eq!(bitmap.next_false_index(0), Some(1));
    assert_eq!(bitmap.prev_index(0), None);
    assert_eq!(bitmap.prev_false_index(0), None);
    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), true);
    bitmap.invert();
    assert_eq!(bitmap.len(), 64);
    assert_eq!(bitmap.size(), 8);
    assert_eq!(bitmap.first_index(), Some(0));
    assert_eq!(bitmap.first_false_index(), None);
    assert_eq!(bitmap.last_index(), Some(63));
    assert_eq!(bitmap.last_false_index(), None);
    assert_eq!(bitmap.next_index(0), Some(1));
    assert_eq!(bitmap.next_false_index(0), None);
    assert_eq!(bitmap.prev_index(0), None);
    assert_eq!(bitmap.prev_false_index(0), None);
    assert_eq!(bitmap.is_full(), true);
    assert_eq!(bitmap.is_empty(), false);
}

/// 测试长度为32的bmp，其中第一个元素为1
#[test]
fn test_alloc_bitmap_32_first_1() {
    let mut bitmap = AllocBitmap::new(32);
    bitmap.set(0, true);
    assert_eq!(bitmap.len(), 32);
    assert_eq!(bitmap.size(), 8);
    assert_eq!(bitmap.first_index(), Some(0));
    assert_eq!(bitmap.first_false_index(), Some(1));
    assert_eq!(bitmap.last_index(), Some(0));
    assert_eq!(bitmap.last_false_index(), Some(31));
    assert_eq!(bitmap.next_index(0), None);
    assert_eq!(bitmap.next_false_index(0), Some(1));
    assert_eq!(bitmap.prev_index(0), None);
    assert_eq!(bitmap.prev_false_index(0), None);
    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), false);
    assert_eq!(bitmap.prev_index(2), Some(0));
    assert_eq!(bitmap.prev_false_index(2), Some(1));
    assert_eq!(bitmap.next_index(2), None);
    assert_eq!(bitmap.next_false_index(2), Some(3));

    // 反转

    bitmap.invert();

    assert_eq!(bitmap.first_index(), Some(1));
    assert_eq!(bitmap.first_false_index(), Some(0));
    assert_eq!(bitmap.last_index(), Some(31));
    assert_eq!(bitmap.last_false_index(), Some(0));
    assert_eq!(bitmap.next_index(0), Some(1));
    assert_eq!(bitmap.next_false_index(0), None);
    assert_eq!(bitmap.prev_index(0), None);
    assert_eq!(bitmap.prev_false_index(0), None);
    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), false);
    assert_eq!(bitmap.prev_index(2), Some(1));
    assert_eq!(bitmap.prev_false_index(2), Some(0));
    assert_eq!(bitmap.next_index(2), Some(3));
}

/// 测试长度为32的bmp，其中中间某个元素为1
#[test]
fn test_alloc_bitmap_32_middle_1() {
    let mut bitmap = AllocBitmap::new(32);
    bitmap.set(15, true);
    assert_eq!(bitmap.len(), 32);
    assert_eq!(bitmap.size(), 8);
    assert_eq!(bitmap.first_index(), Some(15));
    assert_eq!(bitmap.first_false_index(), Some(0));
    assert_eq!(bitmap.last_index(), Some(15));
    assert_eq!(bitmap.last_false_index(), Some(31));
    assert_eq!(bitmap.next_index(0), Some(15));
    assert_eq!(bitmap.next_index(15), None);
    assert_eq!(bitmap.next_false_index(15), Some(16));
    assert_eq!(bitmap.prev_index(15), None);
    assert_eq!(bitmap.prev_false_index(15), Some(14));
    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), false);
    assert_eq!(bitmap.prev_index(20), Some(15));
    assert_eq!(bitmap.prev_false_index(20), Some(19));
    assert_eq!(bitmap.next_index(2), Some(15));
    assert_eq!(bitmap.next_false_index(2), Some(3));

    // 反转

    bitmap.invert();

    assert_eq!(bitmap.first_index(), Some(0));
    assert_eq!(bitmap.first_false_index(), Some(15));
    assert_eq!(bitmap.last_index(), Some(31));
    assert_eq!(bitmap.last_false_index(), Some(15));
    assert_eq!(bitmap.next_index(0), Some(1));
    assert_eq!(bitmap.next_false_index(1), Some(15));
    assert_eq!(bitmap.prev_index(15), Some(14));
    assert_eq!(bitmap.prev_false_index(15), None);
    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), false);
    assert_eq!(bitmap.prev_index(30), Some(29));
    assert_eq!(bitmap.prev_false_index(30), Some(15));
    assert_eq!(bitmap.next_index(2), Some(3));
}

/// 测试长度为32的bmp，其中最后一个元素为1
#[test]
fn test_alloc_bitmap_32_last_1() {
    let mut bitmap = AllocBitmap::new(32);
    bitmap.set(31, true);
    assert_eq!(bitmap.len(), 32);
    assert_eq!(bitmap.size(), 8);
    assert_eq!(bitmap.first_index(), Some(31));
    assert_eq!(bitmap.first_false_index(), Some(0));
    assert_eq!(bitmap.last_index(), Some(31));
    assert_eq!(bitmap.last_false_index(), Some(30));
    assert_eq!(bitmap.next_index(0), Some(31));
    assert_eq!(bitmap.next_index(31), None);
    assert_eq!(bitmap.next_false_index(0), Some(1));
    assert_eq!(bitmap.prev_index(31), None);
    assert_eq!(bitmap.prev_false_index(31), Some(30));
    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), false);
    assert_eq!(bitmap.prev_index(2), None);
    assert_eq!(bitmap.prev_false_index(2), Some(1));
    assert_eq!(bitmap.next_index(2), Some(31));
    assert_eq!(bitmap.next_false_index(2), Some(3));

    // 反转

    bitmap.invert();

    assert_eq!(bitmap.first_index(), Some(0));
    assert_eq!(bitmap.first_false_index(), Some(31));
    assert_eq!(bitmap.last_index(), Some(30));
    assert_eq!(bitmap.last_false_index(), Some(31));
    assert_eq!(bitmap.next_index(0), Some(1));
    assert_eq!(bitmap.next_false_index(0), Some(31));
    assert_eq!(bitmap.prev_index(31), Some(30));
    assert_eq!(bitmap.prev_false_index(31), None);
    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), false);
    assert_eq!(bitmap.prev_index(2), Some(1));
    assert_eq!(bitmap.prev_false_index(2), None);
    assert_eq!(bitmap.next_index(2), Some(3));
}

/// 测试长度为64的bmp，其中第一个元素为1
#[test]
fn test_alloc_bitmap_64_first_1() {
    let mut bitmap = AllocBitmap::new(64);
    bitmap.set(0, true);
    assert_eq!(bitmap.len(), 64);
    assert_eq!(bitmap.size(), 8);
    assert_eq!(bitmap.first_index(), Some(0));
    assert_eq!(bitmap.first_false_index(), Some(1));
    assert_eq!(bitmap.last_index(), Some(0));
    assert_eq!(bitmap.last_false_index(), Some(63));
    assert_eq!(bitmap.next_index(0), None);
    assert_eq!(bitmap.next_false_index(0), Some(1));
    assert_eq!(bitmap.prev_index(0), None);
    assert_eq!(bitmap.prev_false_index(0), None);
    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), false);
    assert_eq!(bitmap.prev_index(2), Some(0));
    assert_eq!(bitmap.prev_false_index(2), Some(1));
    assert_eq!(bitmap.next_index(2), None);
    assert_eq!(bitmap.next_false_index(2), Some(3));

    // 反转

    bitmap.invert();

    assert_eq!(bitmap.first_index(), Some(1));
    assert_eq!(bitmap.first_false_index(), Some(0));
    assert_eq!(bitmap.last_index(), Some(63));
    assert_eq!(bitmap.last_false_index(), Some(0));
    assert_eq!(bitmap.next_index(0), Some(1));
    assert_eq!(bitmap.next_false_index(0), None);
    assert_eq!(bitmap.prev_index(0), None);
    assert_eq!(bitmap.prev_false_index(0), None);
    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), false);
    assert_eq!(bitmap.prev_index(2), Some(1));
    assert_eq!(bitmap.prev_false_index(2), Some(0));
    assert_eq!(bitmap.next_index(2), Some(3));
}

/// 测试长度为64的bmp，其中中间某个元素为1
#[test]
fn test_alloc_bitmap_64_middle_1() {
    let mut bitmap = AllocBitmap::new(64);
    bitmap.set(15, true);
    assert_eq!(bitmap.len(), 64);
    assert_eq!(bitmap.size(), 8);
    assert_eq!(bitmap.first_index(), Some(15));
    assert_eq!(bitmap.first_false_index(), Some(0));
    assert_eq!(bitmap.last_index(), Some(15));
    assert_eq!(bitmap.last_false_index(), Some(63));
    assert_eq!(bitmap.next_index(0), Some(15));
    assert_eq!(bitmap.next_index(15), None);
    assert_eq!(bitmap.next_false_index(15), Some(16));
    assert_eq!(bitmap.prev_index(15), None);
    assert_eq!(bitmap.prev_false_index(15), Some(14));
    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), false);
    assert_eq!(bitmap.prev_index(20), Some(15));
    assert_eq!(bitmap.prev_false_index(20), Some(19));
    assert_eq!(bitmap.next_index(2), Some(15));
    assert_eq!(bitmap.next_false_index(2), Some(3));

    // 反转

    bitmap.invert();

    assert_eq!(bitmap.first_index(), Some(0));
    assert_eq!(bitmap.first_false_index(), Some(15));
    assert_eq!(bitmap.last_index(), Some(63));
    assert_eq!(bitmap.last_false_index(), Some(15));
    assert_eq!(bitmap.next_index(0), Some(1));
    assert_eq!(bitmap.next_false_index(1), Some(15));
    assert_eq!(bitmap.prev_index(15), Some(14));
    assert_eq!(bitmap.prev_false_index(15), None);
    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), false);
    assert_eq!(bitmap.prev_index(63), Some(62));
    assert_eq!(bitmap.prev_false_index(62), Some(15));
    assert_eq!(bitmap.next_index(2), Some(3));
}

/// 测试长度为64的bmp，其中最后一个元素为1
#[test]
fn test_alloc_bitmap_64_last_1() {
    let mut bitmap = AllocBitmap::new(64);
    bitmap.set(63, true);
    assert_eq!(bitmap.len(), 64);
    assert_eq!(bitmap.size(), 8);
    assert_eq!(bitmap.first_index(), Some(63));
    assert_eq!(bitmap.first_false_index(), Some(0));
    assert_eq!(bitmap.last_index(), Some(63));
    assert_eq!(bitmap.last_false_index(), Some(62));
    assert_eq!(bitmap.next_index(0), Some(63));
    assert_eq!(bitmap.next_index(63), None);
    assert_eq!(bitmap.next_false_index(0), Some(1));
    assert_eq!(bitmap.prev_index(63), None);
    assert_eq!(bitmap.prev_false_index(63), Some(62));
    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), false);
    assert_eq!(bitmap.prev_index(2), None);
    assert_eq!(bitmap.prev_false_index(2), Some(1));
    assert_eq!(bitmap.next_index(2), Some(63));
    assert_eq!(bitmap.next_false_index(2), Some(3));

    // 反转

    bitmap.invert();

    assert_eq!(bitmap.first_index(), Some(0));
    assert_eq!(bitmap.first_false_index(), Some(63));
    assert_eq!(bitmap.last_index(), Some(62));
    assert_eq!(bitmap.last_false_index(), Some(63));
    assert_eq!(bitmap.next_index(0), Some(1));
    assert_eq!(bitmap.next_false_index(0), Some(63));
    assert_eq!(bitmap.prev_index(63), Some(62));
    assert_eq!(bitmap.prev_false_index(63), None);
    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), false);
    assert_eq!(bitmap.prev_index(2), Some(1));
    assert_eq!(bitmap.prev_false_index(2), None);
    assert_eq!(bitmap.next_index(2), Some(3));
}

/// 测试长度为64的bmp，其中第一个和最后一个元素为1
#[test]
fn test_alloc_bitmap_64_two_1_first() {
    let mut bitmap = AllocBitmap::new(64);
    bitmap.set(0, true);
    bitmap.set(63, true);

    assert_eq!(bitmap.len(), 64);
    assert_eq!(bitmap.size(), 8);
    assert_eq!(bitmap.first_index(), Some(0));
    assert_eq!(bitmap.first_false_index(), Some(1));
    assert_eq!(bitmap.last_index(), Some(63));
    assert_eq!(bitmap.last_false_index(), Some(62));
    assert_eq!(bitmap.next_index(0), Some(63));
    assert_eq!(bitmap.next_index(63), None);
    assert_eq!(bitmap.next_false_index(0), Some(1));
    assert_eq!(bitmap.prev_index(63), Some(0));
    assert_eq!(bitmap.prev_false_index(63), Some(62));
    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), false);

    // 反转

    bitmap.invert();

    assert_eq!(bitmap.first_index(), Some(1));
    assert_eq!(bitmap.first_false_index(), Some(0));
    assert_eq!(bitmap.last_index(), Some(62));
    assert_eq!(bitmap.last_false_index(), Some(63));
    assert_eq!(bitmap.next_index(0), Some(1));
    assert_eq!(bitmap.next_false_index(0), Some(63));
    assert_eq!(bitmap.prev_index(63), Some(62));
    assert_eq!(bitmap.prev_false_index(63), Some(0));
    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), false);
    assert_eq!(bitmap.prev_index(2), Some(1));
    assert_eq!(bitmap.prev_false_index(2), Some(0));
    assert_eq!(bitmap.next_index(2), Some(3));
    assert_eq!(bitmap.next_false_index(2), Some(63));
}

/// 测试长度为64的bmp，中间两个不相邻的元素为1
#[test]
fn test_alloc_bitmap_64_two_1_middle() {
    let mut bitmap = AllocBitmap::new(64);
    bitmap.set(15, true);
    bitmap.set(63, true);

    assert_eq!(bitmap.len(), 64);
    assert_eq!(bitmap.size(), 8);
    assert_eq!(bitmap.first_index(), Some(15));
    assert_eq!(bitmap.first_false_index(), Some(0));
    assert_eq!(bitmap.last_index(), Some(63));
    assert_eq!(bitmap.last_false_index(), Some(62));
    assert_eq!(bitmap.next_index(0), Some(15));
    assert_eq!(bitmap.next_index(15), Some(63));
    assert_eq!(bitmap.next_index(63), None);
    assert_eq!(bitmap.next_false_index(0), Some(1));
    assert_eq!(bitmap.prev_index(63), Some(15));
    assert_eq!(bitmap.prev_false_index(63), Some(62));
    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), false);

    // 反转

    bitmap.invert();

    assert_eq!(bitmap.first_index(), Some(0));
    assert_eq!(bitmap.first_false_index(), Some(15));
    assert_eq!(bitmap.last_index(), Some(62));
    assert_eq!(bitmap.last_false_index(), Some(63));
    assert_eq!(bitmap.next_index(0), Some(1));
    assert_eq!(bitmap.next_false_index(0), Some(15));
    assert_eq!(bitmap.next_false_index(15), Some(63));
    assert_eq!(bitmap.prev_index(63), Some(62));
    assert_eq!(bitmap.prev_false_index(63), Some(15));
    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), false);
    assert_eq!(bitmap.prev_index(2), Some(1));
    assert_eq!(bitmap.prev_false_index(2), None);
    assert_eq!(bitmap.next_index(2), Some(3));
    assert_eq!(bitmap.next_false_index(2), Some(15));
}

#[test]
fn test_alloc_bitmap_128_two_1_seperate_first() {
    let mut bitmap = AllocBitmap::new(128);

    bitmap.set(0, true);
    bitmap.set(127, true);

    assert_eq!(bitmap.len(), 128);
    assert_eq!(bitmap.size(), 16);
    assert_eq!(bitmap.first_index(), Some(0));
    assert_eq!(bitmap.first_false_index(), Some(1));
    assert_eq!(bitmap.last_index(), Some(127));
    assert_eq!(bitmap.last_false_index(), Some(126));
    assert_eq!(bitmap.next_index(0), Some(127));
    assert_eq!(bitmap.next_index(127), None);
    assert_eq!(bitmap.next_false_index(0), Some(1));
    assert_eq!(bitmap.prev_index(127), Some(0));
    assert_eq!(bitmap.prev_false_index(127), Some(126));
    assert_eq!(bitmap.prev_index(64), Some(0));
    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), false);

    // 反转

    bitmap.invert();

    assert_eq!(bitmap.first_index(), Some(1));
    assert_eq!(bitmap.first_false_index(), Some(0));
    assert_eq!(bitmap.last_index(), Some(126));
    assert_eq!(bitmap.last_false_index(), Some(127));
    assert_eq!(bitmap.next_index(0), Some(1));
    assert_eq!(bitmap.next_false_index(0), Some(127));
    assert_eq!(bitmap.prev_index(127), Some(126));
    assert_eq!(bitmap.prev_false_index(127), Some(0));
    assert_eq!(bitmap.prev_false_index(64), Some(0));
    assert_eq!(bitmap.is_empty(), false);
    assert_eq!(bitmap.is_full(), false);
}

/// 长度128, 第63、64bit为1
#[test]
fn test_alloc_bitmap_128_two_1_nearby_middle() {
    let mut bitmap = AllocBitmap::new(128);

    bitmap.set(63, true);
    bitmap.set(64, true);

    assert_eq!(bitmap.len(), 128);
    assert_eq!(bitmap.size(), 16);

    assert_eq!(bitmap.first_index(), Some(63));
    assert_eq!(bitmap.first_false_index(), Some(0));
    assert_eq!(bitmap.last_index(), Some(64));
    assert_eq!(bitmap.last_false_index(), Some(127));
    assert_eq!(bitmap.next_index(0), Some(63));
    assert_eq!(bitmap.next_index(63), Some(64));
    assert_eq!(bitmap.next_index(64), None);

    assert_eq!(bitmap.next_false_index(0), Some(1));
    assert_eq!(bitmap.next_false_index(63), Some(65));
    assert_eq!(bitmap.prev_index(64), Some(63));
    assert_eq!(bitmap.prev_false_index(64), Some(62));
    assert_eq!(bitmap.prev_index(63), None);
    assert_eq!(bitmap.prev_false_index(63), Some(62));
    assert_eq!(bitmap.prev_index(65), Some(64));

    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), false);

    // 反转
    bitmap.invert();

    assert_eq!(bitmap.first_index(), Some(0));
    assert_eq!(bitmap.first_false_index(), Some(63));
    assert_eq!(bitmap.last_index(), Some(127));
    assert_eq!(bitmap.last_false_index(), Some(64));
    assert_eq!(bitmap.next_index(0), Some(1));
    assert_eq!(bitmap.next_false_index(0), Some(63));
    assert_eq!(bitmap.next_false_index(63), Some(64));
    assert_eq!(bitmap.next_index(63), Some(65));
    assert_eq!(bitmap.prev_false_index(127), Some(64));
    assert_eq!(bitmap.prev_index(127), Some(126));
    assert_eq!(bitmap.prev_false_index(64), Some(63));
    assert_eq!(bitmap.prev_index(64), Some(62));
    assert_eq!(bitmap.prev_index(63), Some(62));

    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), false);
}

#[test]
fn test_alloc_bitmap_full_32() {
    let mut bitmap = AllocBitmap::new(32);
    bitmap.set_all(true);

    assert_eq!(bitmap.len(), 32);
    assert_eq!(bitmap.size(), 8);
    assert_eq!(bitmap.first_index(), Some(0));
    assert_eq!(bitmap.first_false_index(), None);
    assert_eq!(bitmap.last_index(), Some(31));
    assert_eq!(bitmap.last_false_index(), None);
    assert_eq!(bitmap.next_index(0), Some(1));
    assert_eq!(bitmap.next_index(31), None);
    assert_eq!(bitmap.next_false_index(0), None);
    assert_eq!(bitmap.prev_index(31), Some(30));
    assert_eq!(bitmap.prev_false_index(31), None);
    assert_eq!(bitmap.prev_index(0), None);

    assert_eq!(bitmap.is_full(), true);
    assert_eq!(bitmap.is_empty(), false);

    // 反转
    bitmap.invert();

    assert_eq!(bitmap.first_index(), None);
    assert_eq!(bitmap.first_false_index(), Some(0));
    assert_eq!(bitmap.last_index(), None);
    assert_eq!(bitmap.last_false_index(), Some(31));
    assert_eq!(bitmap.next_index(0), None);
    assert_eq!(bitmap.next_false_index(0), Some(1));
    assert_eq!(bitmap.prev_index(31), None);
    assert_eq!(bitmap.prev_false_index(31), Some(30));
    assert_eq!(bitmap.prev_index(0), None);

    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), true);
}

#[test]
fn test_alloc_bitmap_full_64() {
    let mut bitmap = AllocBitmap::new(64);
    bitmap.set_all(true);

    assert_eq!(bitmap.len(), 64);
    assert_eq!(bitmap.size(), 8);
    assert_eq!(bitmap.first_index(), Some(0));
    assert_eq!(bitmap.first_false_index(), None);
    assert_eq!(bitmap.last_index(), Some(63));
    assert_eq!(bitmap.last_false_index(), None);
    assert_eq!(bitmap.next_index(0), Some(1));
    assert_eq!(bitmap.next_index(63), None);
    assert_eq!(bitmap.next_false_index(0), None);
    assert_eq!(bitmap.prev_index(63), Some(62));
    assert_eq!(bitmap.prev_false_index(63), None);
    assert_eq!(bitmap.prev_index(0), None);

    assert_eq!(bitmap.is_full(), true);
    assert_eq!(bitmap.is_empty(), false);

    // 反转
    bitmap.invert();

    assert_eq!(bitmap.first_index(), None);
    assert_eq!(bitmap.first_false_index(), Some(0));
    assert_eq!(bitmap.last_index(), None);
    assert_eq!(bitmap.last_false_index(), Some(63));
    assert_eq!(bitmap.next_index(0), None);
    assert_eq!(bitmap.next_false_index(0), Some(1));
    assert_eq!(bitmap.prev_index(63), None);
    assert_eq!(bitmap.prev_false_index(63), Some(62));
    assert_eq!(bitmap.prev_index(0), None);

    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), true);
}

#[test]
fn test_alloc_bitmap_full_100() {
    let mut bitmap = AllocBitmap::new(100);
    bitmap.set_all(true);

    assert_eq!(bitmap.len(), 100);
    assert_eq!(bitmap.size(), 16);
    assert_eq!(bitmap.first_index(), Some(0));
    assert_eq!(bitmap.first_false_index(), None);
    assert_eq!(bitmap.last_index(), Some(99));
    assert_eq!(bitmap.last_false_index(), None);
    assert_eq!(bitmap.next_index(0), Some(1));
    assert_eq!(bitmap.next_index(99), None);
    assert_eq!(bitmap.next_false_index(0), None);
    assert_eq!(bitmap.prev_index(99), Some(98));
    assert_eq!(bitmap.prev_false_index(99), None);
    assert_eq!(bitmap.prev_index(0), None);

    assert_eq!(bitmap.is_full(), true);
    assert_eq!(bitmap.is_empty(), false);

    // 反转
    bitmap.invert();

    assert_eq!(bitmap.first_index(), None);
    assert_eq!(bitmap.first_false_index(), Some(0));
    assert_eq!(bitmap.last_index(), None);
    assert_eq!(bitmap.last_false_index(), Some(99));
    assert_eq!(bitmap.next_index(0), None);
    assert_eq!(bitmap.next_false_index(0), Some(1));
    assert_eq!(bitmap.prev_index(99), None);
    assert_eq!(bitmap.prev_false_index(99), Some(98));
    assert_eq!(bitmap.prev_index(0), None);

    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), true);
}

#[test]
fn test_alloc_bitmap_full_128() {
    let mut bitmap = AllocBitmap::new(128);
    bitmap.set_all(true);

    assert_eq!(bitmap.len(), 128);
    assert_eq!(bitmap.size(), 16);
    assert_eq!(bitmap.first_index(), Some(0));
    assert_eq!(bitmap.first_false_index(), None);
    assert_eq!(bitmap.last_index(), Some(127));
    assert_eq!(bitmap.last_false_index(), None);
    assert_eq!(bitmap.next_index(0), Some(1));
    assert_eq!(bitmap.next_index(127), None);
    assert_eq!(bitmap.next_false_index(0), None);
    assert_eq!(bitmap.prev_index(127), Some(126));
    assert_eq!(bitmap.prev_false_index(127), None);
    assert_eq!(bitmap.prev_index(0), None);

    assert_eq!(bitmap.is_full(), true);
    assert_eq!(bitmap.is_empty(), false);

    // 反转
    bitmap.invert();

    assert_eq!(bitmap.first_index(), None);
    assert_eq!(bitmap.first_false_index(), Some(0));
    assert_eq!(bitmap.last_index(), None);
    assert_eq!(bitmap.last_false_index(), Some(127));
    assert_eq!(bitmap.next_index(0), None);
    assert_eq!(bitmap.next_false_index(0), Some(1));
    assert_eq!(bitmap.prev_index(127), None);
    assert_eq!(bitmap.prev_false_index(127), Some(126));
    assert_eq!(bitmap.prev_index(0), None);

    assert_eq!(bitmap.is_full(), false);
    assert_eq!(bitmap.is_empty(), true);
}

#[test]
fn test_alloc_bitmap_bitand_128() {
    let mut bitmap = AllocBitmap::new(128);
    bitmap.set_all(true);

    let mut bitmap2 = AllocBitmap::new(128);

    bitmap2.set(0, true);
    bitmap2.set(1, true);
    bitmap2.set(67, true);

    let bitmap3 = bitmap & bitmap2;

    assert_eq!(bitmap3.len(), 128);
    assert_eq!(bitmap3.size(), 16);
    assert_eq!(bitmap3.first_index(), Some(0));
    assert_eq!(bitmap3.first_false_index(), Some(2));
    assert_eq!(bitmap3.last_index(), Some(67));
}

#[test]
fn test_alloc_bitmap_bitand_assign_128() {
    let mut bitmap = AllocBitmap::new(128);
    bitmap.set_all(true);

    let mut bitmap2 = AllocBitmap::new(128);

    bitmap2.set(0, true);
    bitmap2.set(1, true);
    bitmap2.set(67, true);

    bitmap.bitand_assign(&bitmap2);

    assert_eq!(bitmap.len(), 128);
    assert_eq!(bitmap.size(), 16);
    assert_eq!(bitmap.first_index(), Some(0));
    assert_eq!(bitmap.first_false_index(), Some(2));
    assert_eq!(bitmap.last_index(), Some(67));
}
