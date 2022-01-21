cmake_minimum_required(VERSION 3.20)
set(CMAKE_VERBOSE_MAKEFILE on)

set(DRAGONOS_ARCH "x86_64")

mark_as_advanced(CMAKE_INSTALL_PREFIX)

set(CROSS_COMPILE "${DRAGONOS_ARCH}-linux-gnu-")
set(CMAKE_C_COMPILER "${CROSS_COMPILE}gcc")
set(CMAKE_ASM_COMPILER "${CROSS_COMPILE}gcc")
enable_language(ASM ASM_NASM)

set(CMAKE_C_FLAGS
        "-Wall -fPIC -nostdlib -nostartfiles -ffreestanding ")


project(dragonOS C ASM)

set(CMAKE_C_STANDARD 11)
add_subdirectory(bootloader)
add_subdirectory(kernel)
include_directories(kernel)

