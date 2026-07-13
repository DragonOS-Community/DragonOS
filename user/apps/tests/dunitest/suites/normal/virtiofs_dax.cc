#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#include <limits>
#include <string>
#include <vector>

namespace {

constexpr size_t kPageSize = 4096;
constexpr off_t kDaxRangeSize = 2 * 1024 * 1024;
constexpr unsigned kWaitIterations = 150;
constexpr unsigned long kMaxPressureRanges = 4096;

bool dax_required() {
    const char* value = getenv("DRAGONOS_VIRTIOFS_DAX_REQUIRED");
    if (value == nullptr) {
        return false;
    }
    return strcmp(value, "1") == 0 || strcmp(value, "y") == 0 || strcmp(value, "Y") == 0 ||
           strcmp(value, "yes") == 0 || strcmp(value, "YES") == 0 ||
           strcmp(value, "true") == 0 || strcmp(value, "TRUE") == 0 ||
           strcmp(value, "on") == 0 || strcmp(value, "ON") == 0;
}

int ensure_dir(const char* path) {
    struct stat st = {};
    if (stat(path, &st) == 0) {
        return S_ISDIR(st.st_mode) ? 0 : -1;
    }
    return mkdir(path, 0755);
}

std::string read_all(const char* path) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        return {};
    }

    std::string result;
    char buffer[1024];
    for (;;) {
        ssize_t n = read(fd, buffer, sizeof(buffer));
        if (n == 0) {
            break;
        }
        if (n < 0) {
            result.clear();
            break;
        }
        result.append(buffer, static_cast<size_t>(n));
    }
    close(fd);
    return result;
}

long long parse_counter(const std::string& stats, unsigned opcode) {
    std::string field = "opcode_" + std::to_string(opcode) + "_requests_total ";
    size_t pos = stats.find(field);
    if (pos == std::string::npos) {
        return -1;
    }
    pos += field.size();
    char* end = nullptr;
    long long value = strtoll(stats.c_str() + pos, &end, 10);
    return end == stats.c_str() + pos ? -1 : value;
}

long long parse_named_counter(const std::string& stats, const char* name) {
    std::string field = std::string(name) + " ";
    size_t pos = stats.find(field);
    if (pos == std::string::npos) {
        return -1;
    }
    pos += field.size();
    char* end = nullptr;
    long long value = strtoll(stats.c_str() + pos, &end, 10);
    return end == stats.c_str() + pos ? -1 : value;
}

struct OpcodeSnapshot {
    long long read = -1;
    long long write = -1;
    long long setup_mapping = -1;
    long long remove_mapping = -1;
    long long mapping_created = -1;
    long long mapping_removed = -1;
    long long pressure_reclaims = -1;
    long long device_resets = -1;
};

OpcodeSnapshot snapshot(const char* stats_path) {
    std::string contents = read_all(stats_path);
    OpcodeSnapshot result;
    result.read = parse_counter(contents, 15);
    result.write = parse_counter(contents, 16);
    result.setup_mapping = parse_counter(contents, 48);
    result.remove_mapping = parse_counter(contents, 49);
    result.mapping_created = parse_named_counter(contents, "dax_mapping_created_total");
    result.mapping_removed = parse_named_counter(contents, "dax_mapping_removed_total");
    result.pressure_reclaims = parse_named_counter(contents, "dax_pressure_reclaims_total");
    result.device_resets = parse_named_counter(contents, "dax_device_resets_total");
    return result;
}

void assert_valid_snapshot(const OpcodeSnapshot& value) {
    ASSERT_GE(value.read, 0);
    ASSERT_GE(value.write, 0);
    ASSERT_GE(value.setup_mapping, 0);
    ASSERT_GE(value.remove_mapping, 0);
    ASSERT_GE(value.mapping_created, 0);
    ASSERT_GE(value.mapping_removed, 0);
    ASSERT_GE(value.pressure_reclaims, 0);
    ASSERT_GE(value.device_resets, 0);
}

void expect_dax_data_path(const OpcodeSnapshot& before, const OpcodeSnapshot& after,
                          long long minimum_setups = 1) {
    assert_valid_snapshot(before);
    assert_valid_snapshot(after);
    EXPECT_GE(after.setup_mapping - before.setup_mapping, minimum_setups);
    EXPECT_EQ(before.read, after.read);
    EXPECT_EQ(before.write, after.write);
}

OpcodeSnapshot wait_for_dax_reset(const char* stats_path, long long before) {
    OpcodeSnapshot current;
    for (unsigned i = 0; i < kWaitIterations; ++i) {
        current = snapshot(stats_path);
        if (current.device_resets > before) {
            break;
        }
        usleep(100000);
    }
    return current;
}

uint8_t pattern_byte(off_t offset) {
    return static_cast<uint8_t>((static_cast<uint64_t>(offset) * 131 + 17) % 251);
}

bool write_pattern_file(const char* path, off_t size) {
    int fd = open(path, O_CREAT | O_TRUNC | O_RDWR, 0644);
    if (fd < 0) {
        return false;
    }

    std::vector<uint8_t> buffer(64 * 1024);
    for (off_t offset = 0; offset < size;) {
        size_t length = static_cast<size_t>(size - offset);
        if (length > buffer.size()) {
            length = buffer.size();
        }
        for (size_t i = 0; i < length; ++i) {
            buffer[i] = pattern_byte(offset + static_cast<off_t>(i));
        }
        ssize_t written = pwrite(fd, buffer.data(), length, offset);
        if (written != static_cast<ssize_t>(length)) {
            close(fd);
            return false;
        }
        offset += written;
    }
    bool ok = fsync(fd) == 0 && close(fd) == 0;
    return ok;
}

bool write_pressure_file(const char* path, off_t size, unsigned long ranges) {
    int fd = open(path, O_CREAT | O_TRUNC | O_RDWR, 0644);
    if (fd < 0) {
        return false;
    }
    bool ok = ftruncate(fd, size) == 0;
    for (unsigned long i = 0; ok && i <= ranges; ++i) {
        off_t offset = static_cast<off_t>(i) * kDaxRangeSize + 17;
        uint8_t value = pattern_byte(offset);
        ok = pwrite(fd, &value, 1, offset) == 1;
    }
    if (ok && fsync(fd) != 0) {
        ok = false;
    }
    if (close(fd) != 0) {
        ok = false;
    }
    return ok;
}

bool wait_for_child(pid_t child, int* status, unsigned iterations = kWaitIterations) {
    for (unsigned i = 0; i < iterations; ++i) {
        pid_t result = waitpid(child, status, WNOHANG);
        if (result == child) {
            return true;
        }
        if (result < 0) {
            return false;
        }
        usleep(100000);
    }
    kill(child, SIGKILL);
    waitpid(child, status, 0);
    return false;
}

struct TestPaths {
    char root[160] = {};
    char mountpoint[192] = {};
    char debugfs[192] = {};
    char stats[224] = {};
    char file[256] = {};

    TestPaths(const char* suffix) {
        snprintf(root, sizeof(root), "/tmp/virtiofs_dax_%s_%d", suffix, getpid());
        snprintf(mountpoint, sizeof(mountpoint), "%s/mnt", root);
        snprintf(debugfs, sizeof(debugfs), "%s/debug", root);
        snprintf(stats, sizeof(stats), "%s/fuse/stats", debugfs);
        snprintf(file, sizeof(file), "%s/dax_test.bin", mountpoint);
    }

    bool create() const {
        return ensure_dir("/tmp") == 0 && ensure_dir(root) == 0 && ensure_dir(mountpoint) == 0 &&
               ensure_dir(debugfs) == 0;
    }

    void cleanup() const {
        umount(mountpoint);
        umount(debugfs);
        rmdir(mountpoint);
        rmdir(debugfs);
        rmdir(root);
    }
};

bool mount_virtiofs(const char* mountpoint, const char* dax_mode, int* error);

struct CleanupGuard {
    const TestPaths& paths;

    ~CleanupGuard() {
        // Fatal gtest assertions return immediately. Keep those paths from
        // leaking mounts or host-share fixtures into subsequent runs.
        umount(paths.mountpoint);
        int ignored_error = 0;
        if (mount_virtiofs(paths.mountpoint, "never", &ignored_error)) {
            unlink(paths.file);
            umount(paths.mountpoint);
        }
        umount(paths.debugfs);
        rmdir(paths.mountpoint);
        rmdir(paths.debugfs);
        rmdir(paths.root);
    }
};

struct ExtraMountGuard {
    const char* mountpoint;
    bool mounted = false;

    ~ExtraMountGuard() {
        if (mounted) {
            umount(mountpoint);
        }
    }
};

bool mount_virtiofs(const char* mountpoint, const char* dax_mode, int* error) {
    char options[64] = {};
    snprintf(options, sizeof(options), "dax=%s", dax_mode);
    if (mount("hostshare", mountpoint, "virtiofs", 0, options) == 0) {
        return true;
    }
    *error = errno;
    return false;
}

bool prepare_non_dax_file(const TestPaths& paths, off_t size, int* error) {
    if (!mount_virtiofs(paths.mountpoint, "never", error)) {
        return false;
    }
    bool ok = write_pattern_file(paths.file, size);
    if (!ok) {
        *error = errno;
    }
    if (umount(paths.mountpoint) != 0) {
        if (ok) {
            *error = errno;
        }
        ok = false;
    }
    return ok;
}

bool prepare_non_dax_pressure_file(const TestPaths& paths, off_t size, unsigned long ranges,
                                   int* error) {
    if (!mount_virtiofs(paths.mountpoint, "never", error)) {
        return false;
    }
    bool ok = write_pressure_file(paths.file, size, ranges);
    if (!ok) {
        *error = errno;
    }
    if (umount(paths.mountpoint) != 0) {
        if (ok) {
            *error = errno;
        }
        ok = false;
    }
    return ok;
}

void remove_non_dax_file(const TestPaths& paths) {
    int error = 0;
    if (mount_virtiofs(paths.mountpoint, "never", &error)) {
        unlink(paths.file);
        umount(paths.mountpoint);
    }
}

bool mount_debugfs(const TestPaths& paths, int* error) {
    if (mount("none", paths.debugfs, "debugfs", 0, nullptr) == 0) {
        return true;
    }
    *error = errno;
    return false;
}

void expect_child_signal(const char* path, off_t offset, int prot, int flags, int signal) {
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        int fd = open(path, O_RDONLY);
        if (fd < 0) {
            _exit(101);
        }
        void* mapping = mmap(nullptr, kPageSize, prot, flags, fd, offset);
        if (mapping == MAP_FAILED) {
            _exit(102);
        }
        volatile uint8_t* byte = static_cast<volatile uint8_t*>(mapping);
        if (signal == SIGSEGV) {
            *byte = static_cast<uint8_t>(*byte + 1);
        } else {
            (void)*byte;
        }
        _exit(103);
    }

    int status = 0;
    ASSERT_TRUE(wait_for_child(child, &status)) << "child timed out";
    ASSERT_TRUE(WIFSIGNALED(status)) << "child status=" << status;
    EXPECT_EQ(signal, WTERMSIG(status));
}

void verify_zero_range(int fd, off_t offset, size_t length) {
    std::vector<uint8_t> buffer(length, 0xff);
    ASSERT_EQ(static_cast<ssize_t>(length), pread(fd, buffer.data(), length, offset));
    for (size_t i = 0; i < length; ++i) {
        ASSERT_EQ(0, buffer[i]) << "non-zero sparse byte at " << offset + i;
    }
}

}  // namespace

TEST(VirtioFsDax, IoMmapPermissionsEofAndTeardown) {
    TestPaths paths("io");
    ASSERT_TRUE(paths.create());
    CleanupGuard cleanup{paths};

    int error = 0;
    constexpr off_t initial_size = 8 * kDaxRangeSize + 123;
    if (!prepare_non_dax_file(paths, initial_size, &error)) {
        paths.cleanup();
        if (dax_required()) {
            FAIL() << "DAX is required but ordinary virtiofs preparation failed: errno=" << error
                   << " (" << strerror(error) << ")";
        }
        GTEST_SKIP() << "ordinary virtiofs is unavailable: errno=" << error << " ("
                     << strerror(error) << ")";
    }
    ASSERT_TRUE(mount_debugfs(paths, &error)) << strerror(error);

    if (!mount_virtiofs(paths.mountpoint, "always", &error)) {
        remove_non_dax_file(paths);
        paths.cleanup();
        if (dax_required()) {
            FAIL() << "DAX is required but dax=always mount failed: errno=" << error << " ("
                   << strerror(error) << ")";
        }
        GTEST_SKIP() << "DAX window/backend is unavailable in optional mode: errno=" << error
                     << " (" << strerror(error) << ")";
    }

    // The stats file is global. This test runs with an exclusive virtiofs
    // connection and takes the baseline before creating the first DAX mapping.
    OpcodeSnapshot lifecycle_before = snapshot(paths.stats);
    assert_valid_snapshot(lifecycle_before);

    int fd = open(paths.file, O_RDWR);
    ASSERT_GE(fd, 0) << strerror(errno);

    OpcodeSnapshot before = snapshot(paths.stats);
    ASSERT_EQ(0, lseek(fd, 64, SEEK_SET));
    uint8_t read_buffer[64] = {};
    ASSERT_EQ(static_cast<ssize_t>(sizeof(read_buffer)), read(fd, read_buffer, sizeof(read_buffer)));
    for (size_t i = 0; i < sizeof(read_buffer); ++i) {
        EXPECT_EQ(pattern_byte(64 + static_cast<off_t>(i)), read_buffer[i]);
    }
    expect_dax_data_path(before, snapshot(paths.stats));

    constexpr off_t cross_offset = 2 * kDaxRangeSize - 32;
    before = snapshot(paths.stats);
    ASSERT_EQ(static_cast<ssize_t>(sizeof(read_buffer)),
              pread(fd, read_buffer, sizeof(read_buffer), cross_offset));
    for (size_t i = 0; i < sizeof(read_buffer); ++i) {
        EXPECT_EQ(pattern_byte(cross_offset + static_cast<off_t>(i)), read_buffer[i]);
    }
    expect_dax_data_path(before, snapshot(paths.stats), 2);

    constexpr off_t mmap_offset = 3 * kDaxRangeSize;
    before = snapshot(paths.stats);
    void* read_mapping = mmap(nullptr, kPageSize, PROT_READ, MAP_PRIVATE, fd, mmap_offset);
    ASSERT_NE(MAP_FAILED, read_mapping) << strerror(errno);
    EXPECT_EQ(pattern_byte(mmap_offset + 37), static_cast<uint8_t*>(read_mapping)[37]);
    ASSERT_EQ(0, munmap(read_mapping, kPageSize));
    expect_dax_data_path(before, snapshot(paths.stats));

    static const char inplace_data[] = "dax-in-place-write";
    constexpr off_t inplace_offset = 4 * kDaxRangeSize + 100;
    before = snapshot(paths.stats);
    ASSERT_EQ(static_cast<ssize_t>(sizeof(inplace_data)),
              pwrite(fd, inplace_data, sizeof(inplace_data), inplace_offset));
    expect_dax_data_path(before, snapshot(paths.stats));

    static const char shared_data[] = "dax-shared-mmap";
    constexpr off_t shared_page = 5 * kDaxRangeSize;
    before = snapshot(paths.stats);
    void* shared_mapping = mmap(nullptr, kPageSize, PROT_READ | PROT_WRITE, MAP_SHARED, fd,
                                shared_page);
    ASSERT_NE(MAP_FAILED, shared_mapping) << strerror(errno);
    memcpy(static_cast<uint8_t*>(shared_mapping) + 200, shared_data, sizeof(shared_data));
    ASSERT_EQ(0, msync(shared_mapping, kPageSize, MS_SYNC)) << strerror(errno);
    ASSERT_EQ(0, munmap(shared_mapping, kPageSize));
    expect_dax_data_path(before, snapshot(paths.stats));

    constexpr off_t private_page = 6 * kDaxRangeSize;
    uint8_t private_original = pattern_byte(private_page + 300);
    before = snapshot(paths.stats);
    int readonly_fd = open(paths.file, O_RDONLY);
    ASSERT_GE(readonly_fd, 0);
    void* private_mapping = mmap(nullptr, kPageSize, PROT_READ | PROT_WRITE, MAP_PRIVATE,
                                 readonly_fd, private_page);
    ASSERT_NE(MAP_FAILED, private_mapping) << strerror(errno);
    EXPECT_EQ(private_original, static_cast<uint8_t*>(private_mapping)[300]);
    static_cast<uint8_t*>(private_mapping)[300] ^= 0x5a;
    EXPECT_NE(private_original, static_cast<uint8_t*>(private_mapping)[300]);
    ASSERT_EQ(0, munmap(private_mapping, kPageSize));
    expect_dax_data_path(before, snapshot(paths.stats));

    errno = 0;
    void* denied = mmap(nullptr, kPageSize, PROT_READ | PROT_WRITE, MAP_SHARED, readonly_fd, 0);
    EXPECT_EQ(MAP_FAILED, denied);
    EXPECT_EQ(EACCES, errno);

    void* protected_mapping = mmap(nullptr, kPageSize, PROT_READ, MAP_SHARED, readonly_fd, 0);
    ASSERT_NE(MAP_FAILED, protected_mapping);
    errno = 0;
    EXPECT_EQ(-1, mprotect(protected_mapping, kPageSize, PROT_READ | PROT_WRITE));
    EXPECT_EQ(EACCES, errno);
    ASSERT_EQ(0, munmap(protected_mapping, kPageSize));
    ASSERT_EQ(0, close(readonly_fd));
    expect_child_signal(paths.file, 0, PROT_READ, MAP_SHARED, SIGSEGV);

    off_t final_page = initial_size & ~static_cast<off_t>(kPageSize - 1);
    void* eof_tail = mmap(nullptr, kPageSize, PROT_READ, MAP_PRIVATE, fd, final_page);
    ASSERT_NE(MAP_FAILED, eof_tail) << strerror(errno);
    EXPECT_EQ(pattern_byte(initial_size - 1), static_cast<uint8_t*>(eof_tail)[122]);
    for (size_t i = 123; i < kPageSize; ++i) {
        ASSERT_EQ(0, static_cast<uint8_t*>(eof_tail)[i]) << "offset in EOF tail=" << i;
    }
    ASSERT_EQ(0, munmap(eof_tail, kPageSize));
    expect_child_signal(paths.file, final_page + kPageSize, PROT_READ, MAP_PRIVATE, SIGBUS);

    static const char extension[] = "regular-fuse-extension";
    before = snapshot(paths.stats);
    ASSERT_EQ(static_cast<ssize_t>(sizeof(extension)),
              pwrite(fd, extension, sizeof(extension), initial_size));
    OpcodeSnapshot after = snapshot(paths.stats);
    assert_valid_snapshot(before);
    assert_valid_snapshot(after);
    EXPECT_GT(after.write, before.write);

    constexpr off_t sparse_gap = 2 * kPageSize + 37;
    off_t sparse_offset = initial_size + sizeof(extension) + sparse_gap;
    static const char sparse_data[] = "regular-fuse-sparse-write";
    before = snapshot(paths.stats);
    ASSERT_EQ(static_cast<ssize_t>(sizeof(sparse_data)),
              pwrite(fd, sparse_data, sizeof(sparse_data), sparse_offset));
    after = snapshot(paths.stats);
    EXPECT_GT(after.write, before.write);

    ASSERT_EQ(0, fsync(fd));
    ASSERT_EQ(0, close(fd));

    // Create one last idle owned mapping so teardown, rather than an earlier
    // pressure reclaim, must account for at least one REMOVEMAPPING request.
    fd = open(paths.file, O_RDONLY);
    ASSERT_GE(fd, 0);
    uint8_t last_byte = 0;
    ASSERT_EQ(1, pread(fd, &last_byte, 1, 7 * kDaxRangeSize + 19));
    ASSERT_EQ(0, close(fd));
    OpcodeSnapshot before_umount = snapshot(paths.stats);
    ASSERT_EQ(0, umount(paths.mountpoint)) << strerror(errno);
    OpcodeSnapshot after_umount =
        wait_for_dax_reset(paths.stats, lifecycle_before.device_resets);
    assert_valid_snapshot(before_umount);
    assert_valid_snapshot(after_umount);
    EXPECT_GT(after_umount.remove_mapping, before_umount.remove_mapping);
    EXPECT_GT(after_umount.setup_mapping, lifecycle_before.setup_mapping);
    EXPECT_GT(after_umount.remove_mapping, lifecycle_before.remove_mapping);
    long long created = after_umount.mapping_created - lifecycle_before.mapping_created;
    long long removed = after_umount.mapping_removed - lifecycle_before.mapping_removed;
    EXPECT_GT(created, 0);
    EXPECT_EQ(created, removed);
    EXPECT_GT(after_umount.device_resets, lifecycle_before.device_resets);

    ASSERT_TRUE(mount_virtiofs(paths.mountpoint, "never", &error)) << strerror(error);
    fd = open(paths.file, O_RDONLY);
    ASSERT_GE(fd, 0);
    char verify[64] = {};
    ASSERT_EQ(static_cast<ssize_t>(sizeof(inplace_data)),
              pread(fd, verify, sizeof(inplace_data), inplace_offset));
    EXPECT_EQ(0, memcmp(inplace_data, verify, sizeof(inplace_data)));
    ASSERT_EQ(static_cast<ssize_t>(sizeof(shared_data)),
              pread(fd, verify, sizeof(shared_data), shared_page + 200));
    EXPECT_EQ(0, memcmp(shared_data, verify, sizeof(shared_data)));
    uint8_t private_verify = 0;
    ASSERT_EQ(1, pread(fd, &private_verify, 1, private_page + 300));
    EXPECT_EQ(private_original, private_verify);
    struct stat st = {};
    ASSERT_EQ(0, fstat(fd, &st));
    EXPECT_EQ(sparse_offset + static_cast<off_t>(sizeof(sparse_data)), st.st_size);
    ASSERT_EQ(static_cast<ssize_t>(sizeof(extension)),
              pread(fd, verify, sizeof(extension), initial_size));
    EXPECT_EQ(0, memcmp(extension, verify, sizeof(extension)));
    verify_zero_range(fd, initial_size + sizeof(extension), sparse_gap);
    ASSERT_EQ(static_cast<ssize_t>(sizeof(sparse_data)),
              pread(fd, verify, sizeof(sparse_data), sparse_offset));
    EXPECT_EQ(0, memcmp(sparse_data, verify, sizeof(sparse_data)));
    ASSERT_EQ(0, close(fd));
    ASSERT_EQ(0, unlink(paths.file));
    ASSERT_EQ(0, umount(paths.mountpoint));
    paths.cleanup();
}

TEST(VirtioFsDax, LayoutBreakingForTruncateFallocateAndAtomicOpen) {
    TestPaths paths("layout");
    ASSERT_TRUE(paths.create());
    CleanupGuard cleanup{paths};

    int error = 0;
    constexpr off_t initial_size = 3 * kDaxRangeSize;
    if (!prepare_non_dax_file(paths, initial_size, &error)) {
        paths.cleanup();
        if (dax_required()) {
            FAIL() << "DAX layout test preparation failed: " << strerror(error);
        }
        GTEST_SKIP() << "ordinary virtiofs is unavailable: " << strerror(error);
    }
    ASSERT_TRUE(mount_debugfs(paths, &error)) << strerror(error);
    if (!mount_virtiofs(paths.mountpoint, "always", &error)) {
        remove_non_dax_file(paths);
        paths.cleanup();
        if (dax_required()) {
            FAIL() << "DAX layout test requires dax=always: " << strerror(error);
        }
        GTEST_SKIP() << "DAX window/backend is unavailable: " << strerror(error);
    }

    int fd = open(paths.file, O_RDWR);
    ASSERT_GE(fd, 0) << strerror(errno);
    void* inherited = mmap(nullptr, kPageSize, PROT_READ, MAP_SHARED, fd, kDaxRangeSize);
    ASSERT_NE(MAP_FAILED, inherited) << strerror(errno);
    EXPECT_EQ(pattern_byte(kDaxRangeSize), static_cast<uint8_t*>(inherited)[0]);

    int ready[2] = {-1, -1};
    int proceed[2] = {-1, -1};
    ASSERT_EQ(0, pipe(ready));
    ASSERT_EQ(0, pipe(proceed));
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(ready[0]);
        close(proceed[1]);
        char byte = 'r';
        if (write(ready[1], &byte, 1) != 1 || read(proceed[0], &byte, 1) != 1) {
            _exit(101);
        }
        volatile uint8_t value = static_cast<volatile uint8_t*>(inherited)[0];
        (void)value;
        _exit(102);
    }
    close(ready[1]);
    close(proceed[0]);
    char byte = 0;
    ASSERT_EQ(1, read(ready[0], &byte, 1));
    OpcodeSnapshot before = snapshot(paths.stats);
    ASSERT_EQ(0, ftruncate(fd, kPageSize)) << strerror(errno);
    OpcodeSnapshot after = snapshot(paths.stats);
    assert_valid_snapshot(before);
    assert_valid_snapshot(after);
    EXPECT_GT(after.remove_mapping, before.remove_mapping);
    ASSERT_EQ(1, write(proceed[1], "x", 1));
    close(ready[0]);
    close(proceed[1]);
    int status = 0;
    ASSERT_TRUE(wait_for_child(child, &status)) << "inherited DAX PTE survived truncate timeout";
    ASSERT_TRUE(WIFSIGNALED(status)) << "child status=" << status;
    EXPECT_EQ(SIGBUS, WTERMSIG(status));
    ASSERT_EQ(0, munmap(inherited, kPageSize));

    ASSERT_EQ(0, ftruncate(fd, initial_size));
    std::vector<uint8_t> nonzero(kPageSize, 0x5a);
    ASSERT_EQ(static_cast<ssize_t>(nonzero.size()),
              pwrite(fd, nonzero.data(), nonzero.size(), kDaxRangeSize));
    inherited = mmap(nullptr, kPageSize, PROT_READ, MAP_SHARED, fd, kDaxRangeSize);
    ASSERT_NE(MAP_FAILED, inherited);
    EXPECT_EQ(0x5a, static_cast<uint8_t*>(inherited)[17]);
    before = snapshot(paths.stats);
    ASSERT_EQ(0, fallocate(fd, FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE, kDaxRangeSize,
                           kPageSize))
        << strerror(errno);
    after = snapshot(paths.stats);
    EXPECT_GT(after.remove_mapping, before.remove_mapping);
    EXPECT_EQ(0, static_cast<uint8_t*>(inherited)[17]);
    ASSERT_EQ(0, munmap(inherited, kPageSize));

    ASSERT_EQ(static_cast<ssize_t>(nonzero.size()),
              pwrite(fd, nonzero.data(), nonzero.size(), kDaxRangeSize));
    inherited = mmap(nullptr, kPageSize, PROT_READ, MAP_SHARED, fd, kDaxRangeSize);
    ASSERT_NE(MAP_FAILED, inherited);
    EXPECT_EQ(0x5a, static_cast<uint8_t*>(inherited)[23]);
    before = snapshot(paths.stats);
    ASSERT_EQ(0, fallocate(fd, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, kDaxRangeSize,
                           kPageSize))
        << strerror(errno);
    after = snapshot(paths.stats);
    EXPECT_GT(after.remove_mapping, before.remove_mapping);
    EXPECT_EQ(0, static_cast<uint8_t*>(inherited)[23]);
    ASSERT_EQ(0, munmap(inherited, kPageSize));

    uint8_t mapped = 0;
    ASSERT_EQ(1, pread(fd, &mapped, 1, 2 * kDaxRangeSize + 19));
    before = snapshot(paths.stats);
    int trunc_fd = open(paths.file, O_RDWR | O_TRUNC);
    ASSERT_GE(trunc_fd, 0) << strerror(errno);
    ASSERT_EQ(0, close(trunc_fd));
    after = snapshot(paths.stats);
    EXPECT_GT(after.remove_mapping, before.remove_mapping);
    struct stat st = {};
    ASSERT_EQ(0, fstat(fd, &st));
    EXPECT_EQ(0, st.st_size);

    ASSERT_EQ(0, close(fd));
    ASSERT_EQ(0, umount(paths.mountpoint));
    remove_non_dax_file(paths);
    paths.cleanup();
}

TEST(VirtioFsDax, ConcurrentFaultIoAndLayoutBreakingStress) {
    TestPaths paths("layout_stress");
    ASSERT_TRUE(paths.create());
    CleanupGuard cleanup{paths};

    int error = 0;
    constexpr off_t initial_size = 3 * kDaxRangeSize;
    if (!prepare_non_dax_file(paths, initial_size, &error)) {
        paths.cleanup();
        if (dax_required()) {
            FAIL() << "DAX layout stress preparation failed: " << strerror(error);
        }
        GTEST_SKIP() << "ordinary virtiofs is unavailable: " << strerror(error);
    }
    ASSERT_TRUE(mount_debugfs(paths, &error)) << strerror(error);
    if (!mount_virtiofs(paths.mountpoint, "always", &error)) {
        remove_non_dax_file(paths);
        paths.cleanup();
        if (dax_required()) {
            FAIL() << "DAX layout stress requires dax=always: " << strerror(error);
        }
        GTEST_SKIP() << "DAX window/backend is unavailable: " << strerror(error);
    }

    int fd = open(paths.file, O_RDWR);
    ASSERT_GE(fd, 0) << strerror(errno);
    struct SharedStressState {
        unsigned start;
        unsigned stop;
        unsigned iterations;
    };
    auto* state = static_cast<SharedStressState*>(mmap(
        nullptr, sizeof(SharedStressState), PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, state);
    memset(state, 0, sizeof(*state));
    int ready[2] = {-1, -1};
    ASSERT_EQ(0, pipe(ready));
    OpcodeSnapshot before = snapshot(paths.stats);
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(ready[0]);
        int child_fd = open(paths.file, O_RDWR);
        if (child_fd < 0 || write(ready[1], "r", 1) != 1) {
            _exit(101);
        }
        close(ready[1]);
        while (__atomic_load_n(&state->start, __ATOMIC_ACQUIRE) == 0) {
            usleep(1000);
        }
        for (unsigned i = 0; i < 100000; ++i) {
            void* mapping = mmap(nullptr, kPageSize, PROT_READ | PROT_WRITE, MAP_SHARED, child_fd, 0);
            if (mapping == MAP_FAILED) {
                _exit(102);
            }
            volatile uint8_t* bytes = static_cast<volatile uint8_t*>(mapping);
            size_t index = i % kPageSize;
            uint8_t value = bytes[index];
            bytes[index] = static_cast<uint8_t>(value ^ 1);
            if (munmap(mapping, kPageSize) != 0) {
                _exit(103);
            }
            if (pread(child_fd, &value, 1, static_cast<off_t>(index)) != 1 ||
                pwrite(child_fd, &value, 1, static_cast<off_t>(index)) != 1) {
                _exit(104);
            }
            __atomic_store_n(&state->iterations, i + 1, __ATOMIC_RELEASE);
            if (__atomic_load_n(&state->stop, __ATOMIC_ACQUIRE) != 0) {
                _exit(close(child_fd) == 0 ? 0 : 105);
            }
        }
        _exit(106);
    }
    close(ready[1]);
    char byte = 0;
    ASSERT_EQ(1, read(ready[0], &byte, 1));
    close(ready[0]);
    __atomic_store_n(&state->start, 1, __ATOMIC_RELEASE);
    bool child_active = false;
    for (unsigned i = 0; i < kWaitIterations; ++i) {
        if (__atomic_load_n(&state->iterations, __ATOMIC_ACQUIRE) != 0) {
            child_active = true;
            break;
        }
        usleep(10000);
    }

    int operation_error = child_active ? 0 : ETIMEDOUT;
    for (unsigned i = 0; i < 64; ++i) {
        if (operation_error != 0 || ftruncate(fd, kPageSize) != 0 ||
            ftruncate(fd, initial_size) != 0 ||
            fallocate(fd, FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE, 0, kPageSize) != 0 ||
            fallocate(fd, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, kPageSize / 2,
                      kPageSize / 2) != 0) {
            operation_error = operation_error == 0 ? errno : operation_error;
            break;
        }
    }

    __atomic_store_n(&state->stop, 1, __ATOMIC_RELEASE);
    int status = 0;
    ASSERT_TRUE(wait_for_child(child, &status)) << "concurrent layout stress timed out";
    ASSERT_EQ(0, operation_error) << strerror(operation_error);
    ASSERT_TRUE(WIFEXITED(status)) << "child status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status));
    EXPECT_GT(__atomic_load_n(&state->iterations, __ATOMIC_ACQUIRE), 0U);
    OpcodeSnapshot after = snapshot(paths.stats);
    assert_valid_snapshot(before);
    assert_valid_snapshot(after);
    EXPECT_GT(after.remove_mapping, before.remove_mapping);
    struct stat st = {};
    ASSERT_EQ(0, fstat(fd, &st));
    EXPECT_EQ(initial_size, st.st_size);

    ASSERT_EQ(0, munmap(state, sizeof(*state)));
    ASSERT_EQ(0, close(fd));
    ASSERT_EQ(0, umount(paths.mountpoint));
    remove_non_dax_file(paths);
    paths.cleanup();
}

TEST(VirtioFsDax, HostInvalidationRevokesMappedWindow) {
    TestPaths paths("notify");
    ASSERT_TRUE(paths.create());
    CleanupGuard cleanup{paths};
    char ordinary_mount[224] = {};
    char ordinary_file[288] = {};
    snprintf(ordinary_mount, sizeof(ordinary_mount), "%s/ordinary", paths.root);
    snprintf(ordinary_file, sizeof(ordinary_file), "%s/dax_test.bin", ordinary_mount);
    ASSERT_EQ(0, ensure_dir(ordinary_mount));
    ExtraMountGuard ordinary_cleanup{ordinary_mount};

    int error = 0;
    if (!prepare_non_dax_file(paths, 2 * kDaxRangeSize, &error)) {
        rmdir(ordinary_mount);
        paths.cleanup();
        if (dax_required()) {
            FAIL() << "host invalidation preparation failed: " << strerror(error);
        }
        GTEST_SKIP() << "ordinary virtiofs is unavailable: " << strerror(error);
    }
    ASSERT_TRUE(mount_debugfs(paths, &error)) << strerror(error);
    if (!mount_virtiofs(paths.mountpoint, "always", &error)) {
        remove_non_dax_file(paths);
        rmdir(ordinary_mount);
        paths.cleanup();
        if (dax_required()) {
            FAIL() << "host invalidation requires dax=always: " << strerror(error);
        }
        GTEST_SKIP() << "DAX window/backend is unavailable: " << strerror(error);
    }
    ASSERT_TRUE(mount_virtiofs(ordinary_mount, "never", &error)) << strerror(error);
    ordinary_cleanup.mounted = true;

    int dax_fd = open(paths.file, O_RDONLY);
    ASSERT_GE(dax_fd, 0);
    void* mapping = mmap(nullptr, kPageSize, PROT_READ, MAP_SHARED, dax_fd, 0);
    ASSERT_NE(MAP_FAILED, mapping);
    volatile uint8_t* mapped = static_cast<volatile uint8_t*>(mapping);
    uint8_t original = mapped[37];
    uint8_t replacement = static_cast<uint8_t>(original ^ 0x5a);
    OpcodeSnapshot before = snapshot(paths.stats);

    int ordinary_fd = open(ordinary_file, O_RDWR);
    ASSERT_GE(ordinary_fd, 0) << strerror(errno);
    ASSERT_EQ(1, pwrite(ordinary_fd, &replacement, 1, 37));
    ASSERT_EQ(0, fsync(ordinary_fd));
    ASSERT_EQ(0, close(ordinary_fd));

    bool observed = false;
    for (unsigned i = 0; i < kWaitIterations; ++i) {
        if (mapped[37] == replacement) {
            observed = true;
            break;
        }
        usleep(100000);
    }
    EXPECT_TRUE(observed) << "host invalidation left stale DAX data";
    OpcodeSnapshot after = snapshot(paths.stats);
    assert_valid_snapshot(before);
    assert_valid_snapshot(after);
    EXPECT_GT(after.remove_mapping, before.remove_mapping);

    ASSERT_EQ(0, munmap(mapping, kPageSize));
    ASSERT_EQ(0, close(dax_fd));
    ASSERT_EQ(0, umount(ordinary_mount));
    ordinary_cleanup.mounted = false;
    ASSERT_EQ(0, umount(paths.mountpoint));
    remove_non_dax_file(paths);
    rmdir(ordinary_mount);
    paths.cleanup();
}

TEST(VirtioFsDax, FaultRangePressureCompletesWithoutDeadlock) {
    const char* ranges_text = getenv("DRAGONOS_VIRTIOFS_DAX_WINDOW_RANGES");
    if (ranges_text == nullptr || *ranges_text == '\0') {
        if (dax_required()) {
            FAIL() << "required DAX pressure test needs DRAGONOS_VIRTIOFS_DAX_WINDOW_RANGES";
        }
        GTEST_SKIP() << "set DRAGONOS_VIRTIOFS_DAX_WINDOW_RANGES to the runner's DAX window size";
    }
    char* end = nullptr;
    errno = 0;
    unsigned long ranges = strtoul(ranges_text, &end, 10);
    ASSERT_EQ(0, errno);
    ASSERT_NE(ranges_text, end);
    ASSERT_EQ('\0', *end);
    ASSERT_GE(ranges, 1UL);
    const unsigned long max_ranges = static_cast<unsigned long>(
        (std::numeric_limits<off_t>::max() - kPageSize) / kDaxRangeSize - 1);
    ASSERT_LE(ranges, max_ranges);
    if (ranges > kMaxPressureRanges) {
        if (dax_required()) {
            FAIL() << "required DAX correctness profile supports at most " << kMaxPressureRanges
                   << " ranges";
        }
        GTEST_SKIP() << "DAX pressure profile is bounded to " << kMaxPressureRanges << " ranges";
    }

    TestPaths paths("pressure");
    ASSERT_TRUE(paths.create());
    CleanupGuard cleanup{paths};
    int error = 0;
    off_t file_size = static_cast<off_t>(ranges + 1) * kDaxRangeSize + kPageSize;
    if (!prepare_non_dax_pressure_file(paths, file_size, ranges, &error)) {
        paths.cleanup();
        if (dax_required()) {
            FAIL() << "DAX pressure environment is required but ordinary virtiofs preparation "
                      "failed: "
                   << strerror(error);
        }
        GTEST_SKIP() << "ordinary virtiofs is unavailable: " << strerror(error);
    }
    ASSERT_TRUE(mount_debugfs(paths, &error)) << strerror(error);
    if (!mount_virtiofs(paths.mountpoint, "always", &error)) {
        remove_non_dax_file(paths);
        paths.cleanup();
        if (dax_required()) {
            FAIL() << "DAX pressure environment is required: " << strerror(error);
        }
        GTEST_SKIP() << "DAX pressure environment is unavailable: " << strerror(error);
    }

    OpcodeSnapshot before = snapshot(paths.stats);
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        int fd = open(paths.file, O_RDONLY);
        if (fd < 0) {
            _exit(101);
        }
        volatile uint8_t sum = 0;
        for (unsigned long i = 0; i <= ranges; ++i) {
            off_t offset = static_cast<off_t>(i) * kDaxRangeSize + 17;
            off_t page_offset = offset & ~static_cast<off_t>(kPageSize - 1);
            size_t in_page = static_cast<size_t>(offset - page_offset);
            void* mapping = mmap(nullptr, kPageSize, PROT_READ, MAP_PRIVATE, fd, page_offset);
            if (mapping == MAP_FAILED) {
                _exit(102);
            }
            volatile uint8_t value = static_cast<volatile uint8_t*>(mapping)[in_page];
            if (value != pattern_byte(offset)) {
                _exit(103);
            }
            sum ^= value;
            if (munmap(mapping, kPageSize) != 0) {
                _exit(104);
            }
        }
        (void)sum;
        close(fd);
        _exit(0);
    }

    int status = 0;
    unsigned long extra_iterations = ranges / 8;
    unsigned wait_iterations = extra_iterations > std::numeric_limits<unsigned>::max() -
                                                      kWaitIterations
                                   ? std::numeric_limits<unsigned>::max()
                                   : kWaitIterations + static_cast<unsigned>(extra_iterations);
    ASSERT_TRUE(wait_for_child(child, &status, wait_iterations))
        << "DAX range-pressure fault path deadlocked";
    ASSERT_TRUE(WIFEXITED(status)) << "pressure child status=" << status;
    ASSERT_EQ(0, WEXITSTATUS(status));
    OpcodeSnapshot after = snapshot(paths.stats);
    assert_valid_snapshot(before);
    assert_valid_snapshot(after);
    EXPECT_GE(after.setup_mapping - before.setup_mapping, static_cast<long long>(ranges + 1));
    EXPECT_GT(after.remove_mapping, before.remove_mapping);
    EXPECT_GT(after.pressure_reclaims, before.pressure_reclaims);
    EXPECT_EQ(before.read, after.read);

    ASSERT_EQ(0, umount(paths.mountpoint));
    remove_non_dax_file(paths);
    paths.cleanup();
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
