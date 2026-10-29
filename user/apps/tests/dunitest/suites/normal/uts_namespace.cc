#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <sched.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>

namespace {

std::string hostname() {
    struct utsname value {};
    EXPECT_EQ(0, uname(&value)) << strerror(errno);
    return value.nodename;
}

std::string domainname() {
    struct utsname value {};
    EXPECT_EQ(0, uname(&value)) << strerror(errno);
    return value.domainname;
}

std::string read_file(const char* path) {
    int fd = open(path, O_RDONLY);
    EXPECT_GE(fd, 0) << strerror(errno);
    if (fd < 0) return {};
    char buf[128] = {};
    ssize_t n = read(fd, buf, sizeof(buf));
    EXPECT_GE(n, 0) << strerror(errno);
    close(fd);
    if (n < 0) return {};
    return std::string(buf, static_cast<size_t>(n));
}

void write_all(int fd, const char* data, size_t len, off_t offset = 0) {
    ssize_t n = pwrite(fd, data, len, offset);
    ASSERT_EQ(static_cast<ssize_t>(len), n) << strerror(errno);
}

}  // namespace

TEST(UtsNamespace, UnshareIsolatesHostnameAndSetnsRestoresNamespace) {
    const std::string original = hostname();
    int old_ns = open("/proc/self/ns/uts", O_RDONLY);
    ASSERT_GE(old_ns, 0) << strerror(errno);

    struct stat before {};
    ASSERT_EQ(0, fstat(old_ns, &before)) << strerror(errno);
    ASSERT_EQ(0, unshare(CLONE_NEWUTS)) << strerror(errno);

    struct stat after {};
    ASSERT_EQ(0, stat("/proc/self/ns/uts", &after)) << strerror(errno);
    ASSERT_NE(before.st_ino, after.st_ino);

    constexpr char changed[] = "dragonos-uts-child";
    ASSERT_EQ(0, sethostname(changed, sizeof(changed) - 1)) << strerror(errno);
    EXPECT_EQ(changed, hostname());

    ASSERT_EQ(0, setns(old_ns, CLONE_NEWUTS)) << strerror(errno);
    EXPECT_EQ(original, hostname());
    struct stat restored {};
    ASSERT_EQ(0, stat("/proc/self/ns/uts", &restored)) << strerror(errno);
    EXPECT_EQ(before.st_ino, restored.st_ino);
    close(old_ns);
}

TEST(UtsNamespace, ProcHostnameUsesCurrentNamespace) {
    const std::string original = hostname();
    const std::string original_domain = domainname();
    int old_ns = open("/proc/self/ns/uts", O_RDONLY);
    ASSERT_GE(old_ns, 0) << strerror(errno);
    ASSERT_EQ(0, unshare(CLONE_NEWUTS)) << strerror(errno);

    int fd = open("/proc/sys/kernel/hostname", O_RDWR);
    ASSERT_GE(fd, 0) << strerror(errno);
    constexpr char first[] = "proc-uts-name\nignored";
    write_all(fd, first, sizeof(first) - 1);
    EXPECT_EQ("proc-uts-name", hostname());
    EXPECT_EQ("proc-uts-name\n", read_file("/proc/sys/kernel/hostname"));

    constexpr char suffix[] = "XYZ\n";
    write_all(fd, suffix, sizeof(suffix) - 1, 5);
    EXPECT_EQ("proc-XYZ", hostname());

    constexpr char past_end[] = "ignored";
    write_all(fd, past_end, sizeof(past_end) - 1, 32);
    EXPECT_EQ("proc-XYZ", hostname());

    std::string oversized(80, 'a');
    write_all(fd, oversized.data(), oversized.size());
    EXPECT_EQ(64u, hostname().size());
    close(fd);

    int domain_fd = open("/proc/sys/kernel/domainname", O_RDWR);
    ASSERT_GE(domain_fd, 0) << strerror(errno);
    constexpr char new_domain[] = "uts.example\n";
    write_all(domain_fd, new_domain, sizeof(new_domain) - 1);
    EXPECT_EQ("uts.example", domainname());
    EXPECT_EQ("uts.example\n", read_file("/proc/sys/kernel/domainname"));
    close(domain_fd);

    ASSERT_EQ(0, setns(old_ns, CLONE_NEWUTS)) << strerror(errno);
    EXPECT_EQ(original, hostname());
    EXPECT_EQ(original_domain, domainname());
    close(old_ns);
}

TEST(UtsNamespace, SethostnameLengthBoundaries) {
    ASSERT_EQ(0, unshare(CLONE_NEWUTS)) << strerror(errno);
    std::string max_name(64, 'h');
    ASSERT_EQ(0, sethostname(max_name.data(), max_name.size())) << strerror(errno);
    EXPECT_EQ(max_name, hostname());

    std::string too_long(65, 'x');
    errno = 0;
    EXPECT_EQ(-1, sethostname(too_long.data(), too_long.size()));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(max_name, hostname());

    ASSERT_EQ(0, syscall(SYS_sethostname, nullptr, 0)) << strerror(errno);
    EXPECT_EQ("", hostname());
}

TEST(UtsNamespace, OperationsRequireSysAdmin) {
    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
        __user_cap_data_struct caps[2] = {};
        if (syscall(SYS_capget, &header, caps) != 0) _exit(10);
        caps[0].effective &= ~(1u << CAP_SYS_ADMIN);
        if (syscall(SYS_capset, &header, caps) != 0) _exit(11);

        errno = 0;
        if (sethostname("denied", 6) != -1 || errno != EPERM) _exit(12);
        errno = 0;
        if (unshare(CLONE_NEWUTS) != -1 || errno != EPERM) _exit(13);

        int fd = open("/proc/sys/kernel/hostname", O_WRONLY);
        if (fd < 0) _exit(14);
        errno = 0;
        ssize_t n = write(fd, "denied\n", 7);
        close(fd);
        if (n != -1 || errno != EPERM) _exit(15);
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
