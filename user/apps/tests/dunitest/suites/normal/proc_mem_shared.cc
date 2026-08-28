// Visibility regression test for writes to MAP_SHARED pages via /proc/<pid>/mem.
//
// Covered semantics:
// - ftruncate a temp file to one page, then MAP_SHARED-map it;
// - the tracee completes the mappings before SIGSTOP, while the parent (root tracer) uses
//   PTRACE_POKEDATA to modify offset A and /proc/<pid>/mem pwrite to modify offset B;
// - pread via an independent fd (through the page cache, not the mappings) must read new values
//   at both offsets, proving both remote-write paths hit the page cache, not a private COW copy;
// - MAP_PRIVATE control: POKEDATA only changes the tracee's private copy; file content unchanged.
//
// x86_64 user struct offset: offsetof(struct user, u_debugreg[n]) = 848 + 8*n
// (This test does not use debug registers; the convention is cited only for reference.)

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <gtest/gtest.h>

// ptrace request numbers (fallback for when the build environment lacks the headers; values match the Linux x86_64 ABI)
#ifndef PTRACE_TRACEME
#define PTRACE_TRACEME 0
#endif
#ifndef PTRACE_POKEDATA
#define PTRACE_POKEDATA 5
#endif
#ifndef PTRACE_DETACH
#define PTRACE_DETACH 17
#endif

namespace {

constexpr size_t kPageSize = 4096;
// Offset A: modified by PTRACE_POKEDATA; offset B: modified by /proc/<pid>/mem pwrite.
constexpr size_t kOffsetA = 0;
constexpr size_t kOffsetB = 4096 - sizeof(uint64_t);

long ptrace_call(long request, long pid, unsigned long addr, unsigned long data) {
    errno = 0;
    return syscall(SYS_ptrace, request, pid, addr, data);
}

// The tracee writes the mapping addresses back to the parent (a shared-memory file that also
// serves as the result carrier). Convention: word 0 of the first page holds the addresses for
// the parent; data assertions use an independent fd rather than the mapped contents instead.
struct SharedLayout {
    uint64_t shared_map_addr;   // tracee fills in the MAP_SHARED address
    uint64_t private_map_addr;  // tracee fills in the MAP_PRIVATE address
};

// Read 8 bytes via an independent fd.
bool pread_word(int fd, size_t offset, uint64_t* out) {
    ssize_t n = pread(fd, out, sizeof(uint64_t), (off_t)offset);
    return n == (ssize_t)sizeof(uint64_t);
}

}  // namespace

TEST(ProcMemShared, SharedMappingWritesReachPageCachePrivateDoesNot) {
    // Data file: the object mapped with MAP_SHARED / MAP_PRIVATE.
    char data_path[] = "/tmp/dunitest_proc_mem_shared_XXXXXX";
    int data_fd = mkstemp(data_path);
    ASSERT_GE(data_fd, 0) << "mkstemp failed: errno=" << errno << " (" << strerror(errno) << ")";
    unlink(data_path);
    ASSERT_EQ(0, ftruncate(data_fd, (off_t)kPageSize));

    // Sync file: the tracee writes back the two mapping addresses.
    char sync_path[] = "/tmp/dunitest_proc_mem_sync_XXXXXX";
    int sync_fd = mkstemp(sync_path);
    ASSERT_GE(sync_fd, 0) << "mkstemp failed: errno=" << errno << " (" << strerror(errno) << ")";
    unlink(sync_path);
    ASSERT_EQ(0, ftruncate(sync_fd, (off_t)kPageSize));

    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";
    if (child == 0) {
        void* shared_map = mmap(nullptr, kPageSize, PROT_READ | PROT_WRITE, MAP_SHARED, data_fd, 0);
        if (shared_map == MAP_FAILED) {
            _exit(40);
        }
        void* private_map =
            mmap(nullptr, kPageSize, PROT_READ | PROT_WRITE, MAP_PRIVATE, data_fd, 0);
        if (private_map == MAP_FAILED) {
            _exit(41);
        }
        // Write the mapping addresses back to the parent (the sync file itself is MAP_SHARED).
        SharedLayout layout;
        layout.shared_map_addr = reinterpret_cast<uint64_t>(shared_map);
        layout.private_map_addr = reinterpret_cast<uint64_t>(private_map);
        if (pwrite(sync_fd, &layout, sizeof(layout), 0) != (ssize_t)sizeof(layout)) {
            _exit(42);
        }
        if (ptrace_call(PTRACE_TRACEME, 0, 0, 0) != 0) {
            _exit(43);
        }
        if (raise(SIGSTOP) != 0) {
            _exit(44);
        }
        // Exit after DETACH.
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, WUNTRACED));
    ASSERT_TRUE(WIFSTOPPED(status)) << "child status=" << status;

    // Read back the tracee's mapping addresses.
    SharedLayout layout = {};
    ASSERT_EQ((ssize_t)sizeof(layout), pread(sync_fd, &layout, sizeof(layout), 0));
    ASSERT_NE(0u, layout.shared_map_addr);
    ASSERT_NE(0u, layout.private_map_addr);

    // ---- Path one: PTRACE_POKEDATA writes offset A in the MAP_SHARED region ----
    const uint64_t value_a = 0xA5A5A5A55A5A5A5AULL;
    errno = 0;
    ASSERT_EQ(0L, ptrace_call(PTRACE_POKEDATA, child,
                              layout.shared_map_addr + kOffsetA, value_a))
        << "errno=" << errno << " (" << strerror(errno) << ")";

    // ---- Path two: /proc/<pid>/mem pwrite writes offset B in the MAP_SHARED region ----
    char mem_path[64];
    snprintf(mem_path, sizeof(mem_path), "/proc/%d/mem", (int)child);
    int mem_fd = open(mem_path, O_RDWR);
    ASSERT_GE(mem_fd, 0) << "open(" << mem_path << ") failed: errno=" << errno << " ("
                         << strerror(errno) << ")";
    const uint64_t value_b = 0x0BAD0BAD0BAD0BADULL;
    ASSERT_EQ((ssize_t)sizeof(value_b),
              pwrite(mem_fd, &value_b, sizeof(value_b), (off_t)(layout.shared_map_addr + kOffsetB)))
        << "errno=" << errno << " (" << strerror(errno) << ")";
    close(mem_fd);

    // ---- Control: POKEDATA writes the MAP_PRIVATE region; the file content must not change ----
    const uint64_t value_p = 0xDEADBEEFDEADBEEFULL;
    ASSERT_EQ(0L, ptrace_call(PTRACE_POKEDATA, child,
                              layout.private_map_addr + kOffsetA, value_p));

    // ---- Assertions: the data file's own fd is the page-cache source of truth; both offsets must read back the new values ----

    uint64_t word_a = 0;
    ASSERT_TRUE(pread_word(data_fd, kOffsetA, &word_a));
    EXPECT_EQ(value_a, word_a) << "POKEDATA writing MAP_SHARED should reach the page cache";

    uint64_t word_b = 0;
    ASSERT_TRUE(pread_word(data_fd, kOffsetB, &word_b));
    EXPECT_EQ(value_b, word_b) << "/proc/mem pwrite writing MAP_SHARED should reach the page cache";

    // MAP_PRIVATE control: the file content must not change (a COW copy is invisible to the file).
    uint64_t word_p = 0;
    ASSERT_TRUE(pread_word(data_fd, kOffsetA, &word_p));
    // Note kOffsetA is the same offset as word_a above -- MAP_PRIVATE wrote a different mapping,
    // so checking that the same file offset still holds value_a (unpolluted by the private copy) suffices.
    EXPECT_EQ(value_a, word_p) << "POKEDATA writing MAP_PRIVATE must not modify the file content";

    // ---- Cleanup ----
    ASSERT_EQ(0L, ptrace_call(PTRACE_DETACH, child, 0, 0));
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status)) << "child status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status));

    close(data_fd);
    close(sync_fd);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
