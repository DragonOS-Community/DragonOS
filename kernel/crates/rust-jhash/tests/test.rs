#[test]
fn mix() {
    let mut a = 541;
    let mut b = 562059789;
    let mut c = 2410045;

    jhash::jhash_mix(&mut a, &mut b, &mut c);
    assert_eq!(a, 350610097);
    assert_eq!(b, 271134839);
    assert_eq!(c, 4203803819);
}

#[test]
fn jhash() {
    let buf = b"Four score and seven years ago";
    assert_eq!(buf.len(), 30);
    assert_eq!(jhash::jhash(buf, 0), 0x17770551);
    assert_eq!(jhash::jhash(buf, 1), 0xcd628161);

    let buf = b"This is the time for all good men to come to the aid of their country...";
    let checksums = &[
        0x499ae8fa, 0xb9bef31c, 0x8efefdfd, 0xa56b7aab, 0xb1946734, 0x9f31c5ce, 0x0826585d,
        0x55b69dea, 0xf4688dd0, 0xe87eb146, 0xb202fb17, 0x711fe56a,
    ];
    for (i, &checksum) in checksums.iter().enumerate() {
        assert_eq!(jhash::jhash(&buf[..buf.len() - i], 13), checksum);
    }
}
