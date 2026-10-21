// SPDX-License-Identifier: GPL-2.0
// eBPF capability smoke test for DragonOS.
//
// Exercises the eBPF surface that agentrace (eBPF-based agent observability)
// depends on, using only raw syscalls (no libbpf/aya), so failures map
// 1:1 onto kernel capabilities:
//
//   T1  map ops            : HASH / ARRAY / PERCPU_ARRAY / LRU_HASH
//   T2  prog load          : minimal prog + map-fd relocation + helper call
//   T3  tracepoint         : attach syscalls/sys_enter_openat, count hits
//   T4  uprobe             : attach to own binary's target fn, capture rdi arg
//   T5  perf event output  : PERF_EVENT_ARRAY + mmap ring sample read-back
//
// Build: x86_64-linux-musl-gcc -static -no-pie -O2

#define _GNU_SOURCE
#include <elf.h>
#include <sys/auxv.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <sys/wait.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

/* ---------------- uapi (subset of linux/bpf.h + perf_event.h) ------------- */

#define BPF_MAP_CREATE 0
#define BPF_MAP_LOOKUP_ELEM 1
#define BPF_MAP_UPDATE_ELEM 2
#define BPF_MAP_DELETE_ELEM 3
#define BPF_MAP_GET_NEXT_KEY 4

#define BPF_PROG_LOAD 5

#define BPF_MAP_TYPE_HASH 1
#define BPF_MAP_TYPE_ARRAY 2
#define BPF_MAP_TYPE_PERCPU_ARRAY 6
#define BPF_MAP_TYPE_PERF_EVENT_ARRAY 4
#define BPF_MAP_TYPE_LRU_HASH 9

#define BPF_PROG_TYPE_KPROBE 2

#define BPF_ANY 0

union bpf_attr_map {
    struct {
        uint32_t map_type;
        uint32_t key_size;
        uint32_t value_size;
        uint32_t max_entries;
        uint32_t map_flags;
        uint32_t inner_map_fd;
        uint32_t numa_node;
        char padding[112 - 28];
    } map;
    struct {
        uint32_t map_fd;
        uint64_t key;
        uint64_t value;
        uint64_t flags;
        char padding[112 - 28];
    } elem;
    struct {
        uint32_t prog_type;
        uint32_t insn_cnt;
        uint64_t insns;
        uint64_t license;
        uint32_t log_level;
        uint32_t log_size;
        uint64_t log_buf;
        uint32_t kern_version;
        uint32_t prog_flags;
        char padding[112 - 48];
    } prog;
};

static char g_log_buf[1 << 16];

static int bpf_syscall(int cmd, union bpf_attr_map *attr)
{
    return (int)syscall(__NR_bpf, cmd, attr, sizeof(*attr));
}

/* ---------------- eBPF instruction encoding ---------------- */

struct bpf_insn {
    uint8_t code;
    uint8_t dst_reg : 4;
    uint8_t src_reg : 4;
    int16_t off;
    int32_t imm;
};

#define BPF_ALU64_K(OP) (0x07 | (OP)) /* add imm */
#define BPF_MOV64_X 0xbf
#define BPF_MOV64_K 0xb7
#define BPF_ALU64_ADD_K 0x07
#define BPF_LDX_DW 0x79
#define BPF_STX_DW 0x7b
#define BPF_ST_DW 0x7a
#define BPF_JMP_K 0x05
#define BPF_JEQ_K 0x15
#define BPF_JNE_K 0x55
#define BPF_CALL 0x85
#define BPF_EXIT 0x95
#define BPF_LD_IMM64_DW 0x18

#define REG_OFF(name) (__builtin_offsetof(struct regs_layout, name))

/* x86_64 pt_regs order used by DragonOS KProbeContext (== Linux user_pt_regs) */
struct regs_layout {
    uint64_t r15, r14, r13, r12, rbp, rbx;
    uint64_t r11, r10, r9, r8;
    uint64_t rax, rcx, rdx, rsi, rdi;
    uint64_t orig_rax, rip, cs, eflags, rsp, ss;
};

#define PT_REGS_PARM1_OFF REG_OFF(rdi) /* 112 */
#define PT_REGS_RC_OFF REG_OFF(rax)    /* 80 */

/* emit `rD = imm64` as two slots */
#define LD_IMM64(D, IMM)                                                        \
    { .code = BPF_LD_IMM64_DW, .dst_reg = D, .src_reg = 0, .off = 0,            \
      .imm = (int32_t)((uint64_t)(IMM) & 0xffffffff) },                         \
    { .code = 0, .dst_reg = 0, .src_reg = 0, .off = 0,                          \
      .imm = (int32_t)(((uint64_t)(IMM)) >> 32) }

/* emit `r1 = map_fd` pseudo load (src_reg = BPF_PSEUDO_MAP_FD = 1) */
#define LD_MAP_FD(D, FD)                                                        \
    { .code = BPF_LD_IMM64_DW, .dst_reg = D, .src_reg = 1, .off = 0,            \
      .imm = (FD) },                                                            \
    { .code = 0, .dst_reg = 0, .src_reg = 0, .off = 0, .imm = 0 }

/* ---------------- helpers ---------------- */

static int g_failures;
static int g_passes;
static int g_kmsg = -1;

static void report(int ok, const char *name, const char *detail)
{
    /* printk sideband via syslog(2) SYSLOG_ACTION_WRITE: the console log
     * channel stays reliable even when the tty eats userspace output. */
    {
        char line[160];
        int n = snprintf(line, sizeof(line), "[SMOKE] %s %s%s%s",
                         ok ? "PASS" : "FAIL", name, detail ? ": " : "",
                         detail ? detail : "");
        if (n > 0) {
            long rc = syscall(103 /* SYS_syslog */, 5 /* ACTION_WRITE */, line,
                              (size_t)n);
            (void)rc;
        }
    }
    if (ok) {
        g_passes++;
        printf("  [PASS] %s%s%s\n", name, detail ? ": " : "", detail ? detail : "");
    } else {
        g_failures++;
        printf("  [FAIL] %s%s%s (errno=%d %s)\n", name, detail ? ": " : "",
               detail ? detail : "", errno, strerror(errno));
    }
}

static int map_create(uint32_t type, uint32_t key_size, uint32_t value_size,
                      uint32_t max_entries)
{
    union bpf_attr_map attr = {0};
    attr.map.map_type = type;
    attr.map.key_size = key_size;
    attr.map.value_size = value_size;
    attr.map.max_entries = max_entries;
    return bpf_syscall(BPF_MAP_CREATE, &attr);
}

static int map_update(int fd, const void *key, const void *value, uint64_t flags)
{
    union bpf_attr_map attr = {0};
    attr.elem.map_fd = fd;
    attr.elem.key = (uint64_t)(uintptr_t)key;
    attr.elem.value = (uint64_t)(uintptr_t)value;
    attr.elem.flags = flags;
    return bpf_syscall(BPF_MAP_UPDATE_ELEM, &attr);
}

static int map_lookup(int fd, const void *key, void *value)
{
    union bpf_attr_map attr = {0};
    attr.elem.map_fd = fd;
    attr.elem.key = (uint64_t)(uintptr_t)key;
    attr.elem.value = (uint64_t)(uintptr_t)value;
    return bpf_syscall(BPF_MAP_LOOKUP_ELEM, &attr);
}

static int map_delete(int fd, const void *key)
{
    union bpf_attr_map attr = {0};
    attr.elem.map_fd = fd;
    attr.elem.key = (uint64_t)(uintptr_t)key;
    return bpf_syscall(BPF_MAP_DELETE_ELEM, &attr);
}

static int prog_load_kprobe(const struct bpf_insn *insns, size_t cnt)
{
    char license[] = "GPL";
    union bpf_attr_map attr = {0};
    attr.prog.prog_type = BPF_PROG_TYPE_KPROBE;
    attr.prog.insn_cnt = cnt;
    attr.prog.insns = (uint64_t)(uintptr_t)insns;
    attr.prog.license = (uint64_t)(uintptr_t)license;
    attr.prog.log_level = 1;
    attr.prog.log_size = sizeof(g_log_buf);
    attr.prog.log_buf = (uint64_t)(uintptr_t)g_log_buf;
    return bpf_syscall(BPF_PROG_LOAD, &attr);
}

/* ---------------- perf_event uapi subset ---------------- */

struct perf_event_attr_x {
    uint32_t type_;
    uint32_t size;
    uint64_t config;
    union {
        uint64_t sample_period;
        uint64_t sample_freq;
    };
    uint64_t sample_type;
    uint64_t read_format;
    uint64_t bits1; /* flag bits incl. disabled at bit 0, contiguous */
    uint32_t wakeup_events; /* union wakeup_watermark */
    uint32_t bp_type;
    union {
        uint64_t bp_addr;
        uint64_t kprobe_func;
        uint64_t uprobe_path;
        uint64_t config1; /* uprobe path / kprobe symbol ptr */
    };
    union {
        uint64_t bp_len;
        uint64_t kprobe_addr;
        uint64_t probe_offset;
        uint64_t config2; /* probe offset */
    };
    uint64_t branch_sample_type;
    uint64_t sample_regs_user;
    uint32_t sample_stack_user;
    int32_t clockid;
    uint64_t sample_regs_intr;
    uint32_t aux_watermark;
    uint16_t sample_max_stack;
    uint16_t reserved2;
    uint32_t aux_sample_size;
    uint32_t reserved3;
    uint64_t sig_data;
};

#define PERF_IOC_ENABLE _IO('$', 0)
#define PERF_IOC_DISABLE _IO('$', 1)
#define PERF_IOC_SET_BPF _IOW('$', 8, uint32_t)

static int perf_event_open_x(struct perf_event_attr_x *attr, pid_t pid, int cpu,
                             int group_fd, unsigned long flags)
{
    return (int)syscall(__NR_perf_event_open, attr, pid, cpu, group_fd, flags);
}

static int read_file_u64(const char *path, uint64_t *out)
{
    char buf[64];
    int fd = open(path, O_RDONLY);
    if (fd < 0)
        return -1;
    ssize_t n = read(fd, buf, sizeof(buf) - 1);
    close(fd);
    if (n <= 0)
        return -1;
    buf[n] = '\0';
    *out = strtoull(buf, NULL, 0);
    return 0;
}

/* ---------------- T1: maps ---------------- */

static void test_maps(void)
{
    printf("T1: map operations\n");

    /* HASH: update / lookup / delete */
    {
        int fd = map_create(BPF_MAP_TYPE_HASH, 4, 8, 16);
        if (fd < 0) {
            report(0, "BPF_MAP_TYPE_HASH create", NULL);
        } else {
            report(1, "BPF_MAP_TYPE_HASH create", NULL);
            uint32_t key = 42;
            uint64_t val = 0x1122334455667788ULL, got = 0;
            report(map_update(fd, &key, &val, BPF_ANY) == 0, "hash update", NULL);
            report(map_lookup(fd, &key, &got) == 0 && got == val, "hash lookup",
                   NULL);
            report(map_delete(fd, &key) == 0, "hash delete", NULL);
            report(map_lookup(fd, &key, &got) != 0, "hash lookup after delete",
                   NULL);
            close(fd);
        }
    }

    /* ARRAY */
    {
        int fd = map_create(BPF_MAP_TYPE_ARRAY, 4, 8, 4);
        report(fd >= 0, "BPF_MAP_TYPE_ARRAY create", NULL);
        if (fd >= 0) {
            uint32_t key = 3;
            uint64_t val = 7, got = 0;
            report(map_update(fd, &key, &val, BPF_ANY) == 0 &&
                       map_lookup(fd, &key, &got) == 0 && got == 7,
                   "array update+lookup", NULL);
            close(fd);
        }
    }

    /* PERCPU_ARRAY (agentrace scratch map) */
    {
        int fd = map_create(BPF_MAP_TYPE_PERCPU_ARRAY, 4, 64, 1);
        report(fd >= 0, "BPF_MAP_TYPE_PERCPU_ARRAY create", NULL);
        if (fd >= 0)
            close(fd);
    }

    /* LRU_HASH (tcpsniff conn cache) */
    {
        int fd = map_create(BPF_MAP_TYPE_LRU_HASH, 8, 1, 64);
        report(fd >= 0, "BPF_MAP_TYPE_LRU_HASH create", NULL);
        if (fd >= 0) {
            uint64_t key = 0xdeadbeef;
            uint8_t val = 1;
            report(map_update(fd, &key, &val, BPF_ANY) == 0, "lru update", NULL);
            close(fd);
        }
    }
}

/* ---------------- T2: prog load ---------------- */

static void test_prog_load(void)
{
    printf("T2: BPF_PROG_LOAD\n");

    /* minimal: r0 = 0; exit */
    struct bpf_insn minimal[] = {
        { .code = BPF_MOV64_K, .dst_reg = 0, .imm = 0 },
        { .code = BPF_EXIT },
    };
    int fd = prog_load_kprobe(minimal, sizeof(minimal) / sizeof(minimal[0]));
    report(fd >= 0, "minimal prog load", NULL);
    if (fd >= 0)
        close(fd);

    /* map fd relocation + helper: ktime_get_ns() stored to array[0] */
    int mfd = map_create(BPF_MAP_TYPE_ARRAY, 4, 8, 1);
    if (mfd < 0) {
        report(0, "helper prog (no array map)", NULL);
        return;
    }
    struct bpf_insn with_helper[] = {
        /* r7 = bpf_ktime_get_ns() */
        { .code = BPF_CALL, .imm = 5 },
        { .code = BPF_MOV64_X, .dst_reg = 7, .src_reg = 0 },
        /* key = 0 on stack */
        { .code = BPF_ST_DW, .dst_reg = 10, .off = -8, .imm = 0 },
        /* r1 = map; r2 = &key; call lookup */
        LD_MAP_FD(1, mfd),
        { .code = BPF_MOV64_X, .dst_reg = 2, .src_reg = 10 },
        { .code = BPF_ALU64_ADD_K, .dst_reg = 2, .imm = -8 },
        { .code = BPF_CALL, .imm = 1 }, /* map_lookup_elem */
        { .code = BPF_JEQ_K, .dst_reg = 0, .imm = 0, .off = 2 },
        { .code = BPF_STX_DW, .dst_reg = 0, .src_reg = 7 },
        { .code = BPF_MOV64_K, .dst_reg = 0, .imm = 0 },
        { .code = BPF_EXIT },
    };
    size_t cnt = sizeof(with_helper) / sizeof(with_helper[0]);
    fd = prog_load_kprobe(with_helper, cnt);
    report(fd >= 0, "prog with map-fd relocation + ktime helper", NULL);
    if (fd >= 0)
        close(fd);

    /* bogus map fd must fail relocation */
    struct bpf_insn badfd[] = {
        LD_MAP_FD(1, 12345),
        { .code = BPF_EXIT },
    };
    fd = prog_load_kprobe(badfd, sizeof(badfd) / sizeof(badfd[0]));
    report(fd < 0, "bogus map fd rejected", NULL);
    close(mfd);
}

/* ---------------- T3: tracepoint ---------------- */

static void test_tracepoint(void)
{
    printf("T3: tracepoint attach (syscalls/sys_enter_openat)\n");

    uint64_t tp_id;
    if (read_file_u64(
            "/sys/kernel/debug/tracing/events/syscalls/sys_enter_openat/id",
            &tp_id) != 0) {
        report(0, "read tracepoint id", NULL);
        return;
    }

    int mfd = map_create(BPF_MAP_TYPE_ARRAY, 4, 8, 1);
    if (mfd < 0) {
        report(0, "tp counter map", NULL);
        return;
    }

    /* counter++ on array[0] */
    struct bpf_insn prog[] = {
        { .code = BPF_ST_DW, .dst_reg = 10, .off = -8, .imm = 0 },
        LD_MAP_FD(1, mfd),
        { .code = BPF_MOV64_X, .dst_reg = 2, .src_reg = 10 },
        { .code = BPF_ALU64_ADD_K, .dst_reg = 2, .imm = -8 },
        { .code = BPF_CALL, .imm = 1 },
        { .code = BPF_JEQ_K, .dst_reg = 0, .imm = 0, .off = 4 },
        { .code = BPF_LDX_DW, .dst_reg = 1, .src_reg = 0, .off = 0 },
        { .code = BPF_ALU64_ADD_K, .dst_reg = 1, .imm = 1 },
        { .code = BPF_STX_DW, .dst_reg = 0, .src_reg = 1 },
        { .code = BPF_MOV64_K, .dst_reg = 0, .imm = 0 },
        { .code = BPF_EXIT },
    };

    int pfd = prog_load_kprobe(prog, sizeof(prog) / sizeof(prog[0]));
    if (pfd < 0) {
        report(0, "tp prog load", NULL);
        close(mfd);
        return;
    }

    struct perf_event_attr_x attr = {0};
    attr.type_ = 2 /* PERF_TYPE_TRACEPOINT */;
    attr.size = sizeof(attr);
    attr.config = tp_id;
    attr.bits1 = 1; /* disabled */
    int evfd = perf_event_open_x(&attr, -1, 0, -1, 0);
    if (evfd < 0) {
        report(0, "perf_event_open(tracepoint)", NULL);
        close(pfd);
        close(mfd);
        return;
    }
    report(1, "perf_event_open(tracepoint)", NULL);

    if (ioctl(evfd, PERF_IOC_SET_BPF, pfd) != 0) {
        report(0, "PERF_EVENT_IOC_SET_BPF", NULL);
        close(evfd);
        close(pfd);
        close(mfd);
        return;
    }
    report(1, "PERF_EVENT_IOC_SET_BPF", NULL);
    ioctl(evfd, PERF_IOC_ENABLE, 0);

    /* trigger */
    int t = open("/tmp/ebpf_smoke_tp_trigger", O_CREAT | O_RDWR, 0644);
    if (t >= 0) {
        close(t);
        unlink("/tmp/ebpf_smoke_tp_trigger");
    }

    uint32_t key = 0;
    uint64_t hits = 0;
    map_lookup(mfd, &key, &hits);
    report(hits > 0, "tracepoint counter incremented", NULL);

    ioctl(evfd, PERF_IOC_DISABLE, 0);
    close(evfd);
    close(pfd);
    close(mfd);
}

/* ---------------- T4: uprobe ---------------- */

__attribute__((noinline, used)) static int uprobe_target(int x)
{
    /* keep the call frame real so the probe sees rdi = x */
    volatile int y = x + 1;
    return y * 2;
}

/* Translate a runtime function address to its file offset using the program
 * headers mapped in memory (auxv AT_PHDR). The binary is static non-PIE, so
 * the runtime address equals the link-time vaddr. Avoids reading
 * /proc/self/exe (procfs pread is not reliable on DragonOS). */
static int func_addr_to_file_off(uintptr_t func_addr, uint64_t *file_off)
{
    uintptr_t phdr_addr = getauxval(AT_PHDR);
    if (phdr_addr == 0)
        return -1;
    Elf64_Phdr *ph = (Elf64_Phdr *)phdr_addr;
    /* phnum unknown from auxv alone; PT_LOAD entries come first in practice,
     * stop at the first non-PT_LOAD after having seen one. */
    int seen_load = 0;
    for (int i = 0; i < 64; i++) {
        uint32_t p_type = ph[i].p_type;
        if (p_type == 1 /* PT_LOAD */) {
            seen_load = 1;
            if (func_addr >= ph[i].p_vaddr &&
                func_addr < ph[i].p_vaddr + ph[i].p_filesz) {
                *file_off = (uint64_t)(func_addr - ph[i].p_vaddr +
                                       ph[i].p_offset);
                return 0;
            }
        } else if (seen_load) {
            break;
        }
    }
    return -1;
}

static void test_uprobe(void)
{
    printf("T4: uprobe attach (self binary, uprobe_target)\n");

    uint64_t uprobe_type;
    if (read_file_u64("/sys/bus/event_source/devices/uprobe/type",
                      &uprobe_type) != 0) {
        report(0, "read uprobe PMU type", NULL);
        return;
    }

    uint64_t file_off;
    if (func_addr_to_file_off((uintptr_t)&uprobe_target, &file_off) != 0) {
        report(0, "resolve uprobe_target file offset", NULL);
        return;
    }
    printf("    uprobe_target at file offset %#lx\n", (unsigned long)file_off);

    int mfd = map_create(BPF_MAP_TYPE_ARRAY, 4, 8, 1);
    if (mfd < 0) {
        report(0, "uprobe arg map", NULL);
        return;
    }

    /* on hit: map[0] = ctx->rdi (first C arg) */
    struct bpf_insn prog[] = {
        { .code = BPF_MOV64_X, .dst_reg = 6, .src_reg = 1 }, /* r6 = ctx */
        { .code = BPF_LDX_DW, .dst_reg = 7, .src_reg = 6,
          .off = PT_REGS_PARM1_OFF }, /* r7 = rdi */
        { .code = BPF_ST_DW, .dst_reg = 10, .off = -8, .imm = 0 },
        LD_MAP_FD(1, mfd),
        { .code = BPF_MOV64_X, .dst_reg = 2, .src_reg = 10 },
        { .code = BPF_ALU64_ADD_K, .dst_reg = 2, .imm = -8 },
        { .code = BPF_CALL, .imm = 1 },
        { .code = BPF_JEQ_K, .dst_reg = 0, .imm = 0, .off = 2 },
        { .code = BPF_STX_DW, .dst_reg = 0, .src_reg = 7, .off = 0 },
        { .code = BPF_MOV64_K, .dst_reg = 0, .imm = 0 },
        { .code = BPF_EXIT },
    };

    int pfd = prog_load_kprobe(prog, sizeof(prog) / sizeof(prog[0]));
    if (pfd < 0) {
        report(0, "uprobe prog load", NULL);
        close(mfd);
        return;
    }

    struct perf_event_attr_x attr = {0};
    attr.type_ = (uint32_t)uprobe_type;
    attr.size = sizeof(attr);
    attr.config = 0;
    attr.config1 = (uint64_t)(uintptr_t)"/proc/self/exe";
    attr.config2 = file_off;
    attr.bits1 = 1; /* disabled */

    int evfd = perf_event_open_x(&attr, 0 /* current process */, -1, -1, 0);
    if (evfd < 0) {
        report(0, "perf_event_open(uprobe)", NULL);
        close(pfd);
        close(mfd);
        return;
    }
    report(1, "perf_event_open(uprobe)", NULL);

    if (ioctl(evfd, PERF_IOC_SET_BPF, pfd) != 0) {
        report(0, "uprobe PERF_EVENT_IOC_SET_BPF", NULL);
        close(evfd);
        close(pfd);
        close(mfd);
        return;
    }
    report(1, "uprobe PERF_EVENT_IOC_SET_BPF", NULL);
    ioctl(evfd, PERF_IOC_ENABLE, 0);

    int ret = uprobe_target(1234);
    (void)ret;

    uint32_t key = 0;
    uint64_t arg = 0;
    map_lookup(mfd, &key, &arg);
    report(arg == 1234, "uprobe captured rdi arg", NULL);

    ioctl(evfd, PERF_IOC_DISABLE, 0);
    close(evfd);
    close(pfd);
    close(mfd);
}

/* ---------------- T5: perf event output ---------------- */

#ifndef PERF_TYPE_SOFTWARE
#define PERF_TYPE_SOFTWARE 1
#endif
#ifndef PERF_COUNT_SW_BPF_OUTPUT
#define PERF_COUNT_SW_BPF_OUTPUT 10
#endif
#ifndef PERF_SAMPLE_RAW
#define PERF_SAMPLE_RAW (1 << 10)
#endif
#ifndef PERF_RECORD_SAMPLE
#define PERF_RECORD_SAMPLE 9
#endif

static void test_perf_event_output(void)
{
    printf("T5: bpf_perf_event_output channel\n");

    /* possible cpu count: probe by creating the perf array map */
    int mfd = -1;
    int ncpu = 0;
    for (int cand = 1; cand <= 8; cand++) {
        mfd = map_create(BPF_MAP_TYPE_PERF_EVENT_ARRAY, 4, 4, cand);
        if (mfd >= 0) {
            ncpu = cand;
            break;
        }
    }
    if (mfd < 0) {
        report(0, "BPF_MAP_TYPE_PERF_EVENT_ARRAY create", NULL);
        return;
    }
    report(1, "BPF_MAP_TYPE_PERF_EVENT_ARRAY create", NULL);
    printf("    possible cpus = %d\n", ncpu);

    struct perf_event_attr_x attr = {0};
    attr.type_ = PERF_TYPE_SOFTWARE;
    attr.size = sizeof(attr);
    attr.config = PERF_COUNT_SW_BPF_OUTPUT;
    attr.sample_type = PERF_SAMPLE_RAW;
    attr.bits1 = 0;
    int evfd = perf_event_open_x(&attr, -1, 0, -1, 0);
    if (evfd < 0) {
        report(0, "perf_event_open(SW BPF_OUTPUT)", NULL);
        close(mfd);
        return;
    }
    report(1, "perf_event_open(SW BPF_OUTPUT)", NULL);

    uint32_t cpu0 = 0, val = (uint32_t)evfd;
    if (map_update(mfd, &cpu0, &val, BPF_ANY) != 0) {
        report(0, "perf array map_update(cpu0, evfd)", NULL);
        close(evfd);
        close(mfd);
        return;
    }
    report(1, "perf array map_update(cpu0, evfd)", NULL);

    /* mmap ring: 1 metadata page + 1 data page */
    void *ring = mmap(NULL, 2 * 4096, PROT_READ | PROT_WRITE, MAP_SHARED, evfd, 0);
    if (ring == MAP_FAILED) {
        report(0, "mmap perf ring", NULL);
        close(evfd);
        close(mfd);
        return;
    }

    /* prog: bpf_perf_event_output(ctx, map, flags=0 -> cpu index 0, &magic32, 4):
     *   *(u32*)(r10-4) = 0xfeedface; r1 = ctx, r2 = map, r3 = 0,
     *   r4 = r10-4, r5 = 4, call helper 25, r0 = 0; exit.
     * Fixed index 0 instead of BPF_F_CURRENT_CPU: 2 possible cpus, and the
     * perf event is opened on cpu 0 only.
     */
    struct bpf_insn p[] = {
        { .code = 0x62 /* BPF_ST | BPF_MEM | BPF_W */, .dst_reg = 10,
          .off = -4, .imm = (int32_t)0xfeedface },
        LD_MAP_FD(2, mfd),
        { .code = BPF_MOV64_K, .dst_reg = 3, .imm = 0 },
        { .code = BPF_MOV64_X, .dst_reg = 4, .src_reg = 10 },
        { .code = BPF_ALU64_ADD_K, .dst_reg = 4, .imm = -4 },
        { .code = BPF_MOV64_K, .dst_reg = 5, .imm = 4 },
        { .code = BPF_CALL, .imm = 25 },
        { .code = BPF_MOV64_K, .dst_reg = 0, .imm = 0 },
        { .code = BPF_EXIT },
    };

    int pfd = prog_load_kprobe(p, sizeof(p) / sizeof(p[0]));
    if (pfd < 0) {
        report(0, "perf-output prog load", NULL);
        munmap(ring, 2 * 4096);
        close(evfd);
        close(mfd);
        return;
    }

    /* attach to the openat tracepoint so we have a trigger context */
    uint64_t tp_id;
    struct perf_event_attr_x tattr = {0};
    int tevfd = -1;
    if (read_file_u64(
            "/sys/kernel/debug/tracing/events/syscalls/sys_enter_openat/id",
            &tp_id) == 0) {
        tattr.type_ = 2;
        tattr.size = sizeof(tattr);
        tattr.config = tp_id;
        tattr.bits1 = 1;
        tevfd = perf_event_open_x(&tattr, -1, 0, -1, 0);
        if (tevfd >= 0) {
            ioctl(tevfd, PERF_IOC_SET_BPF, pfd);
            ioctl(tevfd, PERF_IOC_ENABLE, 0);
        }
    }

    int t = open("/tmp/ebpf_smoke_perf_trigger", O_CREAT | O_RDWR, 0644);
    if (t >= 0) {
        close(t);
        unlink("/tmp/ebpf_smoke_perf_trigger");
    }

    /* read ring: data_head@1024, data_tail@1032 (uapi perf_event_mmap_page) */
    volatile uint8_t *meta = (volatile uint8_t *)ring;
    uint64_t data_head = *(volatile uint64_t *)(meta + 1024);
    uint64_t data_tail = *(volatile uint64_t *)(meta + 1032);
    int ok = data_head > data_tail;
    uint32_t magic = 0;
    if (ok) {
        /* first record: perf_event_header{u32 type;u16 misc;u16 size} then
         * SampleHeader{u32 size} then raw data */
        uint8_t *base = (uint8_t *)ring + 4096;
        uint32_t type_ = *(uint32_t *)(base + (data_tail % 4096));
        uint16_t rsize = *(uint16_t *)(base + (data_tail % 4096) + 6);
        uint32_t ssize = *(uint32_t *)(base + (data_tail % 4096) + 8);
        magic = *(uint32_t *)(base + (data_tail % 4096) + 12);
        printf("    ring: type=%u rsize=%u ssize=%u magic=%#x\n", type_, rsize,
               ssize, magic);
        ok = (type_ == (uint32_t)PERF_RECORD_SAMPLE) && magic == 0xfeedface;
        /* advance tail */
        *(volatile uint64_t *)(meta + 1032) = data_tail + rsize;
    }
    report(ok, "perf output sample received", NULL);

    if (tevfd >= 0) {
        ioctl(tevfd, PERF_IOC_DISABLE, 0);
        close(tevfd);
    }
    close(pfd);
    munmap(ring, 2 * 4096);
    close(evfd);
    close(mfd);
}

/* ---------------- T6: new helpers (pid_tgid/uid_gid/comm/probe_read_user) ---------------- */

/* slot layout in the 5-entry ARRAY map (value = 16 bytes each):
 * [0] pid_tgid, [1] uid_gid, [2] probe_read_user dst, [3] user src addr,
 * [4] comm (first 16 bytes of value) */
static void test_current_helpers(void)
{
    printf("T6: current-task helpers + bpf_probe_read_user\n");

    int mfd = map_create(BPF_MAP_TYPE_ARRAY, 4, 16, 5);
    if (mfd < 0) {
        report(0, "helper scratch map", NULL);
        return;
    }

    /* magic buffer in user memory to be read by bpf_probe_read_user */
    static const uint64_t magic_src = 0xCAFEBABEDEADBEEFULL;
    uint32_t k3 = 3;
    uint64_t src_addr = (uint64_t)(uintptr_t)&magic_src;
    map_update(mfd, &k3, &src_addr, BPF_ANY);

    struct bpf_insn prog[] = {
        /* m[0] = bpf_get_current_pid_tgid(): value at r10-24, key 0 at r10-8 */
        { .code = BPF_CALL, .imm = 14 },
        { .code = BPF_STX_DW, .dst_reg = 10, .src_reg = 0, .off = -24 },
        { .code = BPF_ST_DW, .dst_reg = 10, .off = -8, .imm = 0 },
        LD_MAP_FD(1, mfd),
        { .code = BPF_MOV64_X, .dst_reg = 2, .src_reg = 10 },
        { .code = BPF_ALU64_ADD_K, .dst_reg = 2, .imm = -8 },
        { .code = BPF_MOV64_X, .dst_reg = 3, .src_reg = 10 },
        { .code = BPF_ALU64_ADD_K, .dst_reg = 3, .imm = -24 },
        { .code = BPF_CALL, .imm = 2 },
        /* m[1] = bpf_get_current_uid_gid(): value at r10-24, key 1 */
        { .code = BPF_CALL, .imm = 15 },
        { .code = BPF_STX_DW, .dst_reg = 10, .src_reg = 0, .off = -24 },
        { .code = BPF_ST_DW, .dst_reg = 10, .off = -8, .imm = 1 },
        LD_MAP_FD(1, mfd),
        { .code = BPF_MOV64_X, .dst_reg = 2, .src_reg = 10 },
        { .code = BPF_ALU64_ADD_K, .dst_reg = 2, .imm = -8 },
        { .code = BPF_MOV64_X, .dst_reg = 3, .src_reg = 10 },
        { .code = BPF_ALU64_ADD_K, .dst_reg = 3, .imm = -24 },
        { .code = BPF_CALL, .imm = 2 },
        /* r7 = m[3] (user src addr); r1 = m[2] dst; r2 = 8; r3 = r7;
         * call bpf_probe_read_user(112); if 0 -> m[2] validated by user */
        { .code = BPF_ST_DW, .dst_reg = 10, .off = -8, .imm = 3 },
        LD_MAP_FD(1, mfd),
        { .code = BPF_MOV64_X, .dst_reg = 2, .src_reg = 10 },
        { .code = BPF_ALU64_ADD_K, .dst_reg = 2, .imm = -8 },
        { .code = BPF_CALL, .imm = 1 },
        { .code = BPF_JNE_K, .dst_reg = 0, .imm = 0, .off = 1 },
        { .code = BPF_EXIT },
        { .code = BPF_LDX_DW, .dst_reg = 7, .src_reg = 0, .off = 0 },
        /* r1 = dst of m[2] (16-byte value slot) */
        { .code = BPF_ST_DW, .dst_reg = 10, .off = -8, .imm = 2 },
        LD_MAP_FD(1, mfd),
        { .code = BPF_MOV64_X, .dst_reg = 2, .src_reg = 10 },
        { .code = BPF_ALU64_ADD_K, .dst_reg = 2, .imm = -8 },
        { .code = BPF_CALL, .imm = 1 },
        { .code = BPF_JEQ_K, .dst_reg = 0, .imm = 0, .off = 6 },
        /* bpf_probe_read_user(dst=r0, size=8, src=r7) */
        { .code = BPF_MOV64_X, .dst_reg = 1, .src_reg = 0 },
        { .code = BPF_MOV64_K, .dst_reg = 2, .imm = 8 },
        { .code = BPF_MOV64_X, .dst_reg = 3, .src_reg = 7 },
        { .code = BPF_CALL, .imm = 112 },
        { .code = BPF_MOV64_K, .dst_reg = 0, .imm = 0 },
        { .code = BPF_EXIT },
        /* comm path: m[4] slot as dst */
        { .code = BPF_ST_DW, .dst_reg = 10, .off = -8, .imm = 4 },
        LD_MAP_FD(1, mfd),
        { .code = BPF_MOV64_X, .dst_reg = 2, .src_reg = 10 },
        { .code = BPF_ALU64_ADD_K, .dst_reg = 2, .imm = -8 },
        { .code = BPF_CALL, .imm = 1 },
        { .code = BPF_JEQ_K, .dst_reg = 0, .imm = 0, .off = 4 },
        { .code = BPF_MOV64_X, .dst_reg = 1, .src_reg = 0 },
        { .code = BPF_MOV64_K, .dst_reg = 2, .imm = 16 },
        { .code = BPF_CALL, .imm = 16 }, /* bpf_get_current_comm */
        { .code = BPF_MOV64_K, .dst_reg = 0, .imm = 0 },
        { .code = BPF_EXIT },
    };

    int pfd = prog_load_kprobe(prog, sizeof(prog) / sizeof(prog[0]));
    if (pfd < 0) {
        report(0, "helper prog load", NULL);
        close(mfd);
        return;
    }

    /* attach to sys_enter_openat to get a trigger context */
    uint64_t tp_id;
    if (read_file_u64(
            "/sys/kernel/debug/tracing/events/syscalls/sys_enter_openat/id",
            &tp_id) != 0) {
        report(0, "read openat tp id", NULL);
        close(pfd);
        close(mfd);
        return;
    }
    struct perf_event_attr_x attr = {0};
    attr.type_ = 2;
    attr.size = sizeof(attr);
    attr.config = tp_id;
    attr.bits1 = 1;
    int evfd = perf_event_open_x(&attr, -1, 0, -1, 0);
    if (evfd < 0 || ioctl(evfd, PERF_IOC_SET_BPF, pfd) != 0) {
        report(0, "helper prog attach", NULL);
        if (evfd >= 0)
            close(evfd);
        close(pfd);
        close(mfd);
        return;
    }
    ioctl(evfd, PERF_IOC_ENABLE, 0);

    /* trigger: open in our own process so pid/uid/comm/magic addr all
     * belong to us */
    int t = open("/tmp/ebpf_smoke_h_trigger", O_CREAT | O_RDWR, 0644);
    if (t >= 0) {
        close(t);
        unlink("/tmp/ebpf_smoke_h_trigger");
    }
    ioctl(evfd, PERF_IOC_DISABLE, 0);
    close(evfd);
    close(pfd);

    uint8_t val[16];
    uint32_t k;
    k = 0;
    uint64_t pid_tgid = 0;
    map_lookup(mfd, &k, val);
    memcpy(&pid_tgid, val, 8);
    uint64_t expect = ((uint64_t)getpid() << 32) | (uint64_t)getpid();
    report(pid_tgid == expect, "bpf_get_current_pid_tgid", NULL);

    k = 1;
    uint64_t uid_gid = 0;
    map_lookup(mfd, &k, val);
    memcpy(&uid_gid, val, 8);
    report((uid_gid >> 32) == (uint64_t)getuid(), "bpf_get_current_uid_gid",
           NULL);

    k = 2;
    uint64_t got_magic = 0;
    map_lookup(mfd, &k, val);
    memcpy(&got_magic, val, 8);
    report(got_magic == 0xCAFEBABEDEADBEEFULL, "bpf_probe_read_user", NULL);

    k = 4;
    char comm[17] = {0};
    map_lookup(mfd, &k, val);
    memcpy(comm, val, 16);
    report(strncmp(comm, "test_ebpf_smoke", 15) == 0,
           "bpf_get_current_comm", comm);

    close(mfd);
}

/* ---------------- T7: new tracepoints ---------------- */

#ifndef __NR_execve
#define __NR_execve 59
#endif

static void test_new_tracepoints(void)
{
    printf("T7: sys_enter/exit_execve + sys_enter_write + sched_process_exit\n");

    /* single map: [0]=enter_execve filename str (64B in 128B slot),
     * [1]=exit ret, [2]=write count, [3]=exit pid, [4]=enter id */
    int mfd = map_create(BPF_MAP_TYPE_ARRAY, 4, 128, 5);
    if (mfd < 0) {
        report(0, "tp scratch map", NULL);
        return;
    }

    /* enter_execve: m[4] = ctx->id; m[0] = string at ctx->args[0] via
     * bpf_probe_read_user_str (helper 114) */
    struct bpf_insn p_enter[] = {
        /* m[4] = ctx->id (i32 @8). value at r10-24, key 4 at r10-8 */
        { .code = 0x61 /* BPF_LDX|BPF_MEM|BPF_W */, .dst_reg = 7,
          .src_reg = 1, .off = 8 },
        { .code = BPF_STX_DW, .dst_reg = 10, .src_reg = 7, .off = -24 },
        { .code = BPF_ST_DW, .dst_reg = 10, .off = -8, .imm = 4 },
        LD_MAP_FD(1, mfd),
        { .code = BPF_MOV64_X, .dst_reg = 2, .src_reg = 10 },
        { .code = BPF_ALU64_ADD_K, .dst_reg = 2, .imm = -8 },
        { .code = BPF_MOV64_X, .dst_reg = 3, .src_reg = 10 },
        { .code = BPF_ALU64_ADD_K, .dst_reg = 3, .imm = -24 },
        { .code = BPF_CALL, .imm = 2 },
        /* filename ptr = ctx->args[0] @16 */
        { .code = BPF_LDX_DW, .dst_reg = 7, .src_reg = 1, .off = 16 },
        /* dst = m[0] value */
        { .code = BPF_ST_DW, .dst_reg = 10, .off = -8, .imm = 0 },
        LD_MAP_FD(1, mfd),
        { .code = BPF_MOV64_X, .dst_reg = 2, .src_reg = 10 },
        { .code = BPF_ALU64_ADD_K, .dst_reg = 2, .imm = -8 },
        { .code = BPF_CALL, .imm = 1 },
        { .code = BPF_JEQ_K, .dst_reg = 0, .imm = 0, .off = 4 },
        { .code = BPF_MOV64_X, .dst_reg = 1, .src_reg = 0 },
        { .code = BPF_MOV64_K, .dst_reg = 2, .imm = 64 },
        { .code = BPF_MOV64_X, .dst_reg = 3, .src_reg = 7 },
        { .code = BPF_CALL, .imm = 114 },
        { .code = BPF_MOV64_K, .dst_reg = 0, .imm = 0 },
        { .code = BPF_EXIT },
    };
    /* exit_execve: m[1] = ctx->ret (i64 @8). value at r10-24, key at r10-8 */
    struct bpf_insn p_exit[] = {
        { .code = BPF_LDX_DW, .dst_reg = 7, .src_reg = 1, .off = 8 },
        { .code = BPF_STX_DW, .dst_reg = 10, .src_reg = 7, .off = -24 },
        { .code = BPF_ST_DW, .dst_reg = 10, .off = -8, .imm = 1 },
        LD_MAP_FD(1, mfd),
        { .code = BPF_MOV64_X, .dst_reg = 2, .src_reg = 10 },
        { .code = BPF_ALU64_ADD_K, .dst_reg = 2, .imm = -8 },
        { .code = BPF_MOV64_X, .dst_reg = 3, .src_reg = 10 },
        { .code = BPF_ALU64_ADD_K, .dst_reg = 3, .imm = -24 },
        { .code = BPF_CALL, .imm = 2 },
        { .code = BPF_MOV64_K, .dst_reg = 0, .imm = 0 },
        { .code = BPF_EXIT },
    };
    /* enter_write: m[2] = ctx->args[2] (count) */
    struct bpf_insn p_write[] = {
        { .code = BPF_LDX_DW, .dst_reg = 7, .src_reg = 1, .off = 32 },
        { .code = BPF_STX_DW, .dst_reg = 10, .src_reg = 7, .off = -24 },
        { .code = BPF_ST_DW, .dst_reg = 10, .off = -8, .imm = 2 },
        LD_MAP_FD(1, mfd),
        { .code = BPF_MOV64_X, .dst_reg = 2, .src_reg = 10 },
        { .code = BPF_ALU64_ADD_K, .dst_reg = 2, .imm = -8 },
        { .code = BPF_MOV64_X, .dst_reg = 3, .src_reg = 10 },
        { .code = BPF_ALU64_ADD_K, .dst_reg = 3, .imm = -24 },
        { .code = BPF_CALL, .imm = 2 },
        { .code = BPF_MOV64_K, .dst_reg = 0, .imm = 0 },
        { .code = BPF_EXIT },
    };
    /* sched_process_exit: m[3] = ctx->pid (i32 @24) */
    struct bpf_insn p_exit_ev[] = {
        { .code = 0x61 /* LDX W */, .dst_reg = 7, .src_reg = 1, .off = 24 },
        { .code = BPF_STX_DW, .dst_reg = 10, .src_reg = 7, .off = -24 },
        { .code = BPF_ST_DW, .dst_reg = 10, .off = -8, .imm = 3 },
        LD_MAP_FD(1, mfd),
        { .code = BPF_MOV64_X, .dst_reg = 2, .src_reg = 10 },
        { .code = BPF_ALU64_ADD_K, .dst_reg = 2, .imm = -8 },
        { .code = BPF_MOV64_X, .dst_reg = 3, .src_reg = 10 },
        { .code = BPF_ALU64_ADD_K, .dst_reg = 3, .imm = -24 },
        { .code = BPF_CALL, .imm = 2 },
        { .code = BPF_MOV64_K, .dst_reg = 0, .imm = 0 },
        { .code = BPF_EXIT },
    };

    struct {
        const char *path;
        const struct bpf_insn *insns;
        size_t cnt;
    } evs[] = {
        { "/sys/kernel/debug/tracing/events/syscalls/sys_enter_execve/id",
          p_enter, sizeof(p_enter) / sizeof(p_enter[0]) },
        { "/sys/kernel/debug/tracing/events/syscalls/sys_exit_execve/id",
          p_exit, sizeof(p_exit) / sizeof(p_exit[0]) },
        { "/sys/kernel/debug/tracing/events/syscalls/sys_enter_write/id",
          p_write, sizeof(p_write) / sizeof(p_write[0]) },
        /* sched_process_exit attach temporarily isolated: its __string
         * entry variant panics the kernel on trigger (ctx=NULL fault at
         * 0x10); re-enable after fixing the string-entry callback path. */
        // { "/sys/kernel/debug/tracing/events/sched/sched_process_exit/id",
        //   p_exit_ev, sizeof(p_exit_ev) / sizeof(p_exit_ev[0]) },
    };
    int evfds[4] = {-1, -1, -1, -1};
    int ok = 1;
    for (int i = 0; i < 3; i++) {
        uint64_t id;
        if (read_file_u64(evs[i].path, &id) != 0) {
            printf("    missing event: %s\n", evs[i].path);
            ok = 0;
            continue;
        }
        int pfd = prog_load_kprobe(evs[i].insns, evs[i].cnt);
        if (pfd < 0) {
            ok = 0;
            continue;
        }
        struct perf_event_attr_x attr = {0};
        attr.type_ = 2;
        attr.size = sizeof(attr);
        attr.config = id;
        attr.bits1 = 1;
        int evfd = perf_event_open_x(&attr, -1, 0, -1, 0);
        if (evfd < 0 || ioctl(evfd, PERF_IOC_SET_BPF, pfd) != 0) {
            ok = 0;
            close(pfd);
            continue;
        }
        ioctl(evfd, PERF_IOC_ENABLE, 0);
        evfds[i] = evfd;
        close(pfd);
    }
    report(ok, "attach 3 syscall tracepoints", NULL);
    if (!ok) {
        for (int i = 0; i < 4; i++)
            if (evfds[i] >= 0)
                close(evfds[i]);
        close(mfd);
        return;
    }

    /* trigger: fork+execve+waitpid, and a distinctive write */
    char wbuf[777];
    memset(wbuf, 'w', sizeof(wbuf));
    ssize_t wr = write(1, wbuf, 0); /* count 0, harmless warm-up */
    (void)wr;
    pid_t child = fork();
    if (child == 0) {
        char *argv[] = {"/bin/busybox", NULL};
        execve("/bin/busybox", argv, NULL);
        _exit(127);
    }
    int wstatus = 0;
    waitpid(child, &wstatus, 0);

    uint8_t val[128];
    uint32_t k;
    k = 4;
    map_lookup(mfd, &k, val);
    int32_t enter_id = 0;
    memcpy(&enter_id, val, 4);
    report(enter_id == __NR_execve, "sys_enter_execve id field", NULL);

    k = 0;
    char fname[65] = {0};
    map_lookup(mfd, &k, val);
    memcpy(fname, val, 64);
    report(strncmp(fname, "/bin/busybox", 12) == 0,
           "sys_enter_execve args[0] filename", fname);

    k = 1;
    int64_t execve_ret = -99;
    map_lookup(mfd, &k, val);
    memcpy(&execve_ret, val, 8);
    report(execve_ret == 0, "sys_exit_execve ret==0", NULL);

    k = 3;
    int32_t exit_pid = 0;
    map_lookup(mfd, &k, val);
    memcpy(&exit_pid, val, 4);
    report(exit_pid == child, "sched_process_exit pid", NULL);

    /* write test last (so map[2] is from OUR write, not children's) */
    memset(wbuf, 'x', sizeof(wbuf));
    wr = write(2, wbuf, 777);
    (void)wr;
    k = 2;
    uint64_t wcount = 0;
    map_lookup(mfd, &k, val);
    memcpy(&wcount, val, 8);
    report(wcount == 777, "sys_enter_write count", NULL);

    for (int i = 0; i < 3; i++) {
        ioctl(evfds[i], PERF_IOC_DISABLE, 0);
        close(evfds[i]);
    }
    close(mfd);
}

#define BPF_MAP_TYPE_RINGBUF 27

/* ---------------- T8: ringbuf map + reserve/submit helpers ---------------- */

static void test_ringbuf(void)
{
    printf("T8: BPF_MAP_TYPE_RINGBUF + bpf_ringbuf_reserve/submit\n");

    const uint32_t data_sz = 16 * 4096;
    int mfd = map_create(BPF_MAP_TYPE_RINGBUF, 0, 0, data_sz);
    if (mfd < 0) {
        report(0, "BPF_MAP_TYPE_RINGBUF create", NULL);
        return;
    }
    report(1, "BPF_MAP_TYPE_RINGBUF create", NULL);

    /* mmap consumer page at offset 0 */
    volatile uint64_t *consumer = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                                       MAP_SHARED, mfd, 0);
    /* mmap [pos page + data ring] at offset PAGE (libbpf layout) */
    volatile uint8_t *producer_area =
        mmap(NULL, 4096 + 2 * data_sz, PROT_READ, MAP_SHARED, mfd, 4096);
    if (consumer == MAP_FAILED || producer_area == MAP_FAILED) {
        report(0, "ringbuf mmap", NULL);
        close(mfd);
        return;
    }
    report(1, "ringbuf mmap (consumer + producer areas)", NULL);
    volatile uint64_t *producer_pos = (volatile uint64_t *)producer_area;
    const uint8_t *data = (const uint8_t *)producer_area + 4096;

    /* prog: reserve(map=r1, size=r2=16, flags=r3=0) -> r0; if r0==0 skip;
     * *(u64*)r0 = magic; *(u64*)(r0+8) = pid_tgid; submit(data=r1=r0, r2=0) */
    struct bpf_insn prog[] = {
        LD_MAP_FD(1, mfd),
        { .code = BPF_MOV64_K, .dst_reg = 2, .imm = 16 },
        { .code = BPF_MOV64_K, .dst_reg = 3, .imm = 0 },
        { .code = BPF_CALL, .imm = 131 }, /* bpf_ringbuf_reserve */
        { .code = BPF_JEQ_K, .dst_reg = 0, .imm = 0, .off = 8 },
        { .code = BPF_MOV64_X, .dst_reg = 6, .src_reg = 0 },
        LD_IMM64(7, 0x9e37beefULL),
        { .code = BPF_STX_DW, .dst_reg = 6, .src_reg = 7, .off = 0 },
        { .code = BPF_CALL, .imm = 14 }, /* pid_tgid */
        { .code = BPF_STX_DW, .dst_reg = 6, .src_reg = 0, .off = 8 },
        { .code = BPF_MOV64_X, .dst_reg = 1, .src_reg = 6 },
        { .code = BPF_MOV64_K, .dst_reg = 2, .imm = 0 },
        { .code = BPF_CALL, .imm = 132 }, /* bpf_ringbuf_submit */
        { .code = BPF_MOV64_K, .dst_reg = 0, .imm = 0 },
        { .code = BPF_EXIT },
    };

    int pfd = prog_load_kprobe(prog, sizeof(prog) / sizeof(prog[0]));
    if (pfd < 0) {
        report(0, "ringbuf prog load", NULL);
        close(mfd);
        return;
    }

    uint64_t tp_id;
    if (read_file_u64(
            "/sys/kernel/debug/tracing/events/syscalls/sys_enter_openat/id",
            &tp_id) != 0) {
        report(0, "read openat tp id", NULL);
        close(pfd);
        close(mfd);
        return;
    }
    struct perf_event_attr_x attr = {0};
    attr.type_ = 2;
    attr.size = sizeof(attr);
    attr.config = tp_id;
    attr.bits1 = 1;
    int evfd = perf_event_open_x(&attr, -1, 0, -1, 0);
    if (evfd < 0 || ioctl(evfd, PERF_IOC_SET_BPF, pfd) != 0) {
        report(0, "ringbuf prog attach", NULL);
        if (evfd >= 0)
            close(evfd);
        close(pfd);
        close(mfd);
        return;
    }
    ioctl(evfd, PERF_IOC_ENABLE, 0);

    /* trigger several events (also exercises the producer lock) */
    for (int i = 0; i < 32; i++) {
        int t = open("/tmp/ebpf_smoke_rb", O_CREAT | O_RDWR, 0644);
        if (t >= 0)
            close(t);
    }
    unlink("/tmp/ebpf_smoke_rb");
    ioctl(evfd, PERF_IOC_DISABLE, 0);
    close(evfd);
    close(pfd);

    /* consume: walk records from consumer_pos to producer_pos */
    uint64_t cons = *consumer;
    uint64_t prod = *producer_pos;
    printf("    ring: consumer=%llu producer=%llu\n",
           (unsigned long long)cons, (unsigned long long)prod);

    uint64_t expect_pid = ((uint64_t)getpid() << 32) | (uint64_t)getpid();
    int seen = 0, ok = 0;
    while (cons < prod && seen < 64) {
        uint64_t off = cons % data_sz;
        uint32_t hdr = *(const uint32_t *)(data + off);
        uint32_t len = hdr >> 2;
        if (hdr & 1 /* BUSY */) {
            printf("    stuck on BUSY record at off=%llu\n",
                   (unsigned long long)off);
            break;
        }
        uint64_t total = 8 + ((len + 7) & ~7u);
        if (!(hdr & 2 /* DISCARD */) && len == 16) {
            uint64_t m0 = *(const uint64_t *)(data + off + 8);
            uint64_t m1 = *(const uint64_t *)(data + off + 16);
            if (m0 == 0x9e37beefULL && m1 == expect_pid) {
                seen++;
                ok = 1;
            }
        }
        cons += total;
        *consumer = cons; /* publish consumed position */
    }
    report(ok, "ringbuf record received and consumed", NULL);
    close(mfd);
}

int main(int argc, char **argv)
{
    /* The DragonOS tty intermittently eats output; mirror everything to a
     * file so results survive console drops. */
    int logfd = open("/tmp/smoke.log", O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (logfd >= 0) {
        dup2(logfd, 1);
        dup2(logfd, 2);
    }
    /* Optional section selector: run only part of the suite when argv[1] is
     * given ("t6", "t7", "t8", or anything else = all). */
    const char *only = argc > 1 ? argv[1] : NULL;
    printf("=== DragonOS eBPF smoke test (agentrace capability matrix) ===\n");
    if (only) {
        if (strcmp(only, "t6") == 0) {
            test_current_helpers();
        } else if (strcmp(only, "t7") == 0) {
            test_new_tracepoints();
        } else if (strcmp(only, "t8") == 0) {
            test_ringbuf();
        }
        printf("=== smoke(section %s): %d passed, %d failed ===\n", only,
               g_passes, g_failures);
        return g_failures ? 1 : 0;
    }
    test_maps();
    test_prog_load();
    test_tracepoint();
    test_uprobe();
    test_perf_event_output();
    test_current_helpers();
    test_new_tracepoints();
    test_ringbuf();
    printf("=== smoke: %d passed, %d failed ===\n", g_passes, g_failures);
    return g_failures ? 1 : 0;
}
