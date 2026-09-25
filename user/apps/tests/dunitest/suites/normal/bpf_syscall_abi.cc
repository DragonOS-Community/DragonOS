#include <gtest/gtest.h>

#include <fcntl.h>
#include <linux/bpf.h>
#include <linux/capability.h>
#include <sys/stat.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <array>
#include <cerrno>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <string>

namespace {

// Linux v6.6 include/uapi/linux/bpf.h: keep the extended query test
// independent of the build host's (possibly older) UAPI headers.
struct BpfQueryAttr66 {
  uint32_t target_fd;
  uint32_t attach_type;
  uint32_t query_flags;
  uint32_t attach_flags;
  alignas(8) uint64_t prog_ids;
  uint32_t prog_cnt;
  uint32_t reserved;
  alignas(8) uint64_t prog_attach_flags;
  alignas(8) uint64_t link_ids;
  alignas(8) uint64_t link_attach_flags;
  uint64_t revision;
};
static_assert(sizeof(BpfQueryAttr66) == 64);
static_assert(offsetof(BpfQueryAttr66, prog_attach_flags) == 32);
static_assert(offsetof(BpfQueryAttr66, link_ids) == 40);
static_assert(offsetof(BpfQueryAttr66, revision) == 56);
constexpr uint32_t kBpfLsmCgroup = 43;

bool HasNetAdmin() {
  __user_cap_header_struct header{};
  header.version = _LINUX_CAPABILITY_VERSION_3;
  __user_cap_data_struct data[2]{};
  if (syscall(SYS_capget, &header, data) != 0) return false;
  return (data[0].effective & (uint32_t{1} << CAP_NET_ADMIN)) != 0;
}

long Query(void* attr, size_t size) {
  return syscall(SYS_bpf, BPF_PROG_QUERY, attr, size);
}

union bpf_attr InvalidDeviceQuery() {
  union bpf_attr attr{};
  attr.query.target_fd = static_cast<uint32_t>(-1);
  attr.query.attach_type = BPF_CGROUP_DEVICE;
  return attr;
}

TEST(BpfSyscallAbi, InvalidFdWithNativeAndExtendedSize) {
  if (!HasNetAdmin()) GTEST_SKIP() << "CAP_NET_ADMIN required by Linux";
  auto attr = InvalidDeviceQuery();
  errno = 0;
  EXPECT_EQ(Query(&attr, sizeof(attr)), -1);
  EXPECT_EQ(errno, EBADF);

  std::array<unsigned char, 512> wide{};
  std::memcpy(wide.data(), &attr, sizeof(attr));
  errno = 0;
  EXPECT_EQ(Query(wide.data(), wide.size()), -1);
  EXPECT_EQ(errno, EBADF);
}

TEST(BpfSyscallAbi, UnknownTailMustBeZero) {
  auto attr = InvalidDeviceQuery();
  std::array<unsigned char, 512> wide{};
  std::memcpy(wide.data(), &attr, sizeof(attr));
  // Linux 6.6 knows 144 bytes. DragonOS's generated Rust binding includes
  // later fields; byte 144 must still be treated as unknown input.
  wide[144] = 1;
  errno = 0;
  EXPECT_EQ(Query(wide.data(), 152), -1);
  EXPECT_EQ(errno, E2BIG);
  wide[144] = 0;
  wide.back() = 1;
  errno = 0;
  EXPECT_EQ(Query(wide.data(), wide.size()), -1);
  EXPECT_EQ(errno, E2BIG);
  errno = 0;
  EXPECT_EQ(Query(nullptr, 4097), -1);
  EXPECT_EQ(errno, E2BIG);
}

TEST(BpfSyscallAbi, ZeroSizedNullAttributeDoesNotFaultKernel) {
  // The syscall may report an ordinary error after zero-initializing attr;
  // the important invariant is that size=0 never dereferences a null pointer.
  errno = 0;
  EXPECT_EQ(Query(nullptr, 0), -1);
  EXPECT_NE(errno, 0);
}

TEST(BpfSyscallAbi, InvalidAndNonCgroupFileDescriptors) {
  if (!HasNetAdmin()) GTEST_SKIP() << "CAP_NET_ADMIN required by Linux";
  const int fd = open("/dev/null", O_RDONLY | O_CLOEXEC);
  ASSERT_GE(fd, 0);
  auto attr = InvalidDeviceQuery();
  attr.query.target_fd = fd;
  errno = 0;
  EXPECT_EQ(Query(&attr, sizeof(attr)), -1);
  EXPECT_EQ(errno, EBADF);
  close(fd);

  const int cgroup_file = open("/sys/fs/cgroup/cgroup.controllers", O_RDONLY | O_CLOEXEC);
  if (cgroup_file >= 0) {
    attr.query.target_fd = cgroup_file;
    errno = 0;
    EXPECT_EQ(Query(&attr, sizeof(attr)), -1);
    EXPECT_EQ(errno, EBADF);
    close(cgroup_file);
  }
}

TEST(BpfSyscallAbi, EmptyCgroupQueryAndFlagsValidation) {
  if (!HasNetAdmin()) GTEST_SKIP() << "CAP_NET_ADMIN required by Linux";
  const int fd = open("/sys/fs/cgroup", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  if (fd < 0) GTEST_SKIP() << "cgroup2 mount unavailable";
  struct {
    BpfQueryAttr66 query;
  } attr{};
  attr.query.attach_type = BPF_CGROUP_DEVICE;
  attr.query.target_fd = fd;
  attr.query.query_flags = BPF_F_QUERY_EFFECTIVE;
  attr.query.attach_flags = 123;
  errno = 0;
  EXPECT_EQ(Query(&attr, sizeof(attr)), 0) << errno;
  EXPECT_EQ(attr.query.attach_flags, 0u);

  attr.query.link_ids = 123;  // Inside the supported query layout, not CHECK_ATTR tail.
  attr.query.revision = 456;
  errno = 0;
  EXPECT_EQ(Query(&attr, sizeof(attr)), 0) << errno;
  attr.query.link_ids = 0;
  attr.query.revision = 0;

  attr.query.prog_attach_flags = 1;
  errno = 0;
  EXPECT_EQ(Query(&attr, sizeof(attr)), -1);
  EXPECT_EQ(errno, EINVAL);

  attr.query.prog_attach_flags = 0;
  attr.query.attach_type = kBpfLsmCgroup;
  attr.query.query_flags = 0;
  uint32_t prog_id = 0;
  attr.query.prog_cnt = 1;
  attr.query.prog_ids = reinterpret_cast<uintptr_t>(&prog_id);
  errno = 0;
  EXPECT_EQ(Query(&attr, sizeof(attr)), -1);
  EXPECT_EQ(errno, EINVAL);

  attr.query.attach_type = BPF_TRACE_RAW_TP;
  errno = 0;
  EXPECT_EQ(Query(&attr, sizeof(attr)), -1);
  EXPECT_EQ(errno, EINVAL);
  close(fd);
}

TEST(BpfSyscallAbi, ShortInputStillWritesFixedOutputFields) {
  if (!HasNetAdmin()) GTEST_SKIP() << "CAP_NET_ADMIN required by Linux";
  const int fd = open("/sys/fs/cgroup", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  if (fd < 0) GTEST_SKIP() << "cgroup2 mount unavailable";

  auto attr = InvalidDeviceQuery();
  attr.query.target_fd = fd;
  attr.query.query_flags = BPF_F_QUERY_EFFECTIVE;
  attr.query.attach_flags = 123;
  constexpr size_t kInputSize = offsetof(union bpf_attr, query.attach_flags);
  errno = 0;
  EXPECT_EQ(Query(&attr, kInputSize), 0) << errno;
  EXPECT_EQ(attr.query.attach_flags, 0u);

  // Place attach_flags in a writable page and prog_cnt in a read-only page.
  // Linux writes flags first, then reports EFAULT without rolling flags back.
  const long page_size = sysconf(_SC_PAGESIZE);
  ASSERT_GT(page_size, 0);
  auto* pages = static_cast<unsigned char*>(
      mmap(nullptr, 2 * page_size, PROT_READ | PROT_WRITE,
           MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
  ASSERT_NE(pages, MAP_FAILED);
  ASSERT_EQ(mprotect(pages + page_size, page_size, PROT_READ), 0);
  auto* raw = pages + page_size - 20;
  uint32_t target = static_cast<uint32_t>(fd);
  uint32_t attach_type = BPF_CGROUP_DEVICE;
  uint32_t flags = 123;
  std::memcpy(raw, &target, sizeof(target));
  std::memcpy(raw + 4, &attach_type, sizeof(attach_type));
  std::memcpy(raw + 12, &flags, sizeof(flags));
  errno = 0;
  EXPECT_EQ(Query(raw, kInputSize), -1);
  EXPECT_EQ(errno, EFAULT);
  std::memcpy(&flags, raw + 12, sizeof(flags));
  EXPECT_EQ(flags, 0u);
  munmap(pages, 2 * page_size);
  close(fd);
}

TEST(BpfSyscallAbi, OtherCommandsNeverPanic) {
  union bpf_attr attr{};
  errno = 0;
  EXPECT_EQ(syscall(SYS_bpf, 0x7fffffff, &attr, sizeof(attr)), -1);
  EXPECT_EQ(errno, EINVAL);
  errno = 0;
  EXPECT_EQ(syscall(SYS_bpf, BPF_PROG_ATTACH, &attr, sizeof(attr)), -1);
  EXPECT_NE(errno, 0);
  errno = 0;
  EXPECT_EQ(syscall(SYS_bpf, BPF_MAP_LOOKUP_BATCH, &attr, sizeof(attr)), -1);
  EXPECT_NE(errno, 0);
}

TEST(BpfSyscallAbi, DeletedCgroupDirectoryIsOffline) {
  if (!HasNetAdmin()) GTEST_SKIP() << "CAP_NET_ADMIN required by Linux";
  const std::string path = "/sys/fs/cgroup/dkc004-" + std::to_string(getpid());
  if (mkdir(path.c_str(), 0755) != 0) GTEST_SKIP() << "cannot create test cgroup";
  const int fd = open(path.c_str(), O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  if (fd < 0) {
    rmdir(path.c_str());
    GTEST_SKIP() << "cannot open test cgroup";
  }
  if (rmdir(path.c_str()) != 0) {
    close(fd);
    GTEST_SKIP() << "cannot remove test cgroup";
  }
  auto attr = InvalidDeviceQuery();
  attr.query.target_fd = fd;
  errno = 0;
  EXPECT_EQ(Query(&attr, sizeof(attr)), -1);
  EXPECT_EQ(errno, ENOENT);
  close(fd);
}

TEST(BpfSyscallAbi, ExistingMapCreateStillWorks) {
  union bpf_attr attr{};
  attr.map_type = BPF_MAP_TYPE_ARRAY;
  attr.key_size = sizeof(uint32_t);
  attr.value_size = sizeof(uint64_t);
  attr.max_entries = 1;
  const int fd = syscall(SYS_bpf, BPF_MAP_CREATE, &attr, sizeof(attr));
  if (fd < 0 && (errno == EPERM || errno == EACCES))
    GTEST_SKIP() << "BPF map creation disallowed by host policy";
  ASSERT_GE(fd, 0) << errno;
  close(fd);
}

TEST(BpfSyscallAbi, UnsupportedMapTypesReturnErrorsWithoutPanic) {
  errno = 0;
  EXPECT_EQ(syscall(SYS_bpf, BPF_MAP_CREATE, nullptr, 0), -1);
  EXPECT_EQ(errno, EINVAL);

  union bpf_attr attr{};
  attr.map_type = BPF_MAP_TYPE_RINGBUF;
  // A zero max_entries is invalid on Linux and also reaches DragonOS's
  // currently unsupported map-type fallback.
  errno = 0;
  EXPECT_EQ(syscall(SYS_bpf, BPF_MAP_CREATE, &attr, sizeof(attr)), -1);
  EXPECT_EQ(errno, EINVAL);
}

}  // namespace

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
