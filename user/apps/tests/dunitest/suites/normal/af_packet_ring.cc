// AF_PACKET mmap ring lifetime and allocation regression tests.

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cstdint>
#include <cstring>
#include <string>
#include <utility>

#ifndef AF_PACKET
#define AF_PACKET 17
#endif
#ifndef SOL_PACKET
#define SOL_PACKET 263
#endif
#ifndef PACKET_VERSION
#define PACKET_VERSION 10
#endif
#ifndef PACKET_RESERVE
#define PACKET_RESERVE 12
#endif
#ifndef PACKET_RX_RING
#define PACKET_RX_RING 5
#endif
#ifndef TPACKET_V1
#define TPACKET_V1 0
#endif

namespace {

constexpr int kEthPAll = 0x0003;

struct TpacketReq {
    uint32_t block_size;
    uint32_t block_nr;
    uint32_t frame_size;
    uint32_t frame_nr;
};

class FdGuard {
  public:
    explicit FdGuard(int fd = -1) : fd_(fd) {}
    FdGuard(const FdGuard&) = delete;
    FdGuard& operator=(const FdGuard&) = delete;
    ~FdGuard() {
        if (fd_ >= 0) close(fd_);
    }
    int Get() const { return fd_; }

  private:
    int fd_;
};

std::string Error() {
    return std::to_string(errno) + " (" + std::strerror(errno) + ")";
}

int MakeRingSocket() {
    return socket(AF_PACKET, SOCK_RAW, htons(kEthPAll));
}

TpacketReq Request(size_t page_size, uint32_t pages) {
    return TpacketReq{static_cast<uint32_t>(page_size), pages,
                      static_cast<uint32_t>(page_size), pages};
}

int Configure(int fd, const TpacketReq& request) {
    return setsockopt(fd, SOL_PACKET, PACKET_RX_RING, &request, sizeof(request));
}

int Teardown(int fd) {
    const TpacketReq empty{};
    return Configure(fd, empty);
}

void* MapRing(int fd, size_t length, int prot = PROT_READ | PROT_WRITE,
              int flags = MAP_SHARED) {
    return mmap(nullptr, length, prot, flags, fd, 0);
}

}  // namespace

TEST(AfPacketRing, PartialMiddleMunmapKeepsBackingBusy) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    FdGuard fd(MakeRingSocket());
    ASSERT_GE(fd.Get(), 0) << Error();
    ASSERT_EQ(Configure(fd.Get(), Request(page, 3)), 0) << Error();
    auto* mapping = static_cast<uint8_t*>(MapRing(fd.Get(), page * 3));
    ASSERT_NE(mapping, MAP_FAILED) << Error();

    ASSERT_EQ(munmap(mapping + page, page), 0) << Error();
    errno = 0;
    EXPECT_EQ(Teardown(fd.Get()), -1);
    EXPECT_EQ(errno, EBUSY) << Error();

    ASSERT_EQ(munmap(mapping, page), 0) << Error();
    errno = 0;
    EXPECT_EQ(Teardown(fd.Get()), -1);
    EXPECT_EQ(errno, EBUSY) << Error();
    ASSERT_EQ(munmap(mapping + page * 2, page), 0) << Error();
    EXPECT_EQ(Teardown(fd.Get()), 0) << Error();
}

TEST(AfPacketRing, MetadataSplitsKeepAccountingBalanced) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    FdGuard fd(MakeRingSocket());
    ASSERT_GE(fd.Get(), 0) << Error();
    ASSERT_EQ(Configure(fd.Get(), Request(page, 5)), 0) << Error();
    auto* mapping = static_cast<uint8_t*>(MapRing(fd.Get(), page * 5));
    ASSERT_NE(mapping, MAP_FAILED) << Error();

    ASSERT_EQ(mprotect(mapping + page, page, PROT_READ), 0) << Error();
#ifdef MADV_DONTFORK
    // mprotect leaves a three-page tail; advising its middle page forces an
    // independent before/middle/after split instead of touching a whole VMA.
    ASSERT_EQ(madvise(mapping + page * 3, page, MADV_DONTFORK), 0) << Error();
#endif
    errno = 0;
    EXPECT_EQ(Teardown(fd.Get()), -1);
    EXPECT_EQ(errno, EBUSY) << Error();
    ASSERT_EQ(munmap(mapping, page * 5), 0) << Error();
    EXPECT_EQ(Teardown(fd.Get()), 0) << Error();
}

TEST(AfPacketRing, ForkedMappingKeepsBackingBusy) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    FdGuard fd(MakeRingSocket());
    ASSERT_GE(fd.Get(), 0) << Error();
    ASSERT_EQ(Configure(fd.Get(), Request(page, 1)), 0) << Error();
    void* mapping = MapRing(fd.Get(), page);
    ASSERT_NE(mapping, MAP_FAILED) << Error();

    int release_pipe[2];
    int done_pipe[2];
    ASSERT_EQ(pipe(release_pipe), 0);
    ASSERT_EQ(pipe(done_pipe), 0);
    const pid_t child = fork();
    ASSERT_GE(child, 0) << Error();
    if (child == 0) {
        close(release_pipe[1]);
        close(done_pipe[0]);
        char byte;
        if (read(release_pipe[0], &byte, 1) != 1 || munmap(mapping, page) != 0) _exit(2);
        if (write(done_pipe[1], "x", 1) != 1) _exit(3);
        _exit(0);
    }
    close(release_pipe[0]);
    close(done_pipe[1]);

    ASSERT_EQ(munmap(mapping, page), 0) << Error();
    errno = 0;
    EXPECT_EQ(Teardown(fd.Get()), -1);
    EXPECT_EQ(errno, EBUSY) << Error();
    ASSERT_EQ(write(release_pipe[1], "x", 1), 1);
    char byte;
    ASSERT_EQ(read(done_pipe[0], &byte, 1), 1);
    EXPECT_EQ(Teardown(fd.Get()), 0) << Error();
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    EXPECT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
    close(release_pipe[1]);
    close(done_pipe[0]);
}

TEST(AfPacketRing, MremapMoveTransfersOneVmaReference) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    FdGuard fd(MakeRingSocket());
    ASSERT_GE(fd.Get(), 0) << Error();
    ASSERT_EQ(Configure(fd.Get(), Request(page, 1)), 0) << Error();
    void* source = MapRing(fd.Get(), page);
    ASSERT_NE(source, MAP_FAILED) << Error();
    void* target = mmap(nullptr, page, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(target, MAP_FAILED) << Error();
    ASSERT_EQ(munmap(target, page), 0);

    void* moved = mremap(source, page, page, MREMAP_MAYMOVE | MREMAP_FIXED, target);
    ASSERT_EQ(moved, target) << Error();
    errno = 0;
    EXPECT_EQ(Teardown(fd.Get()), -1);
    EXPECT_EQ(errno, EBUSY) << Error();
    ASSERT_EQ(munmap(moved, page), 0) << Error();
    EXPECT_EQ(Teardown(fd.Get()), 0) << Error();
}

#ifdef MREMAP_DONTUNMAP
TEST(AfPacketRing, MremapDontunmapKeepsBothVmaReferences) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    FdGuard fd(MakeRingSocket());
    ASSERT_GE(fd.Get(), 0) << Error();
    ASSERT_EQ(Configure(fd.Get(), Request(page, 1)), 0) << Error();
    void* source = MapRing(fd.Get(), page);
    ASSERT_NE(source, MAP_FAILED) << Error();
    void* target = mmap(nullptr, page, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(target, MAP_FAILED) << Error();
    ASSERT_EQ(munmap(target, page), 0);

    void* duplicate = mremap(source, page, page,
                             MREMAP_MAYMOVE | MREMAP_FIXED | MREMAP_DONTUNMAP, target);
    ASSERT_EQ(duplicate, target) << Error();
    ASSERT_EQ(munmap(source, page), 0) << Error();
    errno = 0;
    EXPECT_EQ(Teardown(fd.Get()), -1);
    EXPECT_EQ(errno, EBUSY) << Error();
    ASSERT_EQ(munmap(duplicate, page), 0) << Error();
    EXPECT_EQ(Teardown(fd.Get()), 0) << Error();
}
#endif

TEST(AfPacketRing, ReadOnlyAndPrivateMappingsCloseExactlyOnce) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    for (auto [prot, flags] : {
             std::pair{PROT_READ, MAP_SHARED},
             std::pair{PROT_READ | PROT_WRITE, MAP_PRIVATE},
         }) {
        FdGuard fd(MakeRingSocket());
        ASSERT_GE(fd.Get(), 0) << Error();
        ASSERT_EQ(Configure(fd.Get(), Request(page, 1)), 0) << Error();
        void* mapping = MapRing(fd.Get(), page, prot, flags);
        ASSERT_NE(mapping, MAP_FAILED) << "flags=" << flags << ": " << Error();
        ASSERT_EQ(munmap(mapping, page), 0) << Error();
        EXPECT_EQ(Teardown(fd.Get()), 0) << "flags=" << flags << ": " << Error();
    }
}

TEST(AfPacketRing, ReserveCannotChangeAfterRingCreation) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    FdGuard fd(MakeRingSocket());
    ASSERT_GE(fd.Get(), 0) << Error();
    ASSERT_EQ(Configure(fd.Get(), Request(page, 1)), 0) << Error();
    uint32_t reserve = 16;
    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_RESERVE, &reserve, sizeof(reserve)), -1);
    EXPECT_EQ(errno, EBUSY) << Error();
    EXPECT_EQ(Teardown(fd.Get()), 0) << Error();
}

TEST(AfPacketRing, NonPowerOfTwoBlockExercisesBuddySurplusRelease) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    FdGuard fd(MakeRingSocket());
    ASSERT_GE(fd.Get(), 0) << Error();
    const TpacketReq request{static_cast<uint32_t>(page * 3), 1,
                             static_cast<uint32_t>(page), 3};
    ASSERT_EQ(Configure(fd.Get(), request), 0) << Error();
    void* mapping = MapRing(fd.Get(), page * 3);
    ASSERT_NE(mapping, MAP_FAILED) << Error();
    ASSERT_EQ(munmap(mapping, page * 3), 0) << Error();
    EXPECT_EQ(Teardown(fd.Get()), 0) << Error();
}

TEST(AfPacketRing, HugeBlockVectorReturnsEnomemWithoutPanic) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    FdGuard fd(MakeRingSocket());
    ASSERT_GE(fd.Get(), 0) << Error();
    const TpacketReq huge{static_cast<uint32_t>(page), UINT32_MAX,
                          static_cast<uint32_t>(page), UINT32_MAX};
    errno = 0;
    EXPECT_EQ(Configure(fd.Get(), huge), -1);
    EXPECT_EQ(errno, ENOMEM) << Error();
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
