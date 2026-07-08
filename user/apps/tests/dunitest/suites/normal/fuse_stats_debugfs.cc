#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <unistd.h>

#include <string>

#include "../fuse/fuse_gtest_common.h"

namespace {

std::string read_all_with_chunk(const char* path, size_t chunk) {
    int fd = open(path, O_RDONLY);
    EXPECT_GE(fd, 0) << "open(" << path << ") failed: errno=" << errno << " ("
                     << strerror(errno) << ")";
    if (fd < 0) {
        return {};
    }

    std::string out;
    char buf[64];
    if (chunk > sizeof(buf)) {
        chunk = sizeof(buf);
    }
    while (true) {
        ssize_t n = read(fd, buf, chunk);
        if (n == 0) {
            break;
        }
        EXPECT_GT(n, 0) << "read(" << path << ") failed: errno=" << errno << " ("
                        << strerror(errno) << ")";
        if (n <= 0) {
            close(fd);
            return {};
        }
        out.append(buf, static_cast<size_t>(n));
    }

    EXPECT_EQ(0, close(fd)) << "close(" << path << ") failed: errno=" << errno << " ("
                            << strerror(errno) << ")";
    return out;
}

void expect_field(const std::string& stats, const char* field) {
    ASSERT_NE(std::string::npos, stats.find(field)) << "missing field " << field << "\n"
                                                    << stats;
}

long long parse_counter(const std::string& stats, const char* field) {
    std::string needle = std::string(field) + " ";
    size_t pos = stats.find(needle);
    if (pos == std::string::npos) {
        return -1;
    }
    pos += needle.size();
    char* end = nullptr;
    long long value = strtoll(stats.c_str() + pos, &end, 10);
    if (end == stats.c_str() + pos) {
        return -1;
    }
    return value;
}

void expect_counter_increased(const std::string& before, const std::string& after,
                              const char* field) {
    long long old_value = parse_counter(before, field);
    long long new_value = parse_counter(after, field);
    ASSERT_GE(old_value, 0) << "missing or invalid before counter " << field << "\n" << before;
    ASSERT_GE(new_value, 0) << "missing or invalid after counter " << field << "\n" << after;
    EXPECT_GT(new_value, old_value) << field << " did not increase";
}

void drive_minimal_fuse_request() {
    char mp[128] = {};
    snprintf(mp, sizeof(mp), "/tmp/fuse_stats_mount_%d", getpid());
    ASSERT_EQ(0, ensure_dir(mp)) << strerror(errno);

    int fd = open("/dev/fuse", O_RDWR);
    ASSERT_GE(fd, 0) << "open(/dev/fuse): " << strerror(errno);

    char opts[256] = {};
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    ASSERT_EQ(0, mount("none", mp, "fuse", 0, opts)) << "mount(fuse): " << strerror(errno);

    ASSERT_EQ(0, fuseg_do_init_handshake_basic(fd)) << "FUSE_INIT handshake: " << strerror(errno);

    EXPECT_EQ(0, umount(mp)) << "umount(" << mp << "): " << strerror(errno);
    EXPECT_EQ(0, close(fd)) << "close(/dev/fuse): " << strerror(errno);
    EXPECT_EQ(0, rmdir(mp)) << "rmdir(" << mp << "): " << strerror(errno);
}

}  // namespace

TEST(FuseStatsDebugFs, StatsFileExistsAndSupportsOffsetReads) {
    char root[128] = {};
    char stats_path[192] = {};
    snprintf(root, sizeof(root), "/tmp/fuse_stats_debugfs_%d", getpid());
    snprintf(stats_path, sizeof(stats_path), "%s/fuse/stats", root);

    ASSERT_EQ(0, ensure_dir("/tmp")) << strerror(errno);
    ASSERT_EQ(0, mkdir(root, 0755)) << strerror(errno);

    if (mount("none", root, "debugfs", 0, nullptr) != 0) {
        int saved_errno = errno;
        rmdir(root);
        FAIL() << "mount debugfs failed: errno=" << saved_errno << " (" << strerror(saved_errno)
               << ")";
    }

    std::string whole = read_all_with_chunk(stats_path, 64);
    std::string small = read_all_with_chunk(stats_path, 7);
    drive_minimal_fuse_request();
    std::string after_fuse = read_all_with_chunk(stats_path, 64);

    EXPECT_EQ(0, umount(root)) << strerror(errno);
    EXPECT_EQ(0, rmdir(root)) << strerror(errno);

    ASSERT_FALSE(whole.empty());
    ASSERT_EQ(whole, small);

    expect_field(whole, "[fuse]\n");
    expect_field(whole, "requests_queued_total ");
    expect_field(whole, "requests_dequeued_total ");
    expect_field(whole, "requests_replied_ok_total ");
    expect_field(whole, "requests_replied_err_total ");
    expect_field(whole, "requests_aborted_total ");
    expect_field(whole, "requests_dropped_umount_total ");
    expect_field(whole, "read_buffer_too_small_total ");

    expect_field(whole, "[virtiofs]\n");
    expect_field(whole, "bridge_loop_iterations_total ");
    expect_field(whole, "bridge_idle_sleeps_total ");
    expect_field(whole, "virtqueue_full_total ");
    expect_field(whole, "virtqueue_not_ready_total ");
    expect_field(whole, "bridge_request_clone_bytes ");
    expect_field(whole, "response_buffer_alloc_bytes ");
    expect_field(whole, "bytes_submitted_total ");
    expect_field(whole, "bytes_completed_total ");

    expect_counter_increased(whole, after_fuse, "requests_queued_total");
    expect_counter_increased(whole, after_fuse, "requests_dequeued_total");
    expect_counter_increased(whole, after_fuse, "requests_replied_ok_total");
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
