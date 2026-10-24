// brk is a byte address; only the backing mappings are page aligned. Run
// mutations in children and avoid allocator calls after changing libc's heap.
#include <gtest/gtest.h>
#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
#include <vector>

namespace {
uintptr_t Break(uintptr_t requested = 0) {
    return static_cast<uintptr_t>(syscall(SYS_brk, requested));
}
uintptr_t Align(uintptr_t value, size_t page) {
    return (value + page - 1) / page * page;
}
void ChildSucceeded(pid_t child) {
    ASSERT_GT(child, 0);
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
    EXPECT_EQ(WEXITSTATUS(status), 0);
}
bool VmBytes(size_t* result) {
    char text[8192];
    int fd = open("/proc/self/status", O_RDONLY);
    if (fd < 0) return false;
    size_t used = 0;
    while (used + 1 < sizeof(text)) {
        ssize_t n = read(fd, text + used, sizeof(text) - used - 1);
        if (n < 0 && errno == EINTR) continue;
        if (n < 0) { close(fd); return false; }
        if (!n) break;
        used += n;
    }
    close(fd);
    text[used] = 0;
    const char* p = strstr(text, "VmSize:");
    size_t kb = 0;
    if (!p || sscanf(p, "VmSize: %zu kB", &kb) != 1) return false;
    *result = kb * 1024;
    return true;
}

TEST(BrkSemantics, ByteBreakAndPageMappingBoundaries) {
    const size_t page = sysconf(_SC_PAGESIZE);
    pid_t child = fork();
    if (!child) {
        uintptr_t old = Break();
        uintptr_t base = Align(old, page) + page;
        if (Break(base + 17) != base + 17) _exit(1);
        size_t before = 0, after = 0;
        if (!VmBytes(&before)) _exit(2);
        if (Break(base + 33) != base + 33 || Break() != base + 33) _exit(3);
        if (!VmBytes(&after) || after != before) _exit(4);
        if (Break(base + 1) != base + 1) _exit(5);
        if (!VmBytes(&after) || after != before) _exit(6);
        if (Break(base) != base) _exit(7);
        if (!VmBytes(&after) || after + page != before) _exit(8);
        if (Break(base + page + 17) != base + page + 17) _exit(9);
        if (!VmBytes(&after) || after != before + page) _exit(10);
        if (Break(old) != old || Break() != old) _exit(11);
        _exit(0);
    }
    ChildSucceeded(child);
}

TEST(BrkSemantics, InvalidRequestsKeepExactPreviousBreak) {
    const size_t page = sysconf(_SC_PAGESIZE);
    pid_t child = fork();
    if (!child) {
        uintptr_t old = Break();
        uintptr_t current = Align(old, page) + 17;
        if (Break(current) != current) _exit(1);
        if (Break(UINTPTR_MAX) != current || Break() != current) _exit(2);
        if (Break(1) != current || Break() != current) _exit(3);
        rlimit limit{};
        if (getrlimit(RLIMIT_DATA, &limit)) _exit(4);
        limit.rlim_cur = 0;
        if (setrlimit(RLIMIT_DATA, &limit)) _exit(5);
        if (Break(current + 1) != current || Break() != current) _exit(6);
        _exit(0);
    }
    ChildSucceeded(child);
}

TEST(BrkSemantics, LibcSbrkPreservesBytesAndRejectsOverflow) {
    pid_t child = fork();
    if (!child) {
        void* old = sbrk(0);
        const auto value = reinterpret_cast<uintptr_t>(old);
        if (sbrk(1) != old || Break() != value + 1) _exit(1);
        if (sbrk(2) != reinterpret_cast<void*>(value + 1) || Break() != value + 3)
            _exit(2);
        if (sbrk(-3) != reinterpret_cast<void*>(value + 3) || Break() != value)
            _exit(3);
        errno = 0;
        if (sbrk(INTPTR_MAX) != reinterpret_cast<void*>(-1) || errno != ENOMEM ||
            Break() != value) _exit(4);
        errno = 0;
        if (sbrk(INTPTR_MIN) != reinterpret_cast<void*>(-1) || errno != ENOMEM ||
            Break() != value) _exit(5);
        _exit(0);
    }
    ChildSucceeded(child);
}

TEST(BrkSemantics, ConflictingMappingDoesNotCommitBreakOrPartialGrowth) {
    const size_t page = sysconf(_SC_PAGESIZE);
    pid_t child = fork();
    if (!child) {
        uintptr_t current = Align(Break(), page) + 17;
        if (Break(current) != current) _exit(1);
        uintptr_t boundary = Align(current, page) + 2 * page;
        void* mapped = mmap(reinterpret_cast<void*>(boundary), page, PROT_NONE,
                            MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE,
                            -1, 0);
        if (mapped != reinterpret_cast<void*>(boundary)) _exit(2);
        size_t before = 0, after = 0;
        if (!VmBytes(&before)) _exit(3);
        if (Break(boundary + 1) != current || Break() != current) _exit(4);
        if (!VmBytes(&after) || before != after) _exit(5);
        _exit(0);
    }
    ChildSucceeded(child);
}

TEST(BrkSemantics, ForkInheritsExactBreakAndChangesIndependently) {
    const size_t page = sysconf(_SC_PAGESIZE);
    pid_t child = fork();
    if (!child) {
        uintptr_t current = Align(Break(), page) + 17;
        if (Break(current) != current) _exit(1);
        pid_t grandchild = fork();
        if (grandchild < 0) _exit(2);
        if (!grandchild) {
            if (Break() != current || Break(current + 1) != current + 1) _exit(3);
            _exit(0);
        }
        int status = 0;
        if (waitpid(grandchild, &status, 0) != grandchild || !WIFEXITED(status) ||
            WEXITSTATUS(status) || Break() != current) _exit(4);
        _exit(0);
    }
    ChildSucceeded(child);
}

TEST(BrkSemantics, FutureMlockChargesOnlyNewBackingPages) {
    const size_t page = sysconf(_SC_PAGESIZE);
    pid_t child = fork();
    if (!child) {
        // CAP_IPC_LOCK bypasses MEMLOCK on Linux. Drop only that effective
        // capability in this disposable child to exercise the quota boundary.
        __user_cap_header_struct header{_LINUX_CAPABILITY_VERSION_3, 0};
        __user_cap_data_struct caps[2]{};
        if (syscall(SYS_capget, &header, caps)) _exit(7);
        caps[CAP_IPC_LOCK / 32].effective &= ~(1U << (CAP_IPC_LOCK % 32));
        if (syscall(SYS_capset, &header, caps)) _exit(8);
        rlimit limit{};
        if (getrlimit(RLIMIT_MEMLOCK, &limit) || limit.rlim_max < page) _exit(1);
        limit.rlim_cur = page;
        if (setrlimit(RLIMIT_MEMLOCK, &limit) || mlockall(MCL_FUTURE)) _exit(2);
        uintptr_t first = Align(Break(), page) + 1;
        if (Break(first) != first) _exit(3);
        if (Break(first + 17) != first + 17) _exit(4);
        if (Break(first + page) != first + 17) _exit(5);
        if (Break(first + 1) != first + 1) _exit(6);
        munlockall();
        _exit(0);
    }
    ChildSucceeded(child);
}

#if defined(__x86_64__)
// A libc-free ELF entry makes the initial brk observable before any runtime
// allocation. Code and its RIP-relative constants are copied together into an
// ordinary executable fixture, following exec_abi's production ABI tests.
extern "C" const unsigned char brk_fixture_begin[], brk_fixture_end[];
extern "C" const unsigned char brk_fixture_limit[], brk_fixture_mapping[];
asm(R"(
.pushsection .rodata.brk_fixture,"a",@progbits
.global brk_fixture_begin, brk_fixture_end, brk_fixture_limit, brk_fixture_mapping
brk_fixture_begin:
    sub $16, %rsp
    xor %edi, %edi
    mov $12, %eax
    syscall
    mov %rax, %r12
    mov brk_fixture_mapping(%rip), %rsi
    test %rsi, %rsi
    jz 1f
    xor %edi, %edi
    mov $7, %edx
    mov $0x22, %r10d
    mov $-1, %r8
    xor %r9d, %r9d
    mov $9, %eax
    syscall
    cmp $-4095, %rax
    jae 9f
1:
    mov $2, %edi
    mov %rsp, %rsi
    mov $97, %eax
    syscall
    test %eax, %eax
    jnz 9f
    mov brk_fixture_limit(%rip), %rax
    mov %rax, (%rsp)
    mov $2, %edi
    mov %rsp, %rsi
    mov $160, %eax
    syscall
    test %eax, %eax
    jnz 9f
    lea 17(%r12), %rdi
    mov $12, %eax
    syscall
    cmpq $0, brk_fixture_mapping(%rip)
    jne 2f
    cmp %rdi, %rax
    jne 9f
    lea 18(%r12), %rdi
    mov $12, %eax
    syscall
    lea 17(%r12), %rdi
    cmp %rdi, %rax
    jne 9f
    xor %edi, %edi
    mov $12, %eax
    syscall
    lea 17(%r12), %rdi
    cmp %rdi, %rax
    jne 9f
    jmp 3f
2:
    cmp %r12, %rax
    jne 9f
3:
    xor %edi, %edi
    mov $60, %eax
    syscall
9:
    mov $42, %edi
    mov $60, %eax
    syscall
brk_fixture_limit: .quad 0
brk_fixture_mapping: .quad 0
brk_fixture_end:
.popsection
)");

void RunDataFixture(bool map_rwx) {
    const size_t page = sysconf(_SC_PAGESIZE);
    const size_t data_bytes = 2 * page + 127;
    const size_t code_offset = sizeof(Elf64_Ehdr) + 2 * sizeof(Elf64_Phdr);
    const size_t code_size = reinterpret_cast<uintptr_t>(brk_fixture_end) -
                             reinterpret_cast<uintptr_t>(brk_fixture_begin);
    ASSERT_LT(code_offset + code_size, page);
    std::vector<unsigned char> image(page + data_bytes, 0);
    Elf64_Ehdr header{};
    memcpy(header.e_ident, ELFMAG, SELFMAG);
    header.e_ident[EI_CLASS] = ELFCLASS64;
    header.e_ident[EI_DATA] = ELFDATA2LSB;
    header.e_ident[EI_VERSION] = EV_CURRENT;
    header.e_type = ET_EXEC;
    header.e_machine = EM_X86_64;
    header.e_version = EV_CURRENT;
    // A conventional ET_EXEC base specifies only the fixture's own LOAD
    // addresses. Heap requests always derive from the runtime brk query.
    const uint64_t base = 0x400000;
    header.e_entry = base + code_offset;
    header.e_phoff = sizeof(header);
    header.e_ehsize = sizeof(header);
    header.e_phentsize = sizeof(Elf64_Phdr);
    header.e_phnum = 2;
    Elf64_Phdr load[2]{};
    for (auto& segment : load) {
        segment.p_type = PT_LOAD;
        segment.p_flags = PF_R | PF_X;
        segment.p_align = page;
    }
    load[0].p_vaddr = base;
    load[0].p_filesz = load[0].p_memsz = page;
    load[1].p_offset = page;
    load[1].p_vaddr = base + 4 * page;
    load[1].p_filesz = load[1].p_memsz = data_bytes;
    // Linux's static DATA bytes are max(filesz endpoint) - max(LOAD start).
    // RX LOADs leave data_vm zero so this exact-byte test does not conflate
    // the separate writable-data page limit with check_data_rlimit().
    memcpy(image.data(), &header, sizeof(header));
    memcpy(image.data() + sizeof(header), load, sizeof(load));
    memcpy(image.data() + code_offset, brk_fixture_begin, code_size);
    const uint64_t limit = data_bytes + 17;
    const uint64_t mapping = map_rwx ? 3 * page : 0;
    memcpy(image.data() + code_offset + (reinterpret_cast<uintptr_t>(brk_fixture_limit) -
                                            reinterpret_cast<uintptr_t>(brk_fixture_begin)),
           &limit, sizeof(limit));
    memcpy(image.data() + code_offset + (reinterpret_cast<uintptr_t>(brk_fixture_mapping) -
                                            reinterpret_cast<uintptr_t>(brk_fixture_begin)),
           &mapping, sizeof(mapping));

    char path[] = "/tmp/brk-semantics-XXXXXX";
    int fd = mkstemp(path);
    ASSERT_GE(fd, 0);
    bool ok = fchmod(fd, 0700) == 0;
    size_t written = 0;
    while (ok && written < image.size()) {
        ssize_t n = write(fd, image.data() + written, image.size() - written);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) { ok = false; break; }
        written += n;
    }
    close(fd);
    if (!ok) { unlink(path); FAIL() << "cannot write executable fixture"; }
    pid_t child = fork();
    if (!child) {
        char* const argv[] = {path, nullptr};
        char* const env[] = {nullptr};
        execve(path, argv, env);
        _exit(43);
    }
    ChildSucceeded(child);
    EXPECT_EQ(unlink(path), 0);
}

TEST(BrkSemantics, StaticDataUsesExactElfByteEndpoints) {
    RunDataFixture(false);
}
TEST(BrkSemantics, ExistingPrivateRwxMappingsCountTowardsDataPageLimit) {
    RunDataFixture(true);
}
#endif
} // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
