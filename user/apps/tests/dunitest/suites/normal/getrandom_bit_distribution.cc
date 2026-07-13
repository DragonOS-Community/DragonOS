#include <gtest/gtest.h>

#include <array>
#include <cerrno>
#include <cstdint>
#include <cstring>
#include <sys/syscall.h>
#include <unistd.h>

namespace {

long RawGetrandom(void* buffer, size_t length, unsigned int flags) {
    return syscall(SYS_getrandom, buffer, length, flags);
}

bool HasLegacyOverlappingBits(const std::array<uint8_t, 4>& sample) {
    return (sample[0] >> 2) == (sample[1] & 0x3f) &&
           (sample[1] >> 2) == (sample[2] & 0x3f) &&
           (sample[2] >> 2) == (sample[3] & 0x3f);
}

TEST(GetrandomBitDistribution, DoesNotReuseOverlappingBitWindows) {
    constexpr size_t kSampleCount = 32;
    size_t overlapping_samples = 0;

    for (size_t i = 0; i < kSampleCount; ++i) {
        std::array<uint8_t, 4> sample = {};
        errno = 0;
        ASSERT_EQ(static_cast<long>(sample.size()),
                  RawGetrandom(sample.data(), sample.size(), 0))
            << "sample " << i << " failed with errno=" << errno << " ("
            << strerror(errno) << ")";

        if (HasLegacyOverlappingBits(sample)) {
            ++overlapping_samples;
        }
    }

    EXPECT_LT(overlapping_samples, kSampleCount)
        << "all samples match the legacy offset*2 bit-overlap pattern";
}

TEST(GetrandomBitDistribution, NullBufferWithZeroLengthSucceeds) {
    errno = 0;
    EXPECT_EQ(0, RawGetrandom(nullptr, 0, 0));
    EXPECT_EQ(0, errno);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
