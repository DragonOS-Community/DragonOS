#include <gtest/gtest.h>

#include <linux/keyctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cerrno>
#include <cstdlib>
#include <cstring>
#include <string>

namespace {

constexpr unsigned long kPossessorSearch = 0x08000000UL;

std::string self_path;

long Keyctl(int command, unsigned long arg2 = 0, unsigned long arg3 = 0,
            unsigned long arg4 = 0, unsigned long arg5 = 0) {
    return syscall(SYS_keyctl, command, arg2, arg3, arg4, arg5);
}

long AddKey(const char* type, const char* description, const void* payload,
            size_t length, int ring) {
    return syscall(SYS_add_key, type, description, payload, length, ring);
}

long RequestKey(const char* type, const char* description, const char* callout,
                int destination) {
    return syscall(SYS_request_key, type, description, callout, destination);
}

unsigned long Arg(int value) {
    return static_cast<unsigned long>(static_cast<long>(value));
}

unsigned long Ptr(const void* value) {
    return reinterpret_cast<unsigned long>(value);
}

int WaitForChild(pid_t child) {
    int status = 0;
    pid_t waited;
    do {
        waited = waitpid(child, &status, 0);
    } while (waited < 0 && errno == EINTR);
    if (waited != child || !WIFEXITED(status)) return -1;
    return WEXITSTATUS(status);
}

class KeysCoreTest : public ::testing::Test {
protected:
    int session_ = -1;

    void SetUp() override {
        const long result = Keyctl(KEYCTL_JOIN_SESSION_KEYRING);
        ASSERT_GT(result, 0) << "join anonymous session keyring: " << strerror(errno);
        session_ = static_cast<int>(result);
    }

    long AddUser(const char* description, const char* data) {
        return AddKey("user", description, data, strlen(data), session_);
    }
};

TEST_F(KeysCoreTest, UserAddReadUpdateAndSearch) {
    const long key = AddUser("dunitest-user", "first");
    ASSERT_GT(key, 0) << strerror(errno);
    EXPECT_EQ(key, Keyctl(KEYCTL_SEARCH, Arg(session_), Ptr("user"),
                          Ptr("dunitest-user"), 0));
    EXPECT_EQ(key, RequestKey("user", "dunitest-user", nullptr, 0));

    char data[16] = {};
    EXPECT_EQ(5, Keyctl(KEYCTL_READ, Arg(key), Ptr(data), sizeof(data)));
    EXPECT_EQ(std::string("first"), std::string(data, 5));

    const char replacement[] = {'n', 'e', 'w', '\0', 'x'};
    ASSERT_EQ(0, Keyctl(KEYCTL_UPDATE, Arg(key), Ptr(replacement),
                        sizeof(replacement)));
    std::memset(data, 0, sizeof(data));
    EXPECT_EQ(static_cast<long>(sizeof(replacement)),
              Keyctl(KEYCTL_READ, Arg(key), Ptr(data), sizeof(data)));
    EXPECT_EQ(0, std::memcmp(data, replacement, sizeof(replacement)));
}

TEST_F(KeysCoreTest, DuplicateAddUpdatesExistingKey) {
    const long first = AddUser("dunitest-same", "one");
    ASSERT_GT(first, 0) << strerror(errno);
    const long second = AddUser("dunitest-same", "two");
    ASSERT_GT(second, 0) << strerror(errno);
    EXPECT_EQ(first, second);

    char data[8] = {};
    EXPECT_EQ(3, Keyctl(KEYCTL_READ, Arg(first), Ptr(data), sizeof(data)));
    EXPECT_EQ(0, std::memcmp(data, "two", 3));
}

TEST_F(KeysCoreTest, ShortReadReportsLengthWithoutCopying) {
    const long key = AddUser("dunitest-short", "abcdef");
    ASSERT_GT(key, 0) << strerror(errno);

    char data[3] = {'x', 'x', 'x'};
    EXPECT_EQ(6, Keyctl(KEYCTL_READ, Arg(key), Ptr(data), sizeof(data)));
    EXPECT_EQ(std::string("xxx"), std::string(data, sizeof(data)));
    EXPECT_EQ(6, Keyctl(KEYCTL_READ, Arg(key), 0, 0));
}

TEST_F(KeysCoreTest, PossessedKeyCanBeReadWithoutReadPermission) {
    const long key = AddUser("dunitest-possessed-read", "value");
    ASSERT_GT(key, 0) << strerror(errno);
    ASSERT_EQ(0, Keyctl(KEYCTL_SETPERM, Arg(key), kPossessorSearch));

    char data[8] = {};
    EXPECT_EQ(5, Keyctl(KEYCTL_READ, Arg(key), Ptr(data), sizeof(data)));
    EXPECT_EQ(std::string("value"), std::string(data, 5));
}

TEST_F(KeysCoreTest, LogonKeyIsSearchableButNotReadable) {
    const char payload[] = "secret";
    errno = 0;
    EXPECT_EQ(-1, AddKey("logon", "missing-prefix", payload, sizeof(payload) - 1,
                         session_));
    EXPECT_EQ(EINVAL, errno);

    const long key = AddKey("logon", "test:credential", payload,
                            sizeof(payload) - 1, session_);
    ASSERT_GT(key, 0) << strerror(errno);
    EXPECT_EQ(key, Keyctl(KEYCTL_SEARCH, Arg(session_), Ptr("logon"),
                          Ptr("test:credential"), 0));

    char data[16] = {};
    errno = 0;
    EXPECT_EQ(-1, Keyctl(KEYCTL_READ, Arg(key), Ptr(data), sizeof(data)));
    EXPECT_EQ(EOPNOTSUPP, errno);
}

TEST_F(KeysCoreTest, NestedRingLinkSearchAndUnlink) {
    const long child = AddKey("keyring", "dunitest-child", nullptr, 0, session_);
    ASSERT_GT(child, 0) << strerror(errno);
    const long key = AddUser("dunitest-linked", "value");
    ASSERT_GT(key, 0) << strerror(errno);

    ASSERT_EQ(0, Keyctl(KEYCTL_LINK, Arg(key), Arg(child)));
    ASSERT_EQ(0, Keyctl(KEYCTL_UNLINK, Arg(key), Arg(session_)));
    EXPECT_EQ(key, Keyctl(KEYCTL_SEARCH, Arg(session_), Ptr("user"),
                          Ptr("dunitest-linked"), 0));
    ASSERT_EQ(0, Keyctl(KEYCTL_UNLINK, Arg(key), Arg(child)));
    errno = 0;
    EXPECT_EQ(-1, Keyctl(KEYCTL_SEARCH, Arg(session_), Ptr("user"),
                          Ptr("dunitest-linked"), 0));
    EXPECT_EQ(ENOKEY, errno);
}

TEST_F(KeysCoreTest, PermissionMaskControlsRingSearch) {
    const long key = AddUser("dunitest-permission", "value");
    ASSERT_GT(key, 0) << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, Keyctl(KEYCTL_SETPERM, Arg(session_), 0x80000000UL));
    EXPECT_EQ(EINVAL, errno);

    ASSERT_EQ(0, Keyctl(KEYCTL_SETPERM, Arg(session_), 0));
    errno = 0;
    EXPECT_EQ(-1, Keyctl(KEYCTL_SEARCH, Arg(session_), Ptr("user"),
                          Ptr("dunitest-permission"), 0));
    EXPECT_EQ(EACCES, errno);
}

TEST_F(KeysCoreTest, RevocationAndInvalidationAreVisible) {
    const long revoked = AddUser("dunitest-revoked", "value");
    ASSERT_GT(revoked, 0) << strerror(errno);
    ASSERT_EQ(0, Keyctl(KEYCTL_REVOKE, Arg(revoked)));
    char data[8] = {};
    errno = 0;
    EXPECT_EQ(-1, Keyctl(KEYCTL_READ, Arg(revoked), Ptr(data), sizeof(data)));
    EXPECT_EQ(EKEYREVOKED, errno);

    const long invalid = AddUser("dunitest-invalid", "value");
    ASSERT_GT(invalid, 0) << strerror(errno);
    ASSERT_EQ(0, Keyctl(KEYCTL_INVALIDATE, Arg(invalid)));
    errno = 0;
    EXPECT_EQ(-1, Keyctl(KEYCTL_SEARCH, Arg(session_), Ptr("user"),
                          Ptr("dunitest-invalid"), 0));
    EXPECT_EQ(ENOKEY, errno);
}

TEST_F(KeysCoreTest, MissingRequestAndSearchReturnNoKey) {
    errno = 0;
    EXPECT_EQ(-1, RequestKey("user", "dunitest-absent", nullptr, 0));
    EXPECT_EQ(ENOKEY, errno);
    errno = 0;
    EXPECT_EQ(-1, Keyctl(KEYCTL_SEARCH, Arg(session_), Ptr("user"),
                          Ptr("dunitest-absent"), 0));
    EXPECT_EQ(ENOKEY, errno);
}

TEST_F(KeysCoreTest, ForkKeepsSessionButStartsWithoutThreadAndProcessRings) {
    const long thread = Keyctl(KEYCTL_GET_KEYRING_ID, Arg(KEY_SPEC_THREAD_KEYRING), 1);
    ASSERT_GT(thread, 0) << strerror(errno);
    const long process = Keyctl(KEYCTL_GET_KEYRING_ID, Arg(KEY_SPEC_PROCESS_KEYRING), 1);
    ASSERT_GT(process, 0) << strerror(errno);
    ASSERT_GT(AddUser("dunitest-fork", "inherited"), 0) << strerror(errno);

    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (Keyctl(KEYCTL_GET_KEYRING_ID, Arg(KEY_SPEC_SESSION_KEYRING)) != session_)
            _exit(10);
        errno = 0;
        if (Keyctl(KEYCTL_GET_KEYRING_ID, Arg(KEY_SPEC_THREAD_KEYRING)) != -1 ||
            errno != ENOKEY)
            _exit(11);
        errno = 0;
        if (Keyctl(KEYCTL_GET_KEYRING_ID, Arg(KEY_SPEC_PROCESS_KEYRING)) != -1 ||
            errno != ENOKEY)
            _exit(12);
        if (RequestKey("user", "dunitest-fork", nullptr, 0) <= 0) _exit(13);
        _exit(0);
    }
    EXPECT_EQ(0, WaitForChild(child));
    EXPECT_EQ(thread, Keyctl(KEYCTL_GET_KEYRING_ID, Arg(KEY_SPEC_THREAD_KEYRING)));
    EXPECT_EQ(process, Keyctl(KEYCTL_GET_KEYRING_ID, Arg(KEY_SPEC_PROCESS_KEYRING)));
}

TEST_F(KeysCoreTest, ExecDropsThreadProcessButKeepsSessionRing) {
    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        const long thread = Keyctl(KEYCTL_GET_KEYRING_ID,
                                   Arg(KEY_SPEC_THREAD_KEYRING), 1);
        const long process = Keyctl(KEYCTL_GET_KEYRING_ID,
                                    Arg(KEY_SPEC_PROCESS_KEYRING), 1);
        if (thread <= 0 || process <= 0) _exit(10);
        const std::string session_arg = std::to_string(session_);
        execl(self_path.c_str(), self_path.c_str(), "--keyring-exec-probe",
              session_arg.c_str(),
              static_cast<char*>(nullptr));
        _exit(11);
    }
    EXPECT_EQ(0, WaitForChild(child));
}

TEST_F(KeysCoreTest, ChildCanTransferSessionToParentAtUserReturn) {
    int pipe_fd[2] = {-1, -1};
    ASSERT_EQ(0, pipe(pipe_fd)) << strerror(errno);
    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        close(pipe_fd[0]);
        const long next = Keyctl(KEYCTL_JOIN_SESSION_KEYRING);
        if (next <= 0 || next == session_) _exit(10);
        if (Keyctl(KEYCTL_SESSION_TO_PARENT) != 0) _exit(11);
        const int serial = static_cast<int>(next);
        const ssize_t written = write(pipe_fd[1], &serial, sizeof(serial));
        _exit(written == sizeof(serial) ? 0 : 12);
    }
    close(pipe_fd[1]);
    int new_session = -1;
    ssize_t received;
    do {
        received = read(pipe_fd[0], &new_session, sizeof(new_session));
    } while (received < 0 && errno == EINTR);
    close(pipe_fd[0]);
    EXPECT_EQ(0, WaitForChild(child));
    ASSERT_EQ(static_cast<ssize_t>(sizeof(new_session)), received);
    EXPECT_NE(session_, new_session);
    EXPECT_EQ(new_session, Keyctl(KEYCTL_GET_KEYRING_ID,
                                  Arg(KEY_SPEC_SESSION_KEYRING)));
}

TEST_F(KeysCoreTest, RejoinedNamedSessionRetainsPossessorRights) {
    constexpr char name[] = "dunitest-named-session-possession";
    const long named = Keyctl(KEYCTL_JOIN_SESSION_KEYRING, Ptr(name));
    ASSERT_GT(named, 0) << strerror(errno);
    ASSERT_EQ(0, Keyctl(KEYCTL_SETPERM, Arg(KEY_SPEC_SESSION_KEYRING),
                        0x3f1b0000UL));  // Permit a non-possessor to find it.

    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (Keyctl(KEYCTL_JOIN_SESSION_KEYRING) <= 0) _exit(10);
        if (Keyctl(KEYCTL_JOIN_SESSION_KEYRING, Ptr(name)) != named) _exit(11);
        // The named ring grants SETATTR to a possessor, not to its owner.
        if (Keyctl(KEYCTL_SETPERM, Arg(KEY_SPEC_SESSION_KEYRING),
                   0x3f1b1000UL) != 0)
            _exit(12);
        _exit(0);
    }
    EXPECT_EQ(0, WaitForChild(child));
}

TEST_F(KeysCoreTest, NamedSessionLookupKeepsDistinctLiveCandidates) {
    constexpr char name[] = "dunitest-named-session-collision";
    constexpr unsigned long owner_setattr = 0x00200000UL;
    constexpr unsigned long owner_search = 0x00080000UL;
    constexpr unsigned long possessor_all = 0x3f000000UL;
    const long first = Keyctl(KEYCTL_JOIN_SESSION_KEYRING, Ptr(name));
    ASSERT_GT(first, 0) << strerror(errno);
    ASSERT_EQ(0, Keyctl(KEYCTL_SETPERM, Arg(first),
                        possessor_all | owner_setattr));

    // Keep the first ring alive after leaving it, but do not make it
    // searchable by a non-possessing process yet.
    const long user = Keyctl(KEYCTL_GET_KEYRING_ID,
                             Arg(KEY_SPEC_USER_KEYRING), 1);
    ASSERT_GT(user, 0) << strerror(errno);
    ASSERT_EQ(0, Keyctl(KEYCTL_LINK, Arg(first), Arg(user)));
    ASSERT_GT(Keyctl(KEYCTL_JOIN_SESSION_KEYRING), 0);

    int ready[2] = {-1, -1};
    int release[2] = {-1, -1};
    ASSERT_EQ(0, pipe(ready));
    ASSERT_EQ(0, pipe(release));
    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        close(ready[0]);
        close(release[1]);
        const long second = Keyctl(KEYCTL_JOIN_SESSION_KEYRING, Ptr(name));
        if (second <= 0 || second == first) _exit(10);
        const int serial = static_cast<int>(second);
        if (write(ready[1], &serial, sizeof(serial)) != sizeof(serial)) _exit(11);
        char done;
        if (read(release[0], &done, 1) != 1) _exit(12);
        _exit(0);
    }
    close(ready[1]);
    close(release[0]);
    int second = -1;
    const ssize_t got = read(ready[0], &second, sizeof(second));
    close(ready[0]);
    EXPECT_EQ(static_cast<ssize_t>(sizeof(second)), got);
    EXPECT_NE(first, second);

    // The owner may now search the first candidate.  The second candidate
    // remains alive and cannot replace the first entry in the name index.
    EXPECT_EQ(0, Keyctl(KEYCTL_SETPERM, Arg(first),
                        possessor_all | owner_setattr | owner_search));
    EXPECT_EQ(first, Keyctl(KEYCTL_JOIN_SESSION_KEYRING, Ptr(name)));
    const char done = 'x';
    EXPECT_EQ(1, write(release[1], &done, 1));
    close(release[1]);
    EXPECT_EQ(0, WaitForChild(child));
}

TEST_F(KeysCoreTest, AddKeyKeyringIsPublishedForNamedSessionLookup) {
    constexpr char name[] = "dunitest-add-keyring-name";
    const long created = AddKey("keyring", name, nullptr, 0, session_);
    ASSERT_GT(created, 0) << strerror(errno);
    ASSERT_EQ(0, Keyctl(KEYCTL_SETPERM, Arg(created),
                        0x3f280000UL));  // Possessor all, owner SEARCH/SETATTR.
    EXPECT_EQ(created, Keyctl(KEYCTL_JOIN_SESSION_KEYRING, Ptr(name)));
}

TEST_F(KeysCoreTest, UserKeyringIsPublishedForNamedSessionLookup) {
    const long user = Keyctl(KEYCTL_GET_KEYRING_ID,
                             Arg(KEY_SPEC_USER_KEYRING), 1);
    ASSERT_GT(user, 0) << strerror(errno);
    const std::string name = "_uid." + std::to_string(getuid());
    EXPECT_EQ(user, Keyctl(KEYCTL_JOIN_SESSION_KEYRING, Ptr(name.c_str())));
}

}  // namespace

int main(int argc, char** argv) {
    if (argc == 3 && strcmp(argv[1], "--keyring-exec-probe") == 0) {
        const long session = strtol(argv[2], nullptr, 10);
        errno = 0;
        if (Keyctl(KEYCTL_GET_KEYRING_ID, Arg(KEY_SPEC_THREAD_KEYRING)) != -1 ||
            errno != ENOKEY)
            return 21;
        errno = 0;
        if (Keyctl(KEYCTL_GET_KEYRING_ID, Arg(KEY_SPEC_PROCESS_KEYRING)) != -1 ||
            errno != ENOKEY)
            return 22;
        if (Keyctl(KEYCTL_GET_KEYRING_ID, Arg(KEY_SPEC_SESSION_KEYRING)) != session)
            return 23;
        return 0;
    }
    self_path = argv[0];
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
