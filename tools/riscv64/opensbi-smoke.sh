#!/usr/bin/env bash
set -euo pipefail

mode=${1:-sbi}
case "$mode" in
  sbi|u-boot|edk2) ;;
  *) echo "Usage: $0 [sbi|u-boot|edk2]" >&2; exit 2 ;;
esac
(( $# <= 1 )) || { echo "Expected at most one boot mode" >&2; exit 2; }
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
readonly opensbi="${OPENSBI_FIRMWARE:-default}"
readonly boot_timeout="${DRAGONOS_BOOT_TIMEOUT_SECS:-180}"
[[ "$boot_timeout" =~ ^[1-9][0-9]*$ ]] || { echo "Invalid boot timeout" >&2; exit 2; }
readonly UBOOT_VERSION=2024.04
readonly UBOOT_SHA256=d6b57ce574a0a0504a5b6596644ceacb7f77bde9353779bcf2fde07c4b9a2b92
readonly EDK2_REV=6951dfe7d59d144a3a980bd7eda699db2d8554ac
readonly BUSYBOX_VERSION=1.35.0
readonly BUSYBOX_SHA256=faeeb244c35a348a334f4a59e44626ee870fb07b6884d68c10ae8bc19f83a694
readonly MUSL_URL=https://github.com/DragonOS-Community/musl-cross-make/releases/download/9.4.0-231114/riscv64-linux-musl-cross-gcc-9.4.0.tar.xz
readonly MUSL_SHA256=b3833579b91138e496d4bcbee74fa744cf3bff743e9b6f9d1094fb7c3057a93a
readonly work_dir="$(realpath -m "${DRAGONOS_WORK_DIR:-bin/riscv64-smoke-work}")"
readonly cache_dir="$(realpath -m "${DRAGONOS_CACHE_DIR:-bin/riscv64-smoke-cache}")"
readonly log_dir="$(realpath -m "${DRAGONOS_LOG_DIR:-bin/riscv64-smoke-logs}")"
mkdir -p "$log_dir"
readonly run_dir="$(mktemp -d "$log_dir/${mode}.XXXXXX")"
readonly download_timeout="${DRAGONOS_DOWNLOAD_TIMEOUT_SECS:-900}"
readonly connect_timeout="${DRAGONOS_DOWNLOAD_CONNECT_TIMEOUT_SECS:-30}"

download_asset() {
  local url=$1 destination=$2 digest=$3 temporary
  if [[ -f "$destination" ]] && printf '%s  %s\n' "$digest" "$destination" | sha256sum --check --status; then
    return
  fi
  temporary=$(mktemp "${destination}.part.XXXXXX")
  if ! curl --fail --location --retry 3 --retry-all-errors \
    --connect-timeout "$connect_timeout" --max-time "$download_timeout" \
    --retry-max-time "$download_timeout" --output "$temporary" "$url" ||
    ! printf '%s  %s\n' "$digest" "$temporary" | sha256sum --check; then
    rm -f "$temporary"
    return 1
  fi
  mv "$temporary" "$destination"
}

prepare_dragonos() {
  local kernel efi
  if [[ -n ${DRAGONOS_KERNEL_ELF:-} ]]; then
    kernel=$DRAGONOS_KERNEL_ELF
    efi=${DRAGONOS_EFI:-}
    if [[ "$mode" != sbi && -z "$efi" ]]; then
      echo 'Set DRAGONOS_EFI to the EFI payload built with DRAGONOS_KERNEL_ELF' >&2
      return 1
    fi
  else
    CARGO_BUILD_JOBS=4 make ARCH=riscv64 ROOTFS_MANIFEST=default NPROCS=4 kernel
    kernel=bin/kernel/kernel.elf
    efi=bin/sysroot/efi/boot/bootriscv64.efi
  fi
  test -s "$kernel"
  if ! LC_ALL=C readelf -h "$kernel" | grep -E 'Machine:[[:space:]]+RISC-V' >/dev/null; then
    echo "Expected a RISC-V ELF kernel: $kernel" >&2
    return 1
  fi
  # Flatten the linked ELF so QEMU places the S-mode kernel at 0x80200000.
  "${OBJCOPY:-riscv64-linux-gnu-objcopy}" -O binary "$kernel" "$run_dir/kernel.bin"
  if [[ "$mode" != sbi ]]; then
    test -s "$efi"
    cp "$efi" "$run_dir/bootriscv64.efi"
  fi
  if [[ "$opensbi" != default ]]; then
    test -s "$opensbi"
  fi
}

prepare_busybox() {
  [[ -s "$cache_dir/busybox" ]] && return
  local tarball="$work_dir/busybox-${BUSYBOX_VERSION}.tar.bz2"
  download_asset "$MUSL_URL" "$cache_dir/riscv64-linux-musl-cross-gcc-9.4.0.tar.xz" "$MUSL_SHA256"
  tar -xJf "$cache_dir/riscv64-linux-musl-cross-gcc-9.4.0.tar.xz" -C "$work_dir"
  download_asset "https://mirrors.dragonos.org.cn/pub/third_party/busybox/busybox-${BUSYBOX_VERSION}.tar.bz2" \
    "$tarball" "$BUSYBOX_SHA256"
  tar -xjf "$tarball" -C "$work_dir"
  local tree="$work_dir/busybox-${BUSYBOX_VERSION}"
  make -C "$tree" defconfig
  # Match DragonOS's static BusyBox build; TC needs removed Linux CBQ headers.
  sed -i -e 's/# CONFIG_STATIC is not set/CONFIG_STATIC=y/' \
    -e 's/CONFIG_TC=y/# CONFIG_TC is not set/' "$tree/.config"
  make -C "$tree" CROSS_COMPILE="$work_dir/riscv64-linux-musl-cross-gcc-9.4.0/bin/riscv64-linux-musl-" \
    EXTRA_CFLAGS='-idirafter /usr/riscv64-linux-gnu/include' -j"${JOBS:-4}"
  cp "$tree/busybox" "$cache_dir/busybox"
}

prepare_bootloader() {
  if [[ "$mode" == u-boot ]]; then
    if [[ ! -s "$cache_dir/u-boot.bin" ]]; then
      if [[ -n ${DRAGONOS_UBOOT_SOURCE_DIR:-} ]]; then
        tree=$DRAGONOS_UBOOT_SOURCE_DIR
        [[ $(git -C "$tree" rev-parse HEAD) == 25049ad560826f7dc1c4740883b0016014a59789 ]]
      else
        tarball="$work_dir/u-boot-${UBOOT_VERSION}.tar.gz"
        download_asset "https://github.com/u-boot/u-boot/archive/refs/tags/v${UBOOT_VERSION}.tar.gz" \
          "$tarball" "$UBOOT_SHA256"
        tar -xzf "$tarball" -C "$work_dir"
        tree="$work_dir/u-boot-${UBOOT_VERSION}"
      fi
      make -C "$tree" O="$work_dir/uboot-build" ARCH=riscv \
        CROSS_COMPILE=riscv64-linux-gnu- qemu-riscv64_smode_defconfig
      make -C "$tree" O="$work_dir/uboot-build" ARCH=riscv \
        CROSS_COMPILE=riscv64-linux-gnu- -j"${JOBS:-4}"
      cp "$work_dir/uboot-build/u-boot.bin" "$cache_dir/u-boot.bin"
    fi
  elif [[ "$mode" == edk2 ]]; then
    if [[ $(stat -c %s "$cache_dir/RISCV_VIRT_CODE.fd" 2>/dev/null || true) != 33554432 ||
          $(stat -c %s "$cache_dir/RISCV_VIRT_VARS.fd" 2>/dev/null || true) != 33554432 ]]; then
      tree="$work_dir/edk2"
      if [[ ! -d "$tree/.git" ]]; then
        git init --quiet "$tree"
        git -C "$tree" remote add origin https://github.com/tianocore/edk2.git
      fi
      timeout "$download_timeout" git -C "$tree" fetch --quiet --depth=1 origin "$EDK2_REV"
      git -C "$tree" checkout --detach "$EDK2_REV"
      [[ $(git -C "$tree" rev-parse HEAD) == "$EDK2_REV" ]]
      timeout "$download_timeout" git -C "$tree" submodule update --init --depth=1
      (
        cd "$work_dir"
        export WORKSPACE="$PWD" PACKAGES_PATH="$tree" EDK_TOOLS_PATH="$tree/BaseTools"
        export GCC5_RISCV64_PREFIX=riscv64-linux-gnu-
        set +u
        # shellcheck disable=SC1090
        source "$tree/edksetup.sh" --reconfig
        set -u
        make -C "$tree/BaseTools" -j"${JOBS:-4}"
        set +u
        # shellcheck disable=SC1090
        source "$tree/edksetup.sh" BaseTools
        set -u
        build -a RISCV64 -b RELEASE -p OvmfPkg/RiscVVirt/RiscVVirtQemu.dsc -t GCC5
      )
      fv="$work_dir/Build/RiscVVirtQemu/RELEASE_GCC5/FV"
      cp "$fv/RISCV_VIRT_CODE.fd" "$cache_dir/RISCV_VIRT_CODE.fd"
      cp "$fv/RISCV_VIRT_VARS.fd" "$cache_dir/RISCV_VIRT_VARS.fd"
      truncate -s 32M "$cache_dir/RISCV_VIRT_CODE.fd" "$cache_dir/RISCV_VIRT_VARS.fd"
    fi
  fi
}

# Each run gets a fresh FAT disk; mtools avoids privileged loop mounts.
make_disk() {
  local disk="$run_dir/disk.img"
  fat=$(mktemp "${disk}.fat.XXXXXX")
  rm -f "$disk"
  truncate -s 2147483648 "$disk"
  printf 'label: dos\nunit: sectors\n\nstart=2048, size=4192256, type=c, bootable\n' \
    | sfdisk "$disk" >/dev/null
  truncate -s 2146435072 "$fat"
  # Match the official image builder: no FAT volume-label directory entry.
  mkfs.fat -F 32 -S 512 -h 2048 --invariant -n '' "$fat" >/dev/null
  mmd -i "$fat" ::/efi ::/efi/boot ::/bin
  if [[ "$mode" != sbi ]]; then
    mcopy -i "$fat" "$run_dir/bootriscv64.efi" ::/efi/boot/bootriscv64.efi
  fi
  mcopy -i "$fat" "$cache_dir/busybox" ::/bin/busybox
  mcopy -i "$fat" "$cache_dir/busybox" ::/bin/sh
  dd if="$fat" of="$disk" bs=1M seek=1 conv=notrunc,sparse status=none
  if [[ "$mode" != sbi ]]; then
    mdir -i "${disk}@@1048576" ::/efi/boot/bootriscv64.efi
  fi
  mdir -i "${disk}@@1048576" ::/bin/sh
  rm -f "$fat"
}

# QEMU's pipe backends provide stdin for U-Boot and the guest shell; output
# goes straight to logs. No terminal automation or custom guest protocol.
start_qemu() {
  local channel
  for channel in uart guest; do
    mkfifo "$run_dir/$channel.in" "$run_dir/$channel.out"
  done
  exec 3<>"$run_dir/uart.in" 4<>"$run_dir/guest.in"
  local -a args=(-machine virt -accel tcg -m 2G -smp 1 -no-reboot
    -display none -monitor none -nic none -bios "$opensbi"
    -chardev "pipe,id=uart,path=$run_dir/uart" -serial chardev:uart
    -chardev "pipe,id=guest,path=$run_dir/guest"
    -device virtio-serial-device -device 'virtconsole,chardev=guest'
    -drive "if=none,id=hd0,format=raw,file=$run_dir/disk.img"
    -device 'virtio-blk-device,drive=hd0')
  case "$mode" in
    sbi) args+=(-kernel "$run_dir/kernel.bin" -append "$bootargs") ;;
    u-boot) args+=(-kernel "$cache_dir/u-boot.bin") ;;
    edk2)
      cp "$cache_dir/RISCV_VIRT_VARS.fd" "$run_dir/VARS.fd"
      args[1]=virt,pflash0=pflash0,pflash1=pflash1,acpi=off
      args+=(-blockdev "node-name=pflash0,driver=file,read-only=on,filename=$cache_dir/RISCV_VIRT_CODE.fd"
        -blockdev "node-name=pflash1,driver=file,filename=$run_dir/VARS.fd"
        -kernel "$run_dir/bootriscv64.efi")
      # This DragonStub does not parse EFI LoadOptions; pass bootargs in FDT.
      timeout 20 qemu-system-riscv64 "${args[@]}" \
        -machine "${args[1]},dumpdtb=$run_dir/guest.dtb" >"$run_dir/dump-dtb.log" 2>&1
      fdtput -t s "$run_dir/guest.dtb" /chosen bootargs "$bootargs"
      args+=(-dtb "$run_dir/guest.dtb")
      ;;
  esac
  for channel in uart guest; do
    cat "$run_dir/$channel.out" >"$run_dir/$channel.log" &
    readers+=("$!")
  done
  qemu-system-riscv64 "${args[@]}" >"$run_dir/qemu.log" 2>&1 &
  qemu_pid=$!
  deadline=$((SECONDS + boot_timeout))
}

wait_for() {
  local log=$1 pattern=$2
  while (( SECONDS < deadline )); do
    if grep -Eq 'Kernel panic|Kernel Panic Occurred|panicked at|Unhandled exception:|EXCEPT_RISCV_ILLEGAL_INST|Synchronous Exception|do_trap_(insn|load|store)_page_fault' "$run_dir/"{uart,guest}.log; then
      echo 'DragonOS boot failed' >&2
      return 1
    fi
    grep -Eq "$pattern" "$log" && return
    if ! kill -0 "$qemu_pid" 2>/dev/null; then
      echo 'QEMU exited before the smoke test completed' >&2
      return 1
    fi
    sleep 0.2
  done
  echo "Timed out waiting for $pattern in $log" >&2
  return 1
}

boot_userspace() {
  if [[ "$mode" == u-boot ]]; then
    wait_for "$run_dir/uart.log" 'Hit any key to stop autoboot:'
    printf '\r' >&3
    wait_for "$run_dir/uart.log" '=> '
    # U-Boot expands fdtcontroladdr after receiving this command.
    # shellcheck disable=SC2016
    printf '%s' 'virtio scan; ' \
      'fatload virtio 0:1 0x84000000 /efi/boot/bootriscv64.efi; ' \
      'setenv bootargs; fdt move ${fdtcontroladdr} 0x88000000 0x10000; ' \
      'fdt addr 0x88000000; ' "fdt set /chosen bootargs \"$bootargs\"; " \
      'bootefi 0x84000000 0x88000000' $'\r' >&3
  elif [[ "$mode" == edk2 ]]; then
    wait_for "$run_dir/uart.log" 'RISC-V EDK2 firmware version'
  fi
  if [[ "$mode" != sbi ]]; then
    wait_for "$run_dir/uart.log" 'Booting DragonOS kernel'
  fi
  wait_for "$run_dir/guest.log" '# '
  # Split the marker so terminal command echo cannot satisfy the assertion.
  printf '%s\n' "/bin/busybox true && printf 'DRAGONOS-%s\\n' EXEC-OK" >&4
  wait_for "$run_dir/guest.log" '^DRAGONOS-EXEC-OK[[:space:]]*$'
  printf '%s\n' "printf 'DRAGONOS-%s\\n' SMOKE-OK" >&4
  wait_for "$run_dir/guest.log" '^DRAGONOS-SMOKE-OK[[:space:]]*$'
  echo "DragonOS userspace smoke passed ($mode); logs: $run_dir"
}

cleanup() {
  local status=$? pid
  for pid in "${qemu_pid:-}" "${readers[@]}"; do
    [[ -n "$pid" ]] || continue
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  done
  rm -f "$run_dir/"{uart,guest}.{in,out} "${fat:-}" "$run_dir/disk.img"
  if (( status != 0 )); then
    tail -n 80 "$run_dir/"*.log >&2 || true
  fi
}

main() {
  mkdir -p "$work_dir" "$cache_dir" "$run_dir"
  rm -f "$run_dir/"{uart.log,guest.log,qemu.log,dump-dtb.log}
  exec > >(tee "$run_dir/build.log") 2>&1
  readers=()
  trap cleanup EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  echo "OpenSBI firmware: $opensbi; logs: $run_dir"
  qemu-system-riscv64 --version
  prepare_dragonos
  prepare_busybox
  prepare_bootloader
  make_disk
  bootargs='root=/dev/vda1 console=/dev/hvc0 init=/bin/sh rw -- -i'
  start_qemu
  boot_userspace
}

main "$@"
