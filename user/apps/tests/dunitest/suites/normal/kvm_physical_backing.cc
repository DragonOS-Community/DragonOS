#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#if defined(__x86_64__)
#include <linux/kvm.h>

namespace {
class KvmPhysicalBacking : public ::testing::Test {
  protected:
    void SetUp() override {
        kvm_ = open("/dev/kvm", O_RDWR | O_CLOEXEC);
        if (kvm_ < 0 && (errno == ENOENT || errno == ENODEV || errno == EACCES || errno == EPERM)) {
            GTEST_SKIP() << "KVM is unavailable: " << strerror(errno);
        }
        ASSERT_GE(kvm_, 0) << strerror(errno);
    }
    void TearDown() override {
        if (kvm_ >= 0) close(kvm_);
    }
    int kvm_ = -1;
};

TEST_F(KvmPhysicalBacking, RepeatedVcpuConstructionAndOwnerRelease) {
    // vCPU initialization programs VMCS pointers before publishing the vCPU.
    // Repeated construction/destruction covers valid Rust initialization and
    // backing lifetime even without running nested guest instructions.
    for (int iteration = 0; iteration < 3; ++iteration) {
        const int vm = ioctl(kvm_, KVM_CREATE_VM, 0);
        ASSERT_GE(vm, 0) << "iteration=" << iteration << ": " << strerror(errno);
        const int vcpu = ioctl(vm, KVM_CREATE_VCPU, 0);
        if (vcpu < 0) {
            const int error = errno;
            close(vm);
            FAIL() << "KVM_CREATE_VCPU iteration=" << iteration << ": " << strerror(error);
        }
        EXPECT_EQ(close(vcpu), 0) << strerror(errno);
        EXPECT_EQ(close(vm), 0) << strerror(errno);
    }
}
}  // namespace
#else
TEST(KvmPhysicalBacking, RequiresX86Vmx) { GTEST_SKIP() << "requires x86 VMX"; }
#endif

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
