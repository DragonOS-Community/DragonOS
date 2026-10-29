#include <gtest/gtest.h>
#include "fuse_gtest_common.h"
#include <atomic>
#include <thread>
#include <string>
#include <vector>

namespace {
class FuseBlocks : public ::testing::Test {
protected:
    void SetUp() override {
        char directory[] = "/tmp/fuse_blocks_XXXXXX";
        char* root = mkdtemp(directory);
        ASSERT_NE(nullptr, root) << strerror(errno);
        root_ = root;
        fd_ = open("/dev/fuse", O_RDWR | O_NONBLOCK);
        ASSERT_GE(fd_, 0) << strerror(errno);
        char options[256];
        snprintf(options, sizeof(options), "fd=%d,rootmode=040755,user_id=%u,group_id=%u",
                 fd_, getuid(), getgid());
        ASSERT_EQ(0, mount("none", root_.c_str(), "fuse", 0, options)) << strerror(errno);
        mounted_ = true;
        ASSERT_EQ(0, fuseg_do_init_handshake_basic(fd_)) << strerror(errno);
        server_ = std::thread([this] { serve(); });
    }
    void TearDown() override {
        if (mounted_) { EXPECT_EQ(0, umount(root_.c_str())) << strerror(errno); }
        stop_.store(true);
        if (server_.joinable()) server_.join();
        if (fd_ >= 0) close(fd_);
        if (!root_.empty()) { EXPECT_EQ(0, rmdir(root_.c_str())) << strerror(errno); }
        EXPECT_EQ(0, error_.load());
    }
    static fuse_attr attr(uint64_t id) {
        fuse_attr result {};
        result.ino = id;
        result.mode = id == 1 ? S_IFDIR | 0755 : S_IFREG | 0644;
        result.nlink = id == 1 ? 2 : 1;
        result.uid = getuid(); result.gid = getgid();
        result.blksize = 4096;
        // Intentional independent values: neither EOF nor the I/O hint can
        // recover the daemon's allocation report.
        result.size = id == 2 ? 1 : (id == 3 ? 1024 * 1024 : 0);
        result.blocks = id == 2 ? 37 : 0;
        return result;
    }
    void serve() {
        // The shared INIT handshake advertises a 1 MiB max_write. Linux
        // requires every subsequent device read to accommodate that payload
        // plus protocol headers, even when this fixture only serves getattr.
        std::vector<unsigned char> request(1024 * 1024 + FUSE_TEST_BUF_SIZE);
        while (!stop_.load()) {
            pollfd poller {fd_, POLLIN, 0};
            int ready = poll(&poller, 1, 50);
            if (ready < 0 && errno == EINTR) continue;
            if (ready < 0) { error_.store(errno); return; }
            if (ready == 0) continue;
            const ssize_t count = read(fd_, request.data(), request.size());
            if (count < 0 && (errno == EAGAIN || errno == EINTR)) continue;
            if (count < 0 && errno == ENODEV) return;
            if (count == 0) return;
            if (count < static_cast<ssize_t>(sizeof(fuse_in_header))) {
                error_.store(count < 0 ? errno : EPROTO); return;
            }
            fuse_in_header header {};
            memcpy(&header, request.data(), sizeof(header));
            int result = 0;
            if (header.opcode == FUSE_LOOKUP) {
                const char* name = reinterpret_cast<const char*>(request.data() + sizeof(header));
                const size_t length = static_cast<size_t>(count) - sizeof(header);
                uint64_t id = 0;
                if (length != 0 && name[length - 1] == '\0') {
                    if (strcmp(name, "allocated") == 0) id = 2;
                    if (strcmp(name, "sparse") == 0) id = 3;
                }
                if (id == 0) {
                    result = fuse_write_reply(fd_, header.unique, -ENOENT, nullptr, 0);
                } else {
                    fuse_entry_out entry {};
                    entry.nodeid = id; entry.generation = 1; entry.attr = attr(id);
                    result = fuse_write_reply(fd_, header.unique, 0, &entry, sizeof(entry));
                }
            } else if (header.opcode == FUSE_GETATTR) {
                fuse_attr_out out {}; out.attr = attr(header.nodeid);
                result = fuse_write_reply(fd_, header.unique, 0, &out, sizeof(out));
            } else if (header.opcode == FUSE_FORGET || header.opcode == 42 /* FUSE_BATCH_FORGET */) {
                continue; // These protocol requests have no reply.
            } else if (header.opcode == FUSE_DESTROY) {
                return;
            } else {
                result = fuse_write_reply(fd_, header.unique, -ENOSYS, nullptr, 0);
            }
            if (result != 0) { error_.store(errno ? errno : EIO); return; }
        }
    }
    void check(const char* name, blkcnt_t blocks, off_t size) {
        const std::string path = root_ + "/" + name;
        int fd = open(path.c_str(), O_PATH);
        ASSERT_GE(fd, 0) << strerror(errno);
        struct stat basic {}; const int stat_result = fstat(fd, &basic);
        const int stat_error = errno;
        struct statx extended {};
        const int extended_result = statx(fd, "", AT_EMPTY_PATH, STATX_BLOCKS | STATX_SIZE, &extended);
        const int extended_error = errno;
        close(fd);
        ASSERT_EQ(0, stat_result) << strerror(stat_error);
        ASSERT_EQ(0, extended_result) << strerror(extended_error);
        EXPECT_EQ(blocks, basic.st_blocks); EXPECT_EQ(size, basic.st_size);
        EXPECT_EQ(4096, basic.st_blksize);
        EXPECT_EQ(static_cast<uint64_t>(blocks), extended.stx_blocks);
        EXPECT_EQ(static_cast<uint64_t>(size), extended.stx_size);
        EXPECT_NE(0u, extended.stx_mask & STATX_BLOCKS);
    }
    std::string root_;
    int fd_ = -1;
    bool mounted_ = false;
    std::atomic<bool> stop_ {false};
    std::atomic<int> error_ {0};
    std::thread server_;
};
TEST_F(FuseBlocks, DaemonBlocksAre512SectorsIndependentOfSizeAndBlksize) {
    ASSERT_NO_FATAL_FAILURE(check("allocated", 37, 1));
    ASSERT_NO_FATAL_FAILURE(check("sparse", 0, 1024 * 1024));
}
} // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
