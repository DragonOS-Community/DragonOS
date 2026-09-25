#include <gtest/gtest.h>

#include <fcntl.h>
#include <linux/keyctl.h>
#include <sched.h>
#include <sys/fsuid.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cerrno>
#include <cstdio>
#include <cstring>
#include <sstream>
#include <string>

#ifndef CLONE_NEWUSER
#define CLONE_NEWUSER 0x10000000
#endif

namespace {

constexpr int kProcessKeyring = -2;
constexpr unsigned kPossessorAllAndOtherView = 0x3f000001;

std::string read_file(const char* path, bool* ok = nullptr) {
    if (ok) *ok = false;
    int fd = open(path, O_RDONLY);
    if (fd < 0) return {};
    std::string result;
    char buffer[512];
    for (;;) {
        ssize_t count = read(fd, buffer, sizeof(buffer));
        if (count < 0 && errno == EINTR) continue;
        if (count < 0) {
            close(fd);
            return {};
        }
        if (count == 0) break;
        result.append(buffer, static_cast<size_t>(count));
    }
    close(fd);
    if (ok) *ok = true;
    return result;
}

std::string line_with(const std::string& text, const std::string& needle) {
    std::istringstream input(text);
    std::string line;
    while (std::getline(input, line)) {
        if (line.find(needle) != std::string::npos) return line;
    }
    return {};
}

struct Account {
    unsigned uid = 0;
    unsigned refs = 0;
    unsigned keys = 0;
    unsigned instantiated = 0;
    unsigned quota_keys = 0;
    unsigned maxkeys = 0;
    unsigned quota_bytes = 0;
    unsigned maxbytes = 0;
};

bool parse_account(const std::string& users, unsigned uid, Account* account) {
    std::istringstream input(users);
    std::string line;
    while (std::getline(input, line)) {
        Account parsed;
        if (std::sscanf(line.c_str(), "%u: %u %u/%u %u/%u %u/%u", &parsed.uid,
                        &parsed.refs, &parsed.keys, &parsed.instantiated,
                        &parsed.quota_keys, &parsed.maxkeys, &parsed.quota_bytes,
                        &parsed.maxbytes) == 8 && parsed.uid == uid) {
            *account = parsed;
            return true;
        }
    }
    return false;
}

bool find_account(unsigned uid, Account* account) {
    return parse_account(read_file("/proc/key-users"), uid, account);
}

int make_user_key(const std::string& name) {
    constexpr char payload[] = "procfs-test";
    return static_cast<int>(syscall(SYS_add_key, "user", name.c_str(), payload,
                                    sizeof(payload) - 1, kProcessKeyring));
}

long keyctl(int command, unsigned long arg2 = 0, unsigned long arg3 = 0,
            unsigned long arg4 = 0) {
    return syscall(SYS_keyctl, command, arg2, arg3, arg4, 0);
}

TEST(KeysProcfs, ReadOnlyFilesAndVisibleKeyFormat) {
    for (const char* path : {"/proc/keys", "/proc/key-users"}) {
        struct stat st = {};
        ASSERT_EQ(0, stat(path, &st)) << path << ": " << strerror(errno);
        EXPECT_TRUE(S_ISREG(st.st_mode));
        EXPECT_EQ(0444u, static_cast<unsigned>(st.st_mode & 0777));
    }

    const std::string name = "dunitest-proc-visible-" + std::to_string(getpid());
    const int serial = make_user_key(name);
    ASSERT_GT(serial, 0) << strerror(errno);
    const std::string line = line_with(read_file("/proc/keys"), name);
    ASSERT_FALSE(line.empty());
    unsigned shown_serial = 0, shown_perm = 0, uid = 0, gid = 0;
    unsigned refs = 0;
    char flags[8] = {}, timeout[32] = {}, type[32] = {};
    ASSERT_EQ(8, std::sscanf(line.c_str(), "%x %7s %u %31s %x %u %u %31s",
                             &shown_serial, flags, &refs, timeout, &shown_perm,
                             &uid, &gid, type));
    EXPECT_EQ(static_cast<unsigned>(serial), shown_serial);
    EXPECT_EQ(7u, std::strlen(flags));
    EXPECT_EQ('I', flags[0]);
    EXPECT_STREQ("perm", timeout);
    EXPECT_STREQ("user", type);
    EXPECT_EQ(static_cast<unsigned>(geteuid()), uid);
    EXPECT_NE(std::string::npos, line.find(": 11"));
}

TEST(KeysProcfs, QuotaAccountChangesWhenAddingKey) {
    ASSERT_GT(keyctl(KEYCTL_GET_KEYRING_ID, kProcessKeyring, 1), 0)
        << strerror(errno);
    Account before;
    ASSERT_TRUE(find_account(geteuid(), &before));
    const std::string name = "dunitest-proc-quota-" + std::to_string(getpid());
    ASSERT_GT(make_user_key(name), 0) << strerror(errno);
    Account after;
    ASSERT_TRUE(find_account(geteuid(), &after));
    EXPECT_EQ(before.keys + 1, after.keys);
    EXPECT_EQ(before.instantiated + 1, after.instantiated);
    EXPECT_EQ(before.quota_keys + 1, after.quota_keys);
    EXPECT_GT(after.quota_bytes, before.quota_bytes);
    EXPECT_GT(after.maxkeys, 0u);
    EXPECT_GT(after.maxbytes, 0u);
}

TEST(KeysProcfs, KeysWithoutViewPermissionStayHidden) {
    const std::string name = "dunitest-proc-hidden-" + std::to_string(getpid());
    const int serial = make_user_key(name);
    ASSERT_GT(serial, 0) << strerror(errno);
    ASSERT_FALSE(line_with(read_file("/proc/keys"), name).empty());
    ASSERT_EQ(0, keyctl(KEYCTL_SETPERM, serial, 0)) << strerror(errno);
    bool read_ok = false;
    const std::string keys = read_file("/proc/keys", &read_ok);
    ASSERT_TRUE(read_ok);
    EXPECT_TRUE(line_with(keys, name).empty());
}

TEST(KeysProcfs, ChildUserNamespaceOmitsUnmappedOwners) {
    ASSERT_EQ(0u, geteuid());
    const std::string name = "dunitest-proc-unmapped-" + std::to_string(getpid());
    const int serial = make_user_key(name);
    ASSERT_GT(serial, 0) << strerror(errno);
    ASSERT_EQ(0, keyctl(KEYCTL_SETPERM, serial, kPossessorAllAndOtherView))
        << strerror(errno);
    ASSERT_EQ(0, keyctl(KEYCTL_CHOWN, serial, 1000, -1UL)) << strerror(errno);
    EXPECT_FALSE(line_with(read_file("/proc/keys"), name).empty());
    Account host_account;
    ASSERT_TRUE(find_account(1000, &host_account));

    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (unshare(CLONE_NEWUSER) != 0) _exit(10);
        const int fd = open("/proc/self/uid_map", O_WRONLY);
        if (fd < 0) _exit(11);
        constexpr char map[] = "0 0 1\n";
        if (write(fd, map, sizeof(map) - 1) != sizeof(map) - 1) _exit(12);
        close(fd);
        bool read_ok = false;
        const std::string keys = read_file("/proc/keys", &read_ok);
        if (!read_ok) _exit(13);
        if (!line_with(keys, name).empty()) _exit(14);
        const std::string users = read_file("/proc/key-users", &read_ok);
        if (!read_ok) _exit(15);
        Account hidden;
        if (parse_account(users, 1000, &hidden)) _exit(16);
        _exit(0);
    }
    int status = 0;
    pid_t waited = -1;
    do {
        waited = waitpid(child, &status, 0);
    } while (waited < 0 && errno == EINTR);
    ASSERT_EQ(child, waited) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(KeysProcfs, ThreadRingOwnerCanDifferFromQuotaOwner) {
    ASSERT_EQ(0u, geteuid());
    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        const long serial = keyctl(KEYCTL_GET_KEYRING_ID, -1UL, 1);
        if (serial <= 0) _exit(10);
        if (setfsuid(1000) != 0) _exit(11);
        char serial_prefix[16];
        std::snprintf(serial_prefix, sizeof(serial_prefix), "%08x ",
                      static_cast<unsigned>(serial));
        const std::string line = line_with(read_file("/proc/keys"), serial_prefix);
        if (line.empty()) _exit(12);
        unsigned shown_serial = 0, perm = 0, uid = 0, gid = 0, refs = 0;
        char flags[8] = {}, timeout[32] = {}, type[32] = {};
        if (std::sscanf(line.c_str(), "%x %7s %u %31s %x %u %u %31s",
                        &shown_serial, flags, &refs, timeout, &perm, &uid, &gid,
                        type) != 8 || uid != 1000 ||
            shown_serial != static_cast<unsigned>(serial)) {
            _exit(13);
        }
        _exit(0);
    }
    int status = 0;
    pid_t waited = -1;
    do {
        waited = waitpid(child, &status, 0);
    } while (waited < 0 && errno == EINTR);
    ASSERT_EQ(child, waited) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

} // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
