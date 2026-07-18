#!/bin/bash
#
# DragonOS QEMU启动脚本
#
# 环境变量支持:
# - DRAGONOS_LOGLEVEL: 设置内核日志级别 (0-7)
#   0: EMERG   1: ALERT   2: CRIT   3: ERR
#   4: WARN    5: NOTICE  6: INFO   7: DEBUG
#   示例: export DRAGONOS_LOGLEVEL=4  # 只显示WARN及以上级别的日志
# - DRAGONOS_QEMU_ACCEL: 覆盖 QEMU 加速器选择，可选 kvm/tcg/hvf
#   示例: DRAGONOS_QEMU_ACCEL=tcg make run-nographic
#
# - AUTO_TEST: 自动测试选项
# - SYSCALL_TEST_DIR: 系统调用测试目录
# - DUNITEST_PATTERN: dunitest runner pattern filter
# - DRAGONOS_QEMU_SMP: 覆盖 vCPU 拓扑，例如 1,cores=1,threads=1,sockets=1
# - DRAGONOS_QEMU_SERIAL_SOCKET: nographic 模式下使用独立 Unix socket 承载 guest virtconsole
# - DRAGONOS_QEMU_ARGV_FILE: 以 JSON 数组记录实际执行的 QEMU argv（目标必须不存在）
# - DRAGONOS_QEMU_DISK_IMAGE: 覆盖主磁盘镜像路径
# - DRAGONOS_QEMU_SNAPSHOT: 设为1时以 QEMU snapshot 模式运行，避免修改基线镜像
# - DRAGONOS_QEMU_TRACE: 覆盖传给 QEMU -d 的日志项；性能测试应设为 none
# - DRAGONOS_VIRTIOFS_ENABLE: 是否启用 virtiofs（1启用，默认0）
# - DRAGONOS_VIRTIOFS_SOCKET: virtiofsd socket路径
# - DRAGONOS_VIRTIOFS_TAG: virtiofs 挂载tag
# - DRAGONOS_VIRTIOFS_ENV_FILE: virtiofs配置文件路径（默认 ${ROOT_PATH}/tools/virtiofs/env.sh）
# - DRAGONOS_VIRTIOFS_QUEUE_SIZE: (optional) queue-size passed to vhost-user-fs-pci
# - DRAGONOS_VIRTIOFS_NUM_REQUEST_QUEUES: (optional) num-request-queues passed to vhost-user-fs-pci
# - DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE: DAX cache aperture in bytes; requires a preflight stamp
# - DRAGONOS_VIRTIOFS_DAX_REQUIRED: fail rather than silently boot without a DAX cache
#

check_dependencies()
{
    # Check the selected QEMU, including a DAX-patched binary from env.sh.
    if [ -z "${QEMU}" ] || [ ! -x "${QEMU}" ]; then
      if [ "$ARCH" == "loongarch64" ]; then
        echo -e "\nPlease install qemu-system-loongarch64 first!"
        echo -e "\nYou can install it by running:  (if you are using ubuntu)"
        echo -e "    ${ROOT_PATH}/tools/qemu/build-qemu-la64-for-ubuntu.sh"
        echo -e ""
        exit 1
      else
        echo "Please install qemu-system-${ARCH} or configure an executable DRAGONOS_VIRTIOFS_QEMU_BIN"
        exit 1
      fi
    fi

    # Check if brctl is installed
    if [ -z "$(which brctl)" ]; then
        echo "Please install bridge-utils first!"
        exit 1
    fi

    # Check if dnsmasq is installed
    if [ -z "$(which dnsmasq)" ]; then
        echo "Please install dnsmasq first!"
        exit 1
    fi

    # Check if iptable is installed
    if [ -z "$(which iptables)" ]; then
        echo "Please install iptables first!"
        exit 1
    fi

}


# 进行启动前检查
flag_can_run=1
ARGS=`getopt -o p -l bios:,display: -- "$@"`
eval set -- "${ARGS}"
echo "$@"
allflags= 
# allflags=$(qemu-system-x86_64 -cpu help | awk '/flags/ {y=1; getline}; y {print}' | tr ' ' '\n' | grep -Ev "^$" | sed -r 's|^|+|' | tr '\n' ',' | sed -r "s|,$||")
# 设置ARCH环境变量，如果没有设置，就默认为x86_64
export ARCH=${ARCH:=x86_64}
echo "ARCH=${ARCH}"


#ARCH="i386"
# 请根据自己的需要，在-d 后方加入所需的 trace 事件

# 标准的trace events
qemu_trace_std=cpu_reset,guest_errors,trace:virtio*,trace:e1000e_rx*,trace:e1000e_tx*,trace:e1000e_irq*
# 调试usb的trace
qemu_trace_usb=trace:usb_xhci_reset,trace:usb_xhci_run,trace:usb_xhci_stop,trace:usb_xhci_irq_msi,trace:usb_xhci_irq_msix,trace:usb_xhci_port_reset,trace:msix_write_config,trace:usb_xhci_irq_msix,trace:usb_xhci_irq_msix_use,trace:usb_xhci_irq_msix_unuse,trace:usb_xhci_irq_msi,trace:usb_xhci_*

# 根据架构设置qemu的加速方式
if [ ${ARCH} == "i386" ] || [ ${ARCH} == "x86_64" ]; then
  qemu_accel="kvm"
  if [ $(uname) == Darwin ]; then
    qemu_accel=hvf
  else
    # 判断系统kvm模块是否加载
    if [ ! -e /dev/kvm ]; then
      # kvm模块未加载，使用tcg加速
      qemu_accel="tcg"
    fi
  fi
fi

if [ -n "${DRAGONOS_QEMU_ACCEL:-}" ]; then
  case "${DRAGONOS_QEMU_ACCEL}" in
    kvm|tcg|hvf)
      qemu_accel="${DRAGONOS_QEMU_ACCEL}"
      ;;
    *)
      echo "[错误] 不支持的 DRAGONOS_QEMU_ACCEL=${DRAGONOS_QEMU_ACCEL}，可选值: kvm/tcg/hvf"
      exit 1
      ;;
  esac
fi
echo "QEMU accel=${qemu_accel}"

# uboot版本
UBOOT_VERSION="v2023.10"
RISCV64_UBOOT_PATH="arch/riscv64/u-boot-${UBOOT_VERSION}-riscv64"


DISK_NAME="disk-image-${ARCH}.img"
EXT4_DISK_NAME="ext4.img"
FAT_DISK_NAME="fat.img"

QEMU=$(which qemu-system-${ARCH})
QEMU_DISK_IMAGE="${DRAGONOS_QEMU_DISK_IMAGE:-../bin/${DISK_NAME}}"
QEMU_EXT4_DISK_IMAGE="../bin/${EXT4_DISK_NAME}"
QEMU_FAT_DISK_IMAGE="../bin/${FAT_DISK_NAME}"
QEMU_MEMORY="2G"
PMEM_IMAGE_PATH="${PMEM_IMAGE_PATH:-}"
PMEM_SIZE="${PMEM_SIZE:-}"
QEMU_ENABLE_PMEM="${QEMU_ENABLE_PMEM:-false}"
QEMU_NVDIMM_SLOTS="${QEMU_NVDIMM_SLOTS:-4}"
QEMU_NVDIMM_MAXMEM="${QEMU_NVDIMM_MAXMEM:-4G}"
PMEM_ENABLED=false
PMEM_QEMU_ARGS=()
DRAGONOS_VIRTIOFS_DAX_QEMU_OVERRIDE=${DRAGONOS_VIRTIOFS_QEMU_BIN:-}
DRAGONOS_VIRTIOFS_DAX_CACHE_OVERRIDE=${DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE:-}
DRAGONOS_VIRTIOFS_DAX_STAMP_OVERRIDE=${DRAGONOS_VIRTIOFS_DAX_PREFLIGHT_STAMP:-}
DRAGONOS_VIRTIOFS_DAX_REQUIRED_OVERRIDE=${DRAGONOS_VIRTIOFS_DAX_REQUIRED:-}
DRAGONOS_VIRTIOFS_BACKEND_ATTESTATION_OVERRIDE=${DRAGONOS_VIRTIOFS_BACKEND_ATTESTATION:-}
DRAGONOS_VIRTIOFS_ENABLE=${DRAGONOS_VIRTIOFS_ENABLE:=0}
DRAGONOS_VIRTIOFS_SOCKET=${DRAGONOS_VIRTIOFS_SOCKET:=/tmp/dragonos-virtiofsd.sock}
DRAGONOS_VIRTIOFS_TAG=${DRAGONOS_VIRTIOFS_TAG:=hostshare}
DRAGONOS_VIRTIOFS_ENV_FILE=${DRAGONOS_VIRTIOFS_ENV_FILE:=}
DRAGONOS_VIRTIOFS_QUEUE_SIZE=${DRAGONOS_VIRTIOFS_QUEUE_SIZE:=}
DRAGONOS_VIRTIOFS_NUM_REQUEST_QUEUES=${DRAGONOS_VIRTIOFS_NUM_REQUEST_QUEUES:=}
DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE=${DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE:=}
DRAGONOS_VIRTIOFS_DAX_REQUIRED=${DRAGONOS_VIRTIOFS_DAX_REQUIRED:=0}
DRAGONOS_VIRTIOFS_DAX_PREFLIGHT_STAMP=${DRAGONOS_VIRTIOFS_DAX_PREFLIGHT_STAMP:=}

# 检查必要的环境变量
if [ -z "${ROOT_PATH}" ]; then
    echo "[错误] ROOT_PATH 环境变量未设置"
    echo "[错误] 请通过 Makefile 运行本脚本 (make qemu, make run 等)"
    exit 1
fi
PMEM_IMAGE_PATH="${PMEM_IMAGE_PATH:-${ROOT_PATH}/bin/pmem.img}"

# Load virtiofs configuration before dependency and device-model checks so a
# DAX-patched QEMU selected by env.sh is the binary validated and launched.
if [ "${DRAGONOS_VIRTIOFS_ENABLE}" == "1" ]; then
    if [ -z "${DRAGONOS_VIRTIOFS_ENV_FILE}" ]; then
        DRAGONOS_VIRTIOFS_ENV_FILE="${ROOT_PATH}/tools/virtiofs/env.sh"
    fi
    if [ -f "${DRAGONOS_VIRTIOFS_ENV_FILE}" ]; then
        # shellcheck source=/dev/null
        . "${DRAGONOS_VIRTIOFS_ENV_FILE}"
        [ -z "${DRAGONOS_VIRTIOFS_DAX_QEMU_OVERRIDE}" ] || \
            DRAGONOS_VIRTIOFS_QEMU_BIN="${DRAGONOS_VIRTIOFS_DAX_QEMU_OVERRIDE}"
        [ -z "${DRAGONOS_VIRTIOFS_DAX_CACHE_OVERRIDE}" ] || \
            DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE="${DRAGONOS_VIRTIOFS_DAX_CACHE_OVERRIDE}"
        [ -z "${DRAGONOS_VIRTIOFS_DAX_STAMP_OVERRIDE}" ] || \
            DRAGONOS_VIRTIOFS_DAX_PREFLIGHT_STAMP="${DRAGONOS_VIRTIOFS_DAX_STAMP_OVERRIDE}"
        [ -z "${DRAGONOS_VIRTIOFS_DAX_REQUIRED_OVERRIDE}" ] || \
            DRAGONOS_VIRTIOFS_DAX_REQUIRED="${DRAGONOS_VIRTIOFS_DAX_REQUIRED_OVERRIDE}"
        [ -z "${DRAGONOS_VIRTIOFS_BACKEND_ATTESTATION_OVERRIDE}" ] || \
            DRAGONOS_VIRTIOFS_BACKEND_ATTESTATION="${DRAGONOS_VIRTIOFS_BACKEND_ATTESTATION_OVERRIDE}"
        if [ "${DRAGONOS_VIRTIOFS_SOCKET}" = "/tmp/dragonos-virtiofsd.sock" ] && [ -n "${SOCKET_PATH:-}" ]; then
            DRAGONOS_VIRTIOFS_SOCKET="${SOCKET_PATH}"
        fi
        if [ "${DRAGONOS_VIRTIOFS_TAG}" = "hostshare" ] && [ -n "${VIRTIOFS_TAG:-}" ]; then
            DRAGONOS_VIRTIOFS_TAG="${VIRTIOFS_TAG}"
        fi
    fi
    if [ -n "${DRAGONOS_VIRTIOFS_QEMU_BIN:-}" ]; then
        if [ ! -x "${DRAGONOS_VIRTIOFS_QEMU_BIN}" ]; then
            echo "[ERROR] DRAGONOS_VIRTIOFS_QEMU_BIN is not executable"
            exit 1
        fi
        QEMU="${DRAGONOS_VIRTIOFS_QEMU_BIN}"
    fi
fi

# 状态文件目录（优先使用环境变量，否则使用默认值）
VMSTATE_DIR="${VMSTATE_DIR:-${ROOT_PATH}/bin/vmstate}"
mkdir -p "${VMSTATE_DIR}"

is_truthy() {
    case "${1}" in
        1|y|Y|yes|YES|true|TRUE|on|ON) return 0 ;;
        *) return 1 ;;
    esac
}

detect_file_size_bytes() {
    if size=$(stat -c%s "$1" 2>/dev/null); then
        echo "${size}"
        return 0
    fi
    if size=$(stat -f%z "$1" 2>/dev/null); then
        echo "${size}"
        return 0
    fi
    return 1
}

setup_pmem_support() {
    PMEM_ENABLED=false
    PMEM_QEMU_ARGS=()

    if ! is_truthy "${QEMU_ENABLE_PMEM}"; then
        return 0
    fi

    if [ "${ARCH}" != "x86_64" ]; then
        echo "[QEMU] 错误: PMEM/NVDIMM 目前仅支持 ARCH=x86_64 (当前: ${ARCH})"
        exit 1
    fi

    if [ ! -f "${PMEM_IMAGE_PATH}" ]; then
        echo "[QEMU] 错误: PMEM镜像不存在: ${PMEM_IMAGE_PATH}"
        echo "[QEMU] 请先准备只读ext4镜像文件，再启动QEMU。"
        exit 1
    fi

    if [ ! -r "${PMEM_IMAGE_PATH}" ]; then
        echo "[QEMU] 错误: PMEM镜像不可读: ${PMEM_IMAGE_PATH}"
        exit 1
    fi

    case "${QEMU_NVDIMM_SLOTS}" in
        ''|*[!0-9]*|0)
            echo "[QEMU] 错误: QEMU_NVDIMM_SLOTS 必须是正整数 (当前: ${QEMU_NVDIMM_SLOTS})"
            exit 1
            ;;
    esac

    if [ -z "${QEMU_NVDIMM_MAXMEM}" ]; then
        echo "[QEMU] 错误: QEMU_NVDIMM_MAXMEM 不能为空"
        exit 1
    fi

    if [ -z "${PMEM_SIZE}" ]; then
        PMEM_SIZE=$(detect_file_size_bytes "${PMEM_IMAGE_PATH}") || {
            echo "[QEMU] 错误: 无法自动获取PMEM镜像大小，请显式设置 PMEM_SIZE"
            exit 1
        }
    fi

    PMEM_ENABLED=true
    PMEM_QEMU_ARGS=(
        -object "memory-backend-file,id=pmem0,mem-path=${PMEM_IMAGE_PATH},size=${PMEM_SIZE},share=on,readonly=on"
        -device "nvdimm,id=nv0,memdev=pmem0,unarmed=on"
    )
    echo "[QEMU] PMEM已启用: image=${PMEM_IMAGE_PATH}, size=${PMEM_SIZE}, slots=${QEMU_NVDIMM_SLOTS}, maxmem=${QEMU_NVDIMM_MAXMEM}"
}

setup_pmem_support

# 分配可用端口的函数
find_free_port() {
    local start_port=$1
    local port=$start_port
    while netstat -tuln 2>/dev/null | grep -q ":${port} " || \
          ss -tuln 2>/dev/null | grep -q ":${port} "; do
        port=$((port + 1))
    done
    echo $port
}

# vsock CID 注册表（用于避免并发启动时分配重复CID）
VSOCK_CID_REGISTRY="/tmp/dragonos-vsock-cid-registry"
VSOCK_CID_LOCKDIR="/tmp/dragonos-vsock-cid-lock"

acquire_vsock_lock() {
    local retries=200
    local i=0
    while ! mkdir "${VSOCK_CID_LOCKDIR}" 2>/dev/null; do
        i=$((i + 1))
        if [ "${i}" -ge "${retries}" ]; then
            echo "[WARN] failed to acquire vsock CID lock; skip vsock device"
            return 1
        fi
        sleep 0.05
    done
    return 0
}

release_vsock_lock() {
    rmdir "${VSOCK_CID_LOCKDIR}" 2>/dev/null || true
}

cleanup_stale_vsock_registry() {
    local tmp_file
    tmp_file=$(mktemp)
    if [ -f "${VSOCK_CID_REGISTRY}" ]; then
        while IFS=' ' read -r cid owner_pid owner_vmstate; do
            if [ -z "${cid}" ] || [ -z "${owner_pid}" ]; then
                continue
            fi
            if kill -0 "${owner_pid}" 2>/dev/null; then
                printf '%s %s %s\n' "${cid}" "${owner_pid}" "${owner_vmstate}" >> "${tmp_file}"
            fi
        done < "${VSOCK_CID_REGISTRY}"
    fi
    mv "${tmp_file}" "${VSOCK_CID_REGISTRY}"
}

is_vsock_cid_in_registry() {
    local cid="$1"
    grep -q "^${cid} " "${VSOCK_CID_REGISTRY}" 2>/dev/null
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

    if [ -z "${requested}" ] || [ "${requested}" = "random" ]; then
        while [ "${i}" -lt "${attempts}" ]; do
            chosen=$(generate_random_vsock_cid)
            if [ "${chosen}" != "2" ] && ! is_vsock_cid_in_registry "${chosen}"; then
                break
            fi
            i=$((i + 1))
        done
        if [ "${i}" -ge "${attempts}" ]; then
            release_vsock_lock
            echo "[WARN] failed to allocate unique random vsock CID; skip vsock device"
            return 1
        fi
    else
        if ! [[ "${requested}" =~ ^[0-9]+$ ]]; then
            release_vsock_lock
            echo "[WARN] invalid QEMU_VSOCK_GUEST_CID='${requested}'; skip vsock device"
            return 1
        fi
        if [ "${requested}" -le 2 ]; then
            release_vsock_lock
            echo "[WARN] guest CID must be > 2; skip vhost-vsock-pci"
            return 1
        fi
        if is_vsock_cid_in_registry "${requested}"; then
            release_vsock_lock
            echo "[WARN] guest CID=${requested} already in use by another DragonOS instance; skip vhost-vsock-pci"
            return 1
        fi
        chosen="${requested}"
    fi

    QEMU_VSOCK_GUEST_CID="${chosen}"
    printf '%s %s %s\n' "${QEMU_VSOCK_GUEST_CID}" "$$" "${VMSTATE_DIR}" >> "${VSOCK_CID_REGISTRY}"
    release_vsock_lock
    return 0
}

# 先分配网络端口
HOST_PORT=$(find_free_port 12580)
# GDB端口从网络端口的下一位开始搜索，确保不重复
GDB_PORT=$(find_free_port $((HOST_PORT + 1)))

# 写入状态文件
echo "${HOST_PORT}" > "${VMSTATE_DIR}/port"
echo "${GDB_PORT}" > "${VMSTATE_DIR}/gdb"

QEMU_SMP="${DRAGONOS_QEMU_SMP:-2,cores=2,threads=1,sockets=1}"
if ! [[ "${QEMU_SMP}" =~ ^[0-9]+(,(cores|threads|sockets|maxcpus)=[0-9]+)*$ ]]; then
  echo "[错误] DRAGONOS_QEMU_SMP 格式无效: ${QEMU_SMP}"
  exit 1
fi
IFS=',' read -r -a qemu_smp_fields <<< "${QEMU_SMP}"
qemu_smp_cpus="${qemu_smp_fields[0]}"
if [ "${qemu_smp_cpus}" -lt 1 ] || [ "${qemu_smp_cpus}" -gt 64 ]; then
  echo "[错误] DRAGONOS_QEMU_SMP 的 vCPU 数必须位于 1..64: ${QEMU_SMP}"
  exit 1
fi
declare -A qemu_smp_values=()
for qemu_smp_field in "${qemu_smp_fields[@]:1}"; do
  qemu_smp_key="${qemu_smp_field%%=*}"
  qemu_smp_value="${qemu_smp_field#*=}"
  if [ -n "${qemu_smp_values[${qemu_smp_key}]+set}" ] || \
     [ "${qemu_smp_value}" -lt 1 ] || [ "${qemu_smp_value}" -gt 64 ]; then
    echo "[错误] DRAGONOS_QEMU_SMP 含重复键或越界值: ${QEMU_SMP}"
    exit 1
  fi
  qemu_smp_values["${qemu_smp_key}"]="${qemu_smp_value}"
done
if [ -n "${qemu_smp_values[maxcpus]+set}" ] && \
   { [ "${qemu_smp_values[maxcpus]}" -lt "${qemu_smp_cpus}" ] || \
     [ "${qemu_smp_values[maxcpus]}" -gt 64 ]; }; then
  echo "[错误] DRAGONOS_QEMU_SMP 的 maxcpus 必须不小于当前 vCPU 数且不超过64: ${QEMU_SMP}"
  exit 1
fi
if [ -n "${qemu_smp_values[cores]+set}" ] && \
   [ -n "${qemu_smp_values[threads]+set}" ] && \
   [ -n "${qemu_smp_values[sockets]+set}" ]; then
  qemu_smp_product=$((qemu_smp_values[cores] * qemu_smp_values[threads] * qemu_smp_values[sockets]))
  qemu_smp_expected_product="${qemu_smp_values[maxcpus]:-${qemu_smp_cpus}}"
  if [ "${qemu_smp_product}" -ne "${qemu_smp_expected_product}" ]; then
    echo "[错误] DRAGONOS_QEMU_SMP 的 cores*threads*sockets 必须等于 maxcpus（未指定时等于 vCPU 数）: ${QEMU_SMP}"
    exit 1
  fi
fi
QEMU_MONITOR_ARGS=(-monitor stdio)
QEMU_TRACE="${DRAGONOS_QEMU_TRACE:-${qemu_trace_std}}"
QEMU_CPU_FEATURES=""
QEMU_RTC_CLOCK=""
QEMU_SERIAL_LOG_FILE="../serial_opt.txt"
QEMU_SERIAL_ARGS=(-serial "file:${QEMU_SERIAL_LOG_FILE}")
QEMU_CONSOLE_CHARDEV_ID="mux"
if [ "${DRAGONOS_QEMU_SNAPSHOT:-0}" != "0" ] && [ "${DRAGONOS_QEMU_SNAPSHOT:-0}" != "1" ]; then
  echo "[错误] DRAGONOS_QEMU_SNAPSHOT 只能是0或1"
  exit 1
fi
QEMU_DRIVE="id=disk,file=${QEMU_DISK_IMAGE},if=none,format=raw"
if [ "${DRAGONOS_QEMU_SNAPSHOT:-0}" = "1" ]; then
  QEMU_DRIVE+=",snapshot=on"
fi
QEMU_DRIVE_ARGS=(-drive "${QEMU_DRIVE}")
QEMU_ACCEL_ARGS=()
QEMU_DEVICE_ARGS=()
QEMU_DISPLAY_ARGS=()
QEMU_OBJECT_ARGS=()
QEMU_NUMA_ARGS=()
QEMU_CHARDEV_ARGS=()
QEMU_ARGS=()

# vsock 配置：
# - QEMU_ENABLE_VSOCK=1: 默认启用，条件不满足时自动降级跳过
# - QEMU_VSOCK_GUEST_CID: guest CID；默认 random（可显式指定 >2 的数字）
QEMU_ENABLE_VSOCK=1
QEMU_VSOCK_GUEST_CID=${QEMU_VSOCK_GUEST_CID:=random}
QEMU_ATTACH_VSOCK=0
# 推荐 non-transitional 模型，PCI device id 对应 0x1053 (VSOCK)。
QEMU_VSOCK_DEVICE_MODEL="vhost-vsock-pci-non-transitional"
# GDB调试支持：
# - QEMU_GDB_WAIT=1: QEMU 启动后立即暂停CPU（等同 -S），等待 GDB/monitor 手动继续
# - QEMU_GDB_WAIT=0: 默认不暂停
QEMU_GDB_WAIT=0

if [ -f "${QEMU_EXT4_DISK_IMAGE}" ]; then
  QEMU_DRIVE_ARGS+=(-drive "id=ext4disk,file=${QEMU_EXT4_DISK_IMAGE},if=none,format=raw")
fi
if [ -f "${QEMU_FAT_DISK_IMAGE}" ]; then
  QEMU_DRIVE_ARGS+=(-drive "id=fatdisk,file=${QEMU_FAT_DISK_IMAGE},if=none,format=raw")
fi

check_dependencies

# 设置无图形界面模式
QEMU_NOGRAPHIC=false

KERNEL_CMDLINE=" rw "

# 自动测试选项，支持的选项：
# - none: 不进行自动测试
# - syscall: 进行gvisor系统调用测试
# - dunit: 进行dunitest测试
AUTO_TEST=${AUTO_TEST:=none}
# gvisor测试目录
SYSCALL_TEST_DIR=${SYSCALL_TEST_DIR:=/opt/tests/gvisor}
# dunitest测试目录
DUNITEST_DIR=${DUNITEST_DIR:=/opt/tests/dunitest}
# dunitest pattern过滤条件
DUNITEST_PATTERN=${DUNITEST_PATTERN:=}

BIOS_TYPE=""
#这个变量为true则使用virtio磁盘
VIRTIO_BLK_DEVICE=true

# 如果qemu_accel不为空
if [ -n "${qemu_accel}" ]; then
  if [ "${qemu_accel}" == "kvm" ]; then
    QEMU_ACCEL_ARGS+=(-enable-kvm)
  fi
fi

if [ ${ARCH} == "i386" ] || [ ${ARCH} == "x86_64" ]; then
    qemu_machine="q35"
    if [ -n "${qemu_accel}" ]; then
        qemu_machine+=",accel=${qemu_accel}"
    fi
    # KVM加速时禁用HPET，使用kvm-clock（性能更好且延迟更低）
    if [ "${qemu_accel}" == "kvm" ]; then
        qemu_machine+=",hpet=off"
    fi
    if [ "${PMEM_ENABLED}" == "true" ]; then
        qemu_machine+=",nvdimm=on"
    fi
    QEMU_MACHINE_ARGS=(-machine "${qemu_machine}")
    # 根据加速方式选择CPU型号：KVM使用host，TCG使用IvyBridge
    cpu_model=$([ "${qemu_accel}" == "kvm" ] && echo "host" || echo "IvyBridge")
    if [ -n "${allflags}" ]; then
      QEMU_CPU_ARGS=(-cpu "${cpu_model},apic,x2apic,+fpu,check,+vmx,${allflags}")
    else
      QEMU_CPU_ARGS=(-cpu "${cpu_model},apic,x2apic,+fpu,check,+vmx")
    fi
    # RTC配置：clock=host 使guest使用host的时钟源，支持kvm-clock
    # base=localtime 设置RTC基准时间为本地时间
    QEMU_RTC_ARGS=(-rtc clock=host,base=localtime)
    if [ ${VIRTIO_BLK_DEVICE} == false ]; then
      QEMU_DEVICE_DISK_ARGS=(-device ahci,id=ahci -device ide-hd,drive=disk,bus=ahci.0)
    else
      QEMU_DEVICE_DISK_ARGS=(-device virtio-blk-pci,drive=disk -device pci-bridge,chassis_nr=1,id=pci.1 -device pcie-root-port)
    fi
    if [ -f "${QEMU_EXT4_DISK_IMAGE}" ]; then
      QEMU_DEVICE_DISK_ARGS+=(-device virtio-blk-pci,drive=ext4disk)
    fi
    if [ -f "${QEMU_FAT_DISK_IMAGE}" ]; then
      QEMU_DEVICE_DISK_ARGS+=(-device virtio-blk-pci,drive=fatdisk)
    fi

    # 默认启用 vsock；若宿主环境不满足条件则降级为跳过该设备。
    if [ "${QEMU_ENABLE_VSOCK}" = "1" ]; then
      if [ "${ARCH}" != "x86_64" ]; then
        echo "[WARN] vsock enabled but unsupported arch (${ARCH}); skip vsock device"
      elif [ ! -e /dev/vhost-vsock ]; then
        echo "[WARN] /dev/vhost-vsock not found; skip vsock device"
        echo "[WARN] Hint: sudo modprobe vhost_vsock"
      elif ! "${QEMU}" -device help 2>/dev/null | grep -q "${QEMU_VSOCK_DEVICE_MODEL}"; then
        echo "[WARN] QEMU device model '${QEMU_VSOCK_DEVICE_MODEL}' not supported; skip vsock device"
      elif ! resolve_vsock_guest_cid "${QEMU_VSOCK_GUEST_CID}"; then
        :
      else
        QEMU_ATTACH_VSOCK=1
      fi
    else
      echo "[INFO] vsock disabled by script config (QEMU_ENABLE_VSOCK=0)"
    fi

elif [ ${ARCH} == "riscv64" ]; then
    QEMU_MACHINE_ARGS=(-machine virt)
    QEMU_CPU_ARGS=(-cpu sifive-u54)
    QEMU_RTC_ARGS=()
    QEMU_DEVICE_DISK_ARGS=(-device virtio-blk-device,drive=disk)
elif [ ${ARCH} == "loongarch64" ]; then
    QEMU_MACHINE_ARGS=(-machine virt)
    QEMU_CPU_ARGS=()
    QEMU_RTC_ARGS=()
    QEMU_DEVICE_DISK_ARGS=(-device virtio-blk-pci,drive=disk -device pci-bridge,chassis_nr=1,id=pci.1 -device pcie-root-port)
else
    echo "Unsupported architecture: ${ARCH}"
    exit 1
fi

if [ ${ARCH} == "riscv64" ]; then
# 如果是riscv64架构，就不需要图形界面
    QEMU_NOGRAPHIC=true
fi

while true;do
    case "$1" in
        --bios)
        case "$2" in
              uefi) #uefi启动新增ovmf.fd固件
              BIOS_TYPE=uefi
            ;;
              legacy)
              BIOS_TYPE=legacy
              ;;
        esac;shift 2;;
        --display)
        case "$2" in
              vnc)
              QEMU_DISPLAY_ARGS=(-display vnc=:00)
              ;;
              window)
              ;;
              nographic)
              QEMU_NOGRAPHIC=true

              ;;
        esac;shift 2;;
        *) break
      esac
  done

setup_kernel_init_program() {
    if [ ${ARCH} == "x86_64" ]; then
        KERNEL_CMDLINE+=" init=/bin/busybox init AUTO_TEST=${AUTO_TEST} SYSCALL_TEST_DIR=${SYSCALL_TEST_DIR} DUNITEST_DIR=${DUNITEST_DIR} DUNITEST_PATTERN=${DUNITEST_PATTERN} "
        # KERNEL_CMDLINE+=" init=/bin/dragonreach "
    elif [ ${ARCH} == "riscv64" ]; then
        KERNEL_CMDLINE+=" init=/bin/riscv_rust_init "
    fi
}

# 检测环境变量并设置内核命令行参数
setup_kernel_cmdline_from_env() {
    # 检测 DRAGONOS_LOGLEVEL 环境变量
    # 设置内核日志级别，支持0-7:
    # 0: EMERG   1: ALERT   2: CRIT   3: ERR
    # 4: WARN    5: NOTICE  6: INFO   7: DEBUG
    if [ -n "${DRAGONOS_LOGLEVEL}" ]; then
        KERNEL_CMDLINE+=" loglevel=${DRAGONOS_LOGLEVEL} "
        echo "[INFO] Setting kernel loglevel to ${DRAGONOS_LOGLEVEL} from environment variable"
    fi

    # 检测其他环境变量可以在这里添加
    # 例如：
    # if [ -n "${DRAGONOS_DEBUG}" ]; then
    #     KERNEL_CMDLINE+=" debug "
    # fi
}

# 设置内核init程序
setup_kernel_init_program

# 从环境变量设置内核命令行参数
setup_kernel_cmdline_from_env

if [ "${DRAGONOS_VIRTIOFS_ENABLE}" == "1" ]; then
    if [ "${ARCH}" != "x86_64" ]; then
        echo "[错误] virtiofs临时运行支持当前仅实现x86_64"
        exit 1
    fi

    if [ ! -S "${DRAGONOS_VIRTIOFS_SOCKET}" ]; then
        echo "[错误] 未检测到virtiofsd socket: ${DRAGONOS_VIRTIOFS_SOCKET}"
        echo "[提示] 请先在另一个终端启动: tools/virtiofs/start_virtiofsd.sh"
        exit 1
    fi

    virtiofs_device_opts="vhost-user-fs-pci,chardev=char_virtiofs,tag=${DRAGONOS_VIRTIOFS_TAG}"
    if is_truthy "${DRAGONOS_VIRTIOFS_DAX_REQUIRED}" && [ -z "${DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE}" ]; then
        echo "[ERROR] DAX is required but DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE is unset"
        exit 1
    fi
    if [ -n "${DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE}" ]; then
        if ! [[ "${DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE}" =~ ^[0-9]+$ ]] || \
           [ "${DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE}" -lt 2097152 ] || \
           [ $((DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE & (DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE - 1))) -ne 0 ]; then
            echo "[ERROR] DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE must be a power-of-two byte count >= 2 MiB"
            exit 1
        fi

        if [ -z "${DRAGONOS_VIRTIOFS_DAX_PREFLIGHT_STAMP}" ]; then
            DRAGONOS_VIRTIOFS_DAX_PREFLIGHT_STAMP="${RUNTIME_DIR:-${ROOT_PATH}/bin/virtiofs-runtime}/dax-preflight.stamp"
        fi
        if [ ! -r "${DRAGONOS_VIRTIOFS_DAX_PREFLIGHT_STAMP}" ]; then
            echo "[ERROR] missing DAX preflight stamp: ${DRAGONOS_VIRTIOFS_DAX_PREFLIGHT_STAMP}"
            echo "[HINT] run make virtiofs-dax-preflight with the same env.sh"
            exit 1
        fi

        stamp_qemu_sha="$(awk -F= '$1 == "QEMU_SHA256" { print $2 }' "${DRAGONOS_VIRTIOFS_DAX_PREFLIGHT_STAMP}")"
        stamp_virtiofsd_sha="$(awk -F= '$1 == "VIRTIOFSD_SHA256" { print $2 }' "${DRAGONOS_VIRTIOFS_DAX_PREFLIGHT_STAMP}")"
        stamp_config_sha="$(awk -F= '$1 == "CONFIG_SHA256" { print $2 }' "${DRAGONOS_VIRTIOFS_DAX_PREFLIGHT_STAMP}")"
        stamp_cache_size="$(awk -F= '$1 == "CACHE_SIZE" { print $2 }' "${DRAGONOS_VIRTIOFS_DAX_PREFLIGHT_STAMP}")"
        stamp_correctness_profile="$(awk -F= '$1 == "CORRECTNESS_PROFILE" { print $2 }' "${DRAGONOS_VIRTIOFS_DAX_PREFLIGHT_STAMP}")"
        if [ "${DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE}" -le 8589934592 ]; then
            current_correctness_profile=1
        else
            current_correctness_profile=0
        fi
        # shellcheck source=virtiofs/common.sh
        . "${ROOT_PATH}/tools/virtiofs/common.sh"
        current_virtiofsd="$(virtiofs_detect_daemon || true)"
        if [ -z "${current_virtiofsd}" ]; then
            echo "[ERROR] unable to identify the configured virtiofsd binary"
            exit 1
        fi
        current_qemu_sha="$(sha256sum "${QEMU}" | awk '{print $1}')"
        current_virtiofsd_sha="$(sha256sum "${current_virtiofsd}" | awk '{print $1}')"
        current_config_sha="$(printf '%s\0%s\0%s\0' "${VIRTIOFSD_CACHE:-}" \
          "${VIRTIOFSD_EXTRA_ARGS:-}" \
          "cache-size=${DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE},modern-pio-notify=off" | \
          sha256sum | awk '{print $1}')"
        if [ "${stamp_cache_size}" != "${DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE}" ] || \
           [ "${stamp_qemu_sha}" != "${current_qemu_sha}" ] || \
           [ "${stamp_virtiofsd_sha}" != "${current_virtiofsd_sha}" ] || \
           [ "${stamp_config_sha}" != "${current_config_sha}" ] || \
           [ "${stamp_correctness_profile}" != "${current_correctness_profile}" ]; then
            echo "[ERROR] DAX preflight stamp does not match the current binaries and configuration"
            exit 1
        fi
        if is_truthy "${DRAGONOS_VIRTIOFS_DAX_REQUIRED}" && [ "${current_correctness_profile}" != "1" ]; then
            echo "[ERROR] DAX required correctness runs support at most 4096 cache ranges (8 GiB)"
            exit 1
        fi

        backend_attestation="${DRAGONOS_VIRTIOFS_BACKEND_ATTESTATION:-${RUNTIME_DIR:-${ROOT_PATH}/bin/virtiofs-runtime}/virtiofsd.attestation}"
        if [ ! -e "${backend_attestation}" ]; then
            echo "[ERROR] missing live virtiofsd attestation: ${backend_attestation}"
            echo "[HINT] restart tools/virtiofs/start_virtiofsd.sh with the same env.sh"
            exit 1
        fi
        virtiofs_build_daemon_command "${current_virtiofsd}" "${DRAGONOS_VIRTIOFS_SOCKET}" \
          "${HOST_SHARE_DIR}" "${VIRTIOFSD_CACHE:-auto}" "${VIRTIOFSD_EXTRA_ARGS:-}"
        live_command_sha="$(virtiofs_command_sha256)"
        if ! sudo bash "${ROOT_PATH}/tools/virtiofs/verify_backend_attestation.sh" \
          "${backend_attestation}" "${DRAGONOS_VIRTIOFS_SOCKET}" \
          "${current_virtiofsd_sha}" "${live_command_sha}"; then
            echo "[ERROR] live virtiofsd process/socket does not match the DAX configuration"
            exit 1
        fi
        virtiofs_device_opts="${virtiofs_device_opts},cache-size=${DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE},modern-pio-notify=off"
    fi
    if [ -n "${DRAGONOS_VIRTIOFS_QUEUE_SIZE}" ]; then
        if ! [[ "${DRAGONOS_VIRTIOFS_QUEUE_SIZE}" =~ ^[0-9]+$ ]] || [ "${DRAGONOS_VIRTIOFS_QUEUE_SIZE}" -eq 0 ]; then
            echo "[ERROR] DRAGONOS_VIRTIOFS_QUEUE_SIZE must be a positive integer"
            exit 1
        fi
        virtiofs_device_opts="${virtiofs_device_opts},queue-size=${DRAGONOS_VIRTIOFS_QUEUE_SIZE}"
    fi
    if [ -n "${DRAGONOS_VIRTIOFS_NUM_REQUEST_QUEUES}" ]; then
        if ! [[ "${DRAGONOS_VIRTIOFS_NUM_REQUEST_QUEUES}" =~ ^[0-9]+$ ]] || [ "${DRAGONOS_VIRTIOFS_NUM_REQUEST_QUEUES}" -eq 0 ]; then
            echo "[ERROR] DRAGONOS_VIRTIOFS_NUM_REQUEST_QUEUES must be a positive integer"
            exit 1
        fi
        if [ "${DRAGONOS_VIRTIOFS_NUM_REQUEST_QUEUES}" -gt 64 ]; then
            echo "[ERROR] DRAGONOS_VIRTIOFS_NUM_REQUEST_QUEUES cannot exceed 64"
            exit 1
        fi
        virtiofs_device_opts="${virtiofs_device_opts},num-request-queues=${DRAGONOS_VIRTIOFS_NUM_REQUEST_QUEUES}"
    fi

    echo "[INFO] Enabling virtiofs: tag=${DRAGONOS_VIRTIOFS_TAG}, socket=${DRAGONOS_VIRTIOFS_SOCKET}, queue_size=${DRAGONOS_VIRTIOFS_QUEUE_SIZE:-qemu-default}, request_queues=${DRAGONOS_VIRTIOFS_NUM_REQUEST_QUEUES:-qemu-default}, dax_cache=${DRAGONOS_VIRTIOFS_DAX_CACHE_SIZE:-disabled}"

    QEMU_OBJECT_ARGS+=(
      -object "memory-backend-memfd,id=mem,size=${QEMU_MEMORY},share=on"
    )
    QEMU_NUMA_ARGS+=(-numa "node,memdev=mem")
    QEMU_CHARDEV_ARGS+=(-chardev "socket,id=char_virtiofs,path=${DRAGONOS_VIRTIOFS_SOCKET}")
    QEMU_DEVICE_ARGS+=(-device "${virtiofs_device_opts}")
fi


if [ ${QEMU_NOGRAPHIC} == true ]; then
    if [ -n "${DRAGONOS_QEMU_SERIAL_SOCKET:-}" ]; then
      qemu_serial_socket_dir="$(dirname "${DRAGONOS_QEMU_SERIAL_SOCKET}")"
      qemu_serial_socket_real_dir="$(realpath -e -- "${qemu_serial_socket_dir}" 2>/dev/null || true)"
      if [[ "${DRAGONOS_QEMU_SERIAL_SOCKET}" != /* ]] || \
         [ ${#DRAGONOS_QEMU_SERIAL_SOCKET} -gt 100 ] || \
         [ ! -d "${qemu_serial_socket_dir}" ] || \
         [ -L "${qemu_serial_socket_dir}" ] || \
         [ "${qemu_serial_socket_real_dir}" != "${qemu_serial_socket_dir}" ] || \
         [ -e "${DRAGONOS_QEMU_SERIAL_SOCKET}" ]; then
        echo "[错误] DRAGONOS_QEMU_SERIAL_SOCKET 必须位于已存在的非符号链接目录，长度不超过100且目标不存在"
        exit 1
      fi
      if [ "$(stat -c '%u' "${qemu_serial_socket_dir}")" -ne "$(id -u)" ] || \
         [ "$(stat -c '%a' "${qemu_serial_socket_dir}")" != "700" ]; then
        echo "[错误] DRAGONOS_QEMU_SERIAL_SOCKET 父目录必须由当前用户拥有且权限严格为0700"
        exit 1
      fi
      QEMU_CONSOLE_CHARDEV_ID="calib_console"
      QEMU_SERIAL_ARGS=(-serial none -monitor none -chardev "socket,id=${QEMU_CONSOLE_CHARDEV_ID},path=${DRAGONOS_QEMU_SERIAL_SOCKET},server=on,wait=off,logfile=${QEMU_SERIAL_LOG_FILE}")
    else
      QEMU_SERIAL_ARGS=(-serial chardev:mux -monitor chardev:mux -chardev "stdio,id=mux,mux=on,signal=off,logfile=${QEMU_SERIAL_LOG_FILE}")
    fi

    # 添加 virtio console 设备
    if [ ${ARCH} == "x86_64" ]; then
      QEMU_DEVICE_ARGS+=(-device virtio-serial -device "virtconsole,chardev=${QEMU_CONSOLE_CHARDEV_ID}")
    elif [ ${ARCH} == "loongarch64" ]; then
      QEMU_DEVICE_ARGS+=(-device virtio-serial -device "virtconsole,chardev=${QEMU_CONSOLE_CHARDEV_ID}")
    elif [ ${ARCH} == "riscv64" ]; then
      QEMU_DEVICE_ARGS+=(-device virtio-serial-device -device "virtconsole,chardev=${QEMU_CONSOLE_CHARDEV_ID}")
    fi

    KERNEL_CMDLINE=" console=/dev/hvc0 ${KERNEL_CMDLINE}"
    QEMU_MONITOR_ARGS=()
    QEMU_ARGS+=(--nographic)

    KERNEL_CMDLINE=$(echo "${KERNEL_CMDLINE}" | sed 's/^[ \t]*//;s/[ \t]*$//')

    if [ ${ARCH} == "x86_64" ]; then
      QEMU_ARGS+=(-kernel ../bin/kernel/kernel.elf -append "${KERNEL_CMDLINE}")
    elif [ ${ARCH} == "loongarch64" ]; then
      QEMU_ARGS+=(-kernel ../bin/kernel/kernel.elf -append "${KERNEL_CMDLINE}")
    elif [ ${ARCH} == "riscv64" ]; then
      QEMU_ARGS+=(-append "${KERNEL_CMDLINE}")
    fi
fi


# ps: 下面这条使用tap的方式，无法dhcp获取到ip，暂时不知道为什么
# QEMU_DEVICES="-device ahci,id=ahci -device ide-hd,drive=disk,bus=ahci.0 -net nic,netdev=nic0 -netdev tap,id=nic0,model=virtio-net-pci,script=qemu/ifup-nat,downscript=qemu/ifdown-nat -usb -device qemu-xhci,id=xhci,p2=8,p3=4 "
QEMU_DEVICE_ARGS+=("${QEMU_DEVICE_DISK_ARGS[@]}")
QEMU_DEVICE_ARGS+=(
  -netdev "user,id=hostnet0,hostfwd=tcp::${HOST_PORT}-:12580"
  -device "virtio-net-pci,vectors=5,netdev=hostnet0,id=net0"
  -usb
  -device "qemu-xhci,id=xhci,p2=8,p3=4"
) 

if [ "${QEMU_ATTACH_VSOCK}" = "1" ]; then
  QEMU_DEVICE_ARGS+=(-device "${QEMU_VSOCK_DEVICE_MODEL},guest-cid=${QEMU_VSOCK_GUEST_CID}")
  echo "[INFO] enable vsock device: ${QEMU_VSOCK_DEVICE_MODEL},guest-cid=${QEMU_VSOCK_GUEST_CID}"
fi

QEMU_DEVICE_ARGS+=("${PMEM_QEMU_ARGS[@]}")
# E1000E
# QEMU_DEVICES="-device ahci,id=ahci -device ide-hd,drive=disk,bus=ahci.0 -netdev user,id=hostnet0,hostfwd=tcp::12580-:12580 -net nic,model=e1000e,netdev=hostnet0,id=net0 -netdev user,id=hostnet1,hostfwd=tcp::12581-:12581 -device virtio-net-pci,vectors=5,netdev=hostnet1,id=net1 -usb -device qemu-xhci,id=xhci,p2=8,p3=4 " 


if [ "${PMEM_ENABLED}" == "true" ]; then
  QEMU_MEMORY_ARG="${QEMU_MEMORY},slots=${QEMU_NVDIMM_SLOTS},maxmem=${QEMU_NVDIMM_MAXMEM}"
else
  QEMU_MEMORY_ARG="${QEMU_MEMORY}"
fi

QEMU_ARGS+=(
  -m "${QEMU_MEMORY_ARG}"
  -smp "${QEMU_SMP}"
  -boot order=d
)
QEMU_ARGS+=("${QEMU_MONITOR_ARGS[@]}")
QEMU_ARGS+=("${QEMU_DISPLAY_ARGS[@]}")
if [ "${QEMU_TRACE}" != "none" ]; then
  QEMU_ARGS+=(-d "${QEMU_TRACE}")
fi

QEMU_ARGS+=(
  "${QEMU_MACHINE_ARGS[@]}"
  "${QEMU_CPU_ARGS[@]}"
  "${QEMU_RTC_ARGS[@]}"
  "${QEMU_OBJECT_ARGS[@]}"
  "${QEMU_NUMA_ARGS[@]}"
  "${QEMU_CHARDEV_ARGS[@]}"
  "${QEMU_SERIAL_ARGS[@]}"
  "${QEMU_DRIVE_ARGS[@]}"
  "${QEMU_DEVICE_ARGS[@]}"
)
QEMU_ARGS+=("${QEMU_ACCEL_ARGS[@]}")

QEMU_ARGS+=(-D ../qemu.log)

# GDB调试支持（默认不暂停CPU；需要暂停请显式设置 QEMU_GDB_WAIT=1）
QEMU_ARGS+=(-gdb "tcp::${GDB_PORT}")
if [ "${QEMU_GDB_WAIT}" == "1" ]; then
  QEMU_ARGS+=(-S)
fi


# 安装riscv64的uboot
install_riscv_uboot()
{

    if [ ! -d ${RISCV64_UBOOT_PATH} ]; then
        echo "正在下载u-boot..."
        uboot_tar_name="u-boot-${UBOOT_VERSION}-riscv64.tar.xz"
        
        uboot_parent_path=$(dirname ${RISCV64_UBOOT_PATH}) || (echo "获取riscv u-boot 版本 ${UBOOT_VERSION} 的父目录失败" && exit 1)

        if [ ! -f ${uboot_tar_name} ]; then
            wget https://mirrors.dragonos.org.cn/pub/third_party/u-boot/${uboot_tar_name} || (echo "下载riscv u-boot 版本 ${UBOOT_VERSION} 失败" && exit 1)
        fi
        echo "下载完成"
        echo "正在解压u-boot到 '$uboot_parent_path'..."
        mkdir -p $uboot_parent_path
        tar xvf u-boot-${UBOOT_VERSION}-riscv64.tar.xz -C ${uboot_parent_path} || (echo "解压riscv u-boot 版本 ${UBOOT_VERSION} 失败" && exit 1)
        echo "解压完成"
        rm -rf u-boot-${UBOOT_VERSION}-riscv64.tar.xz
    fi
    echo "riscv u-boot 版本 ${UBOOT_VERSION} 已经安装"
} 


if [ $flag_can_run -eq 1 ]; then

  # 清理旧的状态文件
  rm -f "${VMSTATE_DIR}/pid"
  rm -f "${VMSTATE_DIR}/vsock_cid"

  if [ "${QEMU_ATTACH_VSOCK}" = "1" ]; then
    echo "${QEMU_VSOCK_GUEST_CID}" > "${VMSTATE_DIR}/vsock_cid"
  fi

  # 启动QEMU的函数
  launch_qemu() {
    local -a bios_args=()
    if [ $# -gt 0 ]; then
      bios_args=("$@")
    fi
    echo "[QEMU] 启动中... (网络端口: ${HOST_PORT}, GDB端口: ${GDB_PORT})"
    if [ "${QEMU_GDB_WAIT}" == "1" ]; then
      echo "[QEMU] 等待GDB连接... (使用 'make gdb' 连接)"
    fi
    local -a cmd=("${QEMU}" "${bios_args[@]}" "${QEMU_ARGS[@]}")
    if [ -n "${DRAGONOS_QEMU_ARGV_FILE:-}" ]; then
      if [[ "${DRAGONOS_QEMU_ARGV_FILE}" != /* ]] || \
         [ ! -d "$(dirname "${DRAGONOS_QEMU_ARGV_FILE}")" ] || \
         [ -e "${DRAGONOS_QEMU_ARGV_FILE}" ]; then
        echo "[错误] DRAGONOS_QEMU_ARGV_FILE 必须是父目录已存在且目标不存在的绝对路径"
        return 1
      fi
      python3 - "${DRAGONOS_QEMU_ARGV_FILE}" "${cmd[@]}" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
with path.open("x", encoding="utf-8") as output:
    json.dump(sys.argv[2:], output, ensure_ascii=True, indent=2)
    output.write("\n")
PY
    fi
    printf '[QEMU] 执行: sudo ' >&2
    printf '%q ' "${cmd[@]}" >&2
    printf '\n' >&2
    sudo bash -c 'pidfile="$1"; shift; echo $$ > "$pidfile"; exec "$@"' bash "${VMSTATE_DIR}/pid" "${cmd[@]}"
  }

  if [ ${BIOS_TYPE} == uefi ] ;then
    if [ ${ARCH} == x86_64 ] ;then
      launch_qemu -bios arch/x86_64/efi/OVMF-pure-efi.fd
    elif [ ${ARCH} == i386 ] ;then
      launch_qemu -bios arch/i386/efi/OVMF-pure-efi.fd
    elif [ ${ARCH} == riscv64 ] ;then
      install_riscv_uboot
      launch_qemu -kernel "${RISCV64_UBOOT_PATH}/u-boot.bin"
    else
      echo "不支持的架构: ${ARCH}"
    fi
  else
    # 如果是i386架构或者x86_64架构，就直接启动
    if [ ${ARCH} == x86_64 ] || [ ${ARCH} == i386 ] ;then
      launch_qemu
    elif [ ${ARCH} == riscv64 ] ;then
      # 如果是riscv64架构，就与efi启动一样
      install_riscv_uboot
      launch_qemu -kernel "${RISCV64_UBOOT_PATH}/u-boot.bin"
    elif [ ${ARCH} == loongarch64 ] ;then
      launch_qemu
    else
      echo "不支持的架构: ${ARCH}"
    fi
  fi
else
  echo "不满足运行条件"
fi
