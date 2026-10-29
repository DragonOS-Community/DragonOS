# From Software Packages to RootFS Image

After defining the software packages, no further modifications are needed. Simply `nix run .#rootfs-${target}` to build the RootFS.

## Generating a Docker Image

We leverage the flattening and copying properties of `pkgs.dockerTools.buildImage` to copy the packages under `user/apps` and the configuration files in `user/sysconfig` (together with their `/nix/store` closure) into a single-layer image (following the Docker principle). The output of `buildImage` is an uncompressed tar that can be used by `docker load`. See `rootfs-tar.nix`.

<<< ../../../user/rootfs-tar.nix{nix}


## Generating the Disk Image

Inside the Nix sandbox, the first layer's `layer.tar` is extracted under `fakeroot`, then `mke2fs -d` writes the directory straight into a partition at a 1MiB offset in the disk image, and `sfdisk` writes the MBR partition table (the kernel mounts the rootfs from `/dev/vda1`). No root privileges or loop devices are needed. See `user/disk-image.nix`.

<<< ../../../user/disk-image.nix{nix}

`nix build .#checks.x86_64-linux.rootfs-image-x86_64` validates the generated image (partition table, `e2fsck`, ownership of key files); see `user/check-disk-image.sh`.


## Behind the Nix Script

The disk image is an ordinary Nix derivation cached in /nix/store: it is not regenerated unless `user/` changes. Old copies remain after repeated changes to `user/` and may take up a lot of disk space.

hint: To free up disk space, run `nix store gc`

For interactive runs, the script run by `nix run .#rootfs-${target}` (`user/default.nix`) only copies the image from the store to the writable `bin/disk-image-${target}.img` when it has changed, recording its source in `bin/.disk-image-${target}.src`. When the image is unchanged the copy is skipped, so changes made to the disk inside DragonOS are kept; `-- --force` restores a clean image. When image preparation selects `AUTO_TEST=dunit` or `AUTO_TEST=syscall` (including `make test-dunit` and `make test-syscall`), it always restores the clean image and discards guest disk changes.

<<< ../../../user/default.nix{nix}

To inspect the final image, run `nix build .#packages.x86_64-linux.rootfs-x86_64.diskImage -o bin/result` in the project root; the image is at `bin/result/disk.img`.
