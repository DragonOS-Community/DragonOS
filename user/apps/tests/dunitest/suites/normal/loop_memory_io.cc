#include "loop_ext4_test_support.h"

#include <poll.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/vfs.h>
#include <sys/wait.h>
#include <time.h>

#include <cstdint>
#include <cstdlib>
#include <iostream>
#include <sstream>
#include <vector>

namespace {
constexpr uint64_t kMiB = 1024 * 1024;
constexpr size_t kBlock = 4096;
constexpr size_t kTargetBytes = 2 * kMiB;
constexpr size_t kDonorBytes = 8 * kMiB;
constexpr uint64_t kSecond = 1000000000;

uint64_t Now() {
    timespec time{};
    if (clock_gettime(CLOCK_MONOTONIC, &time)) return 0;
    return uint64_t(time.tv_sec) * kSecond + time.tv_nsec;
}

// EINTR and partial transfers consume the original absolute deadline.
bool Transfer(int fd, void* data, size_t length, bool send, uint64_t end) {
    auto* bytes = static_cast<char*>(data);
    while (length) {
        uint64_t now = Now();
        if (!now || now >= end) return false;
        pollfd event{fd, short(send ? POLLOUT : POLLIN), 0};
        int ready = poll(&event, 1, int((end - now + 999999) / 1000000));
        if (ready < 0 && errno == EINTR) continue;
        if (ready <= 0 || !(event.revents & (send ? POLLOUT : POLLIN))) return false;
        ssize_t count = send ? write(fd, bytes, length) : read(fd, bytes, length);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) return false;
        bytes += count;
        length -= count;
    }
    return true;
}

int Set(const std::string& path, const std::string& value) {
    int fd = open(path.c_str(), O_WRONLY | O_CLOEXEC);
    if (fd < 0) return errno;
    ssize_t count;
    do { count = write(fd, value.data(), value.size()); } while (count < 0 && errno == EINTR);
    int error = count < 0 ? errno : (count == ssize_t(value.size()) ? 0 : EIO);
    close(fd);
    return error;
}

std::string Get(const std::string& path) {
    int fd = open(path.c_str(), O_RDONLY | O_CLOEXEC);
    if (fd < 0) return {};
    std::string text;
    char buffer[4096];
    for (;;) {
        ssize_t count = read(fd, buffer, sizeof(buffer));
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) break;
        text.append(buffer, count);
        if (text.size() > 65536) break;
    }
    close(fd);
    return text;
}

uint64_t Number(const std::string& text) {
    std::istringstream stream(text);
    uint64_t number = UINT64_MAX;
    stream >> number >> std::ws;
    return stream.eof() ? number : UINT64_MAX;
}

uint64_t Field(const std::string& text, const char* name) {
    std::istringstream stream(text);
    std::string key, value;
    while (stream >> key >> value) if (key == name) return Number(value);
    return UINT64_MAX;
}

bool Has(const std::string& text, const char* name) {
    std::istringstream stream(text);
    std::string value;
    while (stream >> value) if (value == name) return true;
    return false;
}

// Fill every block: sparse holes would bypass the cold backing allocation.
int Fill(const std::string& path, size_t size) {
    int fd = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR | O_CLOEXEC, 0600);
    if (fd < 0) return errno;
    unsigned char buffer[kBlock];
    memset(buffer, 0x5a, sizeof(buffer));
    int error = 0;
    for (size_t offset = 0; offset < size;) {
        ssize_t count = pwrite(fd, buffer, sizeof(buffer), offset);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) { error = count < 0 ? errno : EIO; break; }
        offset += size_t(count);
    }
    if (!error && fsync(fd)) error = errno;
    // Ensure later child reads, rather than parent writes, own donor pages.
    if (!error) error = posix_fadvise(fd, 0, 0, POSIX_FADV_DONTNEED);
    close(fd);
    return error;
}

struct Reply {
    int error = 0;
    uint64_t bytes = 0;
    uint64_t checksum = 0;
};

struct SharedReply {
    uint32_t sequence;
    Reply reply;
};

class Reader {
  public:
    ~Reader() { Stop(); }

    bool Start(int donor, int target, int writer) {
        int request[2];
        if (pipe(request)) return false;
        // The reply channel must not require a fresh child allocation after
        // a zero-headroom failure. Prefault shared control storage in the
        // unlimited runner; otherwise reporting ENOMEM can itself enter OOM.
        void* shared = mmap(nullptr, kBlock, PROT_READ | PROT_WRITE,
                            MAP_SHARED | MAP_ANONYMOUS, -1, 0);
        if (shared == MAP_FAILED) { close(request[0]); close(request[1]); return false; }
        memset(shared, 0, kBlock);
        shared_ = static_cast<SharedReply*>(shared);
        pid_ = fork();
        if (!pid_) {
            close(request[1]);
            void* allocation = nullptr;
            int error = posix_memalign(&allocation, kBlock, kBlock);
            if (error) _exit(90);
            auto* buffer = static_cast<unsigned char*>(allocation);
            // Fault the user destination before measuring memory.current.
            memset(buffer, 0, kBlock);
            Reply reply;
            uint32_t sequence = 1;
            shared_->reply = reply;
            __atomic_store_n(&shared_->sequence, sequence, __ATOMIC_RELEASE);
            char operation;
            while (Transfer(request[0], &operation, 1, false, Now() + 30 * kSecond)) {
                if (operation == 'Q') break;
                reply = {};
                const bool writing = operation == 'W';
                int fd = writing ? writer : (operation == 'D' ? donor : target);
                size_t size = operation == 'D' ? kDonorBytes : kTargetBytes;
                if (writing) memset(buffer, 0x5a, kBlock);
                while (reply.bytes < size) {
                    ssize_t count = writing ? pwrite(fd, buffer, kBlock, reply.bytes)
                                            : pread(fd, buffer, kBlock, reply.bytes);
                    if (count < 0 && errno == EINTR) continue;
                    if (count <= 0) { reply.error = count < 0 ? errno : EIO; break; }
                    for (ssize_t i = 0; i < count; ++i) reply.checksum += buffer[i];
                    reply.bytes += size_t(count);
                }
                if (writing && !reply.error && fsync(writer)) reply.error = errno;
                shared_->reply = reply;
                __atomic_store_n(&shared_->sequence, ++sequence, __ATOMIC_RELEASE);
                // Do not start another poll admission after a zero-headroom
                // failure. The operation result is already published; exit
                // normally so the parent can distinguish it from OOM/SIGKILL.
                if (reply.error) break;
            }
            free(allocation);
            _exit(0);
        }
        close(request[0]);
        input_ = request[1];
        if (pid_ < 0) return false;
        Reply ready;
        return WaitReply(1, &ready, Now() + 10 * kSecond);
    }

    pid_t pid() const { return pid_; }

    bool Run(char operation, Reply* reply) {
        uint64_t end = Now() + 20 * kSecond;
        return Transfer(input_, &operation, 1, true, end)
            && WaitReply(++sequence_, reply, end);
    }

    bool ExitedNormally() {
        uint64_t end = Now() + 3 * kSecond;
        while (Now() && Now() < end) {
            int status = 0;
            pid_t result = waitpid(pid_, &status, WNOHANG);
            if (result == pid_) {
                pid_ = -1;
                return WIFEXITED(status) && WEXITSTATUS(status) == 0;
            }
            if (result < 0 && errno != EINTR) return false;
            usleep(1000);
        }
        return false;
    }

    void Stop() {
        if (pid_ > 0) {
            // Root cleanup must remain possible after a quota failure or hang.
            kill(pid_, SIGKILL);
            uint64_t end = Now() + 3 * kSecond;
            bool reaped = false;
            while (Now() && Now() < end) {
                int status;
                pid_t result = waitpid(pid_, &status, WNOHANG);
                if (result == pid_ || (result < 0 && errno == ECHILD)) { reaped = true; break; }
                if (result < 0 && errno != EINTR) break;
                usleep(1000);
            }
            EXPECT_TRUE(reaped) << "loop child did not retire after SIGKILL";
            pid_ = -1;
        }
        if (input_ >= 0) close(input_);
        input_ = -1;
        if (shared_) munmap(shared_, kBlock);
        shared_ = nullptr;
    }

  private:
    bool WaitReply(uint32_t sequence, Reply* reply, uint64_t end) {
        while (__atomic_load_n(&shared_->sequence, __ATOMIC_ACQUIRE) < sequence) {
            uint64_t now = Now();
            if (!now || now >= end) return false;
            usleep(1000);
        }
        *reply = shared_->reply;
        return true;
    }
    pid_t pid_ = -1;
    int input_ = -1;
    SharedReply* shared_ = nullptr;
    uint32_t sequence_ = 1;
};

class LoopMemoryIo : public ::testing::Test {
  protected:
    void SetUp() override {
        Configure(false);
    }

    void Configure(bool raw) {
        ASSERT_EQ(0u, geteuid());
        const char* parent = getenv("DUNITEST_MEMORY_PARENT");
        parent_ = parent ? parent : "/sys/fs/cgroup";
        ASSERT_TRUE(Has(Get(parent_ + "/cgroup.controllers"), "memory"));
        if (!Has(Get(parent_ + "/cgroup.subtree_control"), "memory")) {
            ASSERT_EQ(0, Set(parent_ + "/cgroup.subtree_control", "+memory"));
            enabled_ = true;
        }
        static unsigned sequence = 0;
        group_ = parent_ + "/dunitest-loop-memory-" + std::to_string(getpid())
            + "-" + std::to_string(sequence++);
        ASSERT_EQ(0, mkdir(group_.c_str(), 0755));
        if (!access((group_ + "/memory.swap.max").c_str(), F_OK)) {
            ASSERT_EQ(0, Set(group_ + "/memory.swap.max", "0"));
        }

        const char* supplied = getenv("DUNITEST_LOOP_BACKING_DIR");
        const char* directory = supplied ? supplied : "/var/tmp";
        struct statfs fs{};
        ASSERT_EQ(0, statfs(directory, &fs)) << directory << ": " << strerror(errno);
        ASSERT_NE(0x01021994, fs.f_type) << "backing must not be tmpfs";
        ASSERT_NE(0x858458f6, fs.f_type) << "backing must not be ramfs";
        std::string pattern = std::string(directory) + "/dunitest-loop-memory-XXXXXX";
        std::vector<char> name(pattern.begin(), pattern.end());
        name.push_back('\0');
        char* created = mkdtemp(name.data());
        ASSERT_NE(nullptr, created) << strerror(errno);
        directory_ = created;
        ASSERT_NO_FATAL_FAILURE(loop_.SetUp("ext4_inode_identity.img", directory_.c_str()));
        if (!raw) {
            ASSERT_NO_FATAL_FAILURE(loop_.Mount());
            target_path_ = loop_.mount_point() + "/cold-target";
            ASSERT_EQ(0, Fill(target_path_, kTargetBytes)) << strerror(errno);
        }
        donor_path_ = directory_ + "/donor";
        ASSERT_EQ(0, Fill(donor_path_, kDonorBytes)) << strerror(errno);

        backing_ = open(loop_.backing_path().c_str(), O_RDWR | O_CLOEXEC);
        ASSERT_GE(backing_, 0);
        if (raw) {
            // This image is intentionally not mounted as ext4. Overwrite an
            // existing, bounded range without truncating its configured loop
            // capacity; the raw request tests only the lower backing cache.
            unsigned char buffer[kBlock];
            memset(buffer, 0x5a, sizeof(buffer));
            for (size_t offset = 0; offset < kTargetBytes;) {
                const size_t remaining = kTargetBytes - offset;
                const size_t length = remaining < sizeof(buffer) ? remaining : sizeof(buffer);
                ssize_t count = pwrite(backing_, buffer, length, offset);
                if (count < 0 && errno == EINTR) continue;
                ASSERT_GT(count, 0) << strerror(errno);
                offset += size_t(count);
            }
        }
        ASSERT_EQ(0, fsync(backing_)) << strerror(errno);
        ASSERT_EQ(0, posix_fadvise(backing_, 0, 0, POSIX_FADV_DONTNEED));
        donor_ = open(donor_path_.c_str(), O_RDONLY | O_CLOEXEC);
        target_ = raw ? open(loop_.loop_path().c_str(), O_RDONLY | O_CLOEXEC)
                      : open(target_path_.c_str(), O_RDONLY | O_DIRECT | O_CLOEXEC);
        if (!raw) writer_ = open(target_path_.c_str(), O_RDWR | O_CLOEXEC);
        ASSERT_GE(donor_, 0);
        ASSERT_GE(target_, 0);
        if (!raw) { ASSERT_GE(writer_, 0); }
        ASSERT_TRUE(reader_.Start(donor_, target_, writer_));
        ASSERT_EQ(0, Set(group_ + "/cgroup.procs", std::to_string(reader_.pid())));
    }

    void TearDown() override {
        // Only the isolated child moves cgroup; the runner stays unlimited.
        reader_.Stop();
        for (int fd : {target_, writer_, donor_, backing_}) if (fd >= 0) close(fd);
        if (!target_path_.empty()) { EXPECT_EQ(0, unlink(target_path_.c_str())); }
        if (!donor_path_.empty()) { EXPECT_EQ(0, unlink(donor_path_.c_str())); }
        if (!protected_group_.empty()) {
            EXPECT_EQ(0, rmdir(protected_group_.c_str())) << strerror(errno);
        }
        if (!group_.empty()) { EXPECT_EQ(0, rmdir(group_.c_str())) << strerror(errno); }
        if (enabled_) { EXPECT_EQ(0, Set(parent_ + "/cgroup.subtree_control", "-memory")); }
        // LoopExt4 removes the backing image at fixture destruction, then the
        // containing directory is removed by DirectoryCleanup below.
    }

    uint64_t Current() { return Number(Get(group_ + "/memory.current")); }
    uint64_t FileBytes() { return Field(Get(group_ + "/memory.stat"), "file"); }
    uint64_t MaxEvents() { return Field(Get(group_ + "/memory.events"), "max"); }

    void ExpectNoProgressWithoutOomKill(uint64_t max_events, uint64_t oom_kills) {
        ASSERT_NE(UINT64_MAX, max_events);
        ASSERT_NE(UINT64_MAX, oom_kills);
        Reply reply;
        ASSERT_TRUE(reader_.Run('R', &reply)) << "NOIO failure must finish within the IPC deadline";
        PrintSample("no-progress-after-read");
        // Charge -> legacy block ENOMEM -> ext4 ErrCode::ENOMEM preserves
        // ENOMEM. EIO or arbitrary errno would conceal a broken error path.
        EXPECT_EQ(ENOMEM, reply.error) << strerror(reply.error) << "; completed=" << reply.bytes;
        EXPECT_LT(reply.bytes, kTargetBytes);
        EXPECT_EQ(reply.bytes * 0x5au, reply.checksum);
        EXPECT_GT(MaxEvents(), max_events) << "the lower allocation must hit the quota";
        EXPECT_TRUE(reader_.ExitedNormally()) << "I/O errors must not kill the caller";
        EXPECT_EQ(oom_kills, Field(Get(group_ + "/memory.events"), "oom_kill"));
    }

    void PrintSample(const char* stage) {
        std::cout << "[loop-memory " << stage << "] current=" << Current()
                  << " file=" << FileBytes() << " max_events=" << MaxEvents() << '\n';
    }

    std::string parent_, group_, protected_group_, directory_, target_path_, donor_path_;
    struct DirectoryCleanup {
        std::string* path;
        ~DirectoryCleanup() {
            if (!path->empty()) { EXPECT_EQ(0, rmdir(path->c_str())); }
        }
    } directory_cleanup_{&directory_};
    // Declaration order makes loop destruction precede directory cleanup.
    dunitest::LoopExt4 loop_;
    Reader reader_;
    int target_ = -1, writer_ = -1, donor_ = -1, backing_ = -1;
    bool enabled_ = false;
};

class RawLoopMemoryIo : public LoopMemoryIo {
  protected:
    void SetUp() override {
        Configure(true);
    }
};

TEST_F(RawLoopMemoryIo, ColdBackingReadsUseSubmittingMemoryAccount) {
    const uint64_t before = FileBytes();
    ASSERT_NE(UINT64_MAX, before);
    PrintSample("raw-before-read");
    Reply reply;
    ASSERT_TRUE(reader_.Run('R', &reply));
    EXPECT_EQ(0, reply.error) << strerror(reply.error);
    EXPECT_EQ(kTargetBytes, reply.bytes);
    EXPECT_EQ(kTargetBytes * 0x5au, reply.checksum);
    PrintSample("raw-after-read");
    EXPECT_GE(FileBytes(), before + kTargetBytes);
}

TEST_F(RawLoopMemoryIo, ColdBackingChargeReclaimsCleanUnmappedDonor) {
    Reply donor;
    ASSERT_TRUE(reader_.Run('D', &donor));
    ASSERT_EQ(0, donor.error) << strerror(donor.error);
    ASSERT_EQ(kDonorBytes, donor.bytes);
    ASSERT_EQ(kDonorBytes * 0x5au, donor.checksum);
    ASSERT_GE(FileBytes(), kDonorBytes);
    const uint64_t current = Current();
    const uint64_t events = MaxEvents();
    ASSERT_NE(UINT64_MAX, current);
    ASSERT_NE(UINT64_MAX, events);
    ASSERT_EQ(0, Set(group_ + "/memory.max", std::to_string(current + 128 * 1024)));
    PrintSample("raw-limited-before-read");
    Reply reply;
    ASSERT_TRUE(reader_.Run('R', &reply));
    PrintSample("raw-limited-after-read");
    EXPECT_EQ(0, reply.error) << strerror(reply.error) << "; completed=" << reply.bytes;
    EXPECT_EQ(kTargetBytes, reply.bytes);
    EXPECT_EQ(kTargetBytes * 0x5au, reply.checksum);
    EXPECT_GT(MaxEvents(), events);
    EXPECT_LE(Current(), current + 128 * 1024);
}

TEST_F(LoopMemoryIo, ColdBackingReadsUseSubmittingMemoryAccount) {
    const uint64_t before = FileBytes();
    ASSERT_NE(UINT64_MAX, before);
    PrintSample("before-read");
    Reply reply;
    ASSERT_TRUE(reader_.Run('R', &reply));
    EXPECT_EQ(0, reply.error) << strerror(reply.error);
    EXPECT_EQ(kTargetBytes, reply.bytes);
    EXPECT_EQ(kTargetBytes * 0x5au, reply.checksum);
    PrintSample("after-read");
    // O_DIRECT bypasses the upper cache. The cold lower regular-file cache
    // must belong to the source child, not the loop kernel worker's root.
    EXPECT_GE(FileBytes(), before + kTargetBytes);
}

TEST_F(LoopMemoryIo, BufferedWritebackChargesColdBackingToProducer) {
    // SetUp fsyncs and advises DONTNEED on both the upper target and the
    // backing image. Neither cache's existing parent ownership can satisfy
    // this assertion; fd creation and the reply storage precede migration.
    const uint64_t before = FileBytes();
    ASSERT_EQ(0u, before) << "the source group must begin without file cache";
    PrintSample("writeback-before-write");
    Reply written;
    ASSERT_TRUE(reader_.Run('W', &written));
    ASSERT_EQ(0, written.error) << strerror(written.error);
    ASSERT_EQ(kTargetBytes, written.bytes);
    ASSERT_EQ(kTargetBytes * 0x5au, written.checksum);
    PrintSample("writeback-after-sync");
    // The producer owns its buffered upper pages and the cold lower image
    // pages allocated by asynchronous writeback, not merely one cache layer.
    EXPECT_GE(FileBytes(), before + 2 * kTargetBytes);
    Reply observed;
    ASSERT_TRUE(reader_.Run('R', &observed));
    EXPECT_EQ(0, observed.error) << strerror(observed.error);
    EXPECT_EQ(kTargetBytes, observed.bytes);
    EXPECT_EQ(kTargetBytes * 0x5au, observed.checksum);
}

TEST_F(LoopMemoryIo, ColdBackingChargeReclaimsCleanUnmappedDonor) {
    Reply donor;
    ASSERT_TRUE(reader_.Run('D', &donor));
    ASSERT_EQ(0, donor.error) << strerror(donor.error);
    ASSERT_EQ(kDonorBytes, donor.bytes);
    ASSERT_EQ(kDonorBytes * 0x5au, donor.checksum);
    ASSERT_GE(FileBytes(), kDonorBytes);
    const uint64_t current = Current();
    const uint64_t events = MaxEvents();
    ASSERT_NE(UINT64_MAX, current);
    ASSERT_NE(UINT64_MAX, events);
    // A fixed admission allowance, much smaller than the 2 MiB cold target.
    // Do not increase this after a failing baseline or recover in userspace.
    ASSERT_EQ(0, Set(group_ + "/memory.max", std::to_string(current + 128 * 1024)));
    PrintSample("limited-before-read");
    Reply reply;
    ASSERT_TRUE(reader_.Run('R', &reply));
    PrintSample("limited-after-read");
    EXPECT_EQ(0, reply.error) << strerror(reply.error) << "; completed=" << reply.bytes;
    EXPECT_EQ(kTargetBytes, reply.bytes);
    EXPECT_EQ(kTargetBytes * 0x5au, reply.checksum);
    EXPECT_GT(MaxEvents(), events) << "test must actually exercise a charge limit";
    EXPECT_LE(Current(), current + 128 * 1024);
}

TEST_F(LoopMemoryIo, ColdBackingNoReclaimProgressReturnsEnomemWithoutOomKill) {
    const uint64_t current = Current();
    const uint64_t max_events = MaxEvents();
    const uint64_t oom_kills = Field(Get(group_ + "/memory.events"), "oom_kill");
    ASSERT_NE(UINT64_MAX, current);
    // There are no donor pages. NOIO cannot manufacture space, wait for an
    // OOM victim, or bypass the exact current budget by charging root.
    ASSERT_EQ(0, Set(group_ + "/memory.max", std::to_string(current)));
    PrintSample("no-progress-before-read");
    ASSERT_NO_FATAL_FAILURE(ExpectNoProgressWithoutOomKill(max_events, oom_kills));
}

TEST_F(LoopMemoryIo, ParentQuotaNoioReclaimHonorsChildMemoryMin) {
    // A reclaim target's own min does not protect it against its own max.
    // Put the producer/donor in a child of the quota/reclaim target instead.
    protected_group_ = group_ + "/protected";
    ASSERT_EQ(0, mkdir(protected_group_.c_str(), 0755));
    ASSERT_EQ(0, Set(protected_group_ + "/cgroup.procs", std::to_string(reader_.pid())));
    // Remove the parent's direct task before enabling its domain controller.
    ASSERT_EQ(0, Set(group_ + "/cgroup.subtree_control", "+memory"));
    Reply donor;
    ASSERT_TRUE(reader_.Run('D', &donor));
    ASSERT_EQ(0, donor.error) << strerror(donor.error);
    ASSERT_EQ(kDonorBytes, donor.bytes);
    ASSERT_EQ(kDonorBytes * 0x5au, donor.checksum);
    ASSERT_GE(Field(Get(protected_group_ + "/memory.stat"), "file"), kDonorBytes);
    const uint64_t child_current = Number(Get(protected_group_ + "/memory.current"));
    const uint64_t parent_current = Current();
    const uint64_t max_events = MaxEvents();
    const uint64_t oom_kills = Field(Get(group_ + "/memory.events"), "oom_kill");
    ASSERT_NE(UINT64_MAX, child_current);
    ASSERT_NE(UINT64_MAX, parent_current);
    // This is the reclaim target's immediate child; its effective min is
    // declared min. No unprotected sibling can dilute the protected budget.
    ASSERT_EQ(0, Set(protected_group_ + "/memory.min",
                     std::to_string(child_current + 128 * 1024)));
    ASSERT_EQ(0, Set(group_ + "/memory.max", std::to_string(parent_current + 128 * 1024)));
    PrintSample("protected-before-read");
    ASSERT_NO_FATAL_FAILURE(ExpectNoProgressWithoutOomKill(max_events, oom_kills));
    EXPECT_GE(Field(Get(protected_group_ + "/memory.stat"), "file"), kDonorBytes)
        << "NOIO must not consume the protected donor stock";
}
}  // namespace

int main(int argc, char** argv) {
    // Closed worker pipes are failure evidence, not SIGPIPE for the runner.
    signal(SIGPIPE, SIG_IGN);
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
