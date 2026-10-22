{
  lib,
  pkgs,
  diskPath,
  kernel,
  testOpt,
  debug ? false,
  enableVsock ? true,
  vsockGuestCid ? 3,
  vsockDeviceModel ? "vhost-vsock-pci-non-transitional",
  vmstateDir ? null,
  preferSystemQemu ? false,
}:

assert lib.assertMsg (vsockGuestCid > 2) "vsockGuestCid must be > 2 (host CID = 2)";

let
  qemuFirmware = if preferSystemQemu then null else pkgs.callPackage ./qemu-firmware.nix { };
  consoleTermHelper = ./console-term.sh;

  baseConfig = {
    nographic = true;
    # 与 tools/run-qemu.sh 保持一致（部分 dunitest 用例在 512M 下会耗尽内核堆）
    memory = "2G";
    cores = "2";
    shmId = "dragonos-qemu-shm.ram";
  };

  riscv-uboot = pkgs.pkgsCross.riscv64-embedded.buildUBoot {
    defconfig = "qemu-riscv64_smode_defconfig";
    extraMeta.platforms = [ "riscv64-linux" ];
    filesToInstall = [ "u-boot.bin" ];
  };

  # 参数生成器 (Nix List -> Nix List)
  # 注意：网络配置中的端口使用 $HOST_PORT 变量，在运行时动态替换
  mkQemuArgs =
    { arch, isNographic }:
    let
      baseArgs = [
        "-m"
        baseConfig.memory
        "-smp"
        "${baseConfig.cores},cores=${baseConfig.cores},threads=1,sockets=1"
        "-usb"
        "-device"
        "qemu-xhci,id=xhci,p2=8,p3=4"
        "-D"
        "qemu.log"

        # Boot Order
        "-boot"
        "order=d"
        "-rtc"
        "clock=host,base=localtime"
      ];
      # Trace events 会显著放大 qemu.log 并拖慢 virtio，仅调试时开启
      traceArgs = lib.optionals debug [
        "-d"
        "cpu_reset,guest_errors,trace:virtio*,trace:e1000e_rx*,trace:e1000e_tx*,trace:e1000e_irq*"
        "-trace"
        "fw_cfg*"
      ];
      nographicArgs = lib.optionals isNographic (
        [
          "--nographic"
          "-serial"
          "chardev:mux"
          "-monitor"
          "chardev:mux"
          "-chardev"
          "stdio,id=mux,mux=on,signal=off,logfile=serial_opt.txt"
        ]
        ++ (
          if arch == "riscv64" then
            [
              "-device"
              "virtio-serial-device"
              "-device"
              "virtconsole,chardev=mux"
            ]
          else
            [
              "-device"
              "virtio-serial"
              "-device"
              "virtconsole,chardev=mux"
            ]
        )
      );
      kernelCmdlinePart = if isNographic then "console=/dev/hvc0" else "";
    in
    {
      flags = baseArgs ++ traceArgs ++ nographicArgs;
      cmdlineExtra = kernelCmdlinePart;
    };

  # 运行脚本生成器
  mkRunScript =
    {
      name,
      arch,
      isNographic,
      qemuBin,
    }:
    let
      qemuConfig = mkQemuArgs { inherit arch isNographic; };
      qemuFlagsStr = lib.escapeShellArgs qemuConfig.flags;

      initProgram = if arch == "riscv64" then "/bin/riscv_rust_init" else "/bin/busybox init";

      commonArchArgs =
        if arch == "x86_64" then
          [
            "-machine"
            "q35,memory-backend=${baseConfig.shmId}"
            "-cpu"
            "IvyBridge,apic,x2apic,+fpu,check,+vmx,"
          ]
        else
          [
            "-cpu"
            "sifive-u54"
          ];

      kernelPath = if arch == "x86_64" then kernel else "${riscv-uboot}/u-boot.bin";

      diskArgs =
        if arch == "x86_64" then
          [
            "-device"
            "virtio-blk-pci,drive=disk"
            "-device"
            "pci-bridge,chassis_nr=1,id=pci.1"
            "-device"
            "pcie-root-port"
            "-drive"
            "id=disk,file=${diskPath},if=none,format=raw"
          ]
        else
          [
            "-device"
            "virtio-blk-device,drive=disk"
            "-drive"
            "id=disk,file=${diskPath},if=none,format=raw"
          ];

      archSpecificBash =
        if arch == "x86_64" then
          ''
            if [ "$ACCEL" == "kvm" ]; then
                ARCH_FLAGS+=( "-machine" "accel=kvm" "-enable-kvm" )
            else
                ARCH_FLAGS+=( "-machine" "accel=tcg" )
            fi
          ''
        else
          ''
            ARCH_FLAGS+=( "-machine" "virt,accel=$ACCEL,memory-backend=${baseConfig.shmId}" )
          '';

      hasVmstateDir = vmstateDir != null;
      useVsock = enableVsock && arch == "x86_64";
      vsockCid = toString vsockGuestCid;

    in
    pkgs.writeScriptBin name ''
      #!${pkgs.runtimeShell}
      # 宿主的 LD_LIBRARY_PATH 可能与 nix 构建的 QEMU 的 glibc 不兼容
      unset LD_LIBRARY_PATH

      if [ ! -d "bin" ]; then echo "Error: Please run from project root (bin/ missing)."; exit 1; fi
      if [ ! -f "${diskPath}" ]; then echo "Error: ${diskPath} not found, run 'nix run .#rootfs-${arch}' first."; exit 1; fi

      ${lib.optionalString preferSystemQemu ''
        if ! command -v "${qemuBin}" >/dev/null 2>&1; then
          echo "Error: 系统未找到 ${qemuBin}，请安装 QEMU 或改用 nix 提供的 start 目标。"
          exit 1
        fi
      ''}

      # 端口查找函数：从指定端口开始查找可用端口
      find_available_port() {
        local port=$1
        while [ "$port" -lt 65535 ]; do
          if ! ${pkgs.iproute2}/bin/ss -tuln | grep -q ":$port "; then
            echo "$port"
            return 0
          fi
          port=$((port + 1))
        done
        echo "$1"
      }

      HOST_PORT=$(find_available_port 12580)
      GDB_PORT=$(find_available_port $((HOST_PORT + 1)))

      # 加速方式：QEMU_ACCEL 可显式指定 kvm / tcg
      if [ -n "''${QEMU_ACCEL:-}" ]; then
        ACCEL="$QEMU_ACCEL"
      elif [ -w /dev/kvm ]; then
        ACCEL="kvm"
      else
        ACCEL="tcg"
        echo "[WARN] /dev/kvm is not writable, falling back to TCG (slow)."
        echo "[WARN] Hint: add yourself to the kvm group: sudo usermod -aG kvm \$USER"
      fi

      VSOCK_ARGS=()
      ${
        if useVsock then
          ''
            # 同一 CID 只能被一个 QEMU 实例占用，已被占用时跳过 vsock 设备
            vsock_cid_in_use() {
              # cmdline 以 NUL 分隔参数；[,] 避免匹配到 grep 自身的参数
              grep -qsaxz ".*[,]guest-cid=$1" /proc/[0-9]*/cmdline
            }
            if [ ! -w /dev/vhost-vsock ]; then
              echo "[WARN] /dev/vhost-vsock not accessible; skip vsock device"
              echo "[WARN] Hint: sudo modprobe vhost_vsock, and make sure you are in the kvm group"
            elif ! ${qemuBin} -device help 2>/dev/null | grep -q "${vsockDeviceModel}"; then
              echo "[WARN] QEMU device model '${vsockDeviceModel}' not supported; skip vsock device"
            elif vsock_cid_in_use ${vsockCid}; then
              echo "[WARN] vsock guest CID=${vsockCid} already in use by another QEMU instance; skip vsock device"
            else
              VSOCK_ARGS=( "-device" "${vsockDeviceModel},guest-cid=${vsockCid}" )
              echo "[INFO] enable vsock device: ${vsockDeviceModel},guest-cid=${vsockCid}"
            fi
          ''
        else
          ''
            echo "[INFO] vsock disabled for ${arch}"
          ''
      }

      ${lib.optionalString hasVmstateDir ''
        VMSTATE_DIR="${vmstateDir}"
        mkdir -p "$VMSTATE_DIR"
        rm -f "$VMSTATE_DIR/pid" "$VMSTATE_DIR/vsock_cid"
        echo "$HOST_PORT" > "$VMSTATE_DIR/port"
        echo "$GDB_PORT" > "$VMSTATE_DIR/gdb"
        if [ ''${#VSOCK_ARGS[@]} -gt 0 ]; then
          echo "${vsockCid}" > "$VMSTATE_DIR/vsock_cid"
        fi
      ''}

      # 每个实例使用独立的共享内存文件，避免多个实例共享同一块 guest 内存
      SHM_PATH="/dev/shm/dragonos-qemu-$$.ram"
      MEM_ARGS=( "-object" "memory-backend-file,size=${baseConfig.memory},id=${baseConfig.shmId},mem-path=$SHM_PATH,share=on" )

      cleanup() {
        rm -f "$SHM_PATH"
        ${lib.optionalString hasVmstateDir ''rm -f "$VMSTATE_DIR/pid" "$VMSTATE_DIR/vsock_cid" "$VMSTATE_DIR/port" "$VMSTATE_DIR/gdb"''}
      }
      trap cleanup EXIT

      EXTRA_CMDLINE="${qemuConfig.cmdlineExtra}"
      CONSOLE_TERM_CMDLINE=""

      ${lib.optionalString (arch == "x86_64") ''
        if ! . ${consoleTermHelper}; then
          echo "[ERROR] failed to load the console TERM resolver"
          exit 1
        fi
        if ! dragonos_resolve_console_term stdio; then
          exit 1
        fi
        if [ -n "$DRAGONOS_RESOLVED_CONSOLE_TERM" ]; then
          CONSOLE_TERM_CMDLINE="TERM=$DRAGONOS_RESOLVED_CONSOLE_TERM"
          echo "[INFO] Guest console TERM=$DRAGONOS_RESOLVED_CONSOLE_TERM (source=$DRAGONOS_RESOLVED_CONSOLE_TERM_SOURCE)"
        else
          echo "[INFO] Guest console TERM uses the current init fallback"
        fi
      ''}

      # AUTO_TEST 可在运行时通过环境变量覆盖；测试目录由 nix 构建的 rootfs 布局决定
      AUTO_TEST="''${AUTO_TEST:-${testOpt.autotest}}"
      TEST_FIXTURE_CMDLINE=""
      if [ "$AUTO_TEST" = "dunit" ]; then
        TEST_FIXTURE_CMDLINE="dragonos.net_test_fixtures"
      fi
      FINAL_CMDLINE="rw init=${initProgram} AUTO_TEST=$AUTO_TEST SYSCALL_TEST_DIR=${testOpt.syscall.testDir} DUNITEST_DIR=${testOpt.dunitest.testDir} $TEST_FIXTURE_CMDLINE $EXTRA_CMDLINE $CONSOLE_TERM_CMDLINE"

      ARCH_FLAGS=( ${lib.escapeShellArgs commonArchArgs} )
      ${archSpecificBash}

      BOOT_ARGS=( "-kernel" "${kernelPath}" "-append" "$FINAL_CMDLINE" )
      DISK_ARGS=( ${lib.escapeShellArgs diskArgs} )
      NET_ARGS=( "-netdev" "user,id=hostnet0,hostfwd=tcp::$HOST_PORT-:12580" "-device" "virtio-net-pci,vectors=5,netdev=hostnet0,id=net0" )
      RNG_ARGS=( "-device" "${if arch == "riscv64" then "virtio-rng-device" else "virtio-rng-pci"}" )
      FIRMWARE_ARGS=( ${lib.optionalString (qemuFirmware != null) "-L ${qemuFirmware}"} )
      GDB_ARGS=()
      ${lib.optionalString debug ''
        GDB_ARGS=( "-gdb" "tcp::$GDB_PORT" )
        if [ "''${QEMU_GDB_WAIT:-0}" = "1" ]; then
          GDB_ARGS+=( "-S" )
        fi
      ''}

      echo "================== DragonOS QEMU Command Preview =================="
      echo "Binary: ${qemuBin}"
      echo "Base Flags: ${qemuFlagsStr}"
      echo "Mem Args: ''${MEM_ARGS[*]}"
      echo "Arch Flags: ''${ARCH_FLAGS[*]}"
      echo "Boot Args: ''${BOOT_ARGS[*]}"
      echo "Disk Args: ''${DISK_ARGS[*]}"
      echo "Net Args: ''${NET_ARGS[*]}"
      echo "Rng Args: ''${RNG_ARGS[*]}"
      echo "Vsock Args: ''${VSOCK_ARGS[*]}"
      echo "Gdb Args: ''${GDB_ARGS[*]}"
      echo "Host Port: $HOST_PORT"
      echo "=================================================================="
      echo ""

      ${qemuBin} --version

      # 不对脚本本身 exec，保证退出后 trap 能清理共享内存与 vmstate；
      # pid 文件记录的是 QEMU 进程本身（测试监控脚本会用它终止 QEMU）
      ${pkgs.runtimeShell} -c 'pidfile="$1"; shift; [ -n "$pidfile" ] && echo $$ > "$pidfile"; exec "$@"' \
        bash "${lib.optionalString hasVmstateDir "$VMSTATE_DIR/pid"}" \
        ${qemuBin} ${qemuFlagsStr} "''${MEM_ARGS[@]}" "''${NET_ARGS[@]}" "''${RNG_ARGS[@]}" "''${FIRMWARE_ARGS[@]}" "''${ARCH_FLAGS[@]}" "''${BOOT_ARGS[@]}" "''${DISK_ARGS[@]}" "''${VSOCK_ARGS[@]}" "''${GDB_ARGS[@]}" "$@"
    '';

  script = lib.genAttrs [ "x86_64" "riscv64" ] (
    arch:
    mkRunScript {
      name = "dragonos-run";
      inherit arch;
      isNographic = if arch == "riscv64" then true else baseConfig.nographic;
      qemuBin =
        if preferSystemQemu then "qemu-system-${arch}" else "${pkgs.qemu_kvm}/bin/qemu-system-${arch}";
    }
  );
in
script
