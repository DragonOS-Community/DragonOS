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

std::string read_fd_with_chunk(int fd, const char* path, size_t chunk) {
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
            return {};
        }
        out.append(buf, static_cast<size_t>(n));
    }

    return out;
}

std::string read_all_with_chunk(const char* path, size_t chunk) {
    int fd = open(path, O_RDONLY);
    EXPECT_GE(fd, 0) << "open(" << path << ") failed: errno=" << errno << " ("
                     << strerror(errno) << ")";
    if (fd < 0) {
        return {};
    }
    std::string out = read_fd_with_chunk(fd, path, chunk);
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

void write_control(const char* path, const char* value) {
    int fd = open(path, O_WRONLY);
    ASSERT_GE(fd, 0) << "open(" << path << ") failed: errno=" << errno << " ("
                     << strerror(errno) << ")";
    if (fd < 0) {
        return;
    }
    size_t len = strlen(value);
    EXPECT_EQ(static_cast<ssize_t>(len), write(fd, value, len))
        << "write(" << path << ") failed: errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_EQ(0, close(fd));
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
    char stats_mode_path[192] = {};
    snprintf(root, sizeof(root), "/tmp/fuse_stats_debugfs_%d", getpid());
    snprintf(stats_path, sizeof(stats_path), "%s/fuse/stats", root);
    snprintf(stats_mode_path, sizeof(stats_mode_path), "%s/fuse/stats_mode", root);

    ASSERT_EQ(0, ensure_dir("/tmp")) << strerror(errno);
    ASSERT_EQ(0, mkdir(root, 0755)) << strerror(errno);

    if (mount("none", root, "debugfs", 0, nullptr) != 0) {
        int saved_errno = errno;
        rmdir(root);
        FAIL() << "mount debugfs failed: errno=" << saved_errno << " (" << strerror(saved_errno)
               << ")";
    }

    const std::string original_mode = read_all_with_chunk(stats_mode_path, 64);
    write_control(stats_mode_path, "off\n");
    EXPECT_EQ("off\n", read_all_with_chunk(stats_mode_path, 64));
    write_control(stats_mode_path, "light\n");
    EXPECT_EQ("light\n", read_all_with_chunk(stats_mode_path, 64));
    write_control(stats_mode_path, "detailed\n");
    EXPECT_EQ("detailed\n", read_all_with_chunk(stats_mode_path, 64));

    int stats_fd = open(stats_path, O_RDONLY);
    ASSERT_GE(stats_fd, 0) << strerror(errno);
    std::string whole = read_fd_with_chunk(stats_fd, stats_path, 64);
    ASSERT_EQ(0, lseek(stats_fd, 0, SEEK_SET)) << strerror(errno);
    std::string small = read_fd_with_chunk(stats_fd, stats_path, 7);
    EXPECT_EQ(0, close(stats_fd));
    write_control(stats_mode_path, "off\n");
    std::string before_fuse = read_all_with_chunk(stats_path, 64);
    drive_minimal_fuse_request();
    std::string after_fuse = read_all_with_chunk(stats_path, 64);

    write_control(stats_mode_path, original_mode.c_str());
    EXPECT_EQ(original_mode, read_all_with_chunk(stats_mode_path, 64));
    EXPECT_EQ(0, umount(root)) << strerror(errno);
    EXPECT_EQ(0, rmdir(root)) << strerror(errno);

    ASSERT_FALSE(whole.empty());
    ASSERT_EQ(whole, small);

    expect_field(whole, "[fuse]\n");
    expect_field(whole, "always_on aggregate_transport,quiescence_owner\n");
    expect_field(whole, "light direct_read_dma,read_size_buckets\n");
    expect_field(whole, "init_epoch ");
    expect_field(whole, "negotiated_max_read_bytes ");
    expect_field(whole, "negotiated_max_pages ");
    expect_field(whole, "negotiated_max_readahead_bytes ");
    expect_field(whole, "negotiated_async_read ");
    expect_field(whole, "effective_read_payload_limit_bytes ");
    expect_field(whole, "request_queue_current ");
    expect_field(whole, "dispatch_current ");
    expect_field(whole, "processing_current ");
    expect_field(whole, "read_reservation_current ");
    expect_field(whole, "requests_queued_total ");
    expect_field(whole, "requests_dequeued_total ");
    expect_field(whole, "requests_replied_ok_total ");
    expect_field(whole, "requests_replied_err_total ");
    expect_field(whole, "requests_aborted_total ");
    expect_field(whole, "requests_dropped_umount_total ");
    expect_field(whole, "read_buffer_too_small_total ");
    expect_field(whole, "reply_payload_transfer_count_total ");
    expect_field(whole, "reply_payload_transfer_bytes_total ");
    expect_field(whole, "reply_payload_copy_count_total ");
    expect_field(whole, "dev_fuse_input_copy_count_total ");
    expect_field(whole, "dev_fuse_input_copy_bytes_total ");
    expect_field(whole, "virtiofs_compat_copy_count_total ");
    expect_field(whole, "virtiofs_compat_copy_bytes_total ");

    expect_counter_increased(before_fuse, after_fuse, "requests_queued_total");
    EXPECT_EQ(0, parse_counter(after_fuse, "request_queue_current"));
    EXPECT_EQ(0, parse_counter(after_fuse, "dispatch_current"));
    EXPECT_EQ(0, parse_counter(after_fuse, "processing_current"));
    EXPECT_EQ(0, parse_counter(after_fuse, "read_reservation_current"));

    expect_field(whole, "[virtiofs]\n");
    expect_field(whole, "device_queue_depth_max ");
    expect_field(whole, "hiprio_vring_size_configured ");
    expect_field(whole, "request_queue_count_configured ");
    expect_field(whole, "request_vring_size_min_configured ");
    expect_field(whole, "request_vring_size_max_configured ");
    expect_field(whole, "sg_limit_pages_configured ");
    expect_field(whole, "inflight_current ");
    expect_field(whole, "inflight_peak ");
    expect_field(whole, "hiprio_inflight_current ");
    expect_field(whole, "hiprio_inflight_peak ");
    expect_field(whole, "request_inflight_current ");
    expect_field(whole, "request_inflight_peak ");
    expect_field(whole, "queue_full_blocked_current ");
    expect_field(whole, "reply_retained_current ");
    expect_field(whole, "reply_retained_peak ");
    expect_field(whole, "reply_retained_capacity_bytes_current ");
    expect_field(whole, "reply_retained_capacity_bytes_peak ");
    expect_field(whole, "reply_credit_blocked_total ");
    expect_field(whole, "reply_credit_blocked_wake_total ");
    expect_field(whole, "bridge_loop_iterations_total ");
    expect_field(whole, "bridge_idle_sleeps_total ");
    expect_field(whole, "virtqueue_full_total ");
    expect_field(whole, "virtqueue_not_ready_total ");
    expect_field(whole, "bridge_request_clone_bytes ");
    expect_field(whole, "response_buffer_alloc_bytes ");
    expect_field(whole, "response_buffer_reuse_count ");
    expect_field(whole, "response_buffer_zero_bytes ");
    expect_field(whole, "response_pool_dropped_count ");
    expect_field(whole, "bytes_submitted_total ");
    expect_field(whole, "bytes_completed_total ");
    expect_field(whole, "bridge_waits_total ");
    expect_field(whole, "bridge_wait_exit_request_pending_total ");
    expect_field(whole, "bridge_wait_exit_completion_total ");
    expect_field(whole, "bridge_wait_exit_teardown_total ");
    expect_field(whole, "bridge_wait_exit_disconnect_total ");
    expect_field(whole, "bridge_wait_exit_spurious_total ");
    expect_field(whole, "bridge_wake_request_total ");
    expect_field(whole, "bridge_wake_completion_total ");
    expect_field(whole, "bridge_wake_reply_released_total ");
    expect_field(whole, "bridge_wake_teardown_total ");
    expect_field(whole, "bridge_wake_disconnect_total ");
    expect_field(whole, "bridge_irq_no_active_conn_total ");
    expect_field(whole, "bridge_irq_stale_session_total ");
    expect_field(whole, "bridge_irq_weak_upgrade_failed_total ");
    expect_field(whole, "bridge_queue_full_blocked_total ");
    expect_field(whole, "bridge_queue_full_retry_total ");
    expect_field(whole, "bridge_queue_full_retry_after_completion_total ");
    expect_field(whole, "bridge_queue_full_retry_success_total ");
    expect_field(whole, "hiprio_queue_full_total ");
    expect_field(whole, "request_queue_full_total ");
    expect_field(whole, "[virtiofs_opcode]\n");
    expect_field(whole, "opcode_1_requests_total ");
    expect_field(whole, "opcode_15_request_bridge_copy_bytes ");
    expect_field(whole, "opcode_16_response_buffer_alloc_count ");
    expect_field(whole, "opcode_16_response_buffer_zero_bytes ");
    expect_field(whole, "opcode_63_reply_payload_copy_bytes ");
    expect_field(whole, "opcode_63_reply_payload_transfer_count ");
    expect_field(whole, "opcode_63_reply_payload_transfer_bytes ");

    EXPECT_GE(parse_counter(whole, "device_queue_depth_max"), 0);
    EXPECT_GE(parse_counter(whole, "hiprio_vring_size_configured"), 0);
    EXPECT_GE(parse_counter(whole, "request_queue_count_configured"), 0);
    EXPECT_GE(parse_counter(whole, "request_vring_size_min_configured"), 0);
    EXPECT_GE(parse_counter(whole, "request_vring_size_max_configured"), 0);
    EXPECT_GE(parse_counter(whole, "sg_limit_pages_configured"), 0);
    EXPECT_GE(parse_counter(whole, "inflight_current"), 0);
    EXPECT_GE(parse_counter(whole, "inflight_peak"), 0);

    expect_counter_increased(whole, after_fuse, "requests_queued_total");
    expect_counter_increased(whole, after_fuse, "requests_dequeued_total");
    expect_counter_increased(whole, after_fuse, "requests_replied_ok_total");
    expect_counter_increased(whole, after_fuse, "bytes_reply_payload_cloned_total");
    expect_counter_increased(whole, after_fuse, "dev_fuse_input_copy_count_total");
    expect_counter_increased(whole, after_fuse, "dev_fuse_input_copy_bytes_total");
    EXPECT_EQ(parse_counter(whole, "reply_payload_transfer_count_total"),
              parse_counter(after_fuse, "reply_payload_transfer_count_total"));
    EXPECT_EQ(parse_counter(whole, "virtiofs_compat_copy_count_total"),
              parse_counter(after_fuse, "virtiofs_compat_copy_count_total"));
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
