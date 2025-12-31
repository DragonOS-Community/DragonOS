#![allow(dead_code)]

/// Linux compatible inotify UAPI definitions.
///
/// Values follow Linux `include/uapi/linux/inotify.h`.
use bitflags::bitflags;
use core::mem::size_of;

bitflags! {
    #[derive(Default)]
    pub struct InotifyMask: u32 {
        const IN_ACCESS = 0x0000_0001;
        const IN_MODIFY = 0x0000_0002;
        const IN_ATTRIB = 0x0000_0004;
        const IN_CLOSE_WRITE = 0x0000_0008;
        const IN_CLOSE_NOWRITE = 0x0000_0010;
        const IN_OPEN = 0x0000_0020;
        const IN_MOVED_FROM = 0x0000_0040;
        const IN_MOVED_TO = 0x0000_0080;
        const IN_CREATE = 0x0000_0100;
        const IN_DELETE = 0x0000_0200;
        const IN_DELETE_SELF = 0x0000_0400;
        const IN_MOVE_SELF = 0x0000_0800;

        const IN_UNMOUNT = 0x0000_2000;
        const IN_Q_OVERFLOW = 0x0000_4000;
        const IN_IGNORED = 0x0000_8000;

        const IN_ONLYDIR = 0x0100_0000;
        const IN_DONT_FOLLOW = 0x0200_0000;
        const IN_EXCL_UNLINK = 0x0400_0000;
        const IN_MASK_CREATE = 0x1000_0000;
        const IN_MASK_ADD = 0x2000_0000;
        const IN_ISDIR = 0x4000_0000;
        const IN_ONESHOT = 0x8000_0000;

        const IN_CLOSE = Self::IN_CLOSE_WRITE.bits | Self::IN_CLOSE_NOWRITE.bits;
        const IN_MOVE = Self::IN_MOVED_FROM.bits | Self::IN_MOVED_TO.bits;
        const IN_ALL_EVENTS = Self::IN_ACCESS.bits
            | Self::IN_MODIFY.bits
            | Self::IN_ATTRIB.bits
            | Self::IN_CLOSE_WRITE.bits
            | Self::IN_CLOSE_NOWRITE.bits
            | Self::IN_OPEN.bits
            | Self::IN_MOVED_FROM.bits
            | Self::IN_MOVED_TO.bits
            | Self::IN_CREATE.bits
            | Self::IN_DELETE.bits
            | Self::IN_DELETE_SELF.bits
            | Self::IN_MOVE_SELF.bits;
    }
}

// inotify_init1 flags (same values as O_* bits on Linux).
pub const IN_CLOEXEC: u32 = 0o2000000;
pub const IN_NONBLOCK: u32 = 0o0004000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct WatchDescriptor(pub i32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct InotifyCookie(pub u32);

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct InotifyEvent {
    pub wd: WatchDescriptor,
    pub mask: InotifyMask,
    pub cookie: InotifyCookie,
    pub len: u32,
}

impl InotifyEvent {
    pub const SIZE: usize = size_of::<InotifyEvent>();
}

#[inline]
pub fn align4(len: usize) -> usize {
    (len + 3) & !3
}
