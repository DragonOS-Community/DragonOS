{
  lib,
  pkgs,
  diskPath,
  kernel,
  testOpt,
  debug ? false,
  enableVsock ? true,
  vsockGuestCid ? "random",
  vsockDeviceModel ? "vhost-vsock-pci-non-transitional",
  vmstateDir ? null,
  preferSystemQemu ? false,
}:

let
  qemuFirmware = if preferSystemQemu then null else pkgs.callPackage ./qemu-firmware.nix { };

  baseConfig = {
    nographic = true;
    memory = "512M";
    cores = "2";
    shmId = "dragonos-qemu-shm.ram";
  };

  riscv-uboot = pkgs.pkgsCross.riscv64-embedded.buildUBoot {
    defconfig = "qemu-riscv64_smode_defconfig";
    extraMeta.platforms = [ "riscv64-linux" ];
    filesToInstall = [ "u-boot.bin" ];
  };

  # 3. 参数生成器 (Nix List -> Nix List)
  # 注意：网络配置中的端口现在使用 $HOST_PORT 变量，在运行时动态替换
  mkQemuArgs =
    { arch, isNographic }:
    let
      baseArgs = [
        "-m"
        baseConfig.memory
        "-smp"
        "${baseConfig.cores},cores=${baseConfig.cores},threads=1,sockets=1"
        "-object"
        "memory-backend-file,size=${baseConfig.memory},id=${baseConfig.shmId},mem-path=/dev/shm/${baseConfig.shmId},share=on"
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
        # Trace events
        "-d"
        "cpu_reset,guest_errors,trace:virtio*,trace:e1000e_rx*,trace:e1000e_tx*,trace:e1000e_irq*"
        "-trace"
        "fw_cfg*"
      ]
      ++ lib.optionals debug [
        # GDB Stub
        "-s"
        "-S"
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
      flags = baseArgs ++ nographicArgs;
      cmdlineExtra = kernelCmdlinePart;
    };

  # 4. 运行脚本生成器
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

      # Define static parts of arguments using Nix lists
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
            "id=disk,file=${diskPath},if=none"
          ]
        else
          [
            "-device"
            "virtio-blk-device,drive=disk"
            "-drive"
            "id=disk,file=${diskPath},if=none"
          ];

      # Generate bash code for dynamic parts
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

      # VM 状态目录配置
      vmstateDirStr = if vmstateDir != null then vmstateDir else "";
      hasVmstateDir = vmstateDir != null;
      preferSystemQemuStr = if preferSystemQemu then "true" else "false";
      enableVsockStr = if enableVsock then "true" else "false";
      vsockGuestCidStr = builtins.toString vsockGuestCid;
      vsockDeviceModelStr = vsockDeviceModel;

    in
    pkgs.writeScriptBin name ''
      #!${pkgs.runtimeShell}

      if [ ! -d "bin" ]; then echo "Error: Please run from project root (bin/ missing)."; exit 1; fi

      if [ "${preferSystemQemuStr}" = "true" ]; then
        if ! command -v "${qemuBin}" >/dev/null 2>&1; then
          echo "Error: 系统未找到 ${qemuBin}，请安装 QEMU 或改用 nix 提供的 start 目标。"
          exit 1
        fi
      fi

      # 端口查找函数：从指定端口开始查找可用端口
      find_available_port() {
        local start_port=$1
        local port=$start_port
        while [ $port -lt 65535 ]; do
          if ! ${pkgs.iproute2}/bin/ss -tuln | grep -q ":$port "; then
            echo $port
            return 0
          fi
          port=$((port + 1))
        done
        echo $start_port
      }

      # 动态分配端口
      HOST_PORT=$(find_available_port 12580)

      ACCEL="tcg"
      if [ -e /dev/kvm ] && [ -w /dev/kvm ]; then ACCEL="kvm"; fi

      VSOCK_CID_REGISTRY="/tmp/dragonos-vsock-cid-registry"
      VSOCK_CID_LOCKDIR="/tmp/dragonos-vsock-cid-lock"

      acquire_vsock_lock() {
        local retries=200
        local i=0
        while ! mkdir "$VSOCK_CID_LOCKDIR" 2>/dev/null; do
          i=$((i + 1))
          if [ "$i" -ge "$retries" ]; then
            echo "[WARN] failed to acquire vsock CID lock; skip vsock device"
            return 1
          fi
          sleep 0.05
        done
        return 0
      }

      release_vsock_lock() {
        rmdir "$VSOCK_CID_LOCKDIR" 2>/dev/null || true
      }

      cleanup_stale_vsock_registry() {
        local tmp_file
        tmp_file=$(mktemp)
        if [ -f "$VSOCK_CID_REGISTRY" ]; then
          while IFS=' ' read -r cid owner_pid owner_vmstate; do
            if [ -z "$cid" ] || [ -z "$owner_pid" ]; then
              continue
            fi
            if kill -0 "$owner_pid" 2>/dev/null; then
              printf '%s %s %s\n' "$cid" "$owner_pid" "$owner_vmstate" >> "$tmp_file"
            fi
          done < "$VSOCK_CID_REGISTRY"
        fi
        mv "$tmp_file" "$VSOCK_CID_REGISTRY"
      }

      is_vsock_cid_in_registry() {
        local cid="$1"
        grep -q "^$cid " "$VSOCK_CID_REGISTRY" 2>/dev/null
      }

      generate_random_vsock_cid() {
        # 有效CID范围: [3, 2147483647]
        echo $(( (RANDOM << 16 | RANDOM) % 2147483645 + 3 ))
      }

      resolve_vsock_guest_cid() {
        local requested="$1"
        local chosen=""
        local attempts=128
        local i=0

        if ! acquire_vsock_lock; then
          return 1
        fi

        cleanup_stale_vsock_registry

        if [ -z "$requested" ] || [ "$requested" = "random" ]; then
          while [ "$i" -lt "$attempts" ]; do
            chosen=$(generate_random_vsock_cid)
            if [ "$chosen" != "2" ] && ! is_vsock_cid_in_registry "$chosen"; then
              break
            fi
            i=$((i + 1))
          done
          if [ "$i" -ge "$attempts" ]; then
            release_vsock_lock
            echo "[WARN] failed to allocate unique random vsock CID; skip vsock device"
            return 1
          fi
        else
          if ! [[ "$requested" =~ ^[0-9]+$ ]]; then
            release_vsock_lock
            echo "[WARN] invalid vsockGuestCid='$requested'; skip vsock device"
            return 1
          fi
          if [ "$requested" -le 2 ]; then
            release_vsock_lock
            echo "[WARN] vsock guest CID must be > 2 (host CID=2); skip"
            return 1
          fi
          if is_vsock_cid_in_registry "$requested"; then
            release_vsock_lock
            echo "[WARN] vsock guest CID=$requested already in use by another DragonOS instance; skip"
            return 1
          fi
          chosen="$requested"
        fi

        VSOCK_GUEST_CID="$chosen"
        printf '%s %s %s\n' "$VSOCK_GUEST_CID" "$$" "$VMSTATE_DIR" >> "$VSOCK_CID_REGISTRY"
        release_vsock_lock
        return 0
      }

      VSOCK_ARGS=()
      VSOCK_GUEST_CID=""
      # 默认启用 vsock；若条件不满足则自动降级为跳过该设备。
      if [ "${enableVsockStr}" = "true" ]; then
        if [ "${arch}" != "x86_64" ]; then
          echo "[WARN] vsock enabled but unsupported arch (${arch}); skip"
        elif [ ! -e /dev/vhost-vsock ]; then
          echo "[WARN] /dev/vhost-vsock not found; skip vsock device"
          echo "[WARN] Hint: sudo modprobe vhost_vsock"
        elif ! ${qemuBin} -device help 2>/dev/null | grep -q "${vsockDeviceModelStr}"; then
          echo "[WARN] QEMU device model '${vsockDeviceModelStr}' not supported; skip vsock device"
        elif ! resolve_vsock_guest_cid "${vsockGuestCidStr}"; then
          :
        else
          VSOCK_ARGS=( "-device" "${vsockDeviceModelStr},guest-cid=$VSOCK_GUEST_CID" )
          echo "[INFO] enable vsock device: ${vsockDeviceModelStr},guest-cid=$VSOCK_GUEST_CID"
        fi
      else
        echo "[INFO] vsock disabled by nix config (enableVsock=false)"
      fi

      ${
        if hasVmstateDir then
          ''
            VMSTATE_DIR="${vmstateDirStr}"
            mkdir -p "$VMSTATE_DIR"
            rm -f "$VMSTATE_DIR/pid" "$VMSTATE_DIR/vsock_cid"
            echo "$HOST_PORT" > "$VMSTATE_DIR/port"
            if [ -n "$VSOCK_GUEST_CID" ]; then
              echo "$VSOCK_GUEST_CID" > "$VMSTATE_DIR/vsock_cid"
            fi
          ''
        else
          ""
      }

      cleanup() {
        sudo rm -f /dev/shm/${baseConfig.shmId}
        ${if hasVmstateDir then ''rm -f "$VMSTATE_DIR/pid" "$VMSTATE_DIR/vsock_cid"'' else ""}
      }
      trap cleanup EXIT
      # FIXED: 既然用了 sudo 运行 qemu，这里创建 shm 也需要权限，
      # 但实际上 qemu 会自己创建，这里只需要保证清理。
      # 原脚本是 rm -rf ... -> qemu -> rm -rf ...

      EXTRA_CMDLINE="${qemuConfig.cmdlineExtra}"

      # FIXED: 补全缺失的默认内核参数 AUTO_TEST 和 SYSCALL_TEST_DIR
      FINAL_CMDLINE="init=${initProgram} AUTO_TEST=${testOpt.autotest} SYSCALL_TEST_DIR=${testOpt.syscall.testDir} $EXTRA_CMDLINE"

      ARCH_FLAGS=( ${lib.escapeShellArgs commonArchArgs} )
      ${archSpecificBash}

      BOOT_ARGS=( "-kernel" "${kernelPath}" "-append" "$FINAL_CMDLINE" )

      DISK_ARGS=( ${lib.escapeShellArgs diskArgs} )

      # 动态网络配置（使用动态分配的端口）
      NET_ARGS=( "-netdev" "user,id=hostnet0,hostfwd=tcp::$HOST_PORT-:12580" "-device" "virtio-net-pci,vectors=5,netdev=hostnet0,id=net0" )

      echo -e "================== DragonOS QEMU Command Preview =================="
      echo -e "Binary: sudo ${qemuBin}"
      echo -e "Base Flags: ${qemuFlagsStr}"
      echo -e "Arch Flags: ''${ARCH_FLAGS[*]}"
      echo -e "Boot Args: ''${BOOT_ARGS[*]}"
      echo -e "Disk Args: ''${DISK_ARGS[*]}"
      echo -e "Net Args: ''${NET_ARGS[*]}"
      echo -e "Vsock Args: ''${VSOCK_ARGS[*]}"
      echo -e "Host Port: $HOST_PORT"
      echo -e "=================================================================="
      echo ""

      # --- 3. 执行 ---
      ${qemuBin} --version

      # 使用 exec 方式启动 QEMU，保持交互能力并记录 PID
      # 参考 tools/run-qemu.sh 的 launch_qemu 函数实现
      ${
        if hasVmstateDir then
          ''
            sudo bash -c 'pidfile="$1"; shift; echo $$ > "$pidfile"; exec "$@"' bash "$VMSTATE_DIR/pid" ${qemuBin} ${qemuFlagsStr} "''${NET_ARGS[@]}" ${
              if qemuFirmware != null then "-L ${qemuFirmware}" else ""
            } "''${ARCH_FLAGS[@]}" "''${BOOT_ARGS[@]}" "''${DISK_ARGS[@]}" "''${VSOCK_ARGS[@]}" "$@"
          ''
        else
          ''
            sudo ${qemuBin} ${qemuFlagsStr} "''${NET_ARGS[@]}" ${
              if qemuFirmware != null then "-L ${qemuFirmware}" else ""
            } "''${ARCH_FLAGS[@]}" "''${BOOT_ARGS[@]}" "''${DISK_ARGS[@]}" "''${VSOCK_ARGS[@]}" "$@"
          ''
      }
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
