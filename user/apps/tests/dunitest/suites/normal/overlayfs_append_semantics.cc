#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <gtest/gtest.h>

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#include <array>
#include <set>
#include <string>
#include <vector>

namespace {

constexpr int kWaitIterations = 1000;
constexpr useconds_t kWaitIntervalUs = 10000;
constexpr size_t kRecordSize = 32;

std::string join_path(const std::string& dir, const char* name) {
    return dir + "/" + name;
}

void remove_recursive(const std::string& path) {
    struct stat st = {};
    if (lstat(path.c_str(), &st) != 0) {
        return;
    }
    if (!S_ISDIR(st.st_mode)) {
        unlink(path.c_str());
        return;
    }

    DIR* dir = opendir(path.c_str());
    if (dir != nullptr) {
        while (dirent* ent = readdir(dir)) {
            if (strcmp(ent->d_name, ".") == 0 || strcmp(ent->d_name, "..") == 0) {
                continue;
            }
            remove_recursive(join_path(path, ent->d_name));
        }
        closedir(dir);
    }
    rmdir(path.c_str());
}

bool write_all(int fd, const void* data, size_t len) {
    const char* current = static_cast<const char*>(data);
    while (len != 0) {
        ssize_t written = write(fd, current, len);
        if (written < 0 && errno == EINTR) {
            continue;
        }
        if (written <= 0) {
            return false;
        }
        current += written;
        len -= static_cast<size_t>(written);
    }
    return true;
}

[[noreturn]] void exit_after_blocked_child(pid_t child) {
    kill(child, SIGKILL);
    fprintf(stderr, "append operation did not return within 10 seconds\n");
    _exit(124);
}

template <typename Operation>
bool run_child_bounded(Operation operation) {
    pid_t child = fork();
    if (child < 0) {
        return false;
    }
    if (child == 0) {
        _exit(operation() ? 0 : 1);
    }

    for (int attempt = 0; attempt < kWaitIterations; ++attempt) {
        int status = 0;
        pid_t result = waitpid(child, &status, WNOHANG);
        if (result == child) {
            return WIFEXITED(status) && WEXITSTATUS(status) == 0;
        }
        if (result < 0) {
            if (errno == EINTR) {
                continue;
            }
            kill(child, SIGKILL);
            return false;
        }
        usleep(kWaitIntervalUs);
    }
    exit_after_blocked_child(child);
}

std::array<char, kRecordSize> make_record(int writer, int sequence) {
    std::array<char, kRecordSize> record = {};
    int prefix = snprintf(
        record.data(), record.size(), "writer=%02d sequence=%03d", writer, sequence);
    if (prefix < 0 || static_cast<size_t>(prefix) >= record.size()) {
        return {};
    }
    for (size_t i = static_cast<size_t>(prefix); i < record.size(); ++i) {
        record[i] = '#';
    }
    return record;
}

bool wait_for_children_bounded(const std::vector<pid_t>& children) {
    std::set<pid_t> remaining(children.begin(), children.end());
    bool success = true;
    for (int attempt = 0; attempt < kWaitIterations && !remaining.empty(); ++attempt) {
        for (auto it = remaining.begin(); it != remaining.end();) {
            int status = 0;
            pid_t result = waitpid(*it, &status, WNOHANG);
            if (result == *it) {
                if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
                    success = false;
                }
                it = remaining.erase(it);
            } else if (result < 0) {
                if (errno == EINTR) {
                    ++it;
                } else {
                    success = false;
                    it = remaining.erase(it);
                }
            } else {
                ++it;
            }
        }
        if (!remaining.empty()) {
            usleep(kWaitIntervalUs);
        }
    }
    if (!remaining.empty()) {
        for (pid_t child : remaining) {
            kill(child, SIGKILL);
        }
        fprintf(stderr, "concurrent append operations did not return within 10 seconds\n");
        _exit(124);
    }
    return success;
}

std::set<std::string> expected_records(int writers, int records_per_writer) {
    std::set<std::string> expected;
    for (int writer = 0; writer < writers; ++writer) {
        for (int sequence = 0; sequence < records_per_writer; ++sequence) {
            const auto record = make_record(writer, sequence);
            expected.emplace(record.data(), record.size());
        }
    }
    return expected;
}

std::string read_file(const std::string& path) {
    int fd = open(path.c_str(), O_RDONLY);
    if (fd < 0) {
        return {};
    }

    std::string result;
    std::array<char, 4096> buffer = {};
    for (;;) {
        ssize_t count = read(fd, buffer.data(), buffer.size());
        if (count < 0 && errno == EINTR) {
            continue;
        }
        if (count <= 0) {
            break;
        }
        result.append(buffer.data(), static_cast<size_t>(count));
    }
    close(fd);
    return result;
}

class OverlayAppendEnv {
public:
    explicit OverlayAppendEnv(const char* name) {
        root = std::string("/tmp/") + name + "_" + std::to_string(getpid());
        upper = join_path(root, "upper");
        lower = join_path(root, "lower");
        work = join_path(root, "work");
        merged = join_path(root, "merged");
    }

    ~OverlayAppendEnv() {
        if (mounted) {
            umount(merged.c_str());
        }
        remove_recursive(root);
    }

    bool prepare() {
        if (mkdir(root.c_str(), 0755) != 0 || mkdir(upper.c_str(), 0755) != 0
            || mkdir(lower.c_str(), 0755) != 0 || mkdir(work.c_str(), 0755) != 0
            || mkdir(merged.c_str(), 0755) != 0) {
            return false;
        }

        return true;
    }

    bool mount_overlay() {
        std::string options =
            "lowerdir=" + lower + ",upperdir=" + upper + ",workdir=" + work;
        if (mount("overlay", merged.c_str(), "overlay", 0, options.c_str()) != 0) {
            return false;
        }
        mounted = true;
        return true;
    }

    bool setup() {
        return prepare() && mount_overlay();
    }

    std::string root;
    std::string upper;
    std::string lower;
    std::string work;
    std::string merged;
    bool mounted = false;
};

TEST(OverlayFsAppend, ReopenPureUpperFileDoesNotDeadlock) {
    OverlayAppendEnv env("overlay_append_reopen");
    ASSERT_TRUE(env.setup()) << strerror(errno);
    const std::string merged_file = join_path(env.merged, "history");
    const std::string upper_file = join_path(env.upper, "history");

    int fd = open(merged_file.c_str(), O_CREAT | O_WRONLY | O_APPEND, 0644);
    ASSERT_GE(fd, 0) << strerror(errno);
    ASSERT_TRUE(write_all(fd, "first\n", 6)) << strerror(errno);
    ASSERT_EQ(0, close(fd)) << strerror(errno);

    ASSERT_TRUE(run_child_bounded([&]() {
        int append_fd = open(merged_file.c_str(), O_WRONLY | O_APPEND);
        if (append_fd < 0) {
            return false;
        }
        bool ok = write_all(append_fd, "second\n", 7);
        return close(append_fd) == 0 && ok;
    }));

    EXPECT_EQ("first\nsecond\n", read_file(merged_file));
    EXPECT_EQ("first\nsecond\n", read_file(upper_file));
}

TEST(OverlayFsAppend, ZeroLengthWriteAfterReopenReturns) {
    OverlayAppendEnv env("overlay_append_zero");
    ASSERT_TRUE(env.setup()) << strerror(errno);
    const std::string path = join_path(env.merged, "file");

    int fd = open(path.c_str(), O_CREAT | O_WRONLY | O_APPEND, 0644);
    ASSERT_GE(fd, 0) << strerror(errno);
    ASSERT_TRUE(write_all(fd, "data", 4));
    ASSERT_EQ(0, close(fd));

    ASSERT_TRUE(run_child_bounded([&]() {
        int append_fd = open(path.c_str(), O_WRONLY | O_APPEND);
        if (append_fd < 0 || write(append_fd, "", 0) != 0) {
            return false;
        }
        bool ok = write_all(append_fd, "!", 1);
        return close(append_fd) == 0 && ok;
    }));
    EXPECT_EQ("data!", read_file(path));
}

TEST(OverlayFsAppend, PwriteStillAppendsWithoutChangingFilePosition) {
    OverlayAppendEnv env("overlay_append_pwrite");
    ASSERT_TRUE(env.setup()) << strerror(errno);
    const std::string path = join_path(env.merged, "file");

    ASSERT_TRUE(run_child_bounded([&]() {
        int fd = open(path.c_str(), O_CREAT | O_RDWR | O_APPEND, 0644);
        if (fd < 0 || !write_all(fd, "abc", 3) || lseek(fd, 1, SEEK_SET) != 1
            || pwrite(fd, "X", 1, 0) != 1 || lseek(fd, 0, SEEK_CUR) != 1) {
            return false;
        }
        return close(fd) == 0;
    }));

    EXPECT_EQ("abcX", read_file(path));
}

TEST(OverlayFsAppend, FSetFlControlsAppendAtOverlayLayer) {
    OverlayAppendEnv env("overlay_append_setfl");
    ASSERT_TRUE(env.setup()) << strerror(errno);
    const std::string path = join_path(env.merged, "file");

    ASSERT_TRUE(run_child_bounded([&]() {
        int fd = open(path.c_str(), O_CREAT | O_RDWR | O_TRUNC, 0644);
        if (fd < 0 || !write_all(fd, "abcd", 4)) {
            return false;
        }
        int flags = fcntl(fd, F_GETFL);
        if (flags < 0 || fcntl(fd, F_SETFL, flags | O_APPEND) != 0
            || (fcntl(fd, F_GETFL) & O_APPEND) == 0 || lseek(fd, 1, SEEK_SET) != 1
            || !write_all(fd, "X", 1)) {
            return false;
        }
        flags = fcntl(fd, F_GETFL);
        if (flags < 0 || fcntl(fd, F_SETFL, flags & ~O_APPEND) != 0
            || (fcntl(fd, F_GETFL) & O_APPEND) != 0 || lseek(fd, 1, SEEK_SET) != 1
            || !write_all(fd, "Y", 1)) {
            return false;
        }
        return close(fd) == 0;
    }));

    EXPECT_EQ("aYcdX", read_file(path));
}

TEST(OverlayFsAppend, IndependentOpenFilesAppendWholeRecords) {
    constexpr int kWriters = 4;
    constexpr int kRecordsPerWriter = 24;

    OverlayAppendEnv env("overlay_append_concurrent");
    ASSERT_TRUE(env.setup()) << strerror(errno);
    const std::string path = join_path(env.merged, "records");

    int initial = open(path.c_str(), O_CREAT | O_WRONLY | O_TRUNC, 0644);
    ASSERT_GE(initial, 0) << strerror(errno);
    ASSERT_EQ(0, close(initial));

    int start_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(start_pipe)) << strerror(errno);
    std::vector<pid_t> children;

    for (int writer = 0; writer < kWriters; ++writer) {
        pid_t child = fork();
        ASSERT_GE(child, 0) << strerror(errno);
        if (child == 0) {
            close(start_pipe[1]);
            char token = 0;
            if (read(start_pipe[0], &token, 1) != 1) {
                _exit(10);
            }
            close(start_pipe[0]);

            int fd = open(path.c_str(), O_WRONLY | O_APPEND);
            if (fd < 0) {
                _exit(11);
            }
            for (int sequence = 0; sequence < kRecordsPerWriter; ++sequence) {
                const auto record = make_record(writer, sequence);
                if (write(fd, record.data(), record.size()) != static_cast<ssize_t>(record.size())) {
                    _exit(13);
                }
            }
            close(fd);
            _exit(0);
        }
        children.push_back(child);
    }

    close(start_pipe[0]);
    std::array<char, kWriters> tokens = {};
    ASSERT_TRUE(write_all(start_pipe[1], tokens.data(), tokens.size()));
    close(start_pipe[1]);

    ASSERT_TRUE(wait_for_children_bounded(children));

    const std::string content = read_file(path);
    ASSERT_EQ(
        static_cast<size_t>(kWriters * kRecordsPerWriter) * kRecordSize, content.size());
    std::set<std::string> records;
    for (size_t offset = 0; offset < content.size(); offset += kRecordSize) {
        records.insert(content.substr(offset, kRecordSize));
    }
    EXPECT_EQ(expected_records(kWriters, kRecordsPerWriter), records);
}

TEST(OverlayFsAppend, HardlinksShareAtomicAppendDomain) {
    constexpr int kWriters = 2;
    constexpr int kRecordsPerWriter = 16;

    OverlayAppendEnv env("overlay_append_hardlink");
    ASSERT_TRUE(env.setup()) << strerror(errno);
    const std::string first = join_path(env.merged, "records");
    const std::string alias = join_path(env.merged, "records_alias");

    int initial = open(first.c_str(), O_CREAT | O_WRONLY | O_TRUNC, 0644);
    ASSERT_GE(initial, 0) << strerror(errno);
    ASSERT_EQ(0, close(initial));
    ASSERT_EQ(0, link(first.c_str(), alias.c_str())) << strerror(errno);

    int start_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(start_pipe)) << strerror(errno);
    std::vector<pid_t> children;
    const std::array<std::string, kWriters> paths = {first, alias};
    for (int writer = 0; writer < kWriters; ++writer) {
        pid_t child = fork();
        ASSERT_GE(child, 0) << strerror(errno);
        if (child == 0) {
            close(start_pipe[1]);
            char token = 0;
            if (read(start_pipe[0], &token, 1) != 1) {
                _exit(10);
            }
            close(start_pipe[0]);
            int append_fd = open(paths[writer].c_str(), O_WRONLY | O_APPEND);
            if (append_fd < 0) {
                _exit(11);
            }
            for (int sequence = 0; sequence < kRecordsPerWriter; ++sequence) {
                const auto record = make_record(writer, sequence);
                if (write(append_fd, record.data(), record.size())
                    != static_cast<ssize_t>(record.size())) {
                    _exit(12);
                }
            }
            close(append_fd);
            _exit(0);
        }
        children.push_back(child);
    }
    close(start_pipe[0]);
    std::array<char, kWriters> tokens = {};
    ASSERT_TRUE(write_all(start_pipe[1], tokens.data(), tokens.size()));
    close(start_pipe[1]);
    ASSERT_TRUE(wait_for_children_bounded(children));

    const std::string content = read_file(first);
    ASSERT_EQ(static_cast<size_t>(kWriters * kRecordsPerWriter) * kRecordSize, content.size());
    std::set<std::string> records;
    for (size_t offset = 0; offset < content.size(); offset += kRecordSize) {
        records.insert(content.substr(offset, kRecordSize));
    }
    EXPECT_EQ(expected_records(kWriters, kRecordsPerWriter), records);
    EXPECT_EQ(content, read_file(alias));
}

TEST(OverlayFsAppend, LowerHardlinkCopyUpKeepsRwfAppendAtomic) {
    constexpr int kWriters = 4;
    constexpr int kRecordsPerWriter = 64;

    OverlayAppendEnv env("overlay_append_copyup_rwf");
    ASSERT_TRUE(env.prepare()) << strerror(errno);
    const std::string lower_file = join_path(env.lower, "records");
    const std::string lower_alias = join_path(env.lower, "records_alias");
    int initial = open(lower_file.c_str(), O_CREAT | O_WRONLY | O_TRUNC, 0644);
    ASSERT_GE(initial, 0) << strerror(errno);
    ASSERT_EQ(0, close(initial)) << strerror(errno);
    // A multiply-linked lower inode cannot use the origin xattr identity.
    // The first writable open therefore changes stat identity during copy-up.
    ASSERT_EQ(0, link(lower_file.c_str(), lower_alias.c_str())) << strerror(errno);
    ASSERT_TRUE(env.mount_overlay()) << strerror(errno);

    const std::string path = join_path(env.merged, "records");
    std::array<int, kWriters> append_fds = {-1, -1};
    // fd 0 opens the lower inode and triggers copy-up; later opens observe the
    // resulting upper inode. None carries O_APPEND, so only RWF_APPEND supplies
    // append semantics and no backing-file append lock can mask a split
    // overlay lock domain.
    for (int& fd : append_fds) {
        fd = open(path.c_str(), O_WRONLY);
        ASSERT_GE(fd, 0) << strerror(errno);
    }

    int start_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(start_pipe)) << strerror(errno);
    std::vector<pid_t> children;
    for (int writer = 0; writer < kWriters; ++writer) {
        pid_t child = fork();
        ASSERT_GE(child, 0) << strerror(errno);
        if (child == 0) {
            close(start_pipe[1]);
            char token = 0;
            if (read(start_pipe[0], &token, 1) != 1) {
                _exit(10);
            }
            close(start_pipe[0]);
            for (int sequence = 0; sequence < kRecordsPerWriter; ++sequence) {
                sched_yield();
                const auto record = make_record(writer, sequence);
                iovec iov = {
                    .iov_base = const_cast<char*>(record.data()),
                    .iov_len = record.size(),
                };
                if (pwritev2(append_fds[writer], &iov, 1, 0, RWF_APPEND)
                    != static_cast<ssize_t>(record.size())) {
                    _exit(11);
                }
            }
            _exit(0);
        }
        children.push_back(child);
    }

    close(start_pipe[0]);
    std::array<char, kWriters> tokens = {};
    ASSERT_TRUE(write_all(start_pipe[1], tokens.data(), tokens.size()));
    close(start_pipe[1]);
    ASSERT_TRUE(wait_for_children_bounded(children));
    for (int fd : append_fds) {
        ASSERT_EQ(0, close(fd)) << strerror(errno);
    }

    const std::string content = read_file(path);
    ASSERT_EQ(static_cast<size_t>(kWriters * kRecordsPerWriter) * kRecordSize, content.size());
    std::set<std::string> records;
    for (size_t offset = 0; offset < content.size(); offset += kRecordSize) {
        records.insert(content.substr(offset, kRecordSize));
    }
    EXPECT_EQ(expected_records(kWriters, kRecordsPerWriter), records);
}

TEST(AppendLockCapability, EventFdIgnoresAppendFilePosition) {
    int fd = eventfd(0, 0);
    ASSERT_GE(fd, 0) << strerror(errno);
    int flags = fcntl(fd, F_GETFL);
    ASSERT_GE(flags, 0) << strerror(errno);
    ASSERT_EQ(0, fcntl(fd, F_SETFL, flags | O_APPEND)) << strerror(errno);

    uint64_t value = 1;
    ASSERT_EQ(static_cast<ssize_t>(sizeof(value)), write(fd, &value, sizeof(value)))
        << strerror(errno);
    value = 0;
    ASSERT_EQ(static_cast<ssize_t>(sizeof(value)), read(fd, &value, sizeof(value)))
        << strerror(errno);
    EXPECT_EQ(1U, value);
    EXPECT_EQ(0, close(fd));
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
