#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <gtest/gtest.h>
#include <arpa/inet.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <net/if.h>
#include <sched.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
#include <functional>
#include <chrono>
#include <string>
#include <thread>

namespace {
constexpr const char* kStart = "/proc/sys/net/ipv4/ip_unprivileged_port_start";
constexpr const char* kRange = "/proc/sys/net/ipv4/ip_local_port_range";
class Fd {
 public:
  explicit Fd(int fd) : fd_(fd) {}
  ~Fd() { reset(); }
  Fd(const Fd&) = delete;
  Fd& operator=(const Fd&) = delete;
  int get() const { return fd_; }
  void reset() { if (fd_ >= 0) close(fd_); fd_ = -1; }
 private:
  int fd_;
};

int WriteFd(int fd, const std::string& value) {
  if (lseek(fd, 0, SEEK_SET) < 0) return errno;
  const ssize_t count = write(fd, value.data(), value.size());
  return count == static_cast<ssize_t>(value.size()) ? 0 : (count < 0 ? errno : EIO);
}
int Write(const char* path, const std::string& value) {
  Fd fd(open(path, O_WRONLY));
  if (fd.get() < 0) return errno;
  return WriteFd(fd.get(), value);
}
std::string ReadFd(int fd) {
  char value[128] {};
  EXPECT_EQ(lseek(fd, 0, SEEK_SET), 0);
  const ssize_t count = read(fd, value, sizeof(value));
  EXPECT_GE(count, 0);
  return count > 0 ? std::string(value, count) : "";
}
std::string Read(const char* path) {
  Fd fd(open(path, O_RDONLY));
  EXPECT_GE(fd.get(), 0) << path;
  return fd.get() >= 0 ? ReadFd(fd.get()) : "";
}
void EffectiveCapability(int capability, bool enabled) {
  __user_cap_header_struct header { _LINUX_CAPABILITY_VERSION_3, 0 };
  __user_cap_data_struct data[2] {};
  ASSERT_EQ(syscall(SYS_capget, &header, data), 0);
  const uint32_t bit = 1u << (capability % 32);
  if (enabled) data[capability / 32].effective |= bit;
  else data[capability / 32].effective &= ~bit;
  ASSERT_EQ(syscall(SYS_capset, &header, data), 0);
}
void InNamespace(const std::function<void()>& test, uid_t initial_uid = 0) {
  int ready_pipe[2], mapped_pipe[2];
  ASSERT_EQ(pipe(ready_pipe), 0);
  Fd ready_read(ready_pipe[0]), ready_write(ready_pipe[1]);
  ASSERT_EQ(pipe(mapped_pipe), 0);
  Fd mapped_read(mapped_pipe[0]), mapped_write(mapped_pipe[1]);
  pid_t child = fork();
  ASSERT_GE(child, 0);
  if (child == 0) {
    ready_read.reset();
    mapped_write.reset();
    char ready = unshare(CLONE_NEWUSER | CLONE_NEWNET) == 0 ? 1 : 0;
    if (write(ready_write.get(), &ready, 1) != 1) _exit(2);
    char mapped = 0;
    if (read(mapped_read.get(), &mapped, 1) != 1 || !mapped) _exit(2);
    if (setgid(0) != 0 || setuid(0) != 0) _exit(5);
    Fd control(socket(AF_INET, SOCK_DGRAM, 0));
    ifreq request {};
    std::snprintf(request.ifr_name, sizeof(request.ifr_name), "lo");
    if (control.get() < 0 || ioctl(control.get(), SIOCGIFFLAGS, &request) != 0) _exit(3);
    request.ifr_flags |= IFF_UP;
    if (ioctl(control.get(), SIOCSIFFLAGS, &request) != 0) _exit(4);
    control.reset();
    test();
    _exit(testing::Test::HasFailure() ? 1 : 0);
  }
  ready_write.reset();
  mapped_read.reset();
  char ready = 0;
  const std::string proc = "/proc/" + std::to_string(child);
  char mapped = read(ready_read.get(), &ready, 1) == 1 && ready &&
      Write((proc + "/uid_map").c_str(), "0 " + std::to_string(initial_uid) + " 1\n") == 0 &&
      Write((proc + "/setgroups").c_str(), "deny\n") == 0 &&
      Write((proc + "/gid_map").c_str(), "0 " + std::to_string(initial_uid) + " 1\n") == 0;
  EXPECT_EQ(write(mapped_write.get(), &mapped, 1), 1);
  int status = 0;
  ASSERT_EQ(waitpid(child, &status, 0), child);
  EXPECT_TRUE(WIFEXITED(status));
  EXPECT_EQ(WEXITSTATUS(status), 0) << "isolated case failed";
}
int Bind(int fd, int family, uint16_t port, bool mapped = false, bool nonlocal = false) {
  if (family == AF_INET) {
    sockaddr_in address {};
    address.sin_family = family;
    address.sin_port = htons(port);
    if (nonlocal) inet_pton(AF_INET, "192.0.2.1", &address.sin_addr);
    return bind(fd, reinterpret_cast<sockaddr*>(&address), sizeof(address));
  }
  sockaddr_in6 address {};
  address.sin6_family = family;
  address.sin6_port = htons(port);
  if (mapped || nonlocal) inet_pton(AF_INET6, mapped ? "::ffff:127.0.0.1" : "fd00::99", &address.sin6_addr);
  return bind(fd, reinterpret_cast<sockaddr*>(&address), sizeof(address));
}
uint16_t Port(int fd) {
  sockaddr_storage address {};
  socklen_t length = sizeof(address);
  EXPECT_EQ(getsockname(fd, reinterpret_cast<sockaddr*>(&address), &length), 0);
  return ntohs(address.ss_family == AF_INET
      ? reinterpret_cast<sockaddr_in*>(&address)->sin_port
      : reinterpret_cast<sockaddr_in6*>(&address)->sin6_port);
}

TEST(InetBindPolicy, DefaultRejectsLowPortWithoutEffectiveCapability) {
  InNamespace([] {
    ASSERT_NO_FATAL_FAILURE(EffectiveCapability(CAP_NET_BIND_SERVICE, false));
    for (int family : {AF_INET, AF_INET6}) for (int type : {SOCK_STREAM, SOCK_DGRAM}) {
      Fd fd(socket(family, type, 0));
      ASSERT_GE(fd.get(), 0);
      EXPECT_EQ(Bind(fd.get(), family, 80), -1);
      EXPECT_EQ(errno, EACCES);
      EXPECT_EQ(Port(fd.get()), 0);
    }
  });
}
TEST(InetBindPolicy, EffectiveCapabilityAllowsLowPort) {
  InNamespace([] {
    for (int family : {AF_INET, AF_INET6}) for (int type : {SOCK_STREAM, SOCK_DGRAM}) {
      Fd fd(socket(family, type, 0));
      ASSERT_GE(fd.get(), 0);
      ASSERT_EQ(Bind(fd.get(), family, 80), 0);
      EXPECT_EQ(Port(fd.get()), 80);
    }
  });
}
TEST(InetBindPolicy, ThresholdIsStrictAndMappedUsesIPv6SocketPolicy) {
  InNamespace([] {
    EXPECT_EQ(Read(kStart), "1024\n");
    ASSERT_EQ(Write(kStart, "6000\n"), 0);
    ASSERT_NO_FATAL_FAILURE(EffectiveCapability(CAP_NET_BIND_SERVICE, false));
    for (int family : {AF_INET, AF_INET6}) for (int type : {SOCK_STREAM, SOCK_DGRAM}) {
      for (bool mapped : {false, true}) {
        if (mapped && family == AF_INET) continue;
        Fd fd(socket(family, type, 0));
        EXPECT_EQ(Bind(fd.get(), family, 5999, mapped), -1);
        EXPECT_EQ(errno, EACCES);
        ASSERT_EQ(Bind(fd.get(), family, 6000, mapped), 0);
      }
    }
  });
}
TEST(InetBindPolicy, ZeroThresholdAllowsAllNonzeroPorts) {
  InNamespace([] {
    ASSERT_EQ(Write(kStart, "0\n"), 0);
    ASSERT_NO_FATAL_FAILURE(EffectiveCapability(CAP_NET_BIND_SERVICE, false));
    for (int family : {AF_INET, AF_INET6}) for (int type : {SOCK_STREAM, SOCK_DGRAM}) {
      Fd fd(socket(family, type, 0));
      ASSERT_EQ(Bind(fd.get(), family, 1), 0);
    }
  });
}
TEST(InetBindPolicy, InvalidSysctlDoesNotChangePolicy) {
  InNamespace([] {
    for (const char* value : {"-1\n", "65536\n", "junk\n", "32769\n"}) {
      EXPECT_EQ(Write(kStart, value), EINVAL);
      EXPECT_EQ(Read(kStart), "1024\n");
    }
  });
}
TEST(InetBindPolicy, RangeAndThresholdCannotOverlap) {
  InNamespace([] {
    ASSERT_EQ(Write(kRange, "4000 4010\n"), 0);
    EXPECT_EQ(Write(kStart, "4001\n"), EINVAL);
    ASSERT_EQ(Write(kStart, "4000\n"), 0);
    EXPECT_EQ(Write(kRange, "3999 4010\n"), EINVAL);
    EXPECT_EQ(Read(kRange), "4000\t4010\n");
    EXPECT_EQ(Read(kStart), "4000\n");
  });
}
TEST(InetBindPolicy, AutomaticPortBindingDoesNotRequireCapability) {
  InNamespace([] {
    ASSERT_EQ(Write(kRange, "4000 4010\n"), 0);
    ASSERT_EQ(Write(kStart, "4000\n"), 0);
    ASSERT_NO_FATAL_FAILURE(EffectiveCapability(CAP_NET_BIND_SERVICE, false));
    for (int family : {AF_INET, AF_INET6}) for (int type : {SOCK_STREAM, SOCK_DGRAM}) {
      Fd fd(socket(family, type, 0));
      ASSERT_EQ(Bind(fd.get(), family, 0), 0);
      EXPECT_GE(Port(fd.get()), 4000);
      EXPECT_LE(Port(fd.get()), 4010);
    }
  });
}
TEST(InetBindPolicy, IPv4AddressAndIPv6AuthorizationErrorOrdering) {
  InNamespace([] {
    ASSERT_NO_FATAL_FAILURE(EffectiveCapability(CAP_NET_BIND_SERVICE, false));
    for (int type : {SOCK_STREAM, SOCK_DGRAM}) {
      Fd ipv4(socket(AF_INET, type, 0));
      EXPECT_EQ(Bind(ipv4.get(), AF_INET, 80, false, true), -1);
      EXPECT_EQ(errno, EADDRNOTAVAIL);
      Fd ipv6(socket(AF_INET6, type, 0));
      EXPECT_EQ(Bind(ipv6.get(), AF_INET6, 80, false, true), -1);
      EXPECT_EQ(errno, EACCES);
      Fd bound(socket(AF_INET, type, 0));
      ASSERT_EQ(Bind(bound.get(), AF_INET, 6000), 0);
      EXPECT_EQ(Bind(bound.get(), AF_INET, 80), -1);
      EXPECT_EQ(errno, EACCES);
      EXPECT_EQ(Port(bound.get()), 6000);
    }
  });
}
TEST(InetBindPolicy, OldSysctlFdAndSocketRemainInTheirNetworkNamespace) {
  InNamespace([] {
    Fd old_config(open(kStart, O_RDWR));
    Fd old_socket(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(old_config.get(), 0);
    ASSERT_EQ(unshare(CLONE_NEWNET), 0);
    EXPECT_EQ(Read(kStart), "1024\n");
    ASSERT_EQ(Write(kStart, "0\n"), 0);
    ASSERT_EQ(WriteFd(old_config.get(), "4096\n"), 0);
    EXPECT_EQ(ReadFd(old_config.get()), "4096\n");
    EXPECT_EQ(Read(kStart), "0\n");
    ASSERT_NO_FATAL_FAILURE(EffectiveCapability(CAP_NET_BIND_SERVICE, false));
    EXPECT_EQ(Bind(old_socket.get(), AF_INET, 80), -1);
    EXPECT_EQ(errno, EACCES);
    Fd new_socket(socket(AF_INET, SOCK_DGRAM, 0));
    EXPECT_EQ(Bind(new_socket.get(), AF_INET, 80), 0);
  });
}
TEST(InetBindPolicy, RetainedSysctlDirectoryUsesOriginalNamespace) {
  InNamespace([] {
    // A real socket keeps A's sysctl table registered after switching to B.
    Fd namespace_owner(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(namespace_owner.get(), 0);
    ASSERT_EQ(Write(kStart, "2048\n"), 0);
    Fd directory(open("/proc/sys/net/ipv4", O_RDONLY | O_DIRECTORY));
    Fd path_directory(open("/proc/sys/net/ipv4", O_PATH | O_DIRECTORY));
    ASSERT_GE(directory.get(), 0);
    ASSERT_GE(path_directory.get(), 0);
    ASSERT_EQ(unshare(CLONE_NEWNET), 0);
    ASSERT_EQ(Write(kStart, "0\n"), 0);
    ASSERT_EQ(Write("/proc/sys/net/ipv4/ip_forward", "0\n"), 0);
    Fd retained(openat(directory.get(), "ip_unprivileged_port_start", O_RDWR));
    ASSERT_GE(retained.get(), 0);
    EXPECT_EQ(ReadFd(retained.get()), "2048\n");
    ASSERT_EQ(WriteFd(retained.get(), "4096\n"), 0);
    EXPECT_EQ(Read(kStart), "0\n");
    Fd path_file(openat(path_directory.get(), "ip_unprivileged_port_start", O_RDONLY));
    ASSERT_GE(path_file.get(), 0);
    EXPECT_EQ(ReadFd(path_file.get()), "4096\n");
    Fd old_range(openat(directory.get(), "ip_local_port_range", O_RDWR));
    ASSERT_GE(old_range.get(), 0);
    ASSERT_EQ(WriteFd(old_range.get(), "5000 5010\n"), 0);
    EXPECT_EQ(Read(kRange), "32768\t60999\n");
    Fd old_forward(openat(directory.get(), "ip_forward", O_RDWR));
    ASSERT_GE(old_forward.get(), 0);
    ASSERT_EQ(WriteFd(old_forward.get(), "1\n"), 0);
    EXPECT_EQ(ReadFd(old_forward.get()), "1\n");
    EXPECT_EQ(Read("/proc/sys/net/ipv4/ip_forward"), "0\n");
    ASSERT_EQ(fchdir(directory.get()), 0);
    EXPECT_EQ(Read("ip_unprivileged_port_start"), "4096\n");
  });
}
TEST(InetBindPolicy, BindMountedSysctlDirectoryKeepsNamespace) {
  InNamespace([] {
    Fd namespace_owner(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(namespace_owner.get(), 0);
    ASSERT_EQ(unshare(CLONE_NEWNS), 0);
    char path[] = "/tmp/dkc091-dir-XXXXXX";
    ASSERT_NE(mkdtemp(path), nullptr);
    struct Cleanup {
      const char* path;
      bool mounted = false;
      ~Cleanup() { if (mounted) umount2(path, MNT_DETACH); rmdir(path); }
    } cleanup { path };
    ASSERT_EQ(Write(kStart, "2048\n"), 0);
    ASSERT_EQ(mount("/proc/sys/net/ipv4", path, nullptr, MS_BIND, nullptr), 0);
    cleanup.mounted = true;
    ASSERT_EQ(unshare(CLONE_NEWNET), 0);
    ASSERT_EQ(Write(kStart, "0\n"), 0);
    const std::string retained = std::string(path) + "/ip_unprivileged_port_start";
    EXPECT_EQ(Read(retained.c_str()), "2048\n");
    ASSERT_EQ(Write(retained.c_str(), "4096\n"), 0);
    EXPECT_EQ(Read(kStart), "0\n");
  });
}
TEST(InetBindPolicy, ExpiredDirectoryNeverFallsBackToCurrentNamespace) {
  InNamespace([] {
    ASSERT_EQ(Write(kStart, "2048\n"), 0);
    Fd directory(open("/proc/sys/net/ipv4", O_RDONLY | O_DIRECTORY));
    Fd leaf(open(kStart, O_PATH));
    ASSERT_GE(directory.get(), 0);
    ASSERT_GE(leaf.get(), 0);
    struct stat status {};
    ASSERT_EQ(fstat(directory.get(), &status), 0);
    ASSERT_EQ(fstat(leaf.get(), &status), 0);
    char entries[1024];
    // Read just the first entry, leaving the VFS iteration snapshot active.
    ASSERT_EQ(syscall(SYS_getdents64, directory.get(), entries, 24), 24);
    ASSERT_EQ(unshare(CLONE_NEWNET), 0);
    ASSERT_EQ(Write(kStart, "0\n"), 0);
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(3);
    // Linux unregisters a dying netns asynchronously. Cached directory/table
    // references do not make it immortal, nor may they select B instead.
    do {
      {
        Fd file(openat(directory.get(), "ip_unprivileged_port_start", O_RDONLY));
        if (file.get() < 0) {
          EXPECT_EQ(errno, ENOENT);
          // Leaf and parent tables can retire at different times. Continue
          // the cached iteration, including EOF, until the parent is gone;
          // do not rebuild the snapshot to make this assertion pass.
          do {
            const long count = syscall(SYS_getdents64, directory.get(), entries, sizeof(entries));
            if (count < 0) {
              EXPECT_EQ(errno, ENOENT);
              ASSERT_EQ(lseek(directory.get(), 0, SEEK_SET), 0);
              EXPECT_EQ(syscall(SYS_getdents64, directory.get(), entries, sizeof(entries)), -1);
              EXPECT_EQ(errno, ENOENT);
              EXPECT_EQ(fstat(directory.get(), &status), -1);
              EXPECT_EQ(errno, ENOENT);
              EXPECT_EQ(fchdir(directory.get()), -1);
              EXPECT_EQ(errno, ENOENT);
              EXPECT_EQ(fstat(leaf.get(), &status), -1);
              EXPECT_EQ(errno, ENOENT);
              EXPECT_EQ(fchdir(leaf.get()), -1);
              EXPECT_EQ(errno, ENOTDIR);
              return;
            }
            usleep(1000);
          } while (std::chrono::steady_clock::now() < deadline);
          FAIL() << "cached iteration ignored namespace retirement";
          return;
        }
        EXPECT_EQ(ReadFd(file.get()), "2048\n");
      }
      usleep(1000);
    } while (std::chrono::steady_clock::now() < deadline);
    FAIL() << "expired namespace directory stayed alive";
  });
}
TEST(InetBindPolicy, ChildUserNamespaceCannotAuthorizeAncestorSocket) {
  InNamespace([] {
    Fd fd(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_EQ(unshare(CLONE_NEWUSER), 0);
    EXPECT_EQ(Bind(fd.get(), AF_INET, 80), -1);
    EXPECT_EQ(errno, EACCES);
  });
}
TEST(InetBindPolicy, OrdinarySysctlDescriptorsDoNotKeepNamespaceAlive) {
  InNamespace([] {
    Fd config(open(kStart, O_RDWR));
    Fd range(open(kRange, O_RDWR));
    Fd forward(open("/proc/sys/net/ipv4/ip_forward", O_RDWR));
    ASSERT_GE(config.get(), 0);
    ASSERT_GE(range.get(), 0);
    ASSERT_GE(forward.get(), 0);
    ASSERT_EQ(unshare(CLONE_NEWNET), 0);
    char bytes[64];
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(3);
    // The forward table and port table unregister independently on Linux.
    for (int fd : {config.get(), range.get(), forward.get()}) {
      bool retired = false;
      do {
        if (read(fd, bytes, sizeof(bytes)) < 0) {
          ASSERT_EQ(errno, ENOENT);
          retired = true;
          break;
        }
        usleep(1000);
      } while (std::chrono::steady_clock::now() < deadline);
      ASSERT_TRUE(retired) << "ordinary sysctl fd pinned the namespace";
      EXPECT_EQ(read(fd, bytes, sizeof(bytes)), -1);
      EXPECT_EQ(errno, ENOENT);
      EXPECT_EQ(write(fd, "0\n", 2), -1);
      EXPECT_EQ(errno, ENOENT);
      struct stat status {};
      EXPECT_EQ(fstat(fd, &status), -1);
      EXPECT_EQ(errno, ENOENT);
      EXPECT_EQ(lseek(fd, 0, SEEK_SET), 0);
      EXPECT_EQ(lseek(fd, 0, SEEK_CUR), 0);
    }
  });
}
TEST(InetBindPolicy, SysctlNumericParsingAndOffsetSemantics) {
  InNamespace([] {
    ASSERT_EQ(Write(kStart, "0x1000\n"), 0);
    EXPECT_EQ(Read(kStart), "4096\n");
    Fd fd(open(kStart, O_RDWR));
    ASSERT_GE(fd.get(), 0);
    EXPECT_EQ(write(fd.get(), "2048 999\n", 9), 5);
    EXPECT_EQ(Read(kStart), "2048\n");
    EXPECT_EQ(write(fd.get(), "0\n", 2), 2);
    EXPECT_EQ(Read(kStart), "2048\n");
  });
}
TEST(InetBindPolicy, CapabilityRetryAndUnspecifiedBind) {
  InNamespace([] {
    for (int type : {SOCK_STREAM, SOCK_DGRAM}) {
      Fd fd(socket(AF_INET, type, 0));
      sockaddr_in addr {};
      addr.sin_family = AF_UNSPEC;
      addr.sin_port = htons(80);
      ASSERT_NO_FATAL_FAILURE(EffectiveCapability(CAP_NET_BIND_SERVICE, false));
      EXPECT_EQ(bind(fd.get(), reinterpret_cast<sockaddr*>(&addr), sizeof(addr)), -1);
      EXPECT_EQ(errno, EACCES);
      EXPECT_EQ(Port(fd.get()), 0);
      ASSERT_NO_FATAL_FAILURE(EffectiveCapability(CAP_NET_BIND_SERVICE, true));
      EXPECT_EQ(bind(fd.get(), reinterpret_cast<sockaddr*>(&addr), sizeof(addr)), 0);
    }
  });
}
TEST(InetBindPolicy, IPv6MulticastAndV6OnlyErrorOrdering) {
  InNamespace([] {
    Fd mapped(socket(AF_INET6, SOCK_STREAM, 0));
    int one = 1;
    ASSERT_EQ(setsockopt(mapped.get(), IPPROTO_IPV6, IPV6_V6ONLY, &one, sizeof(one)), 0);
    ASSERT_NO_FATAL_FAILURE(EffectiveCapability(CAP_NET_BIND_SERVICE, false));
    EXPECT_EQ(Bind(mapped.get(), AF_INET6, 80, true), -1);
    EXPECT_EQ(errno, EACCES);
    EXPECT_EQ(Bind(mapped.get(), AF_INET6, 6000, true), -1);
    EXPECT_EQ(errno, EINVAL);
    Fd multicast(socket(AF_INET6, SOCK_STREAM, 0));
    sockaddr_in6 addr {};
    addr.sin6_family = AF_INET6;
    addr.sin6_port = htons(80);
    ASSERT_EQ(inet_pton(AF_INET6, "ff02::1", &addr.sin6_addr), 1);
    EXPECT_EQ(bind(multicast.get(), reinterpret_cast<sockaddr*>(&addr), sizeof(addr)), -1);
    EXPECT_EQ(errno, EINVAL);
  });
}
TEST(InetBindPolicy, RangePartialUpdateAndReadOffset) {
  InNamespace([] {
    ASSERT_EQ(Write(kRange, "4000 4010\n"), 0);
    ASSERT_EQ(Write(kRange, "4002\n"), 0);
    EXPECT_EQ(Read(kRange), "4002\t4010\n");
    Fd fd(open(kStart, O_RDONLY));
    char byte;
    EXPECT_EQ(read(fd.get(), &byte, 1), 1);
    EXPECT_EQ(read(fd.get(), &byte, 1), 0);
    ASSERT_EQ(Write(kRange, "65535 65535\n"), 0);
    ASSERT_EQ(Write(kStart, "65535\n"), 0);
    EXPECT_EQ(Read(kStart), "65535\n");
  });
}
TEST(InetBindPolicy, SysctlRechecksCapabilityRatherThanMappedRootOrOpener) {
  InNamespace([] {
    EXPECT_EQ(geteuid(), 0u);
    Fd fd(open(kStart, O_RDWR));
    Fd forward(open("/proc/sys/net/ipv4/ip_forward", O_RDWR));
    ASSERT_GE(fd.get(), 0);
    ASSERT_GE(forward.get(), 0);
    ASSERT_EQ(WriteFd(fd.get(), "2048\n"), 0);
    ASSERT_EQ(WriteFd(forward.get(), "1\n"), 0);
    ASSERT_NO_FATAL_FAILURE(EffectiveCapability(CAP_NET_ADMIN, false));
    EXPECT_EQ(WriteFd(fd.get(), "0\n"), EPERM);
    EXPECT_EQ(WriteFd(forward.get(), "0\n"), EPERM);
    EXPECT_EQ(syscall(SYS_faccessat2, AT_FDCWD, kStart, W_OK, AT_EACCESS), -1);
    EXPECT_EQ(errno, EACCES);
    Fd denied(open(kStart, O_WRONLY));
    EXPECT_EQ(denied.get(), -1);
    EXPECT_EQ(errno, EACCES);
    EXPECT_EQ(Read(kStart), "2048\n");
    ASSERT_NO_FATAL_FAILURE(EffectiveCapability(CAP_NET_ADMIN, true));
    ASSERT_EQ(WriteFd(fd.get(), "0\n"), 0);
  }, 1000);
}
TEST(InetBindPolicy, GlobalRootSysctlModeDoesNotRequireNetAdmin) {
  InNamespace([] {
    ASSERT_NO_FATAL_FAILURE(EffectiveCapability(CAP_NET_ADMIN, false));
    ASSERT_EQ(Write(kStart, "0\n"), 0);
    EXPECT_EQ(Write("/proc/sys/net/ipv4/ip_forward", "1\n"), EPERM);
    EXPECT_EQ(Read(kStart), "0\n");
  });
}
TEST(InetBindPolicy, ConcurrentNamespaceLookupsKeepTheirOwnPolicy) {
  InNamespace([] {
    auto worker = [](uint16_t start) {
      ASSERT_EQ(unshare(CLONE_NEWNET), 0);
      const std::string expected = std::to_string(start) + "\n";
      for (int round = 0; round < 32; ++round) {
        ASSERT_EQ(Write(kStart, expected), 0);
        EXPECT_EQ(Read(kStart), expected);
      }
    };
    std::thread first(worker, 0);
    std::thread second(worker, 4096);
    first.join();
    second.join();
    EXPECT_EQ(Read(kStart), "1024\n");
  });
}
TEST(InetBindPolicy, ImplicitConnectSendAndListenDoNotRequireBindCapability) {
  InNamespace([] {
    ASSERT_EQ(Write(kRange, "4000 4010\n"), 0);
    ASSERT_EQ(Write(kStart, "4000\n"), 0);
    ASSERT_NO_FATAL_FAILURE(EffectiveCapability(CAP_NET_BIND_SERVICE, false));
    for (int family : {AF_INET, AF_INET6}) {
      sockaddr_storage storage {};
      socklen_t length;
      if (family == AF_INET) {
        auto* addr = reinterpret_cast<sockaddr_in*>(&storage);
        addr->sin_family = family;
        addr->sin_port = htons(7000);
        addr->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        length = sizeof(*addr);
      } else {
        auto* addr = reinterpret_cast<sockaddr_in6*>(&storage);
        addr->sin6_family = family;
        addr->sin6_port = htons(7000);
        addr->sin6_addr = in6addr_loopback;
        length = sizeof(*addr);
      }
      auto* addr = reinterpret_cast<sockaddr*>(&storage);
      Fd connected(socket(family, SOCK_DGRAM, 0));
      ASSERT_EQ(connect(connected.get(), addr, length), 0);
      EXPECT_GE(Port(connected.get()), 4000);
      EXPECT_LE(Port(connected.get()), 4010);
      Fd sent(socket(family, SOCK_DGRAM, 0));
      ASSERT_EQ(sendto(sent.get(), "x", 1, 0, addr, length), 1);
      EXPECT_GE(Port(sent.get()), 4000);
      EXPECT_LE(Port(sent.get()), 4010);
      Fd listener(socket(family, SOCK_STREAM, 0));
      ASSERT_EQ(listen(listener.get(), 1), 0);
      EXPECT_GE(Port(listener.get()), 4000);
      EXPECT_LE(Port(listener.get()), 4010);
      Fd client(socket(family, SOCK_STREAM | SOCK_NONBLOCK, 0));
      addr = reinterpret_cast<sockaddr*>(&storage);
      length = sizeof(storage);
      ASSERT_EQ(getsockname(listener.get(), addr, &length), 0);
      if (family == AF_INET) reinterpret_cast<sockaddr_in*>(addr)->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
      else reinterpret_cast<sockaddr_in6*>(addr)->sin6_addr = in6addr_loopback;
      const int result = connect(client.get(), addr, length);
      ASSERT_TRUE(result == 0 || (result == -1 && errno == EINPROGRESS));
      EXPECT_GE(Port(client.get()), 4000);
      EXPECT_LE(Port(client.get()), 4010);
    }
  });
}
}  // namespace
int main(int argc, char** argv) {
  testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
