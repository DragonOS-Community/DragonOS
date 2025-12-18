{ 
  lib, 
  pkgs,
  rootfsDisk,
  kernel,
  syscallTestDir,
  autotest
}:

let
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
      
      # FIXED: 这里的逻辑彻底重写，以匹配原脚本的行为
      archSpecificArgs = if arch == "x86_64" then ''
        MACHINE_TYPE_FLAGS=( "-machine" "q35,memory-backend=${baseConfig.shmId}" )
        
        # 保持之前的 CPU 定义
        CPU_FLAGS=( "-cpu" "IvyBridge,apic,x2apic,+fpu,check,+vmx," )
        
        # 保持之前的 KVM 加速定义
        if [ "$ACCEL" == "kvm" ]; then
            ACCEL_FLAGS=( "-machine" "accel=kvm" "-enable-kvm" )
        else
            ACCEL_FLAGS=( "-machine" "accel=tcg" )
        fi

        ARCH_FLAGS=( "''${MACHINE_TYPE_FLAGS[@]}" "''${CPU_FLAGS[@]}" "''${ACCEL_FLAGS[@]}" )
        
        BOOT_ARGS=( "-kernel" "${kernel}/kernel.elf" "-append" "$FINAL_CMDLINE" )
        
        DISK_ARGS=( "-device" "virtio-blk-pci,drive=disk" "-device" "pci-bridge,chassis_nr=1,id=pci.1" "-device" "pcie-root-port" "-drive" "id=disk,file=${rootfsDisk},if=none" )
      '' else ''
        ARCH_FLAGS=( "-cpu" "sifive-u54" "-machine" "virt,accel=$ACCEL,memory-backend=${baseConfig.shmId}" )
        BOOT_ARGS=( "-kernel" "${riscv-uboot}/u-boot.bin" "-append" "$FINAL_CMDLINE" )
        DISK_ARGS=( "-device" "virtio-blk-device,drive=disk" "-drive" "id=disk,file=${rootfsDisk},if=none" )
      '';

      initProgram = if arch == "riscv64" then "/bin/riscv_rust_init" else "/bin/busybox init";

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

      ${archSpecificArgs}

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
      sudo ${qemuBin} ${qemuFlagsStr} "''${ARCH_FLAGS[@]}" "''${BOOT_ARGS[@]}" "''${DISK_ARGS[@]}" "$@"
    '';

  # 5. QEMU 二进制选择器 (支持外部 QEMU)
  # 用法: QEMU_X86=/usr/bin/qemu-system-x86_64 nix run --impure .#x86_64
  getQemuBin = arch: fallback:
    let
      envVar = if arch == "x86_64" then "QEMU_X86" else "QEMU_RISCV";
      externalPath = builtins.getEnv envVar;
    in
      if externalPath != "" then externalPath else fallback;
  
  script = {
    x86_64 = mkRunScript {
      name = "run-dragonos-x86";
      arch = "x86_64";
      isNographic = baseConfig.nographic;
      qemuBin = getQemuBin "x86_64" "${pkgs.qemu_full}/bin/qemu-system-x86_64";
    };

    riscv64 = mkRunScript {
      name = "run-dragonos-riscv";
      arch = "riscv64";
      isNographic = true;
      qemuBin = getQemuBin "riscv64" "${pkgs.qemu_full}/bin/qemu-system-riscv64";
    };
  };
in script