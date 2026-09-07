#include <gtest/gtest.h>

#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>
#include <vector>

namespace {

constexpr size_t kPageSize = 4096;
// Entire pages belong to these arrays. MADV_DONTNEED must never touch adjacent
// runtime globals. Nonzero initializers keep both pages in the ELF file.
alignas(kPageSize) volatile unsigned char g_data[2][kPageSize] = {{0x59}, {0xa6}};
alignas(kPageSize) volatile unsigned char g_bss[3 * kPageSize];
constexpr char kChildMode[] = "--elf-file-backing-child";

bool check_bytes(const volatile unsigned char* data, size_t size,
                 unsigned char first) {
    if (data[0] != first) {
        return false;
    }
    for (size_t i = 1; i < size; ++i) {
        if (data[i] != 0) {
            return false;
        }
    }
    return true;
}

int discard(volatile unsigned char* data) {
    return madvise(const_cast<unsigned char*>(data), kPageSize, MADV_DONTNEED);
}

// Derive the load bias from the file segment containing the program headers;
// this works for both ET_EXEC and PIE without depending on /proc/maps text.
bool read_original_data(unsigned char* bytes) {
    int fd = open("/proc/self/exe", O_RDONLY);
    if (fd < 0) {
        return false;
    }
    Elf64_Ehdr header = {};
    bool ok = pread(fd, &header, sizeof(header), 0) == sizeof(header) &&
              header.e_phentsize == sizeof(Elf64_Phdr) && header.e_phnum > 0;
    std::vector<Elf64_Phdr> phdrs(ok ? header.e_phnum : 0);
    if (ok) {
        ok = pread(fd, phdrs.data(), phdrs.size() * sizeof(Elf64_Phdr),
                   header.e_phoff) ==
             static_cast<ssize_t>(phdrs.size() * sizeof(Elf64_Phdr));
    }
    uintptr_t bias = 0;
    bool found_bias = false;
    for (const auto& phdr : phdrs) {
        if (phdr.p_type == PT_LOAD && header.e_phoff >= phdr.p_offset &&
            header.e_phoff - phdr.p_offset < phdr.p_filesz) {
            bias = getauxval(AT_PHDR) -
                   (phdr.p_vaddr + header.e_phoff - phdr.p_offset);
            found_bias = true;
            break;
        }
    }
    bool found_data = false;
    const uintptr_t address = reinterpret_cast<uintptr_t>(&g_data[0][0]);
    for (const auto& phdr : phdrs) {
        if (!ok || !found_bias || phdr.p_type != PT_LOAD ||
            address < bias + phdr.p_vaddr) {
            continue;
        }
        const uintptr_t offset = address - bias - phdr.p_vaddr;
        if (offset <= phdr.p_filesz && kPageSize <= phdr.p_filesz - offset) {
            found_data = (phdr.p_flags & PF_W) != 0 &&
                         pread(fd, bytes, kPageSize, phdr.p_offset + offset) ==
                             static_cast<ssize_t>(kPageSize);
            break;
        }
    }
    close(fd);
    return found_data;
}

int run_child(const char* mode) {
    if (sysconf(_SC_PAGESIZE) != static_cast<long>(kPageSize)) {
        return 10;
    }
    if (strcmp(mode, "initialized") == 0) {
        if (!check_bytes(g_data[0], kPageSize, 0x59) ||
            !check_bytes(g_data[1], kPageSize, 0xa6)) {
            return 11;
        }
        if (discard(g_data[0]) != 0 || discard(g_data[1]) != 0) {
            return 12;
        }
        return check_bytes(g_data[0], kPageSize, 0x59) &&
                       check_bytes(g_data[1], kPageSize, 0xa6)
                   ? 0 : 13;
    }
    if (strcmp(mode, "private") == 0) {
        unsigned char original[kPageSize];
        if (!read_original_data(original) ||
            !check_bytes(original, kPageSize, 0x59)) {
            return 20;
        }
        g_data[0][0] = 0x31;
        g_data[0][kPageSize - 1] = 0x32;
        if (!read_original_data(original) ||
            !check_bytes(original, kPageSize, 0x59)) {
            return 21;
        }
        if (discard(g_data[0]) != 0 ||
            !check_bytes(g_data[0], kPageSize, 0x59)) {
            return 22;
        }
        return 0;
    }
    if (strcmp(mode, "fork") == 0) {
        g_data[0][0] = 0x41;
        pid_t child = fork();
        if (child < 0) {
            return 30;
        }
        if (child == 0) {
            if (g_data[0][0] != 0x41) {
                _exit(31);
            }
            g_data[0][0] = 0x42;
            _exit(discard(g_data[0]) == 0 &&
                          check_bytes(g_data[0], kPageSize, 0x59)
                      ? 0 : 32);
        }
        int status = 0;
        if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
            WEXITSTATUS(status) != 0 || g_data[0][0] != 0x41) {
            return 33;
        }
        return discard(g_data[0]) == 0 &&
                       check_bytes(g_data[0], kPageSize, 0x59)
                   ? 0 : 34;
    }
    if (strcmp(mode, "anonymous") == 0) {
        auto* data = static_cast<unsigned char*>(
            mmap(nullptr, kPageSize, PROT_READ | PROT_WRITE,
                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
        if (data == MAP_FAILED) {
            return 40;
        }
        memset(data, 0x71, kPageSize);
        bool ok = discard(data) == 0 && check_bytes(data, kPageSize, 0);
        return munmap(data, kPageSize) == 0 && ok ? 0 : 41;
    }
    if (strcmp(mode, "bss") == 0) {
        if (!check_bytes(g_bss, sizeof(g_bss), 0)) {
            return 50;
        }
        // An interior full page belongs to anonymous BSS, not its file tail.
        g_bss[kPageSize] = 0x51;
        g_bss[2 * kPageSize - 1] = 0x52;
        return discard(g_bss + kPageSize) == 0 &&
                       check_bytes(g_bss, sizeof(g_bss), 0)
                   ? 0 : 51;
    }
    return 99;
}

void expect_exec_mode(const char* mode, const char* path = "/proc/self/exe") {
    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        execl(path, path, kChildMode, mode, nullptr);
        _exit(100);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status)) << "mode=" << mode << " status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status)) << "mode=" << mode;
}

// Keep mount and mapping lifetimes in the fixture so failed assertions also
// release them. Each test owns a unique mount point and a single payload file.
class RamfsFileBacking : public ::testing::Test {
  protected:
    void SetUp() override {
        ASSERT_NE(nullptr, mkdtemp(directory_)) << strerror(errno);
        directory_created_ = true;
        if (mount("none", directory_, "ramfs", 0, nullptr) != 0) {
            const int error = errno;
            if (error == EPERM || error == EACCES || error == ENODEV) {
                GTEST_SKIP() << "ramfs mount unavailable: " << strerror(error);
            }
            FAIL() << "mount ramfs: " << strerror(error);
        }
        mounted_ = true;
        path_ = std::string(directory_) + "/payload";
    }

    void TearDown() override {
        if (private_ != MAP_FAILED) {
            EXPECT_EQ(0, munmap(private_, 2 * kPageSize));
        }
        if (shared_ != MAP_FAILED) {
            EXPECT_EQ(0, munmap(shared_, 2 * kPageSize));
        }
        if (fd_ >= 0) {
            EXPECT_EQ(0, close(fd_));
        }
        if (mounted_) {
            if (unlink((std::string(directory_) + "/link").c_str()) != 0) {
                EXPECT_EQ(ENOENT, errno);
            }
            if (unlink(path_.c_str()) != 0) {
                EXPECT_EQ(ENOENT, errno);
            }
            EXPECT_EQ(0, umount(directory_)) << strerror(errno);
        }
        if (directory_created_) {
            EXPECT_EQ(0, rmdir(directory_)) << strerror(errno);
        }
    }

    void create_data_file() {
        fd_ = open(path_.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600);
        ASSERT_GE(fd_, 0) << strerror(errno);
        std::vector<unsigned char> contents(2 * kPageSize, 0x35);
        ASSERT_EQ(static_cast<ssize_t>(contents.size()),
                  write(fd_, contents.data(), contents.size()));
    }

    char directory_[64] = "/tmp/dunitest_ramfs_backing_XXXXXX";
    std::string path_;
    bool directory_created_ = false;
    bool mounted_ = false;
    int fd_ = -1;
    void* private_ = MAP_FAILED;
    void* shared_ = MAP_FAILED;
};

}  // namespace

TEST(ElfFileBacking, InitializedWritablePagesRestoreAfterDontNeed) {
    expect_exec_mode("initialized");
}

TEST(ElfFileBacking, PrivateWritesDoNotChangeFileAndAreDiscarded) {
    expect_exec_mode("private");
}

TEST(ElfFileBacking, ForkPreservesPrivateIsolationAndFileBacking) {
    expect_exec_mode("fork");
}

TEST(ElfFileBacking, AnonymousDontNeedStillZeroFills) {
    expect_exec_mode("anonymous");
}

TEST(ElfFileBacking, BssStartsZeroAndFullPageRemainsAnonymous) {
    expect_exec_mode("bss");
}

namespace {

void expect_bss_fixture(bool has_file_tail, bool readonly_tail = false) {
#if !defined(__x86_64__)
    GTEST_SKIP() << "fixture contains x86_64 instructions";
#else
    // A fixed minimal ELF: one RX header/code page and one unaligned data
    // segment. The normal entry checks every BSS byte, then exits via syscall.
    std::vector<unsigned char> image(has_file_tail ? 2 * kPageSize : kPageSize, 0);
    Elf64_Ehdr eh = {};
    memcpy(eh.e_ident, ELFMAG, SELFMAG);
    eh.e_ident[EI_CLASS] = ELFCLASS64;
    eh.e_ident[EI_DATA] = ELFDATA2LSB;
    eh.e_ident[EI_VERSION] = EV_CURRENT;
    eh.e_type = ET_EXEC;
    eh.e_machine = EM_X86_64;
    eh.e_version = EV_CURRENT;
    eh.e_entry = 0x400100;
    eh.e_phoff = sizeof(eh);
    eh.e_ehsize = sizeof(eh);
    eh.e_phentsize = sizeof(Elf64_Phdr);
    eh.e_phnum = 2;
    Elf64_Phdr ph[2] = {};
    ph[0].p_type = PT_LOAD;
    ph[0].p_flags = PF_R | PF_X;
    ph[0].p_vaddr = 0x400000;
    ph[0].p_filesz = ph[0].p_memsz = kPageSize;
    ph[0].p_align = kPageSize;
    ph[1].p_type = PT_LOAD;
    ph[1].p_flags = readonly_tail ? PF_R : PF_R | PF_W;
    ph[1].p_offset = has_file_tail ? 0x1123 : 0x123;
    ph[1].p_filesz = has_file_tail ? 17 : 0;
    ph[1].p_vaddr = 0x600123;
    ph[1].p_memsz = 2 * kPageSize;
    ph[1].p_align = kPageSize;
    unsigned char code[] = {
        0xbe, 0x23, 0x01, 0x60, 0x00,  // mov $0x600123,%esi
        0xb9, 0x00, 0x20, 0x00, 0x00,  // mov $8192,%ecx
        0x80, 0x3e, 0x00,              // loop: cmpb $0,(%rsi)
        0x75, 0x10,                    // jne fail
        0x48, 0xff, 0xc6,              // inc %rsi
        0xff, 0xc9,                    // dec %ecx
        0x75, 0xf4,                    // jnz loop
        0x31, 0xff,                    // xor %edi,%edi
        0xb8, 0x3c, 0x00, 0x00, 0x00,  // mov $60,%eax
        0x0f, 0x05,                    // syscall
        0xbf, 0x3d, 0x00, 0x00, 0x00,  // fail: mov $61,%edi
        0xb8, 0x3c, 0x00, 0x00, 0x00,  // mov $60,%eax
        0x0f, 0x05,                    // syscall
    };
    memcpy(image.data(), &eh, sizeof(eh));
    memcpy(image.data() + eh.e_phoff, ph, sizeof(ph));
    size_t code_offset = 0x100;
    if (has_file_tail) {
        // Bytes beyond p_filesz are deliberately nonzero in the ELF file;
        // correct initial BSS therefore requires the loader's private clear.
        memset(image.data() + kPageSize, 0xa5, kPageSize);
        memset(image.data() + ph[1].p_offset, 0x59, ph[1].p_filesz);
        constexpr unsigned char check_last_file_byte[] = {
            0xbe, 0x33, 0x01, 0x60, 0x00,  // mov $0x600133,%esi
            0x80, 0x3e, 0x59,              // cmpb $0x59,(%rsi)
            0x75, 0x1f,                    // jne shared fail label
        };
        memcpy(image.data() + code_offset, check_last_file_byte,
               sizeof(check_last_file_byte));
        code_offset += sizeof(check_last_file_byte);
        code[1] = 0x34;  // BSS starts at 0x600123 + 17.
        code[6] = 0xef;  // 8192 - 17 bytes of BSS.
        code[7] = 0x1f;
    }
    memcpy(image.data() + code_offset, code, sizeof(code));
    if (readonly_tail) {
        // Loading this image attempts padzero through a read-only mapping.
        // Its entry does not read the tail: Linux ignores that padzero error.
        constexpr unsigned char exit_zero[] = {
            0x31, 0xff, 0xb8, 0x3c, 0x00, 0x00, 0x00, 0x0f, 0x05,
        };
        memcpy(image.data() + 0x100, exit_zero, sizeof(exit_zero));
    }
    char path[] = "/tmp/elf_bss_XXXXXX";
    int fd = mkstemp(path);
    ASSERT_GE(fd, 0) << strerror(errno);
    ssize_t written = write(fd, image.data(), image.size());
    int chmod_result = fchmod(fd, 0755);
    int close_result = close(fd);
    if (written != static_cast<ssize_t>(image.size()) || chmod_result != 0 ||
        close_result != 0) {
        unlink(path);
        FAIL() << "could not write ELF fixture";
    }
    pid_t child = fork();
    if (child == 0) {
        execl(path, path, nullptr);
        _exit(100);
    }
    int status = 0;
    pid_t waited = child < 0 ? -1 : waitpid(child, &status, 0);
    EXPECT_EQ(0, unlink(path));
    ASSERT_GE(child, 0);
    ASSERT_EQ(child, waited);
    if (readonly_tail && WIFSIGNALED(status)) {
        // This safety regression permits a fatal committed exec failure;
        // it does not require adopting Linux's ignored-padzero policy here.
        EXPECT_TRUE(WTERMSIG(status) == SIGSEGV || WTERMSIG(status) == SIGBUS)
            << "status=" << status;
    } else {
        ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
        EXPECT_EQ(0, WEXITSTATUS(status));
    }
#endif
}

}  // namespace

TEST(ElfFileBacking, PureBssWithUnalignedStartIsMappedAndZero) {
    expect_bss_fixture(false);
}

TEST(ElfFileBacking, UnalignedFileTailPreservesDataAndZeroFillsBss) {
    expect_bss_fixture(true);
}

TEST(ElfFileBacking, ReadOnlyBssTailFaultDoesNotCrashKernel) {
    expect_bss_fixture(true, true);
}

TEST(ElfFileBacking, SyscallBuffersAllowFileTailPageButRejectPagesPastEof) {
    char path[] = "/tmp/elf_file_tail_buffer_XXXXXX";
    int fd = mkstemp(path);
    ASSERT_GE(fd, 0) << strerror(errno);
    int unlink_result = unlink(path);
    const unsigned char original = 0x19;
    ssize_t written = write(fd, &original, 1);
    void* mapping = mmap(nullptr, 2 * kPageSize, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE, fd, 0);
    int close_result = close(fd);
    EXPECT_EQ(0, unlink_result);
    EXPECT_EQ(1, written);
    EXPECT_EQ(0, close_result);
    ASSERT_NE(MAP_FAILED, mapping) << strerror(errno);
    int pipe_fds[2];
    if (pipe(pipe_fds) != 0) {
        munmap(mapping, 2 * kPageSize);
        FAIL() << "pipe: " << strerror(errno);
    }
    auto* bytes = static_cast<unsigned char*>(mapping);
    // Use a pipe in both directions: unlike /dev/null, it must actually copy
    // the user buffer. Never directly dereference the page beyond file EOF.
    const int result = [&]() {
        unsigned char byte = 0x6a;
        if (write(pipe_fds[1], &byte, 1) != 1 ||
            read(pipe_fds[0], bytes + kPageSize - 1, 1) != 1) {
            return 1;
        }
        if (write(pipe_fds[1], bytes + kPageSize - 1, 1) != 1 ||
            read(pipe_fds[0], &byte, 1) != 1 || byte != 0x6a) {
            return 2;
        }
        errno = 0;
        if (write(pipe_fds[1], bytes + kPageSize, 1) != -1 || errno != EFAULT) {
            return 3;
        }
        if (write(pipe_fds[1], &byte, 1) != 1) {
            return 4;
        }
        errno = 0;
        if (read(pipe_fds[0], bytes + kPageSize, 1) != -1 || errno != EFAULT) {
            return 5;
        }
        return 0;
    }();
    EXPECT_EQ(0, close(pipe_fds[0]));
    EXPECT_EQ(0, close(pipe_fds[1]));
    EXPECT_EQ(0, munmap(mapping, 2 * kPageSize));
    EXPECT_EQ(0, result) << "file-tail syscall buffer check failed at phase " << result;
}

TEST_F(RamfsFileBacking, ExecutableRetainsFileBackingAcrossDiscardAndFork) {
    int source = open("/proc/self/exe", O_RDONLY);
    ASSERT_GE(source, 0) << strerror(errno);
    fd_ = open(path_.c_str(), O_CREAT | O_EXCL | O_WRONLY, 0755);
    if (fd_ < 0) {
        close(source);
        FAIL() << "open ramfs executable: " << strerror(errno);
    }
    char buffer[16 * 1024];
    bool copied = true;
    for (;;) {
        ssize_t count = read(source, buffer, sizeof(buffer));
        if (count < 0 && errno == EINTR) {
            continue;
        }
        if (count <= 0) {
            copied = count == 0;
            break;
        }
        ssize_t done = 0;
        while (done < count) {
            ssize_t n = write(fd_, buffer + done, count - done);
            if (n < 0 && errno == EINTR) {
                continue;
            }
            if (n <= 0) {
                copied = false;
                break;
            }
            done += n;
        }
        if (!copied) {
            break;
        }
    }
    int source_close = close(source);
    int chmod_result = fchmod(fd_, 0755);
    int target_close = close(fd_);
    fd_ = -1;  // An executable must not be held open for writing during exec.
    ASSERT_TRUE(copied);
    ASSERT_EQ(0, source_close);
    ASSERT_EQ(0, chmod_result);
    ASSERT_EQ(0, target_close);
    expect_exec_mode("initialized", path_.c_str());
    expect_exec_mode("private", path_.c_str());
    expect_exec_mode("fork", path_.c_str());
}

TEST_F(RamfsFileBacking, BufferedAndSharedWritesAgreeWhilePrivateWritesStayPrivate) {
    ASSERT_NO_FATAL_FAILURE(create_data_file());
    shared_ = mmap(nullptr, 2 * kPageSize, PROT_READ | PROT_WRITE,
                   MAP_SHARED, fd_, 0);
    ASSERT_NE(MAP_FAILED, shared_) << strerror(errno);
    private_ = mmap(nullptr, 2 * kPageSize, PROT_READ | PROT_WRITE,
                    MAP_PRIVATE, fd_, 0);
    ASSERT_NE(MAP_FAILED, private_) << strerror(errno);
    auto* shared = static_cast<volatile unsigned char*>(shared_);
    auto* private_data = static_cast<volatile unsigned char*>(private_);
    EXPECT_EQ(0x35, shared[0]);
    EXPECT_EQ(0x35, private_data[0]);
    unsigned char byte = 0x46;
    ASSERT_EQ(1, pwrite(fd_, &byte, 1, 0));
    EXPECT_EQ(0x46, shared[0]);
    EXPECT_EQ(0x46, private_data[0]);
    shared[kPageSize] = 0x57;
    ASSERT_EQ(1, pread(fd_, &byte, 1, kPageSize));
    EXPECT_EQ(0x57, byte);
    EXPECT_EQ(0x57, private_data[kPageSize]);
    private_data[0] = 0x68;
    EXPECT_EQ(0x46, shared[0]);
    ASSERT_EQ(1, pread(fd_, &byte, 1, 0));
    EXPECT_EQ(0x46, byte);
    ASSERT_EQ(0, discard(private_data));
    EXPECT_EQ(0x46, private_data[0]);
}

TEST_F(RamfsFileBacking, TruncateThenExtendDoesNotRestoreDiscardedTail) {
    ASSERT_NO_FATAL_FAILURE(create_data_file());
    shared_ = mmap(nullptr, 2 * kPageSize, PROT_READ | PROT_WRITE,
                   MAP_SHARED, fd_, 0);
    ASSERT_NE(MAP_FAILED, shared_) << strerror(errno);
    auto* shared = static_cast<volatile unsigned char*>(shared_);
    // Dirty both the retained partial page and the page removed completely.
    shared[kPageSize - 1] = 0x71;
    shared[2 * kPageSize - 1] = 0x72;
    constexpr size_t retained = kPageSize / 2 + 3;
    ASSERT_EQ(0, ftruncate(fd_, retained));
    ASSERT_EQ(0, ftruncate(fd_, 2 * kPageSize));
    std::vector<unsigned char> observed(2 * kPageSize);
    ASSERT_EQ(static_cast<ssize_t>(observed.size()),
              pread(fd_, observed.data(), observed.size(), 0));
    for (size_t i = 0; i < observed.size(); ++i) {
        const unsigned char expected = i < retained ? 0x35 : 0;
        ASSERT_EQ(expected, observed[i]) << "file offset=" << i;
        ASSERT_EQ(expected, shared[i]) << "mapped offset=" << i;
    }
}

TEST_F(RamfsFileBacking, SymlinkAndFileGrowthPreserveContents) {
    ASSERT_NO_FATAL_FAILURE(create_data_file());
    const std::string link = std::string(directory_) + "/link";
    ASSERT_EQ(0, symlink("payload", link.c_str()));
    char target[32] = {};
    ASSERT_EQ(7, readlink(link.c_str(), target, sizeof(target)));
    EXPECT_EQ(0, memcmp(target, "payload", 7));
    int linked_fd = open(link.c_str(), O_RDONLY);
    ASSERT_GE(linked_fd, 0) << strerror(errno);
    unsigned char byte = 0;
    ssize_t count = pread(linked_fd, &byte, 1, 0);
    int closed = close(linked_fd);
    ASSERT_EQ(1, count);
    ASSERT_EQ(0, closed);
    EXPECT_EQ(0x35, byte);

    // Linux ramfs does not implement fallocate; DragonOS supports mode zero.
    // When supported, require real growth without losing the existing prefix.
    if (fallocate(fd_, 0, 0, 3 * kPageSize) == 0) {
        struct stat st = {};
        ASSERT_EQ(0, fstat(fd_, &st));
        EXPECT_EQ(static_cast<off_t>(3 * kPageSize), st.st_size);
    } else {
        ASSERT_EQ(EOPNOTSUPP, errno);
    }
    ASSERT_EQ(0, ftruncate(fd_, 3 * kPageSize));
    std::vector<unsigned char> data(3 * kPageSize);
    ASSERT_EQ(static_cast<ssize_t>(data.size()),
              pread(fd_, data.data(), data.size(), 0));
    for (size_t i = 0; i < data.size(); ++i) {
        ASSERT_EQ(i < 2 * kPageSize ? 0x35 : 0, data[i]) << "offset=" << i;
    }
}

int main(int argc, char** argv) {
    if (argc == 3 && strcmp(argv[1], kChildMode) == 0) {
        return run_child(argv[2]);
    }
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
