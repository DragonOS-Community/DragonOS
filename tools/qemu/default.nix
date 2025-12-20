{
  lib,
  pkgs,
  diskPath,
  kernel,
  syscallTestDir,
  autotest
}:

let
  qemuFirmware = pkgs.callPackage ./qemu-firmware.nix {};

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
  mkQemuArgs = { arch, isNographic }:
    let
      baseArgs = [
        "-m" baseConfig.memory
        "-smp" "${baseConfig.cores},cores=${baseConfig.cores},threads=1,sockets=1"
        "-object" "memory-backend-file,size=${baseConfig.memory},id=${baseConfig.shmId},mem-path=/dev/shm/${baseConfig.shmId},share=on"
        "-netdev" "user,id=hostnet0,hostfwd=tcp::12580-:12580"
        "-device" "virtio-net-pci,vectors=5,netdev=hostnet0,id=net0"
        "-usb"
        "-device" "qemu-xhci,id=xhci,p2=8,p3=4"
        "-D" "qemu.log"

        # Boot Order
        "-boot" "order=d"
        # GDB Stub
        "-s"

        "-rtc" "clock=host,base=localtime"
        # Trace events
        "-d" "cpu_reset,guest_errors,trace:virtio*,trace:e1000e_rx*,trace:e1000e_tx*,trace:e1000e_irq*"
        "-trace" "fw_cfg*"
      ];
      nographicArgs = lib.optionals isNographic ([
        "--nographic"
        "-serial" "chardev:mux"
        "-monitor" "chardev:mux"
        "-chardev" "stdio,id=mux,mux=on,signal=off,logfile=serial_opt.txt"
      ] ++ (if arch == "riscv64" then [
        "-device" "virtio-serial-device" "-device" "virtconsole,chardev=mux"
      ] else [
        "-device" "virtio-serial" "-device" "virtconsole,chardev=mux"
      ]));
      kernelCmdlinePart = if isNographic then "console=/dev/hvc0" else "";
    in {
      flags = baseArgs ++ nographicArgs;
      cmdlineExtra = kernelCmdlinePart;
    };

  # 4. 运行脚本生成器
  mkRunScript = { name, arch, isNographic, qemuBin }:
    let
      qemuConfig = mkQemuArgs { inherit arch isNographic; };
      qemuFlagsStr = lib.escapeShellArgs qemuConfig.flags;

      initProgram = if arch == "riscv64" then "/bin/riscv_rust_init" else "/bin/busybox init";

      # Define static parts of arguments using Nix lists
      commonArchArgs = if arch == "x86_64" then [
        "-machine" "q35,memory-backend=${baseConfig.shmId}"
        "-cpu" "IvyBridge,apic,x2apic,+fpu,check,+vmx,"
      ] else [
        "-cpu" "sifive-u54"
      ];

      kernelPath = if arch == "x86_64" then kernel else "${riscv-uboot}/u-boot.bin";

      diskArgs = if arch == "x86_64" then [
        "-device" "virtio-blk-pci,drive=disk"
        "-device" "pci-bridge,chassis_nr=1,id=pci.1"
        "-device" "pcie-root-port"
        "-drive" "id=disk,file=${diskPath},if=none"
      ] else [
        "-device" "virtio-blk-device,drive=disk"
        "-drive" "id=disk,file=${diskPath},if=none"
      ];

      # Generate bash code for dynamic parts
      archSpecificBash = if arch == "x86_64" then ''
        if [ "$ACCEL" == "kvm" ]; then
            ARCH_FLAGS+=( "-machine" "accel=kvm" "-enable-kvm" )
        else
            ARCH_FLAGS+=( "-machine" "accel=tcg" )
        fi
      '' else ''
        ARCH_FLAGS+=( "-machine" "virt,accel=$ACCEL,memory-backend=${baseConfig.shmId}" )
      '';

    in pkgs.writeScriptBin name ''
      #!${pkgs.runtimeShell}

      if [ ! -d "bin" ]; then echo "Error: Please run from project root (bin/ missing)."; exit 1; fi

      ACCEL="tcg"
      if [ -e /dev/kvm ] && [ -w /dev/kvm ]; then ACCEL="kvm"; fi

      cleanup() { sudo rm -f /dev/shm/${baseConfig.shmId}; }
      trap cleanup EXIT
      # FIXED: 既然用了 sudo 运行 qemu，这里创建 shm 也需要权限，
      # 但实际上 qemu 会自己创建，这里只需要保证清理。
      # 原脚本是 rm -rf ... -> qemu -> rm -rf ...

      EXTRA_CMDLINE="${qemuConfig.cmdlineExtra}"

      # FIXED: 补全缺失的默认内核参数 AUTO_TEST 和 SYSCALL_TEST_DIR
      FINAL_CMDLINE="init=${initProgram} AUTO_TEST=${autotest} SYSCALL_TEST_DIR=${syscallTestDir} $EXTRA_CMDLINE"

      ARCH_FLAGS=( ${lib.escapeShellArgs commonArchArgs} )
      ${archSpecificBash}

      BOOT_ARGS=( "-kernel" "${kernelPath}" "-append" "$FINAL_CMDLINE" )

      DISK_ARGS=( ${lib.escapeShellArgs diskArgs} )

      echo -e "================== DragonOS QEMU Command Preview =================="
      echo -e "Binary: sudo ${qemuBin}"
      echo -e "Base Flags: ${qemuFlagsStr}"
      echo -e "Arch Flags: ''${ARCH_FLAGS[*]}"
      echo -e "Boot Args: ''${BOOT_ARGS[*]}"
      echo -e "Disk Args: ''${DISK_ARGS[*]}"
      echo -e "=================================================================="
      echo ""

      # --- 3. 执行 ---
      ${qemuBin} --version
      sudo ${qemuBin} ${qemuFlagsStr} -L ${qemuFirmware} "''${ARCH_FLAGS[@]}" "''${BOOT_ARGS[@]}" "''${DISK_ARGS[@]}" "$@"
    '';

  script = lib.genAttrs [ "x86_64" "riscv64" ] (arch: mkRunScript {
    name = "run-dragonos";
    inherit arch;
    isNographic = if arch == "riscv64" then true else baseConfig.nographic;
    qemuBin = "${pkgs.qemu_kvm}/bin/qemu-system-${arch}";
  });
in script
