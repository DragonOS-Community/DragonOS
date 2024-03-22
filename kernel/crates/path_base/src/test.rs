use crate::*;
#[test]
fn test_path_abs() {
    let path = Path::new("/home/work/src");
    assert_eq!(path.is_absolute(), true);
    let path = Path::new("~/src");
    assert_eq!(path.is_absolute(), false);
}
#[test]
fn test_path_split() {
    let path = Path::new("/home/work/src");
    assert_eq!(path.file_name(), Some("src"));
    assert_eq!(path.parent(), Some(Path::new("/home/work")));
}
#[test]
fn test_path_prefix() {
    assert_eq!(Path::new("/bin/bash").starts_with("/bin"), true);
}
#[test]
fn test_path_file_name() {
    assert_eq!(
        Path::new("/home/work/../work/dir/./../dir/").file_name(),
        Some("dir")
    )
}
#[test]
fn test_path_search() {
    let path = Path::new("/home/work/../work/dir/./../dir/");
    let mut iter = path.iter();
    assert_eq!(iter.next(), Some("/"));
    assert_eq!(iter.next(), Some("home"));
    assert_eq!(iter.next(), Some("work"));
    assert_eq!(iter.next(), Some(".."));
    assert_eq!(iter.next(), Some("work"));
    assert_eq!(iter.next(), Some("dir"));
    assert_eq!(iter.next(), Some(".."));
}
