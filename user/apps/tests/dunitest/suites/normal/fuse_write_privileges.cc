#include <gtest/gtest.h>

#include "../fuse/fuse_gtest_common.h"

#include <signal.h>
#include <sys/ioctl.h>
#include <sys/inotify.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/uio.h>
#include <sys/wait.h>

#include <algorithm>
#include <string>
#include <vector>

namespace {

constexpr uint32_t kHandleKillpriv = 1u << 19;
constexpr uint32_t kKillSuidgid = 1u << 2;
constexpr uint32_t kDirectIo = 1u;
constexpr size_t kMaxWrite = 64 * 1024;
constexpr unsigned char kCapability[] = {
    0, 0, 0, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0};
static_assert(sizeof(kCapability) == 20, "revision 2 capability requires two capability words");

enum class Input { Fault, Prefix, Empty, Good, RpcFailure, FileLimit, DirtyCapability, DirtyMode };
struct Policy {
    bool direct = false;
    bool handle_killpriv = false;
    bool writeback = false;
};
struct Case {
    Policy policy;
    Input input = Input::Fault;
    bool privileged = false;
    bool capability = false;
    int getxattr_error = 0;
    int remove_error = 0;
    int setattr_error = 0;
    unsigned initial_mode = 06777;
    unsigned failure_ordinal = 2;
    bool malformed_failure = false;
};
struct ClientResult {
    int stage = 0;
    ssize_t written = -1;
    int error = 0;
    unsigned mode = 0;
    off_t size = -1;
    off_t position = -1;
    unsigned attrib = 0;
    unsigned modify = 0;
    bool attrib_after_modify = false;
};
struct Request {
    unsigned opcode;
    unsigned flags;
    unsigned mode;
    unsigned uid;
    uint64_t size;
};

// The parent serves real kernel RPCs while the child performs the syscall. No
// daemon thread shares a tiny guest pthread stack or races on test state.
class WriteDaemon {
 public:
    WriteDaemon(int fd, const Case& test) : mode(test.initial_mode),
        capability(test.capability), fd_(fd), test_(test), buffer_(kMaxWrite + 4096) {}

    unsigned mode;
    uint64_t size = 16;
    bool capability;
    uint64_t committed_write_bytes = 0;
    std::vector<Request> requests;
    bool defer_write_reply = false;
    bool write_pending = false;
    bool defer_read_reply = false;
    bool read_pending = false;

    bool CompleteRead() {
        if (!read_pending) return false;
        read_pending = false;
        std::vector<unsigned char> data(pending_read_size_, 'p');
        return Reply(pending_read_unique_, data.data(), data.size());
    }

    bool CompleteWrite() {
        return CompleteWrite(pending_size_, 0);
    }

    bool CompleteWrite(size_t accepted, int error) {
        if (!write_pending) return false;
        if (accepted > pending_size_) return false;
        write_pending = false;
        if (error) return ReplyError(pending_unique_, error);
        size = std::max<uint64_t>(size, pending_offset_ + accepted);
        committed_write_bytes += accepted;
        fuse_write_out out{};
        out.size = accepted;
        return Reply(pending_unique_, out);
    }

    bool Serve() {
        ssize_t len = read(fd_, buffer_.data(), buffer_.size());
        if (len < static_cast<ssize_t>(sizeof(fuse_in_header))) return false;
        fuse_in_header header{};
        memcpy(&header, buffer_.data(), sizeof(header));
        if (header.len != len) return false;
        const unsigned char* payload = buffer_.data() + sizeof(header);
        size_t count = len - sizeof(header);
        Request request{header.opcode, 0, 0, header.uid, 0};
        if (header.opcode == FUSE_SETATTR && count >= sizeof(fuse_setattr_in)) {
            fuse_setattr_in in{};
            memcpy(&in, payload, sizeof(in));
            request.flags = in.valid;
            request.mode = in.mode;
        } else if (header.opcode == FUSE_WRITE && count >= sizeof(fuse_write_in)) {
            fuse_write_in in{};
            memcpy(&in, payload, sizeof(in));
            request.flags = in.write_flags;
            request.size = in.size;
        }
        requests.push_back(request);
        switch (header.opcode) {
        case FUSE_INIT: {
            if (count < 16) return false;
            fuse_init_in in{};
            memcpy(&in, payload, std::min(count, sizeof(in)));
            fuse_init_out out{};
            out.major = 7;
            out.minor = 39;
            out.flags = FUSE_INIT_EXT | FUSE_MAX_PAGES;
            if (test_.policy.handle_killpriv) out.flags |= kHandleKillpriv;
            if (test_.policy.writeback) out.flags |= FUSE_WRITEBACK_CACHE;
            // Negotiate v1 only: v2 requires additional OPEN/truncate contracts.
            if ((in.flags & out.flags) != out.flags) return false;
            out.max_write = test_.input == Input::RpcFailure ? sysconf(_SC_PAGESIZE) : kMaxWrite;
            out.max_pages = 16;
            out.time_gran = 1;
            return Reply(header.unique, out);
        }
        case FUSE_LOOKUP: {
            if (!NameIs(payload, count, "file")) return ReplyError(header.unique, ENOENT);
            fuse_entry_out out{};
            out.nodeid = 2;
            out.generation = 1;
            out.entry_valid = out.attr_valid = 60;
            out.attr = Attr(false);
            return Reply(header.unique, out);
        }
        case FUSE_GETATTR: {
            fuse_attr_out out{};
            out.attr_valid = 60;
            out.attr = Attr(header.nodeid == 1);
            return Reply(header.unique, out);
        }
        case FUSE_SETATTR: {
            if (count < sizeof(fuse_setattr_in)) return false;
            fuse_setattr_in in{};
            memcpy(&in, payload, sizeof(in));
            if (test_.setattr_error) return ReplyError(header.unique, test_.setattr_error);
            if (in.valid & FATTR_MODE) mode = in.mode & 07777;
            if (in.valid & FATTR_SIZE) size = in.size;
            fuse_attr_out out{};
            out.attr_valid = 60;
            // Intentionally report only daemon-known size. Dirty writeback EOF
            // can be newer than this value on a non-truncate SETATTR reply.
            out.attr = Attr(false);
            return Reply(header.unique, out);
        }
        case FUSE_OPEN: {
            fuse_open_out out{};
            out.fh = 42;
            out.open_flags = test_.policy.direct ? kDirectIo : 0;
            return Reply(header.unique, out);
        }
        case FUSE_READ: {
            if (count < sizeof(fuse_read_in)) return false;
            fuse_read_in in{};
            memcpy(&in, payload, sizeof(in));
            size_t bytes = in.offset < size ? std::min<uint64_t>(in.size, size - in.offset) : 0;
            if (defer_read_reply) {
                if (read_pending) return false;
                read_pending = true;
                pending_read_unique_ = header.unique;
                pending_read_size_ = bytes;
                return true;
            }
            std::vector<unsigned char> data(bytes, 'o');
            return Reply(header.unique, data.data(), data.size());
        }
        case FUSE_WRITE: {
            if (count < sizeof(fuse_write_in)) return false;
            fuse_write_in in{};
            memcpy(&in, payload, sizeof(in));
            if (count - sizeof(in) != in.size) return false;
            if (test_.input == Input::RpcFailure && ++write_attempts_ == test_.failure_ordinal)
                return ReplyError(header.unique, test_.malformed_failure ? 0 : EIO);
            const auto* data = payload + sizeof(in);
            if ((test_.input == Input::Prefix || test_.input == Input::RpcFailure) &&
                !std::all_of(data, data + in.size, [](unsigned char byte) { return byte == 'p'; }))
                return false;
            if (test_.input == Input::Good && in.offset == 0 && in.size && data[0] != 'p')
                return false;
            if ((test_.policy.handle_killpriv && header.uid != 0) ||
                (in.write_flags & kKillSuidgid)) mode &= ~06000;
            capability = false;
            if (defer_write_reply) {
                if (write_pending) return false;
                write_pending = true;
                pending_unique_ = header.unique;
                pending_size_ = in.size;
                pending_offset_ = in.offset;
                return true;
            }
            size = std::max<uint64_t>(size, in.offset + in.size);
            committed_write_bytes += in.size;
            fuse_write_out out{};
            out.size = in.size;
            return Reply(header.unique, out);
        }
        case FUSE_GETXATTR: {
            if (count < sizeof(fuse_getxattr_in)) return false;
            fuse_getxattr_in in{};
            memcpy(&in, payload, sizeof(in));
            if (!NameIs(payload + sizeof(in), count - sizeof(in), "security.capability"))
                return ReplyError(header.unique, ENODATA);
            if (test_.getxattr_error) return ReplyError(header.unique, test_.getxattr_error);
            if (!capability) return ReplyError(header.unique, ENODATA);
            if (!in.size) {
                fuse_getxattr_out out{};
                out.size = sizeof(kCapability);
                return Reply(header.unique, out);
            }
            if (in.size < sizeof(kCapability)) return ReplyError(header.unique, ERANGE);
            return Reply(header.unique, kCapability, sizeof(kCapability));
        }
        case FUSE_REMOVEXATTR:
            if (!NameIs(payload, count, "security.capability"))
                return ReplyError(header.unique, ENODATA);
            if (test_.remove_error) return ReplyError(header.unique, test_.remove_error);
            capability = false;
            return ReplyError(header.unique, 0);
        case FUSE_SETXATTR:
            // Only used to install the benign capability after a dirty write.
            if (count < 8 + sizeof("security.capability") + sizeof(kCapability)) return false;
            if (!NameIs(payload + 8, sizeof("security.capability"), "security.capability"))
                return ReplyError(header.unique, EINVAL);
            capability = true;
            return ReplyError(header.unique, 0);
        case FUSE_FORGET:
        case 42:  // BATCH_FORGET has no reply.
            return true;
        case FUSE_ACCESS:
        case FUSE_FLUSH:
        case FUSE_RELEASE:
        case FUSE_FSYNC:
        case FUSE_DESTROY:
            return ReplyError(header.unique, 0);
        default:
            return ReplyError(header.unique, ENOSYS);
        }
    }

 private:
    static bool NameIs(const unsigned char* bytes, size_t count, const char* name) {
        return count == strlen(name) + 1 && memcmp(bytes, name, count) == 0;
    }
    fuse_attr Attr(bool root) const {
        fuse_attr attr{};
        attr.ino = root ? 1 : 2;
        attr.mode = root ? (S_IFDIR | 0755) : (S_IFREG | mode);
        attr.nlink = root ? 2 : 1;
        attr.uid = attr.gid = 1000;
        attr.size = root ? 0 : size;
        attr.blksize = 4096;
        return attr;
    }
    bool Reply(uint64_t unique, const void* payload, size_t len, int error = 0) {
        fuse_out_header header{};
        header.len = sizeof(header) + len;
        header.unique = unique;
        header.error = -error;
        std::vector<unsigned char> reply(header.len);
        memcpy(reply.data(), &header, sizeof(header));
        if (len) memcpy(reply.data() + sizeof(header), payload, len);
        return write(fd_, reply.data(), reply.size()) == static_cast<ssize_t>(reply.size());
    }
    template<class T> bool Reply(uint64_t unique, const T& payload) {
        return Reply(unique, &payload, sizeof(payload));
    }
    bool ReplyError(uint64_t unique, int error) { return Reply(unique, nullptr, 0, error); }
    int fd_;
    Case test_;
    std::vector<unsigned char> buffer_;
    uint64_t pending_unique_ = 0;
    uint32_t pending_size_ = 0;
    uint64_t pending_offset_ = 0;
    uint64_t pending_read_unique_ = 0;
    size_t pending_read_size_ = 0;
    unsigned write_attempts_ = 0;
};

void ReadEvents(int watcher, ClientResult* result) {
    alignas(inotify_event) unsigned char bytes[4096];
    for (;;) {
        ssize_t len = read(watcher, bytes, sizeof(bytes));
        if (len < 0 && errno == EINTR) continue;
        if (len < 0 && errno == EAGAIN) return;
        if (len <= 0) { result->stage = 11; return; }
        for (size_t offset = 0; offset < static_cast<size_t>(len);) {
            if (sizeof(inotify_event) > static_cast<size_t>(len) - offset) {
                result->stage = 11;
                return;
            }
            const auto* event = reinterpret_cast<const inotify_event*>(bytes + offset);
            size_t next = sizeof(*event) + event->len;
            if (next > static_cast<size_t>(len) - offset) { result->stage = 11; return; }
            if (event->mask & IN_ATTRIB) {
                result->attrib_after_modify |= result->modify != 0;
                ++result->attrib;
            }
            if (event->mask & IN_MODIFY) ++result->modify;
            offset += next;
        }
    }
}

void RunClient(int fuse_fd, int result_fd, const char* path, Case test) {
    close(fuse_fd);
    alarm(15);
    ClientResult result;
    int fd = -1, watcher = -1;
    void* memory = MAP_FAILED;
    long page = sysconf(_SC_PAGESIZE);
    const bool dirty = test.input == Input::DirtyMode || test.input == Input::DirtyCapability;
    auto fail = [&](int stage) { result.stage = stage; result.error = errno; };
    if (!test.privileged && !dirty && (setgid(1000) || setuid(1000))) fail(1);
    if (!result.stage && (fd = open(path, O_RDWR)) < 0) fail(2);
    if (!result.stage && (watcher = inotify_init1(IN_NONBLOCK | IN_CLOEXEC)) < 0) fail(3);
    if (!result.stage && inotify_add_watch(watcher, path, IN_ATTRIB | IN_MODIFY) < 0) fail(4);
    if (!result.stage) {
        memory = mmap(nullptr, 2 * page, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (memory == MAP_FAILED) fail(5);
        else {
            memset(memory, 'p', 2 * page);
            if (test.input != Input::RpcFailure &&
                mprotect(static_cast<char*>(memory) + page, page, PROT_NONE)) fail(5);
        }
    }
    if (!result.stage && dirty) {
        if (pwrite(fd, "x", 1, 8192) != 1) fail(6);
        if (!result.stage && test.input == Input::DirtyCapability &&
            fsetxattr(fd, "security.capability", kCapability, sizeof(kCapability), 0)) fail(7);
        if (!result.stage && test.input == Input::DirtyMode && (setgid(1000) || setuid(1000))) fail(8);
        if (!result.stage) {
            ReadEvents(watcher, &result);
            result.attrib = result.modify = 0;
            result.attrib_after_modify = false;
        }
    }
    if (!result.stage && test.input == Input::FileLimit) {
        const rlimit limit{0, 0};
        if (signal(SIGXFSZ, SIG_IGN) == SIG_ERR || setrlimit(RLIMIT_FSIZE, &limit)) fail(13);
    }
    if (!result.stage) {
        errno = 0;
        void* bad = static_cast<char*>(memory) + page;
        if (test.input == Input::Prefix) {
            iovec vectors[2]{{memory, static_cast<size_t>(page)}, {bad, static_cast<size_t>(page)}};
            result.written = writev(fd, vectors, 2);
        } else {
            const bool readable = test.input == Input::Good || test.input == Input::RpcFailure;
            size_t length = test.input == Input::Empty ? 0 :
                            test.input == Input::RpcFailure ? 2 * page : 1;
            result.written = write(fd, readable ? memory : bad, length);
        }
        result.error = errno;
        result.position = lseek(fd, 0, SEEK_CUR);
        struct stat st{};
        if (fstat(fd, &st)) fail(9);
        else { result.mode = st.st_mode; result.size = st.st_size; }
        if (!result.stage) ReadEvents(watcher, &result);
        if (!result.stage && (test.policy.writeback || dirty) && fsync(fd)) fail(10);
    }
    if (memory != MAP_FAILED) munmap(memory, 2 * page);
    if (watcher >= 0) close(watcher);
    if (fd >= 0 && close(fd) && !result.stage) fail(12);
    // Pipe message is smaller than PIPE_BUF; parent keeps serving RELEASE/FLUSH
    // until the child has closed every mount-owned descriptor.
    if (write(result_fd, &result, sizeof(result)) != sizeof(result)) _exit(2);
    close(result_fd);
    _exit(0);
}

class FuseWritePrivileges : public testing::Test {
 protected:
    void Run(Case test) {
        char mountpoint[128], path[160], options[256];
        snprintf(mountpoint, sizeof(mountpoint), "/tmp/fuse_write_owner_%d", getpid());
        snprintf(path, sizeof(path), "%s/file", mountpoint);
        ASSERT_EQ(0, mkdir(mountpoint, 0755)) << strerror(errno);
        int fd = open("/dev/fuse", O_RDWR | O_CLOEXEC);
        if (fd < 0) { int error = errno; rmdir(mountpoint); FAIL() << strerror(error); }
        snprintf(options, sizeof(options),
                 "fd=%d,rootmode=040755,user_id=0,group_id=0,allow_other", fd);
        if (mount("none", mountpoint, "fuse", 0, options)) {
            int error = errno;
            close(fd);
            rmdir(mountpoint);
            FAIL() << "mount: " << strerror(error);
        }
        WriteDaemon daemon(fd, test);
        bool okay = fuseg_wait_readable(fd, 3000) == 0 && daemon.Serve();
        int results[2]{-1, -1};
        pid_t child = -1;
        if (okay && pipe(results)) okay = false;
        if (okay) child = fork();
        if (child == 0) { close(results[0]); RunClient(fd, results[1], path, test); }
        if (child < 0) okay = false;
        if (results[1] >= 0) close(results[1]);
        ClientResult result;
        if (okay) {
            timespec start{};
            clock_gettime(CLOCK_MONOTONIC, &start);
            while (okay) {
                timespec now{};
                clock_gettime(CLOCK_MONOTONIC, &now);
                long elapsed = (now.tv_sec - start.tv_sec) * 1000 +
                               (now.tv_nsec - start.tv_nsec) / 1000000;
                if (elapsed >= 20000) { okay = false; break; }
                pollfd descriptors[2]{{fd, POLLIN, 0}, {results[0], POLLIN, 0}};
                int ready = poll(descriptors, 2, 20000 - elapsed);
                if (ready < 0 && errno == EINTR) continue;
                if (ready <= 0) { okay = false; break; }
                if (descriptors[1].revents & (POLLIN | POLLHUP)) {
                    okay = read(results[0], &result, sizeof(result)) == sizeof(result);
                    break;
                }
                if (descriptors[0].revents & POLLIN) okay = daemon.Serve();
                else { okay = false; break; }
            }
        }
        if (results[0] >= 0) close(results[0]);
        // Abort a failed client first. Closing the daemon releases any pending
        // FUSE request; reap before unmount so no client owns a live file.
        close(fd);
        if (child > 0) {
            if (!okay) kill(child, SIGKILL);
            int status = 0;
            bool reaped = false;
            for (int i = 0; i < 200 && !reaped; ++i) {
                pid_t got = waitpid(child, &status, WNOHANG);
                if (got == child) reaped = true;
                else if (got < 0 && errno != EINTR) break;
                else usleep(10000);
            }
            okay &= reaped && WIFEXITED(status) && WEXITSTATUS(status) == 0;
        }
        std::string trace = "RPC:";
        for (const auto& request : daemon.requests) {
            char entry[80];
            snprintf(entry, sizeof(entry), " %u(flags=%x,mode=%o,size=%llu)", request.opcode,
                     request.flags, request.mode, static_cast<unsigned long long>(request.size));
            trace += entry;
        }
        SCOPED_TRACE(trace);
        EXPECT_EQ(0, umount(mountpoint)) << strerror(errno);
        EXPECT_EQ(0, rmdir(mountpoint)) << strerror(errno);
        ASSERT_TRUE(okay) << "protocol failure or client timeout";
        ASSERT_EQ(0, result.stage) << "client errno=" << result.error;
        Check(test, daemon, result);
    }

 private:
    void Check(const Case& test, const WriteDaemon& daemon, const ClientResult& result) {
        const bool dirty = test.input == Input::DirtyMode || test.input == Input::DirtyCapability;
        const bool privileged = test.privileged || test.input == Input::DirtyCapability;
        const bool empty = test.input == Input::Empty;
        const bool limited = test.input == Input::FileLimit;
        const bool admitted = !empty && !limited;
        const bool cached = !test.policy.direct;
        const bool mode_mask = cached && !privileged && admitted &&
            ((test.initial_mode & S_ISUID) ||
             ((test.initial_mode & (S_ISGID | S_IXGRP)) == (S_ISGID | S_IXGRP)));
        const bool remove_attempt = cached && admitted &&
            (test.capability || test.input == Input::DirtyCapability) && !test.getxattr_error;
        const int precopy_error = limited ? EFBIG : admitted && cached
            ? (remove_attempt && test.remove_error && test.remove_error != EOPNOTSUPP
                ? test.remove_error : test.setattr_error) : 0;
        const bool first_rpc_failure = test.input == Input::RpcFailure && test.failure_ordinal == 1;
        const int error = precopy_error ? precopy_error : first_rpc_failure ? EIO : 0;
        const ssize_t expected = error ? -1 : test.input == Input::Good ? 1 :
            (test.input == Input::Prefix || test.input == Input::RpcFailure)
                ? sysconf(_SC_PAGESIZE) : empty ? 0 : -1;
        EXPECT_EQ(expected, result.written);
        if (expected == -1) { EXPECT_EQ(error ? error : EFAULT, result.error); }
        EXPECT_EQ(expected > 0 ? expected : 0, result.position);
        EXPECT_EQ(expected > 0 ? 1u : 0u, result.modify);
        EXPECT_EQ(mode_mask && !precopy_error ? 1u : 0u, result.attrib);
        EXPECT_FALSE(result.attrib_after_modify);
        std::vector<size_t> modes, writes, removes, empty_setattrs;
        unsigned flags = 0;
        uint64_t written = 0;
        for (size_t i = 0; i < daemon.requests.size(); ++i) {
            const auto& request = daemon.requests[i];
            if (request.opcode == FUSE_SETATTR && (request.flags & FATTR_MODE)) {
                modes.push_back(i);
                EXPECT_EQ(0u, request.mode & 06000);
            }
            if (!admitted) {
                EXPECT_NE(static_cast<unsigned>(FUSE_SETATTR), request.opcode);
                EXPECT_NE(static_cast<unsigned>(FUSE_GETXATTR), request.opcode);
            }
            if (request.opcode == FUSE_SETATTR && request.flags == 0) empty_setattrs.push_back(i);
            if (request.opcode == FUSE_REMOVEXATTR) removes.push_back(i);
            if (request.opcode == FUSE_WRITE) {
                writes.push_back(i);
                flags |= request.flags;
                written += request.size;
            }
        }
        const bool remove_failed = remove_attempt && test.remove_error &&
                                   test.remove_error != EOPNOTSUPP;
        if (mode_mask && !test.policy.handle_killpriv && !remove_failed) {
            EXPECT_EQ(1u, modes.size());
        } else { EXPECT_TRUE(modes.empty()); }
        EXPECT_EQ(remove_attempt ? 1u : 0u, removes.size());
        if (test.input == Input::DirtyMode) {
            ASSERT_EQ(1u, modes.size());
            ASSERT_FALSE(writes.empty());
            EXPECT_LT(writes.front(), modes.front()) << "dirty data must drain before MODE SETATTR";
        } else if (!modes.empty() && !writes.empty()) {
            EXPECT_LT(modes.front(), writes.front());
        }
        if (!removes.empty() && !writes.empty()) {
            EXPECT_LT(removes.front(), writes.front());
        }
        if (!dirty) {
            EXPECT_EQ(expected > 0 || test.input == Input::RpcFailure, !writes.empty());
            // Cached writeback may flush a full dirty page; direct/writethrough
            // send exactly the bytes admitted by the originating syscall.
            if (test.input == Input::RpcFailure) {
                EXPECT_EQ(test.failure_ordinal, writes.size());
                EXPECT_EQ(static_cast<uint64_t>(expected > 0 ? expected : 0),
                          daemon.committed_write_bytes);
                EXPECT_EQ(expected > 0 ? expected : 16, result.size);
            } else if (!test.policy.writeback) {
                EXPECT_EQ(static_cast<uint64_t>(expected > 0 ? expected : 0), written);
            }
            if (cached) { EXPECT_EQ(0u, flags & kKillSuidgid); }
            else if (!writes.empty()) {
                EXPECT_EQ(privileged ? 0u : kKillSuidgid, flags & kKillSuidgid);
            }
            if (!empty && !precopy_error && mode_mask && !test.policy.handle_killpriv) {
                EXPECT_EQ(0u, result.mode & 06000);
            }
            if (!admitted || (test.policy.direct && expected < 0) || precopy_error ||
                (cached && test.policy.handle_killpriv && expected < 0) || privileged) {
                EXPECT_EQ(test.initial_mode, result.mode & 07777);
            }
            if (!test.policy.writeback && expected > 0 && !privileged) {
                EXPECT_EQ(0u, result.mode & 06000) << "successful daemon mode must invalidate cached attrs";
            }
        } else {
            EXPECT_EQ(8193, result.size) << "SETATTR response must preserve dirty cached EOF";
            ASSERT_FALSE(writes.empty());
            if (test.input == Input::DirtyCapability) {
                ASSERT_EQ(1u, empty_setattrs.size());
                EXPECT_LT(empty_setattrs.front(), writes.front())
                    << "capability-only stage must not require flushing dirty data";
                EXPECT_EQ(0u, result.attrib);
            }
        }
    }
};

class FuseWritePolicy : public FuseWritePrivileges, public testing::WithParamInterface<Policy> {};

TEST_P(FuseWritePolicy, FaultBeforePayloadHasOwnerSpecificPrivilegeEffects) { Run({GetParam()}); }
TEST_P(FuseWritePolicy, SuccessfulWriteUsesCallerAndDaemonContract) {
    Case test{GetParam()}; test.input = Input::Good; Run(test);
}
TEST_P(FuseWritePolicy, PrefixFaultCommitsOnlyTheReadablePrefix) {
    Case test{GetParam()}; test.input = Input::Prefix; Run(test);
}
TEST_P(FuseWritePolicy, ZeroLengthDoesNotEnterPrivilegeStage) {
    Case test{GetParam()}; test.input = Input::Empty; Run(test);
}
TEST_P(FuseWritePolicy, FileLimitWinsBeforePrivilegeAndUserCopy) {
    Case test{GetParam()}; test.input = Input::FileLimit; Run(test);
}
TEST_P(FuseWritePolicy, CapFsetidPreservesSetidOnFault) {
    Case test{GetParam()}; test.privileged = true; Run(test);
}
TEST_P(FuseWritePolicy, CapFsetidControlsDirectWriteFlag) {
    Case test{GetParam()}; test.privileged = true; test.input = Input::Good; Run(test);
}
TEST_P(FuseWritePolicy, CapabilityIsRemovedBeforeCachedPayloadCopy) {
    Case test{GetParam()}; test.capability = true; Run(test);
}
TEST_P(FuseWritePolicy, CapFsetidDoesNotExemptCapabilityRemoval) {
    Case test{GetParam()}; test.capability = test.privileged = true; Run(test);
}
INSTANTIATE_TEST_SUITE_P(OwnerPolicies, FuseWritePolicy, testing::Values(
    Policy{}, Policy{false, true, false}, Policy{false, false, true},
    Policy{false, true, true}, Policy{true, false, false}, Policy{true, true, false}),
    [](const testing::TestParamInfo<Policy>& parameter) {
        const Policy& p = parameter.param;
        return std::string(p.direct ? "Direct" : p.writeback ? "Writeback" : "Cached") +
               (p.handle_killpriv ? "HandleKillpriv" : "KernelKillpriv");
    });

enum class HoldConsumer { Read, Readv, Tee, Splice, Available, FileSplice };
struct HoldCase { HoldConsumer consumer; unsigned accepted; int error; bool input = false; };

class FuseSpliceHold : public testing::TestWithParam<HoldCase> {};

// A real deferred FUSE_WRITE establishes the reservation window. Each case
// owns a fresh source, so only the selected competitor can consume its tail.
TEST_P(FuseSpliceHold, InternalReservationDoesNotBecomeNonblockingEagain) {
    struct Result {
        int role = -1;
        ssize_t count = -1;
        int error = 0;
        off_t position = -1;
        int available = -1;
        char byte = 0;
    };
    const HoldCase parameter = GetParam();
    char mountpoint[128], path[160], options[256];
    snprintf(mountpoint, sizeof(mountpoint), "/tmp/fuse_splice_hold_%d", getpid());
    snprintf(path, sizeof(path), "%s/file", mountpoint);
    ASSERT_EQ(0, mkdir(mountpoint, 0755)) << strerror(errno);
    int fd = open("/dev/fuse", O_RDWR | O_CLOEXEC);
    if (fd < 0) { int error = errno; rmdir(mountpoint); FAIL() << strerror(error); }
    snprintf(options, sizeof(options),
             "fd=%d,rootmode=040755,user_id=0,group_id=0,allow_other", fd);
    if (mount("none", mountpoint, "fuse", 0, options)) {
        int error = errno;
        close(fd);
        rmdir(mountpoint);
        FAIL() << strerror(error);
    }
    Case test{};
    test.input = Input::Good;
    test.initial_mode = 0666;
    // Linux may continue iter_file_splice_write after a short primary write.
    // Fail that next request so both a short-return owner and Linux's loop
    // report exactly the committed prefix without another deferred reply.
    if (!parameter.input && !parameter.error && parameter.accepted == 2) {
        test.input = Input::RpcFailure;
        test.failure_ordinal = 2;
    }
    // Direct I/O makes the primary transfer wait for this exact WRITE reply.
    test.policy.direct = true;
    WriteDaemon daemon(fd, test);
    daemon.defer_write_reply = !parameter.input;
    daemon.defer_read_reply = parameter.input;
    bool okay = fuseg_wait_readable(fd, 3000) == 0 && daemon.Serve();
    int source[2]{-1, -1}, destination[2]{-1, -1}, start[2]{-1, -1};
    int attempted[2]{-1, -1}, results[2]{-1, -1};
    int* channels[]{source, destination, start, attempted, results};
    for (int* channel : channels) {
        if (okay && pipe2(channel, O_CLOEXEC)) okay = false;
    }
    const bool empty_input = parameter.input && parameter.consumer == HoldConsumer::FileSplice;
    const std::string initial = parameter.input ? (empty_input ? "" : "TAIL") : "ppppTAIL";
    if (okay) okay = fcntl(source[0], F_SETFL, O_NONBLOCK) == 0 &&
        write(source[1], initial.data(), initial.size()) == static_cast<ssize_t>(initial.size());
    char sink_name[] = "/tmp/fuse-splice-sink-XXXXXX";
    int sink = okay ? mkstemp(sink_name) : -1;
    if (sink < 0) okay = false;
    else if (unlink(sink_name)) okay = false;
    pid_t children[2]{-1, -1};
    for (int role = 0; role < 2 && okay; ++role) {
        children[role] = fork();
        if (children[role] < 0) { okay = false; break; }
        if (children[role] == 0) {
            close(fd);
            close(start[1]);
            close(attempted[0]);
            close(results[0]);
            if (role != 0 || !parameter.input) close(source[1]);
            alarm(15);
            Result result{};
            result.role = role;
            if (role == 0) {
                close(start[0]);
                close(attempted[1]);
                int file = open(path, parameter.input ? O_RDONLY : O_WRONLY);
                if (file >= 0) {
                    result.count = parameter.input
                        ? splice(file, nullptr, source[1], nullptr, 4, SPLICE_F_NONBLOCK)
                        : splice(source[0], nullptr, file, nullptr, 4, SPLICE_F_NONBLOCK);
                    result.error = result.count < 0 ? errno : 0;
                    result.position = lseek(file, 0, SEEK_CUR);
                    // Report before close: RELEASE may require daemon service.
                    if (write(results[1], &result, sizeof(result)) != sizeof(result)) _exit(2);
                    close(file);
                    _exit(0);
                }
                result.error = errno;
            } else {
                char token;
                if (read(start[0], &token, 1) != 1 || write(attempted[1], "a", 1) != 1)
                    _exit(3);
                iovec vector{&result.byte, 1};
                switch (parameter.consumer) {
                case HoldConsumer::Read:
                    result.count = read(source[0], &result.byte, 1);
                    break;
                case HoldConsumer::Readv:
                    result.count = readv(source[0], &vector, 1);
                    break;
                case HoldConsumer::Tee:
                    result.count = tee(source[0], destination[1], 1, SPLICE_F_NONBLOCK);
                    break;
                case HoldConsumer::Splice:
                    result.count = splice(source[0], nullptr, destination[1], nullptr,
                                          1, SPLICE_F_NONBLOCK);
                    break;
                case HoldConsumer::Available:
                    result.count = ioctl(source[0], FIONREAD, &result.available);
                    break;
                case HoldConsumer::FileSplice:
                    result.count = splice(source[0], nullptr, sink, nullptr, 1,
                                          SPLICE_F_NONBLOCK);
                    if (result.count == 1 && pread(sink, &result.byte, 1, 0) != 1) _exit(4);
                    break;
                }
                result.error = result.count < 0 ? errno : 0;
                if ((parameter.consumer == HoldConsumer::Tee ||
                     parameter.consumer == HoldConsumer::Splice) && result.count == 1 &&
                    read(destination[0], &result.byte, 1) != 1) _exit(4);
            }
            if (write(results[1], &result, sizeof(result)) != sizeof(result)) _exit(2);
            _exit(0);
        }
    }
    for (int descriptor : {start[0], attempted[1], results[1]}) if (descriptor >= 0) close(descriptor);
    start[0] = attempted[1] = results[1] = -1;
    bool complete[2]{false, false};
    Result completions[2]{};
    auto now_ms = [] {
        timespec now{};
        clock_gettime(CLOCK_MONOTONIC, &now);
        return static_cast<int64_t>(now.tv_sec) * 1000 + now.tv_nsec / 1000000;
    };
    const int64_t deadline = now_ms() + 12000;
    auto receive_result = [&] {
        Result result{};
        if (read(results[0], &result, sizeof(result)) != sizeof(result) ||
            result.role < 0 || result.role > 1 || complete[result.role]) {
            okay = false;
            return;
        }
        complete[result.role] = true;
        completions[result.role] = result;
    };
    auto pump_until = [&](auto predicate) {
        while (okay && !predicate()) {
            int timeout = static_cast<int>(deadline - now_ms());
            if (timeout <= 0) { okay = false; break; }
            pollfd descriptors[2]{{fd, POLLIN, 0}, {results[0], POLLIN, 0}};
            int count = poll(descriptors, 2, timeout);
            if (count < 0 && errno == EINTR) continue;
            if (count <= 0) { okay = false; break; }
            if (descriptors[1].revents & POLLIN) receive_result();
            if (descriptors[0].revents & POLLIN) okay &= daemon.Serve();
            if ((descriptors[0].revents | descriptors[1].revents) &
                (POLLERR | POLLNVAL)) okay = false;
        }
    };
    auto pending = [&] { return parameter.input ? daemon.read_pending : daemon.write_pending; };
    pump_until([&] { return pending() || complete[0]; });
    // A kernel that rejects splice to this FUSE mode fails explicitly; this
    // test never weakens the assertion by skipping an unsupported route.
    okay &= pending() && !complete[0];
    bool crossed = false;
    short source_events = 0;
    if (okay) {
        okay = write(start[1], "s", 1) == 1;
        pollfd acknowledgement{attempted[0], POLLIN, 0};
        if (okay) okay = poll(&acknowledgement, 1, 3000) == 1 &&
            (acknowledgement.revents & POLLIN);
        char token;
        if (okay) okay = read(attempted[0], &token, 1) == 1 && token == 'a';
        // The competitor has announced its syscall while WRITE remains held.
        // Keep polling finite even when the implementation returns too early.
        pollfd completion{results[0], POLLIN, 0};
        if (okay) {
            const int count = poll(&completion, 1, 300);
            if (count < 0) okay = false;
            else if (count > 0 && (completion.revents & POLLIN)) {
                receive_result();
                crossed = complete[1];
            }
        }
        pollfd ready{source[0], POLLIN, 0};
        if (okay && poll(&ready, 1, 0) < 0) okay = false;
        source_events = ready.revents;
        if (okay) okay = parameter.input ? daemon.CompleteRead()
            : daemon.CompleteWrite(parameter.accepted, parameter.error);
        pump_until([&] { return complete[0] && complete[1]; });
    }
    // Closing the connection aborts outstanding FUSE RPCs on failure; do not
    // perform a blocking waitpid on a regression in internal pipe locking.
    close(fd);
    if (sink >= 0) close(sink);
    bool all_reaped = true;
    for (pid_t child : children) {
        if (child <= 0) continue;
        if (!okay) kill(child, SIGKILL);
        bool reaped = false;
        int status = 0;
        for (int attempt = 0; attempt < 200 && !reaped; ++attempt) {
            pid_t waited = waitpid(child, &status, WNOHANG);
            if (waited == child) reaped = true;
            else if (waited < 0 && errno != EINTR) break;
            else usleep(10000);
        }
        if (!reaped) kill(child, SIGKILL);
        all_reaped &= reaped;
        okay &= reaped && WIFEXITED(status) && WEXITSTATUS(status) == 0;
    }
    std::string remaining;
    if (okay) {
        char bytes[8];
        ssize_t count = read(source[0], bytes, sizeof(bytes));
        if (count < 0) okay = false;
        else remaining.assign(bytes, count);
    }
    for (int* channel : channels) for (int descriptor : {channel[0], channel[1]})
        if (descriptor >= 0) close(descriptor);
    EXPECT_EQ(0, umount(mountpoint)) << strerror(errno);
    EXPECT_EQ(0, rmdir(mountpoint)) << strerror(errno);
    ASSERT_TRUE(okay) << "FUSE/child protocol failure; splicer=" << completions[0].count
                      << " errno=" << completions[0].error << "; competitor="
                      << completions[1].count << " errno=" << completions[1].error;
    ASSERT_TRUE(all_reaped);
    EXPECT_FALSE(crossed) << "NONBLOCK/FIONREAD bypassed an unfinished pipe reservation";
    EXPECT_EQ(!initial.empty(), (source_events & POLLIN) != 0)
        << "poll must reflect published slots independently of internal transactions";
    const unsigned consumed = parameter.error ? 0 : parameter.accepted;
    EXPECT_EQ(parameter.error ? -1 : static_cast<ssize_t>(consumed), completions[0].count);
    EXPECT_EQ(parameter.error, completions[0].error);
    EXPECT_EQ(consumed, completions[0].position);
    const std::string tail = parameter.input ? initial + "pppp" : initial.substr(consumed);
    if (parameter.consumer == HoldConsumer::Available) {
        EXPECT_EQ(0, completions[1].count);
        EXPECT_EQ(tail.size(), static_cast<size_t>(completions[1].available));
    } else {
        EXPECT_EQ(1, completions[1].count);
        EXPECT_EQ(0, completions[1].error);
        EXPECT_EQ(tail[0], completions[1].byte);
    }
    const bool reader_consumed = parameter.consumer == HoldConsumer::Read ||
        parameter.consumer == HoldConsumer::Readv || parameter.consumer == HoldConsumer::Splice ||
        parameter.consumer == HoldConsumer::FileSplice;
    EXPECT_EQ(tail.substr(reader_consumed ? 1 : 0), remaining);
    EXPECT_EQ(parameter.input ? 0 : consumed, daemon.committed_write_bytes);
}

std::vector<HoldCase> SpliceHoldCases() {
    std::vector<HoldCase> cases;
    for (HoldConsumer consumer : {HoldConsumer::Read, HoldConsumer::Readv,
                                 HoldConsumer::Tee, HoldConsumer::Splice,
                                 HoldConsumer::Available}) {
        cases.push_back({consumer, 4, 0});
        cases.push_back({consumer, 2, 0});
        cases.push_back({consumer, 0, EIO});
    }
    // These windows own the pipe transaction without any reservation flag.
    // The empty FileSplice case also exercises NONBLOCK's early input prep.
    for (HoldConsumer consumer : {HoldConsumer::Read, HoldConsumer::Available,
                                 HoldConsumer::FileSplice}) {
        cases.push_back({consumer, 4, 0, true});
    }
    return cases;
}

std::string HoldCaseName(const testing::TestParamInfo<HoldCase>& parameter) {
    const char* names[]{"Read", "Readv", "Tee", "Splice", "Available", "FileSplice"};
    return std::string(parameter.param.input ? "Input" : "Output") +
        names[static_cast<unsigned>(parameter.param.consumer)] +
        (parameter.param.error ? "Error" : parameter.param.accepted == 4 ? "Full" : "Short");
}

INSTANTIATE_TEST_SUITE_P(Consumers, FuseSpliceHold, testing::ValuesIn(SpliceHoldCases()),
                         HoldCaseName);

// Hold a real WRITE reply while a separately opened root descriptor tries to
// reinstall a capability. Both clients acknowledge readiness; the mutator also
// acknowledges the precise point at which it starts the syscall. A bounded
// poll detects a request crossing the still-open primary write transaction.
TEST(FuseWriteConcurrency, CapabilityMutationWaitsForTheWholeWriteTransaction) {
    struct Completion { int role; int stage; int error; };
    char mountpoint[128], path[160], options[256];
    snprintf(mountpoint, sizeof(mountpoint), "/tmp/fuse_write_concurrent_%d", getpid());
    snprintf(path, sizeof(path), "%s/file", mountpoint);
    ASSERT_EQ(0, mkdir(mountpoint, 0755)) << strerror(errno);
    int fd = open("/dev/fuse", O_RDWR | O_CLOEXEC);
    if (fd < 0) { int error = errno; rmdir(mountpoint); FAIL() << strerror(error); }
    snprintf(options, sizeof(options),
             "fd=%d,rootmode=040755,user_id=0,group_id=0,allow_other", fd);
    if (mount("none", mountpoint, "fuse", 0, options)) {
        int error = errno;
        close(fd);
        rmdir(mountpoint);
        FAIL() << strerror(error);
    }
    Case test{};
    test.input = Input::Good;
    test.privileged = test.capability = true;
    WriteDaemon daemon(fd, test);
    daemon.defer_write_reply = true;
    bool okay = fuseg_wait_readable(fd, 3000) == 0 && daemon.Serve();
    int channels[4][2]{{-1, -1}, {-1, -1}, {-1, -1}, {-1, -1}};
    for (auto& channel : channels) {
        if (okay && pipe(channel)) okay = false;
    }
    pid_t children[2]{-1, -1};
    for (int role = 0; role < 2 && okay; ++role) {
        children[role] = fork();
        if (children[role] < 0) { okay = false; break; }
        if (children[role] == 0) {
            close(fd);
            for (int index = 0; index < 4; ++index) {
                if (index == role) close(channels[index][1]);
                else if (index >= 2) close(channels[index][0]);
                else { close(channels[index][0]); close(channels[index][1]); }
            }
            alarm(15);
            Completion completion{role, 0, 0};
            int file = open(path, O_RDWR);
            if (file < 0) completion.stage = 1;
            char token = role == 0 ? 'w' : 'm';
            if (!completion.stage && write(channels[2][1], &token, 1) != 1) completion.stage = 2;
            if (!completion.stage && read(channels[role][0], &token, 1) != 1) completion.stage = 3;
            if (!completion.stage && role == 1) {
                token = 'a';
                if (write(channels[2][1], &token, 1) != 1) completion.stage = 4;
            }
            if (!completion.stage) {
                if (role == 0 ? write(file, "p", 1) != 1 :
                    fsetxattr(file, "security.capability", kCapability, sizeof(kCapability), 0) != 0)
                    completion.stage = 5;
            }
            completion.error = errno;
            if (file >= 0 && close(file) && !completion.stage) {
                completion.stage = 6;
                completion.error = errno;
            }
            if (write(channels[3][1], &completion, sizeof(completion)) != sizeof(completion)) _exit(2);
            _exit(0);
        }
    }
    for (int index = 0; index < 4; ++index) {
        int closed = index < 2 ? 0 : 1;
        if (channels[index][closed] >= 0) {
            close(channels[index][closed]);
            channels[index][closed] = -1;
        }
    }
    bool ready[2]{false, false}, complete[2]{false, false}, attempted = false;
    Completion completions[2]{};
    auto now_ms = [] {
        timespec now{};
        clock_gettime(CLOCK_MONOTONIC, &now);
        return static_cast<int64_t>(now.tv_sec) * 1000 + now.tv_nsec / 1000000;
    };
    const int64_t deadline = now_ms() + 20000;
    auto pump_until = [&](auto predicate) {
        while (okay && !predicate()) {
            int timeout = static_cast<int>(deadline - now_ms());
            if (timeout <= 0) { okay = false; break; }
            pollfd descriptors[3]{{fd, POLLIN, 0}, {channels[2][0], POLLIN, 0},
                                  {channels[3][0], POLLIN, 0}};
            int count = poll(descriptors, 3, timeout);
            if (count < 0 && errno == EINTR) continue;
            if (count <= 0) { okay = false; break; }
            if (descriptors[1].revents & POLLIN) {
                char tokens[16];
                ssize_t len = read(channels[2][0], tokens, sizeof(tokens));
                if (len <= 0) { okay = false; break; }
                for (ssize_t i = 0; i < len; ++i) {
                    if (tokens[i] == 'w') ready[0] = true;
                    else if (tokens[i] == 'm') ready[1] = true;
                    else if (tokens[i] == 'a') attempted = true;
                    else okay = false;
                }
            }
            if (descriptors[2].revents & POLLIN) {
                Completion messages[2];
                ssize_t len = read(channels[3][0], messages, sizeof(messages));
                if (len <= 0 || len % sizeof(Completion)) { okay = false; break; }
                for (size_t i = 0; i < static_cast<size_t>(len) / sizeof(Completion); ++i) {
                    if (messages[i].role < 0 || messages[i].role > 1) { okay = false; break; }
                    complete[messages[i].role] = true;
                    completions[messages[i].role] = messages[i];
                    if (messages[i].stage) okay = false;
                }
            }
            if (descriptors[0].revents & POLLIN) okay &= daemon.Serve();
            if ((descriptors[1].revents | descriptors[2].revents) & (POLLERR | POLLNVAL))
                okay = false;
        }
    };
    pump_until([&] { return ready[0] && ready[1]; });
    if (okay) okay = write(channels[0][1], "w", 1) == 1;
    pump_until([&] { return daemon.write_pending; });
    if (okay) okay = write(channels[1][1], "m", 1) == 1;
    pump_until([&] { return attempted; });
    bool crossed = false;
    if (okay) {
        const int64_t blocked_until = now_ms() + 1000;
        while (okay && now_ms() < blocked_until) {
            pollfd descriptor{fd, POLLIN, 0};
            int timeout = static_cast<int>(std::max<int64_t>(0, blocked_until - now_ms()));
            int count = poll(&descriptor, 1, timeout);
            if (count < 0 && errno == EINTR) continue;
            if (!count) break;
            if (count < 0 || !(descriptor.revents & POLLIN)) { okay = false; break; }
            okay &= daemon.Serve();
            if (!daemon.requests.empty() && daemon.requests.back().opcode == FUSE_SETXATTR) {
                crossed = true;
                break;
            }
        }
        // Include a request already handled while receiving the attempt token.
        for (const auto& request : daemon.requests) crossed |= request.opcode == FUSE_SETXATTR;
        okay &= daemon.CompleteWrite();
        pump_until([&] { return complete[0] && complete[1]; });
    }
    for (auto& channel : channels) {
        for (int descriptor : channel) { if (descriptor >= 0) close(descriptor); }
    }
    close(fd);
    for (pid_t child : children) {
        if (child <= 0) continue;
        if (!okay) kill(child, SIGKILL);
        bool reaped = false;
        int status = 0;
        for (int i = 0; i < 200 && !reaped; ++i) {
            pid_t waited = waitpid(child, &status, WNOHANG);
            if (waited == child) reaped = true;
            else if (waited < 0 && errno != EINTR) break;
            else usleep(10000);
        }
        okay &= reaped && WIFEXITED(status) && WEXITSTATUS(status) == 0;
    }
    EXPECT_EQ(0, umount(mountpoint)) << strerror(errno);
    EXPECT_EQ(0, rmdir(mountpoint)) << strerror(errno);
    ASSERT_TRUE(okay) << "protocol/client failure; writer stage=" << completions[0].stage
                      << " errno=" << completions[0].error << "; mutator stage="
                      << completions[1].stage << " errno=" << completions[1].error;
    EXPECT_FALSE(crossed) << "SETXATTR crossed an unfinished primary WRITE transaction";
    EXPECT_TRUE(complete[0] && complete[1]);
    EXPECT_TRUE(daemon.capability) << "capability mutation must succeed after the write commits";
}

TEST_F(FuseWritePrivileges, FirstWriteRpcFailurePreservesPrecopyPrivilegeEffects) {
    for (bool direct : {false, true}) {
        SCOPED_TRACE(direct);
        Case test{}; test.policy.direct = direct; test.input = Input::RpcFailure;
        test.failure_ordinal = 1;
        Run(test);
    }
}
TEST_F(FuseWritePrivileges, LaterWriteRpcFailureReturnsTheCommittedPrefix) {
    for (bool direct : {false, true}) {
        SCOPED_TRACE(direct);
        Case test{}; test.policy.direct = direct; test.input = Input::RpcFailure;
        Run(test);
    }
}
TEST_F(FuseWritePrivileges, LaterMalformedWriteReplyReturnsTheCommittedPrefix) {
    for (bool direct : {false, true}) {
        SCOPED_TRACE(direct);
        Case test{}; test.policy.direct = direct; test.input = Input::RpcFailure;
        test.malformed_failure = true;
        Run(test);
    }
}

TEST_F(FuseWritePrivileges, SetattrFailureWinsBeforeUserFaultWithoutModify) {
    Case test{}; test.setattr_error = EPERM; Run(test);
}
TEST_F(FuseWritePrivileges, CapabilityRemovalFailureWinsBeforeUserFault) {
    for (int error : {EPERM, ENODATA}) {
        SCOPED_TRACE(error);
        Case test{}; test.capability = true; test.remove_error = error; Run(test);
    }
}
TEST_F(FuseWritePrivileges, UnsupportedCapabilityRemovalDoesNotFailWrite) {
    Case test{}; test.capability = true; test.remove_error = EOPNOTSUPP; Run(test);
}
TEST_F(FuseWritePrivileges, FailedCapabilityProbeDoesNotBecomeWriteFailure) {
    for (int error : {EIO, EOPNOTSUPP, ENODATA}) {
        SCOPED_TRACE(error);
        Case test{}; test.capability = true; test.getxattr_error = error; Run(test);
    }
}
TEST_F(FuseWritePrivileges, NonExecutableSgidDoesNotRequireModeMutation) {
    Case test{}; test.initial_mode = 02766; Run(test);
}
TEST_F(FuseWritePrivileges, CapabilityOnlySetattrPreservesDirtyWritebackEof) {
    Case test{}; test.policy.writeback = true; test.input = Input::DirtyCapability; Run(test);
}
TEST_F(FuseWritePrivileges, ModeSetattrDrainsExistingDirtyDataBeforeDaemonMutation) {
    Case test{}; test.policy.writeback = true; test.input = Input::DirtyMode; Run(test);
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
