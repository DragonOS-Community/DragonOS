#!/bin/sh
# Fill these in appropriately:
ROOT_PATH=$(dirname $(dirname $(pwd)))
DRAGONOS_SYSROOT=$ROOT_PATH/bin/sysroot



export PKG_CONFIG_SYSROOT_DIR=$DRAGONOS_SYSROOT
export PKG_CONFIG_LIBDIR=$DRAGONOS_SYSROOT/usr/lib/pkgconfig
# TODO: If it works this should probably just be set to the empty string.
# export PKG_CONFIG_PATH=$PKG_CONFIG_LIBDIR
# Use --static here if your OS only has static linking.
# TODO: Perhaps it's a bug in the libraries if their pkg-config files doesn't
#       record that only static libraries were built.
# exec pkg-config --static "$@"