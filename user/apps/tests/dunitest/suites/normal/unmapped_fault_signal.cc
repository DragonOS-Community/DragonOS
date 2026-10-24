#include <gtest/gtest.h>

#include <atomic>
#include <cerrno>
#include <cstdint>
#include <signal.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {
static_assert(std::atomic<uintptr_t>::is_always_lock_free);
std::atomic<uintptr_t> expected_address{0};

void FaultHandler(int signal, siginfo_t* info, void*) {
    _exit(signal == SIGSEGV && info->si_code == SEGV_MAPERR &&
                  reinterpret_cast<uintptr_t>(info->si_addr) ==
                  expected_address.load(std::memory_order_relaxed)
              ? 0
              : 90);
}

void CheckFault(uintptr_t address, const char* name) {
#if defined(__x86_64__)
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        expected_address.store(address, std::memory_order_relaxed);
        struct sigaction action {};
        action.sa_sigaction = FaultHandler;
        action.sa_flags = SA_SIGINFO;
        sigemptyset(&action.sa_mask);
        if (sigaction(SIGSEGV, &action, nullptr) != 0 ||
            prctl(PR_SET_NAME, name, 0, 0, 0) != 0) _exit(91);
        // Ensure the chosen canonical user page is absent. Inline assembly
        // avoids compiler UB optimizations of a deliberate null dereference.
        if (munmap(reinterpret_cast<void*>(address), 4096) != 0) _exit(92);
        unsigned char value;
        asm volatile("movb (%1), %0" : "=r"(value) : "r"(address) : "memory");
        _exit(93);
    }
    int status = 0;
    pid_t waited;
    do { waited = waitpid(child, &status, 0); } while (waited < 0 && errno == EINTR);
    ASSERT_EQ(waited, child);
    ASSERT_TRUE(WIFEXITED(status)) << status;
    EXPECT_EQ(WEXITSTATUS(status), 0);
#else
    GTEST_SKIP() << "x86-64 page-fault diagnostics";
#endif
}
}  // namespace

TEST(UnmappedFaultSignal, LowAddressReportsMaperr) {
    CheckFault(0, "pf-low");
}

TEST(UnmappedFaultSignal, AboveLastVmaReportsMaperr) {
    CheckFault(0x7fffffffd000ULL, "pf-high");
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
