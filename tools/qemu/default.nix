{ 
  lib, 
  pkgs,
  rootfsDisk,
  kernel,
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
        # "-L" "${pkgs.qemu}/share/qemu"
        "-m" baseConfig.memory
        # FIXED: 补全 smp 参数，和原脚本一致
        "-smp" "${baseConfig.cores},cores=${baseConfig.cores},threads=1,sockets=1"
        "-object" "memory-backend-file,size=${baseConfig.memory},id=${baseConfig.shmId},mem-path=/dev/shm/${baseConfig.shmId},share=on"
        "-netdev" "user,id=hostnet0,hostfwd=tcp::12580-:12580"
        "-device" "virtio-net-pci,vectors=5,netdev=hostnet0,id=net0"
        "-usb"
        "-device" "qemu-xhci,id=xhci,p2=8,p3=4"
        "-D" "qemu.log"
        
        # FIXED: 新增缺失的 Boot Order
        "-boot" "order=d"
        # FIXED: 新增缺失的 GDB Stub
        "-s"
        # FIXED: 新增缺失的 RTC 设置
        "-rtc" "clock=host,base=localtime"
        # FIXED: 新增缺失的 Trace 事件 (注意 * 号在 Nix 字符串里是安全的，escapeShellArgs 会处理它)
        "-d" "cpu_reset,guest_errors,trace:virtio*,trace:e1000e_rx*,trace:e1000e_tx*,trace:e1000e_irq*"
      ];
      nographicArgs = lib.optionals isNographic ([
        "-nographic"
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

      DRAGONOS_LOGLEVEL=''${DRAGONOS_LOGLEVEL:-4}
      EXTRA_CMDLINE="${qemuConfig.cmdlineExtra}"
      
      # FIXED: 补全缺失的默认内核参数 AUTO_TEST 和 SYSCALL_TEST_DIR
      FINAL_CMDLINE="init=${initProgram} loglevel=$DRAGONOS_LOGLEVEL AUTO_TEST=none SYSCALL_TEST_DIR=/opt/tests/gvisor $EXTRA_CMDLINE"

      # --- 1. 生成动态参数 ---
      ${archSpecificArgs}

      # --- 2. 命令预览 ---
      GREEN='\033[0;32m'
      NC='\033[0m'
      BOLD='\033[1m'

      echo -e "''${GREEN}================== DragonOS QEMU Command Preview ==================''${NC}"
      echo -e "''${BOLD}Binary:''${NC} sudo ${qemuBin}"
      echo -e "''${BOLD}Base Flags:''${NC} ${qemuFlagsStr}"
      echo -e "''${BOLD}Arch Flags:''${NC} ''${ARCH_FLAGS[*]}"
      echo -e "''${BOLD}Boot Args:''${NC} ''${BOOT_ARGS[*]}"
      echo -e "''${BOLD}Disk Args:''${NC} ''${DISK_ARGS[*]}"
      echo -e "''${GREEN}==================================================================''${NC}"
      echo ""

      # --- 3. 执行 ---
      ${qemuBin} --version
      exec sudo ${qemuBin} ${qemuFlagsStr} "''${ARCH_FLAGS[@]}" "''${BOOT_ARGS[@]}" "''${DISK_ARGS[@]}" "$@"
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