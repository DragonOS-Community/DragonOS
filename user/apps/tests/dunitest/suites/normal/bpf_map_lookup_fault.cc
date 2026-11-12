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
// Linux UAPI values missing from the older musl cross-toolchain headers.
constexpr int kBpfMapLookupAndDeleteElem = 21;

long Bpf(int cmd, union bpf_attr* attr) {
    return syscall(SYS_bpf, cmd, attr, sizeof(*attr));
}

void* ChurnAddressSpace(void* context) {
    auto& stop = *static_cast<std::atomic<bool>*>(context);
    while (!stop.load()) {
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

class BpfMapLookup : public testing::Test {
protected:
    void TearDown() override {
        if (churn_started_) {
            stop_churn_.store(true);
            pthread_join(churn_, nullptr);
        }
        if (map_ >= 0) {
            close(map_);
        }
    }

    int map_ = -1;
    std::atomic<bool> stop_churn_{false};
    pthread_t churn_{};
    bool churn_started_ = false;
};

TEST_F(BpfMapLookup, ValueCopyFaultsWhileAddressSpaceIsContended) {
    union bpf_attr create{};
    create.map_type = BPF_MAP_TYPE_HASH;
    create.key_size = sizeof(uint32_t);
    create.value_size = sizeof(uint64_t);
    create.max_entries = 16;
    map_ = static_cast<int>(Bpf(BPF_MAP_CREATE, &create));
    ASSERT_GE(map_, 0) << "BPF_MAP_CREATE failed: errno=" << errno << " (" << strerror(errno) << ")";

    uint32_t key = 1;
    uint64_t value = 0x1122334455667788ULL;
    union bpf_attr update{};
    update.map_fd = static_cast<uint32_t>(map_);
    update.key = reinterpret_cast<uint64_t>(&key);
    update.value = reinterpret_cast<uint64_t>(&value);
    ASSERT_EQ(0, Bpf(BPF_MAP_UPDATE_ELEM, &update))
        << "BPF_MAP_UPDATE_ELEM failed: errno=" << errno << " (" << strerror(errno) << ")";

    ASSERT_EQ(0, pthread_create(&churn_, nullptr, ChurnAddressSpace, &stop_churn_));
    churn_started_ = true;

    for (int round = 0; round < kRounds; round++) {
        auto* result = static_cast<uint64_t*>(mmap(nullptr, kPageSize, PROT_READ | PROT_WRITE,
                                                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
        ASSERT_NE(MAP_FAILED, result);
        union bpf_attr lookup{};
        lookup.map_fd = static_cast<uint32_t>(map_);
        lookup.key = reinterpret_cast<uint64_t>(&key);
        lookup.value = reinterpret_cast<uint64_t>(result);
        const long rc = Bpf(BPF_MAP_LOOKUP_ELEM, &lookup);
        const int lookup_errno = errno;
        if (rc == 0) {
            EXPECT_EQ(value, *result);
        }
        munmap(result, kPageSize);
        ASSERT_EQ(0, rc) << "round " << round << ": errno=" << lookup_errno;
    }

    ASSERT_EQ(0, Bpf(BPF_MAP_DELETE_ELEM, &update));
    EXPECT_EQ(-1, Bpf(BPF_MAP_LOOKUP_ELEM, &update));
    EXPECT_EQ(ENOENT, errno);
}

TEST_F(BpfMapLookup, ArrayLookupCopiesOnlyLogicalValue) {
    union bpf_attr create{};
    create.map_type = BPF_MAP_TYPE_ARRAY;
    create.key_size = sizeof(uint32_t);
    create.value_size = sizeof(uint32_t);
    create.max_entries = 1;
    map_ = static_cast<int>(Bpf(BPF_MAP_CREATE, &create));
    ASSERT_GE(map_, 0);

    uint32_t key = 1;
    union bpf_attr elem{};
    elem.map_fd = static_cast<uint32_t>(map_);
    elem.next_key = reinterpret_cast<uint64_t>(&key);
    ASSERT_EQ(0, Bpf(BPF_MAP_GET_NEXT_KEY, &elem));
    ASSERT_EQ(0u, key);

    const uint32_t value = 0x12345678;
    elem.key = reinterpret_cast<uint64_t>(&key);
    elem.value = reinterpret_cast<uint64_t>(&value);
    ASSERT_EQ(0, Bpf(BPF_MAP_UPDATE_ELEM, &elem));

    uint32_t output[] = {0, 0xaabbccdd};
    elem.value = reinterpret_cast<uint64_t>(output);
    ASSERT_EQ(0, Bpf(BPF_MAP_LOOKUP_ELEM, &elem));
    EXPECT_EQ(value, output[0]);
    EXPECT_EQ(0xaabbccddu, output[1]);
}

TEST_F(BpfMapLookup, HashLookupAndDeleteReturnsRemovedValue) {
    union bpf_attr create{};
    create.map_type = BPF_MAP_TYPE_HASH;
    create.key_size = sizeof(uint32_t);
    create.value_size = sizeof(uint32_t);
    create.max_entries = 1;
    map_ = static_cast<int>(Bpf(BPF_MAP_CREATE, &create));
    ASSERT_GE(map_, 0);

    const uint32_t key = 1;
    const uint32_t value = 17;
    union bpf_attr elem{};
    elem.map_fd = static_cast<uint32_t>(map_);
    elem.key = reinterpret_cast<uint64_t>(&key);
    elem.value = reinterpret_cast<uint64_t>(&value);
    ASSERT_EQ(0, Bpf(BPF_MAP_UPDATE_ELEM, &elem));

    uint32_t output = 0;
    elem.value = reinterpret_cast<uint64_t>(&output);
    ASSERT_EQ(0, Bpf(kBpfMapLookupAndDeleteElem, &elem));
    EXPECT_EQ(value, output);
    EXPECT_EQ(-1, Bpf(kBpfMapLookupAndDeleteElem, &elem));
    EXPECT_EQ(ENOENT, errno);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
