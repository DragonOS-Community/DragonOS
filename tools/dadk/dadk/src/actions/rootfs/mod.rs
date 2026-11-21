use crate::{console::rootfs::RootFSCommand, context::DADKExecContext};
use anyhow::Result;

mod disk_img;
mod loopdev;
mod sysroot;

pub(super) fn run(ctx: &DADKExecContext, rootfs_cmd: &RootFSCommand) -> Result<()> {
    match rootfs_cmd {
        RootFSCommand::Create(param) => disk_img::create(ctx, param.skip_if_exists),
        RootFSCommand::Delete => disk_img::delete(ctx, false),
        RootFSCommand::DeleteSysroot => sysroot::delete(ctx),
        RootFSCommand::Mount => disk_img::mount(ctx),
        RootFSCommand::Umount => disk_img::umount(ctx),
        RootFSCommand::CheckDiskImageExists => disk_img::check_disk_image_exists(ctx),
        RootFSCommand::ShowMountPoint => disk_img::show_mount_point(ctx),
        RootFSCommand::ShowLoopDevice => disk_img::show_loop_device(ctx),
    }
}
