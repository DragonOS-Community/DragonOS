# From Software Packages to RootFS Image

After defining the software packages, no further modifications are needed. Simply `nix run .#rootfs-${target}` to build the RootFS.

## Generating a Docker Image

We leverage the flattening and copying properties of `pkgs.dockerTools.buildImage` to copy the packages under `user/apps` and the configuration files in `user/sysconfig` into a single-layer overlayfs (following the Docker principle). The final output of `buildImage` is a `tar.gz` that can be used by `docker import`. See `rootfs-tar.nix`.

<<< ../../../user/rootfs-tar.nix{nix}


## Unpack the Docker tar and build the filesystem image

The idea: unpack the Docker image, take `layer.tar` from the first layer, then import it into a virtual disk image with `guestfish`. See `user/default.nix`.

<<< ../../../user/default.nix{23-112 shell}


## Behind the Nix Script

When generating a Docker image, the Docker Image is cached as a binary artifact in /nix/store, so multiple builds may quickly consume disk space.

hint: To free up disk space, run `nix store gc`

However, the second step—extracting the Docker tar to a filesystem disk image file—is implemented by generating and running a shell script. The generated `disk-image-${target}.img` will only exist in the `bin` directory. Meanwhile, the `rootfs.tar` extracted from `.tar.gz` will also reside in the `bin` directory. You can inspect it yourself to see what's inside.

TODO: Future scripts will allow a two-step process, with the option to inject custom files in between (in addition to direct injection via sysconfig).

If you want to see what this script looks like, you can run `nix build .#rootfs-${target} -o bin/result` in the project root directory. Nix will link the script's corresponding derivation to the result directory. Simply `cat bin/result/bin/build-rootfs-image` to view all the commands.
