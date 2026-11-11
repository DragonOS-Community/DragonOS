#include <gtest/gtest.h>

#include <atomic>
#include <cerrno>
#include <cstdint>
#include <cstring>
#include <linux/bpf.h>
#include <pthread.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

namespace {

constexpr size_t kPageSize = 4096;
constexpr size_t kChurnSize = 64 << 20;
constexpr int kRounds = 20000;

std::atomic<bool> g_stop_churn{false};

long Bpf(int cmd, union bpf_attr* attr) {
    return syscall(SYS_bpf, cmd, attr, sizeof(*attr));
}

void* ChurnAddressSpace(void*) {
    while (!g_stop_churn.load()) {
        auto* area = static_cast<char*>(mmap(nullptr, kChurnSize, PROT_READ | PROT_WRITE,
                                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
        if (area == MAP_FAILED) {
            continue;
        }
        for (size_t offset = 0; offset < kChurnSize; offset += kPageSize) {
            area[offset] = 1;
        }
        munmap(area, kChurnSize);
    }
    return nullptr;
}

TEST(BpfMapLookup, ValueCopyFaultsWhileAddressSpaceIsContended) {
    union bpf_attr create{};
    create.map_type = BPF_MAP_TYPE_HASH;
    create.key_size = sizeof(uint32_t);
    create.value_size = sizeof(uint64_t);
    create.max_entries = 16;
    int map = static_cast<int>(Bpf(BPF_MAP_CREATE, &create));
    ASSERT_GE(map, 0) << "BPF_MAP_CREATE failed: errno=" << errno << " (" << strerror(errno) << ")";

    uint32_t key = 1;
    uint64_t value = 0x1122334455667788ULL;
    union bpf_attr update{};
    update.map_fd = static_cast<uint32_t>(map);
    update.key = reinterpret_cast<uint64_t>(&key);
    update.value = reinterpret_cast<uint64_t>(&value);
    ASSERT_EQ(0, Bpf(BPF_MAP_UPDATE_ELEM, &update))
        << "BPF_MAP_UPDATE_ELEM failed: errno=" << errno << " (" << strerror(errno) << ")";

    pthread_t churn;
    ASSERT_EQ(0, pthread_create(&churn, nullptr, ChurnAddressSpace, nullptr));

    for (int round = 0; round < kRounds; round++) {
        auto* result = static_cast<uint64_t*>(mmap(nullptr, kPageSize, PROT_READ | PROT_WRITE,
                                                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
        ASSERT_NE(MAP_FAILED, result);
        union bpf_attr lookup{};
        lookup.map_fd = static_cast<uint32_t>(map);
        lookup.key = reinterpret_cast<uint64_t>(&key);
        lookup.value = reinterpret_cast<uint64_t>(result);
        ASSERT_EQ(0, Bpf(BPF_MAP_LOOKUP_ELEM, &lookup))
            << "round " << round << ": errno=" << errno << " (" << strerror(errno) << ")";
        EXPECT_EQ(value, *result);
        munmap(result, kPageSize);
    }

    g_stop_churn.store(true);
    pthread_join(churn, nullptr);
    close(map);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
