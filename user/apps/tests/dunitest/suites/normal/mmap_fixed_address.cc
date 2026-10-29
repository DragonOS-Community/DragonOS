// Linux rounds ordinary low hints to mmap_min_addr, but a fixed mapping must
// either use the requested address or fail. Exercise both mmap backends.
#include <gtest/gtest.h>
#include <errno.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {
void CheckFixedAddress(bool anonymous, bool no_replace) {
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (!child) {
        const size_t page = sysconf(_SC_PAGESIZE);
        int fd = -1;
        if (!anonymous) {
            fd = syscall(SYS_memfd_create, "fixed-address", 0);
            if (fd < 0 || ftruncate(fd, page)) _exit(1);
        }
        void* requested = reinterpret_cast<void*>(page);
        int flags = MAP_PRIVATE | (no_replace ? MAP_FIXED_NOREPLACE : MAP_FIXED);
        if (anonymous) flags |= MAP_ANONYMOUS;
        errno = 0;
        void* mapped = mmap(requested, page, PROT_READ | PROT_WRITE, flags, fd, 0);
        const int error = errno;
        if (fd >= 0) close(fd);
        if (mapped == MAP_FAILED) {
            // Unprivileged Linux callers cannot map below mmap_min_addr.
            // NOREPLACE may also encounter an already mapped requested page.
            _exit(error == EPERM || error == EACCES ||
                  (no_replace && error == EEXIST) ? 0 : 2);
        }
        const bool exact = mapped == requested;
        const int result = munmap(mapped, page);
        _exit(exact && result == 0 ? 0 : 3);
    }
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status)) << status;
    EXPECT_EQ(WEXITSTATUS(status), 0);
}
TEST(MmapFixedAddress, AnonymousFixed) { CheckFixedAddress(true, false); }
TEST(MmapFixedAddress, AnonymousNoReplace) { CheckFixedAddress(true, true); }
TEST(MmapFixedAddress, FileFixed) { CheckFixedAddress(false, false); }
TEST(MmapFixedAddress, FileNoReplace) { CheckFixedAddress(false, true); }
}  // namespace
int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
