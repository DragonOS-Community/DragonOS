#include <gtest/gtest.h>

#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <time.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {

constexpr int kSentinel = 0x12345678;
constexpr size_t kStackSize = 64 * 1024;

enum class Action { Exit, Cancel, FailedExec, Exec, ReadOnly, Unmapped, ReadOnlyWake };

struct ChildArgs {
    int* tid;
    size_t page_size;
    Action action;
    const char* path;
    int* wait_phase;
    const char* parent_stat_path;
};

// Only the child changes its registration. In particular, never overwrite the
// test runner's libc clear_child_tid, even when clone shares its address space.
int child_main(void* opaque) {
    auto* args = static_cast<ChildArgs*>(opaque);
    if (syscall(SYS_set_tid_address, args->tid) != syscall(SYS_gettid)) {
        return 90;
    }
    if (args->action == Action::Cancel) {
        if (syscall(SYS_set_tid_address, nullptr) != syscall(SYS_gettid)) {
            return 91;
        }
    } else if (args->action == Action::ReadOnly || args->action == Action::ReadOnlyWake) {
        if (mprotect(args->tid, args->page_size, PROT_READ) != 0) {
            return 92;
        }
        if (args->action == Action::ReadOnlyWake) {
            timespec start = {}, now = {};
            if (syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &start) != 0) return 96;
            for (;;) {
                int phase = __atomic_load_n(args->wait_phase, __ATOMIC_ACQUIRE);
                if (phase == 2) return 97;
                if (phase == 1) {
                    // Between publishing phase 1 and phase 2 the parent's only
                    // blocking operation is FUTEX_WAIT on tid. State S proves
                    // that it has entered that wait, without probing with wake.
                    int fd = open(args->parent_stat_path, O_RDONLY);
                    if (fd < 0) return 100;
                    char stat[512] = {};
                    ssize_t size = read(fd, stat, sizeof(stat) - 1);
                    close(fd);
                    if (size <= 0) return 101;
                    // comm is parenthesized and may itself contain ')'.
                    char* comm_end = strrchr(stat, ')');
                    if (!comm_end || comm_end[1] != ' ') return 102;
                    if (comm_end[2] == 'S') break;
                }
                if (syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &now) != 0) return 98;
                if (now.tv_sec - start.tv_sec >= 3) return 99;
                syscall(SYS_sched_yield);
            }
        }
    } else if (args->action == Action::Unmapped) {
        if (munmap(args->tid, args->page_size) != 0) {
            return 93;
        }
    } else if (args->action == Action::Exec || args->action == Action::FailedExec) {
        char* const argv[] = {const_cast<char*>(args->path), nullptr};
        char* const envp[] = {nullptr};
        execve(args->path, argv, envp);
        if (args->action != Action::FailedExec || errno != ENOEXEC) {
            return 94;
        }
        // An early failure must neither clear the word nor discard registration.
        if (__atomic_load_n(args->tid, __ATOMIC_SEQ_CST) != kSentinel) {
            return 95;
        }
    }
    return 0;  // libc clone's trampoline performs SYS_exit, not exit_group.
}

class ClearChildTid : public ::testing::Test {
protected:
    void SetUp() override {
        page_size_ = sysconf(_SC_PAGESIZE);
        ASSERT_GT(page_size_, 0);
        tid_ = static_cast<int*>(mmap(nullptr, page_size_, PROT_READ | PROT_WRITE,
                                     MAP_SHARED | MAP_ANONYMOUS, -1, 0));
        ASSERT_NE(MAP_FAILED, tid_);
        *tid_ = kSentinel;
        stack_ = mmap(nullptr, kStackSize, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        ASSERT_NE(MAP_FAILED, stack_);
    }

    void TearDown() override {
        if (child_ > 0) {
            kill(child_, SIGKILL);
            while (waitpid(child_, nullptr, 0) < 0 && errno == EINTR) {}
        }
        if (tid_ != MAP_FAILED) munmap(tid_, page_size_);
        if (stack_ != MAP_FAILED) munmap(stack_, kStackSize);
        if (path_[0]) unlink(path_);
    }

    void RunChild(Action action, bool share_vm = true) {
        ASSERT_NO_FATAL_FAILURE(StartChild(action, share_vm));
        WaitChild();
    }

    void StartChild(Action action, bool share_vm = true) {
        snprintf(parent_stat_path_, sizeof(parent_stat_path_), "/proc/%d/stat", getpid());
        args_ = {tid_, static_cast<size_t>(page_size_), action, path_,
                 &wait_phase_, parent_stat_path_};
        child_ = clone(child_main, static_cast<char*>(stack_) + kStackSize,
                       SIGCHLD | (share_vm ? CLONE_VM : 0), &args_);
        ASSERT_GT(child_, 0) << strerror(errno);
    }

    void WaitChild() {
        int status = 0;
        // Bounded polling also avoids coupling these tests to futex wake itself.
        for (int i = 0; i < 1000; ++i) {
            pid_t waited = waitpid(child_, &status, WNOHANG);
            if (waited == child_) {
                child_ = -1;
                ASSERT_TRUE(WIFEXITED(status)) << status;
                EXPECT_EQ(0, WEXITSTATUS(status));
                return;
            }
            ASSERT_TRUE(waited == 0 || (waited < 0 && errno == EINTR));
            usleep(10000);
        }
        FAIL() << "child did not exit within 10 seconds";
    }

    void WriteExecutable(const void* data, size_t size) {
        ASSERT_TRUE(mkdir("/tmp", 0755) == 0 || errno == EEXIST);
        snprintf(path_, sizeof(path_), "/tmp/clear_child_tid_%d_XXXXXX", getpid());
        int fd = mkstemp(path_);
        ASSERT_GE(fd, 0);
        const char* bytes = static_cast<const char*>(data);
        size_t done = 0;
        while (done < size) {
            ssize_t n = write(fd, bytes + done, size - done);
            if (n < 0 && errno == EINTR) continue;
            if (n <= 0) {
                close(fd);
                FAIL() << "write executable failed";
            }
            done += static_cast<size_t>(n);
        }
        int chmod_result = fchmod(fd, 0700);
        int close_result = close(fd);
        ASSERT_EQ(0, chmod_result);
        ASSERT_EQ(0, close_result);
    }

    int* tid_ = static_cast<int*>(MAP_FAILED);
    void* stack_ = MAP_FAILED;
    long page_size_ = 0;
    pid_t child_ = -1;
    ChildArgs args_ = {};
    int wait_phase_ = 0;
    char parent_stat_path_[64] = {};
    char path_[128] = {};
};

TEST_F(ClearChildTid, SingleMmUserDoesNotWriteSharedMapping) {
    ASSERT_NO_FATAL_FAILURE(RunChild(Action::Exit, false));
    EXPECT_EQ(kSentinel, *tid_);
}

TEST_F(ClearChildTid, SharedMmExitClearsTid) {
    ASSERT_NO_FATAL_FAILURE(RunChild(Action::Exit));
    EXPECT_EQ(0, *tid_);
}

TEST_F(ClearChildTid, NullAddressCancelsRegistration) {
    ASSERT_NO_FATAL_FAILURE(RunChild(Action::Cancel));
    EXPECT_EQ(kSentinel, *tid_);
}

TEST_F(ClearChildTid, FailedExecPreservesRegistration) {
    static constexpr char invalid_elf[] = "not an ELF executable\n";
    ASSERT_NO_FATAL_FAILURE(WriteExecutable(invalid_elf, sizeof(invalid_elf)));
    ASSERT_NO_FATAL_FAILURE(RunChild(Action::FailedExec));
    EXPECT_EQ(0, *tid_);
}

TEST_F(ClearChildTid, ReadOnlyAddressDoesNotPreventExit) {
    ASSERT_NO_FATAL_FAILURE(RunChild(Action::ReadOnly));
    EXPECT_EQ(kSentinel, *tid_);
}

TEST_F(ClearChildTid, ReadOnlyAddressStillWakesWaiter) {
    ASSERT_NO_FATAL_FAILURE(StartChild(Action::ReadOnlyWake));
    timespec timeout = {5, 0};
    // Publish only immediately before the syscall; never sleep in this phase
    // for any other reason. The child observes /proc state S before exiting.
    __atomic_store_n(&wait_phase_, 1, __ATOMIC_RELEASE);
    long waited = syscall(SYS_futex, tid_, FUTEX_WAIT, kSentinel, &timeout, nullptr, 0);
    __atomic_store_n(&wait_phase_, 2, __ATOMIC_RELEASE);
    int wait_error = errno;
    ASSERT_NO_FATAL_FAILURE(WaitChild());
    EXPECT_EQ(0, waited) << "futex wait errno=" << wait_error;
    EXPECT_EQ(kSentinel, *tid_);
}

TEST_F(ClearChildTid, UnmappedAddressDoesNotPreventExit) {
    ASSERT_NO_FATAL_FAILURE(RunChild(Action::Unmapped));
    // CLONE_VM made the unmap visible here as well. Do not dereference tid_.
    tid_ = static_cast<int*>(MAP_FAILED);
}

TEST_F(ClearChildTid, SuccessfulExecClearsTidInOldMm) {
#if defined(__x86_64__)
    // A tiny ET_EXEC runs exit(0) without libc registering another TID address.
    // Build the standard ELF headers explicitly instead of opaque header bytes.
    struct Image {
        Elf64_Ehdr ehdr;
        Elf64_Phdr phdr;
        unsigned char code[9];
    } image = {};
    memcpy(image.ehdr.e_ident, ELFMAG, SELFMAG);
    image.ehdr.e_ident[EI_CLASS] = ELFCLASS64;
    image.ehdr.e_ident[EI_DATA] = ELFDATA2LSB;
    image.ehdr.e_ident[EI_VERSION] = EV_CURRENT;
    image.ehdr.e_type = ET_EXEC;
    image.ehdr.e_machine = EM_X86_64;
    image.ehdr.e_version = EV_CURRENT;
    image.ehdr.e_entry = 0x400000 + offsetof(Image, code);
    image.ehdr.e_phoff = offsetof(Image, phdr);
    image.ehdr.e_ehsize = sizeof(Elf64_Ehdr);
    image.ehdr.e_phentsize = sizeof(Elf64_Phdr);
    image.ehdr.e_phnum = 1;
    image.phdr.p_type = PT_LOAD;
    image.phdr.p_flags = PF_R | PF_X;
    image.phdr.p_vaddr = 0x400000;
    image.phdr.p_filesz = sizeof(image);
    image.phdr.p_memsz = sizeof(image);
    image.phdr.p_align = 4096;
    const unsigned char code[] = {0x31, 0xff, 0xb8, 0x3c, 0, 0, 0, 0x0f, 0x05};
    memcpy(image.code, code, sizeof(code));
    ASSERT_NO_FATAL_FAILURE(WriteExecutable(&image, sizeof(image)));
    ASSERT_NO_FATAL_FAILURE(RunChild(Action::Exec));
    EXPECT_EQ(0, *tid_) << "exec must notify users of the old mm before detaching";
#else
    GTEST_SKIP() << "raw exec helper currently targets x86_64";
#endif
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
