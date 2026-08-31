#include <gtest/gtest.h>

#include <errno.h>
#include <linux/capability.h>
#include <net/if.h>
#include <net/if_arp.h>
#include <sched.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <string>
#include <vector>

namespace {

constexpr std::array<unsigned long, 4> kQueryCommands = {
    SIOCGIFINDEX, SIOCGIFFLAGS, SIOCGIFMTU, SIOCGIFHWADDR};
constexpr unsigned char kSentinel = 0xa5;

class ScopedFd {
  public:
    explicit ScopedFd(int fd = -1) : fd_(fd) {}
    ~ScopedFd() {
        if (fd_ >= 0) close(fd_);
    }
    ScopedFd(const ScopedFd&) = delete;
    ScopedFd& operator=(const ScopedFd&) = delete;
    int get() const { return fd_; }

  private:
    int fd_;
};

bool IsDragonOS() {
    struct utsname uts {};
    return uname(&uts) == 0 && std::strstr(uts.release, "dragonos") != nullptr;
}

void InitIfreq(struct ifreq* ifr, const char* name) {
    std::memset(ifr, kSentinel, sizeof(*ifr));
    std::memset(ifr->ifr_name, 0, IFNAMSIZ);
    std::strncpy(ifr->ifr_name, name, IFNAMSIZ - 1);
}

void ExpectSentinelFrom(const struct ifreq& ifr, size_t offset) {
    static_assert(sizeof(struct ifreq) == 40);
    const auto* bytes = reinterpret_cast<const unsigned char*>(&ifr) + IFNAMSIZ;
    for (size_t i = offset; i < sizeof(struct ifreq) - IFNAMSIZ; ++i) {
        EXPECT_EQ(bytes[i], kSentinel) << "union byte " << i << " changed";
    }
}

std::vector<std::string> ListInterfaces(int fd) {
    std::array<struct ifreq, 64> entries {};
    struct ifconf ifc {};
    ifc.ifc_len = static_cast<int>(sizeof(entries));
    ifc.ifc_req = entries.data();
    if (ioctl(fd, SIOCGIFCONF, &ifc) != 0) return {};

    std::vector<std::string> names;
    const int count = ifc.ifc_len / static_cast<int>(sizeof(struct ifreq));
    for (int i = 0; i < count; ++i) {
        entries[i].ifr_name[IFNAMSIZ - 1] = '\0';
        std::string name(entries[i].ifr_name);
        if (!name.empty() &&
            std::find(names.begin(), names.end(), name) == names.end()) {
            names.push_back(std::move(name));
        }
    }
    return names;
}

bool Contains(const std::vector<std::string>& names, const std::string& name) {
    return std::find(names.begin(), names.end(), name) != names.end();
}

int DropAllCapabilities() {
    struct __user_cap_header_struct header {};
    std::array<struct __user_cap_data_struct, 2> data {};
    header.version = _LINUX_CAPABILITY_VERSION_3;
    header.pid = 0;
    return static_cast<int>(syscall(SYS_capset, &header, data.data()));
}

TEST(SocketIoctlNetdevQuery, LoopbackFieldsAndUnionWidths) {
    ScopedFd fd(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(fd.get(), 0);

    struct ifreq ifr {};
    InitIfreq(&ifr, "lo");
    ASSERT_EQ(ioctl(fd.get(), SIOCGIFINDEX, &ifr), 0) << strerror(errno);
    EXPECT_EQ(ifr.ifr_ifindex, 1);
    ExpectSentinelFrom(ifr, sizeof(int));

    InitIfreq(&ifr, "lo");
    ASSERT_EQ(ioctl(fd.get(), SIOCGIFFLAGS, &ifr), 0) << strerror(errno);
    EXPECT_NE(ifr.ifr_flags & IFF_LOOPBACK, 0);
    ExpectSentinelFrom(ifr, sizeof(short));

    InitIfreq(&ifr, "lo");
    ASSERT_EQ(ioctl(fd.get(), SIOCGIFMTU, &ifr), 0) << strerror(errno);
    EXPECT_GT(ifr.ifr_mtu, 0);
    ExpectSentinelFrom(ifr, sizeof(int));

    InitIfreq(&ifr, "lo");
    ASSERT_EQ(ioctl(fd.get(), SIOCGIFHWADDR, &ifr), 0) << strerror(errno);
    EXPECT_EQ(ifr.ifr_hwaddr.sa_family, ARPHRD_LOOPBACK);
    for (size_t i = 0; i < 6; ++i) {
        EXPECT_EQ(static_cast<unsigned char>(ifr.ifr_hwaddr.sa_data[i]), 0);
    }
    ExpectSentinelFrom(ifr, sizeof(sa_family_t) + 6);
}

TEST(SocketIoctlNetdevQuery, NameNormalizationAndAliases) {
    ScopedFd fd(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(fd.get(), 0);

    struct ifreq plain {};
    InitIfreq(&plain, "lo");
    ASSERT_EQ(ioctl(fd.get(), SIOCGIFINDEX, &plain), 0);

    struct ifreq alias {};
    InitIfreq(&alias, "lo:1");
    ASSERT_EQ(ioctl(fd.get(), SIOCGIFINDEX, &alias), 0) << strerror(errno);
    EXPECT_EQ(alias.ifr_ifindex, plain.ifr_ifindex);
    EXPECT_STREQ(alias.ifr_name, "lo:1");

    struct ifreq terminated {};
    InitIfreq(&terminated, "lo");
    terminated.ifr_name[IFNAMSIZ - 1] = 'x';
    ASSERT_EQ(ioctl(fd.get(), SIOCGIFINDEX, &terminated), 0) << strerror(errno);
    EXPECT_EQ(terminated.ifr_name[IFNAMSIZ - 1], '\0');

    struct ifreq invalid {};
    InitIfreq(&invalid, "");
    errno = 0;
    EXPECT_EQ(ioctl(fd.get(), SIOCGIFINDEX, &invalid), -1);
    EXPECT_EQ(errno, ENODEV);

    std::memset(&invalid, kSentinel, sizeof(invalid));
    std::memset(invalid.ifr_name, 'x', IFNAMSIZ);
    errno = 0;
    EXPECT_EQ(ioctl(fd.get(), SIOCGIFINDEX, &invalid), -1);
    EXPECT_EQ(errno, ENODEV);

    InitIfreq(&invalid, "lo");
    invalid.ifr_name[0] = static_cast<char>(0xff);
    invalid.ifr_name[1] = '\0';
    errno = 0;
    EXPECT_EQ(ioctl(fd.get(), SIOCGIFINDEX, &invalid), -1);
    EXPECT_EQ(errno, ENODEV);
}

TEST(SocketIoctlNetdevQuery, MissingDeviceAndUserCopyFaults) {
    ScopedFd fd(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(fd.get(), 0);

    for (unsigned long command : kQueryCommands) {
        struct ifreq ifr {};
        InitIfreq(&ifr, "no-such-netdev");
        const auto before = ifr;
        errno = 0;
        EXPECT_EQ(ioctl(fd.get(), command, &ifr), -1) << command;
        EXPECT_EQ(errno, ENODEV) << command;
        EXPECT_EQ(std::memcmp(&ifr, &before, sizeof(ifr)), 0) << command;

        errno = 0;
        EXPECT_EQ(ioctl(fd.get(), command, nullptr), -1) << command;
        EXPECT_EQ(errno, EFAULT) << command;
    }

    const long page_size = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size, 0);
    void* mapping = mmap(nullptr, page_size * 2, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(mapping, MAP_FAILED);
    ASSERT_EQ(mprotect(static_cast<char*>(mapping) + page_size, page_size,
                       PROT_NONE),
              0);
    auto* partial = reinterpret_cast<struct ifreq*>(
        static_cast<char*>(mapping) + page_size - sizeof(struct ifreq) + 1);
    std::memset(partial, 0, sizeof(struct ifreq) - 1);
    std::memcpy(partial->ifr_name, "lo", 3);
    errno = 0;
    EXPECT_EQ(ioctl(fd.get(), SIOCGIFINDEX, partial), -1);
    EXPECT_EQ(errno, EFAULT);
    ASSERT_EQ(mprotect(static_cast<char*>(mapping) + page_size, page_size,
                       PROT_READ | PROT_WRITE),
              0);
    ASSERT_EQ(munmap(mapping, page_size * 2), 0);

    void* readonly = mmap(nullptr, page_size, PROT_READ | PROT_WRITE,
                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(readonly, MAP_FAILED);
    auto* readonly_ifr = static_cast<struct ifreq*>(readonly);
    InitIfreq(readonly_ifr, "lo");
    ASSERT_EQ(mprotect(readonly, page_size, PROT_READ), 0);
    errno = 0;
    EXPECT_EQ(ioctl(fd.get(), SIOCGIFINDEX, readonly_ifr), -1);
    EXPECT_EQ(errno, EFAULT);
    ASSERT_EQ(mprotect(readonly, page_size, PROT_READ | PROT_WRITE), 0);
    ASSERT_EQ(munmap(readonly, page_size), 0);
}

TEST(SocketIoctlNetdevQuery, EnumeratedDeviceProperties) {
    ScopedFd fd(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(fd.get(), 0);
    const auto names = ListInterfaces(fd.get());
    ASSERT_FALSE(names.empty());

    bool found_ethernet = false;
    for (const auto& name : names) {
        struct ifreq ifr {};
        InitIfreq(&ifr, name.c_str());
        ASSERT_EQ(ioctl(fd.get(), SIOCGIFINDEX, &ifr), 0) << name;
        EXPECT_GT(ifr.ifr_ifindex, 0) << name;

        InitIfreq(&ifr, name.c_str());
        ASSERT_EQ(ioctl(fd.get(), SIOCGIFFLAGS, &ifr), 0) << name;

        InitIfreq(&ifr, name.c_str());
        ASSERT_EQ(ioctl(fd.get(), SIOCGIFMTU, &ifr), 0) << name;
        EXPECT_GT(ifr.ifr_mtu, 0) << name;

        InitIfreq(&ifr, name.c_str());
        ASSERT_EQ(ioctl(fd.get(), SIOCGIFHWADDR, &ifr), 0) << name;
        if (ifr.ifr_hwaddr.sa_family == ARPHRD_ETHER) {
            found_ethernet = true;
            bool all_zero = true;
            for (size_t i = 0; i < 6; ++i) {
                all_zero &= ifr.ifr_hwaddr.sa_data[i] == 0;
            }
            EXPECT_FALSE(all_zero) << name;
        }
    }
    if (IsDragonOS()) {
        EXPECT_TRUE(found_ethernet);
    }
}

TEST(SocketIoctlNetdevQuery, IfconfWritesOnlyLength) {
    ScopedFd fd(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(fd.get(), 0);

    struct ifconf ifc;
    std::memset(&ifc, kSentinel, sizeof(ifc));
    ifc.ifc_len = 0;
    ifc.ifc_buf = nullptr;

    std::array<unsigned char, offsetof(struct ifconf, ifc_buf) - sizeof(int)>
        padding {};
    std::memcpy(padding.data(),
                reinterpret_cast<unsigned char*>(&ifc) + sizeof(int),
                padding.size());

    ASSERT_EQ(ioctl(fd.get(), SIOCGIFCONF, &ifc), 0) << strerror(errno);
    EXPECT_GE(ifc.ifc_len, 0);
    EXPECT_EQ(ifc.ifc_buf, nullptr);
    EXPECT_EQ(std::memcmp(reinterpret_cast<unsigned char*>(&ifc) + sizeof(int),
                          padding.data(), padding.size()),
              0);
}

int RunSocketNetnsCase() {
    ScopedFd old_fd(socket(AF_INET, SOCK_DGRAM, 0));
    if (old_fd.get() < 0) return 10;
    const auto old_names = ListInterfaces(old_fd.get());
    auto non_lo = std::find_if(old_names.begin(), old_names.end(),
                               [](const std::string& name) {
                                   return name != "lo";
                               });
    const std::string old_unique =
        non_lo == old_names.end() ? std::string() : *non_lo;

    if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) return 77;

    struct ifreq ifr {};
    if (!old_unique.empty()) {
        InitIfreq(&ifr, old_unique.c_str());
        if (ioctl(old_fd.get(), SIOCGIFINDEX, &ifr) != 0) return 11;
    }

    ScopedFd new_fd(socket(AF_INET, SOCK_DGRAM, 0));
    if (new_fd.get() < 0) return 12;
    InitIfreq(&ifr, "lo");
    if (ioctl(new_fd.get(), SIOCGIFINDEX, &ifr) != 0) return 13;

    if (!old_unique.empty()) {
        InitIfreq(&ifr, old_unique.c_str());
        errno = 0;
        if (ioctl(new_fd.get(), SIOCGIFINDEX, &ifr) != -1 || errno != ENODEV) {
            return 14;
        }
        if (!Contains(ListInterfaces(old_fd.get()), old_unique)) return 15;
        if (Contains(ListInterfaces(new_fd.get()), old_unique)) return 16;
    } else if (IsDragonOS()) {
        return 17;
    }

    if (DropAllCapabilities() != 0) return 18;
    InitIfreq(&ifr, "lo");
    if (ioctl(new_fd.get(), SIOCGIFHWADDR, &ifr) != 0) return 19;
    if (ioctl(new_fd.get(), SIOCGIFFLAGS, &ifr) != 0) return 20;
    if (ioctl(new_fd.get(), SIOCGIFMTU, &ifr) != 0) return 21;
    return 0;
}

TEST(SocketIoctlNetdevQuery, SocketNamespaceIsStableAndQueriesAreUnprivileged) {
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) _exit(RunSocketNetnsCase());

    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    const int result = WEXITSTATUS(status);
    if (result == 77 && !IsDragonOS()) {
        GTEST_SKIP() << "host does not permit user/network namespace creation";
    }
    EXPECT_EQ(result, 0);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
