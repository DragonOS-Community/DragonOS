#include <gtest/gtest.h>
#include <stdlib.h>
#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include <string>
#include <vector>

namespace {

// Two bounded file pages; execution only issues exit(0). Every attempted exec
// runs in a child, and the parent distinguishes errno from post-commit signals.
std::vector<unsigned char> image(unsigned count = 1) {
    std::vector<unsigned char> bytes(8192, 0);
    Elf64_Ehdr header = {};
    memcpy(header.e_ident, ELFMAG, SELFMAG);
    header.e_ident[EI_CLASS] = ELFCLASS64;
    header.e_ident[EI_DATA] = ELFDATA2LSB;
    header.e_ident[EI_VERSION] = EV_CURRENT;
    header.e_type = ET_EXEC;
    header.e_machine = EM_X86_64;
    header.e_version = EV_CURRENT;
    header.e_entry = 0x400000 + 6144;
    header.e_phoff = sizeof(header);
    header.e_ehsize = sizeof(header);
    header.e_phentsize = sizeof(Elf64_Phdr);
    header.e_phnum = count;
    memcpy(bytes.data(), &header, sizeof(header));
    Elf64_Phdr segment = {};
    segment.p_type = PT_LOAD;
    segment.p_flags = PF_R | PF_X;
    segment.p_vaddr = 0x400000;
    segment.p_filesz = segment.p_memsz = bytes.size();
    segment.p_align = 4096;
    memcpy(bytes.data() + sizeof(header), &segment, sizeof(segment));
    const unsigned char code[] = {0xb8, 60, 0, 0, 0, 0x31, 0xff, 0x0f, 0x05};
    memcpy(bytes.data() + 6144, code, sizeof(code));
    return bytes;
}

bool put(const std::string& path, const std::vector<unsigned char>& bytes,
         mode_t mode = 0700) {
    int fd = open(path.c_str(), O_WRONLY | O_CREAT | O_TRUNC, mode);
    if (fd < 0) return false;
    size_t offset = 0;
    while (offset < bytes.size()) {
        ssize_t count = write(fd, bytes.data() + offset, bytes.size() - offset);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) {
            close(fd);
            return false;
        }
        offset += count;
    }
    bool ok = fchmod(fd, mode) == 0;
    return close(fd) == 0 && ok;
}

void interp(std::vector<unsigned char>& bytes, const std::string& path,
            bool terminated = true) {
    reinterpret_cast<Elf64_Ehdr*>(bytes.data())->e_phnum = 2;
    Elf64_Phdr segment = {};
    segment.p_type = PT_INTERP;
    segment.p_offset = 1024;
    segment.p_filesz = path.size() + (terminated ? 1 : 0);
    memcpy(bytes.data() + sizeof(Elf64_Ehdr) + sizeof(Elf64_Phdr), &segment,
           sizeof(segment));
    memcpy(bytes.data() + 1024, path.data(), path.size());
}

int64_t monotonic_ns() {
    timespec now = {};
    clock_gettime(CLOCK_MONOTONIC, &now);
    return static_cast<int64_t>(now.tv_sec) * 1000000000 + now.tv_nsec;
}

bool run(const char* name, const std::string& path, int expected) {
    int pipe_fds[2];
    if (pipe2(pipe_fds, O_CLOEXEC | O_NONBLOCK) != 0) return false;
    pid_t child = fork();
    if (child < 0) {
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        return false;
    }
    if (child == 0) {
        close(pipe_fds[0]);
        char* const argv[] = {const_cast<char*>(path.c_str()), nullptr};
        char* const environment[] = {nullptr};
        execve(path.c_str(), argv, environment);
        int error = errno;
        ssize_t written;
        do {
            written = write(pipe_fds[1], &error, sizeof(error));
        } while (written < 0 && errno == EINTR);
        _exit(written == sizeof(error) ? 111 : 112);
    }
    close(pipe_fds[1]);
    int status = 0;
    bool timed_out = false;
    const auto deadline = monotonic_ns() + 2000000000LL;
    pid_t observed;
    while ((observed = waitpid(child, &status, WNOHANG)) == 0 ||
           (observed < 0 && errno == EINTR)) {
        if (monotonic_ns() > deadline) {
            timed_out = true;
            kill(child, SIGKILL);
            while (waitpid(child, &status, 0) < 0 && errno == EINTR) {}
            break;
        }
        usleep(1000);
    }
    int error = 0;
    ssize_t count = read(pipe_fds[0], &error, sizeof(error));
    close(pipe_fds[0]);
    bool ok = !timed_out && observed == child && WIFEXITED(status) &&
              (expected == 0 ? count == 0 && WEXITSTATUS(status) == 0
                             : count == sizeof(error) && error == expected &&
                                   WEXITSTATUS(status) == 111);
    if (!ok) {
        fprintf(stderr, "%s expected=%d errno=%d pipe=%zd signal=%d exit=%d timeout=%d\n",
                name, expected, error, count,
                WIFSIGNALED(status) ? WTERMSIG(status) : 0,
                WIFEXITED(status) ? WEXITSTATUS(status) : -1, timed_out);
    }
    return ok;
}

}  // namespace

class ElfMetadata : public ::testing::Test {
protected:
    std::string directory;
    std::string executable;
    std::string interpreter;
    void SetUp() override {
#if !defined(__x86_64__)
        GTEST_SKIP() << "fixture contains x86_64 executable instructions";
#endif
        char path[] = "/tmp/elf-metadata-XXXXXX";
        ASSERT_NE(mkdtemp(path), nullptr);
        directory = path;
        executable = directory + "/main";
        interpreter = directory + "/interp";
    }
    void TearDown() override {
        if (!directory.empty()) {
            unlink(executable.c_str());
            unlink(interpreter.c_str());
            rmdir(directory.c_str());
        }
    }
    void expect(std::vector<unsigned char> bytes, int error) {
        ASSERT_TRUE(put(executable, bytes));
        const char* name = ::testing::UnitTest::GetInstance()->current_test_info()->name();
        EXPECT_TRUE(run(name, executable, error));
    }
};
TEST_F(ElfMetadata, ValidStaticImage) { expect(image(), 0); }
TEST_F(ElfMetadata, RejectsProgramHeadersLargerThanPage) { expect(image(74), ENOEXEC); }
TEST_F(ElfMetadata, RejectsEmptyProgramHeadersBeforeCommit) {
    auto bytes = image();
    reinterpret_cast<Elf64_Ehdr*>(bytes.data())->e_phnum = 0;
    expect(bytes, ENOEXEC);
}
TEST_F(ElfMetadata, RejectsWrongEntrySize) {
    auto bytes = image();
    reinterpret_cast<Elf64_Ehdr*>(bytes.data())->e_phentsize = 55;
    expect(bytes, ENOEXEC);
}
TEST_F(ElfMetadata, RejectsOverflowingOffset) {
    auto bytes = image();
    reinterpret_cast<Elf64_Ehdr*>(bytes.data())->e_phoff = UINT64_MAX - 10;
    expect(bytes, ENOEXEC);
}
TEST_F(ElfMetadata, RejectsExtendedCountForExec) {
    auto bytes = image();
    auto* header = reinterpret_cast<Elf64_Ehdr*>(bytes.data());
    header->e_phnum = PN_XNUM;
    header->e_shoff = 4096;
    header->e_shentsize = sizeof(Elf64_Shdr);
    Elf64_Shdr section = {};
    section.sh_info = 1;
    memcpy(bytes.data() + 4096, &section, sizeof(section));
    expect(bytes, ENOEXEC);
}
TEST_F(ElfMetadata, RequiresInterpreterTerminalNul) {
    auto bytes = image();
    interp(bytes, "/does-not-exist-mm232X", false);
    expect(bytes, ENOEXEC);
}
TEST_F(ElfMetadata, RejectsNonElfInterpreterBeforeCommit) {
    ASSERT_TRUE(put(interpreter, std::vector<unsigned char>(64, 0x41)));
    auto bytes = image();
    interp(bytes, interpreter);
    expect(bytes, ELIBBAD);
}
TEST_F(ElfMetadata, PreservesInterpreterPermissionError) {
    ASSERT_TRUE(put(interpreter, std::vector<unsigned char>(64, 0x41), 0600));
    auto bytes = image();
    interp(bytes, interpreter);
    expect(bytes, EACCES);
}
TEST_F(ElfMetadata, RejectsInterpreterOversizeHeadersBeforeCommit) {
    ASSERT_TRUE(put(interpreter, image(74)));
    auto bytes = image();
    interp(bytes, interpreter);
    expect(bytes, ELIBBAD);
}
TEST_F(ElfMetadata, RejectsInterpreterShortHeadersBeforeCommit) {
    auto bytes = image();
    reinterpret_cast<Elf64_Ehdr*>(bytes.data())->e_phoff = 8190;
    ASSERT_TRUE(put(interpreter, bytes));
    bytes = image();
    interp(bytes, interpreter);
    expect(bytes, ELIBBAD);
}
TEST_F(ElfMetadata, PreservesInterpreterMissingError) {
    auto bytes = image();
    interp(bytes, directory + "/missing");
    expect(bytes, ENOENT);
}
TEST_F(ElfMetadata, AcceptsLargestWholeEntryTableWithinPage) {
    expect(image(4096 / sizeof(Elf64_Phdr)), 0);
}
TEST_F(ElfMetadata, UsesOnlyFirstInterpreter) {
    ASSERT_TRUE(put(interpreter, image()));
    auto bytes = image();
    interp(bytes, interpreter);
    reinterpret_cast<Elf64_Ehdr*>(bytes.data())->e_phnum = 3;
    const std::string missing = directory + "/missing";
    Elf64_Phdr extra = {};
    extra.p_type = PT_INTERP;
    extra.p_offset = 2048;
    extra.p_filesz = missing.size() + 1;
    memcpy(bytes.data() + sizeof(Elf64_Ehdr) + 2 * sizeof(Elf64_Phdr), &extra, sizeof(extra));
    memcpy(bytes.data() + 2048, missing.c_str(), missing.size() + 1);
    expect(bytes, 0);
}
TEST_F(ElfMetadata, InterpreterPathStopsAtFirstNul) {
    ASSERT_TRUE(put(interpreter, image()));
    std::string path = interpreter;
    path.push_back('\0');
    path += "ignored";
    auto bytes = image();
    interp(bytes, path);
    expect(bytes, 0);
}
TEST_F(ElfMetadata, RejectsInterpreterWrongArchitectureBeforeCommit) {
    auto bytes = image();
    reinterpret_cast<Elf64_Ehdr*>(bytes.data())->e_machine = EM_RISCV;
    ASSERT_TRUE(put(interpreter, bytes));
    bytes = image();
    interp(bytes, interpreter);
    expect(bytes, ELIBBAD);
}
TEST_F(ElfMetadata, ShortInterpreterHeaderPreservesReadError) {
    ASSERT_TRUE(put(interpreter, std::vector<unsigned char>(32, 0)));
    auto bytes = image();
    interp(bytes, interpreter);
    expect(bytes, EIO);
    // Failed pre-commit preparation must release the executable write guard.
    int fd = open(interpreter.c_str(), O_WRONLY);
    ASSERT_GE(fd, 0);
    EXPECT_EQ(close(fd), 0);
}
TEST_F(ElfMetadata, ShortInterpreterPathPreservesReadError) {
    auto bytes = image();
    interp(bytes, "/missing");
    auto* segment = reinterpret_cast<Elf64_Phdr*>(bytes.data() + sizeof(Elf64_Ehdr));
    segment[1].p_offset = bytes.size() - 2;
    expect(bytes, EIO);
}
int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
