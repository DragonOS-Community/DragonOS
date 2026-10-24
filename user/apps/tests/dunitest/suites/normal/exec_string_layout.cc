#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>
#include <vector>

namespace {

constexpr char kPrefix[] = "dragonos-exec-layout-";
constexpr char kEmptyArgvEnv[] = "DRAGONOS_EXEC_LAYOUT_EMPTY_ARGV=1";
constexpr char kScriptMode[] = "--exec-layout-script";
constexpr char kLongArgZeroMode[] = "--exec-layout-long-arg-zero";
constexpr char kBinaryArgZeroMode[] = "--exec-layout-binary-arg-zero";
std::string self_path;

struct Input {
    std::vector<std::string> args;
    std::vector<std::string> env;
};

Input input_for(const std::string& mode, const std::string& script = {}) {
    Input input{{std::string(kPrefix) + mode}, {}};
    if (mode == "multiple") {
        input.args.insert(input.args.end(), {"alpha", "two words", "omega"});
        input.env = {"FIRST=one", "SECOND=two words", "LAST=three"};
    } else if (mode == "empty-strings") {
        input.args.insert(input.args.end(), {"", "middle", ""});
        input.env = {"", "VALUE=", ""};
    } else if (mode == "empty-env") {
        input.args.insert(input.args.end(), {"first", "", "last"});
    } else if (mode == "large") {
        const size_t page = static_cast<size_t>(getpagesize());
        input.args.push_back(std::string(page * 2 + 37, 'a'));
        input.env.push_back("LARGE=" + std::string(page * 2 + 19, 'e'));
        for (int i = 0; i < 32; ++i) {
            input.args.push_back("arg-" + std::to_string(i));
            input.env.push_back("ENV_" + std::to_string(i) + "=value");
        }
    } else if (mode == "many-entries") {
        // Each kernel pointer array exceeds two pages on 64-bit guests.
        for (int i = 0; i < 1537; ++i) {
            input.args.push_back("arg-" + std::to_string(i));
            input.env.push_back("ENV_" + std::to_string(i) + "=value");
        }
    } else if (mode == "max-string") {
        // Linux MAX_ARG_STRLEN includes the terminating NUL.
        input.args.push_back(std::string(32 * static_cast<size_t>(getpagesize()) - 1, 'm'));
    } else if (mode == "many-empty") {
        // Exceeds a single page-sized directory of CString-sized entries.
        input.args.insert(input.args.end(), 60000, "");
        input.env.insert(input.env.end(), 30000, "");
    } else if (mode == "binary") {
        input.args.push_back(std::string("arg-\xff\x80", 6));
        input.env.push_back(std::string("KEY=\xfe\x81", 6));
    } else if (mode == "long-arg-zero") {
        // argv[0] is opaque data, not a pathname. Exercise the entire legal
        // Linux string length without asking path resolution to copy it.
        input.args = {std::string(32 * static_cast<size_t>(getpagesize()) - 1, 'z'),
                      kLongArgZeroMode, "payload"};
    } else if (mode == "binary-arg-zero") {
        input.args = {std::string("argzero-\xff\x80", 10), kBinaryArgZeroMode, "payload"};
    } else if (mode == "page-edge") {
        input.args.push_back("end");
    } else if (mode == "proc-snapshot") {
        input.args.push_back(std::string(3 * static_cast<size_t>(getpagesize()) + 37, 'p'));
        input.args.insert(input.args.end(), {"", "last"});
    } else if (mode == "small-stack-fit") {
        input.args.insert(input.args.end(), 28, std::string(4095, 's'));
    } else if (mode == "empty-argv") {
        // Linux normalizes argv={NULL} to argc=1, argv[0]="".
        input.args = {""};
        input.env = {kEmptyArgvEnv};
    } else if (mode == "script") {
        input.args = {self_path, kScriptMode, script, "payload", ""};
        input.env = {"SCRIPT_PATH=" + script, "VALUE=script"};
    }
    return input;
}

bool check_group(char** actual, const std::vector<std::string>& expected,
                 const char* name, size_t* bytes) {
    *bytes = 0;
    for (size_t i = 0; i < expected.size(); ++i) {
        if (actual[i] == nullptr || expected[i] != actual[i]) {
            dprintf(STDERR_FILENO, "%s[%zu]: content mismatch\n", name, i);
            return false;
        }
        const size_t length = expected[i].size() + 1;
        const uintptr_t start = reinterpret_cast<uintptr_t>(actual[i]);
        if (start > UINTPTR_MAX - length) {
            return false;
        }
        if (i + 1 < expected.size() &&
            reinterpret_cast<uintptr_t>(actual[i + 1]) != start + length) {
            dprintf(STDERR_FILENO, "%s[%zu]: strings not contiguous in index order\n",
                    name, i);
            return false;
        }
        *bytes += length;
    }
    if (actual[expected.size()] != nullptr) {
        dprintf(STDERR_FILENO, "%s: missing NULL terminator\n", name);
        return false;
    }
    return true;
}

int check_layout(int argc, char** argv, char** envp, const Input& expected,
                 const std::string& exec_path) {
    if (argc != static_cast<int>(expected.args.size())) {
        dprintf(STDERR_FILENO, "argc: got %d, expected %zu\n", argc, expected.args.size());
        return 1;
    }
    size_t arg_bytes = 0;
    size_t env_bytes = 0;
    if (!check_group(argv, expected.args, "argv", &arg_bytes) ||
        !check_group(envp, expected.env, "envp", &env_bytes)) {
        return 2;
    }
    const uintptr_t begin = reinterpret_cast<uintptr_t>(argv[0]);
    const uintptr_t last = reinterpret_cast<uintptr_t>(argv[argc - 1]);
    const uintptr_t end = last + expected.args.back().size() + 1;
    // Never subtract unrelated C++ pointers or clear an unvalidated span.
    if (end < begin || end - begin != arg_bytes ||
        (!expected.env.empty() && reinterpret_cast<uintptr_t>(envp[0]) != end)) {
        dprintf(STDERR_FILENO, "argv span or argv/env boundary mismatch\n");
        return 3;
    }

    const auto* random = reinterpret_cast<const unsigned char*>(getauxval(AT_RANDOM));
    const auto* execfn = reinterpret_cast<const char*>(getauxval(AT_EXECFN));
    if (random == nullptr || execfn == nullptr || exec_path != execfn) {
        dprintf(STDERR_FILENO, "AT_RANDOM missing or AT_EXECFN mismatch\n");
        return 4;
    }
    unsigned char saved_random[16];
    memcpy(saved_random, random, sizeof(saved_random));
    const std::string saved_execfn(execfn);

    // Model the argv-only title area used by libuv, preserving environment data.
    memset(argv[0], 0, arg_bytes);
    if (arg_bytes > 1) {
        argv[0][0] = 't';
    }
    if (!check_group(envp, expected.env, "envp after title write", &env_bytes) ||
        memcmp(saved_random, random, sizeof(saved_random)) != 0 || saved_execfn != execfn) {
        dprintf(STDERR_FILENO, "title write corrupted environment or auxiliary data\n");
        return 5;
    }
    return 0;
}

// Read in small pieces and then seek across a page boundary. This must not
// depend on a kernel-side contiguous copy of the entire command line.
int check_proc_cmdline(const Input& input) {
    std::string expected;
    for (const auto& arg : input.args) {
        expected.append(arg);
        expected.push_back('\0');
    }
    const int fd = open("/proc/self/cmdline", O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return 10;
    }
    char bytes[137];
    size_t offset = 0;
    int result = 0;
    for (;;) {
        const ssize_t n = read(fd, bytes, sizeof(bytes));
        if (n < 0 && errno == EINTR) {
            continue;
        }
        if (n < 0 || static_cast<size_t>(n) > expected.size() - offset ||
            (n > 0 && memcmp(bytes, expected.data() + offset, n) != 0)) {
            result = 11;
            break;
        }
        if (n == 0) {
            if (offset != expected.size()) {
                result = 12;
            }
            break;
        }
        offset += static_cast<size_t>(n);
    }
    const off_t boundary = static_cast<off_t>(getpagesize()) - 3;
    if (result == 0 && lseek(fd, boundary, SEEK_SET) != boundary) {
        result = 13;
    }
    if (result == 0) {
        const ssize_t n = read(fd, bytes, sizeof(bytes));
        if (n != static_cast<ssize_t>(sizeof(bytes)) ||
            memcmp(bytes, expected.data() + boundary, sizeof(bytes)) != 0) {
            result = 14;
        }
    }
    close(fd);
    return result;
}

int check_proc_snapshot_and_fork(const Input& input) {
    const int result = check_proc_cmdline(input);
    if (result != 0) {
        return result;
    }
    const pid_t child = fork();
    if (child < 0) {
        return 15;
    }
    if (child == 0) {
        _exit(check_proc_cmdline(input));
    }
    int status = 0;
    pid_t waited;
    do {
        waited = waitpid(child, &status, 0);
    } while (waited < 0 && errno == EINTR);
    return waited == child && WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : 16;
}

std::vector<char*> pointers(std::vector<std::string>& strings) {
    std::vector<char*> result;
    for (auto& value : strings) {
        result.push_back(value.data());
    }
    result.push_back(nullptr);
    return result;
}

void run_case(const std::string& mode, const std::string& script = {}) {
    Input input = input_for(mode, script);
    if (mode == "empty-argv") {
        input.args.clear();
    } else if (mode == "script") {
        input.args = {"ignored-script-argv0", "payload", ""};
    }
    auto argv = pointers(input.args);
    auto envp = pointers(input.env);
    const std::string path = script.empty() ? self_path : script;
    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        alarm(30);
        if (mode == "small-stack-fit") {
            struct rlimit limit;
            if (getrlimit(RLIMIT_STACK, &limit) != 0) {
                _exit(120);
            }
            limit.rlim_cur = 512 * 1024;
            if (setrlimit(RLIMIT_STACK, &limit) != 0) {
                _exit(121);
            }
        }
        if (mode == "page-edge") {
            const size_t page = static_cast<size_t>(getpagesize());
            void* mapping = mmap(nullptr, 2 * page, PROT_READ | PROT_WRITE,
                                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            if (mapping == MAP_FAILED ||
                mprotect(static_cast<char*>(mapping) + page, page, PROT_NONE) != 0) {
                _exit(122);
            }
            char* ending = static_cast<char*>(mapping) + page - 4;
            memcpy(ending, "end", 4);
            argv[1] = ending;
        }
        execve(path.c_str(), argv.data(), envp.data());
        dprintf(STDERR_FILENO, "execve failed: %s\n", strerror(errno));
        _exit(127);
    }
    int status = 0;
    pid_t waited;
    do {
        waited = waitpid(child, &status, 0);
    } while (waited < 0 && errno == EINTR);
    ASSERT_EQ(child, waited) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status)) << "child status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status));
}

// A rejected call must return to the old image, which then performs a valid
// exec. Unexpected success enters an intentionally mismatched helper case and
// fails deterministically rather than constructing an unbounded argument list.
void expect_e2big_and_retry(size_t strings, size_t string_bytes, rlim_t soft_stack) {
    std::vector<std::string> rejected(strings, std::string(string_bytes, 'x'));
    rejected.insert(rejected.begin(), std::string(kPrefix) + "unexpected-rejected-exec");
    auto rejected_argv = pointers(rejected);
    char* empty_env[] = {nullptr};
    Input valid = input_for("single");
    auto valid_argv = pointers(valid.args);
    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        alarm(30);
        if (soft_stack != 0) {
            struct rlimit limit;
            if (getrlimit(RLIMIT_STACK, &limit) != 0) {
                _exit(120);
            }
            limit.rlim_cur = soft_stack;
            if (setrlimit(RLIMIT_STACK, &limit) != 0) {
                _exit(121);
            }
        }
        execve(self_path.c_str(), rejected_argv.data(), empty_env);
        if (errno != E2BIG) {
            dprintf(STDERR_FILENO, "rejected exec errno=%d, expected E2BIG\n", errno);
            _exit(123);
        }
        execve(self_path.c_str(), valid_argv.data(), empty_env);
        _exit(124);
    }
    int status = 0;
    pid_t waited;
    do {
        waited = waitpid(child, &status, 0);
    } while (waited < 0 && errno == EINTR);
    ASSERT_EQ(child, waited) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status)) << "child status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status));
}

class TemporaryScript {
 public:
    char path[64] = "/tmp/exec-string-layout-XXXXXX";
    int fd = -1;
    ~TemporaryScript() {
        if (fd >= 0) {
            close(fd);
        }
        if (created) {
            unlink(path);
        }
    }
    bool created = false;
};

}  // namespace

TEST(ExecStringLayout, MultipleArgumentsAndEnvironment) { run_case("multiple"); }
TEST(ExecStringLayout, EmptyStrings) { run_case("empty-strings"); }
TEST(ExecStringLayout, EmptyEnvironment) { run_case("empty-env"); }
TEST(ExecStringLayout, SingleArgument) { run_case("single"); }
TEST(ExecStringLayout, CrossPageStringsAndManyEntries) { run_case("large"); }
TEST(ExecStringLayout, ManyArgumentAndEnvironmentPointers) { run_case("many-entries"); }
TEST(ExecStringLayout, EmptyArgumentVector) { run_case("empty-argv"); }
TEST(ExecStringLayout, LargestLinuxArgumentString) { run_case("max-string"); }
TEST(ExecStringLayout, ManyEmptyArgumentsAndEnvironmentEntries) { run_case("many-empty"); }
TEST(ExecStringLayout, NonUtf8ArgumentsAndEnvironment) { run_case("binary"); }
TEST(ExecStringLayout, LargestLinuxArgZeroIsOpaqueData) { run_case("long-arg-zero"); }
TEST(ExecStringLayout, NonUtf8ArgZeroIsOpaqueData) { run_case("binary-arg-zero"); }
TEST(ExecStringLayout, TerminatingNulAtProtectedPageBoundary) { run_case("page-edge"); }
TEST(ExecStringLayout, ProcCmdlinePartialReadsAndForkSnapshot) { run_case("proc-snapshot"); }
TEST(ExecStringLayout, SmallStackAcceptsArgumentsBelowBudget) { run_case("small-stack-fit"); }

TEST(ExecStringLayout, OverlongArgumentReturnsE2bigAndCanRetry) {
    expect_e2big_and_retry(1, 32 * static_cast<size_t>(getpagesize()), 0);
}

TEST(ExecStringLayout, SmallStackEnforcesTotalArgumentBudget) {
    expect_e2big_and_retry(40, 4095, 512 * 1024);
}

TEST(ExecStringLayout, SmallStackChargesArgumentPointers) {
    // Every empty string consumes one byte plus one pointer on the new stack.
    expect_e2big_and_retry(16000, 0, 512 * 1024);
}


TEST(ExecStringLayout, ShebangInterpreterArguments) {
    ASSERT_TRUE(mkdir("/tmp", 0755) == 0 || errno == EEXIST);
    TemporaryScript script;
    script.fd = mkstemp(script.path);
    ASSERT_GE(script.fd, 0) << strerror(errno);
    script.created = true;
    const std::string content = "#!" + self_path + " " + kScriptMode + "\n";
    size_t written = 0;
    while (written < content.size()) {
        const ssize_t n = write(script.fd, content.data() + written, content.size() - written);
        if (n < 0 && errno == EINTR) {
            continue;
        }
        ASSERT_GT(n, 0) << strerror(errno);
        written += static_cast<size_t>(n);
    }
    ASSERT_EQ(0, fchmod(script.fd, 0755));
    const int fd = script.fd;
    script.fd = -1;
    ASSERT_EQ(0, close(fd));
    run_case("script", script.path);
}

int main(int argc, char** argv, char** envp) {
    char path[PATH_MAX];
    const ssize_t length = readlink("/proc/self/exe", path, sizeof(path) - 1);
    if (length <= 0 || length >= static_cast<ssize_t>(sizeof(path) - 1)) {
        return 100;
    }
    path[length] = '\0';
    self_path = path;

    // Dispatch before GoogleTest can modify argc/argv; also supports argc=1
    // with an empty environment and Linux's synthesized empty argv[0].
    std::string mode;
    std::string script;
    if (argc > 0 && strncmp(argv[0], kPrefix, sizeof(kPrefix) - 1) == 0) {
        mode = argv[0] + sizeof(kPrefix) - 1;
    } else if (argc > 1 && strcmp(argv[1], kLongArgZeroMode) == 0) {
        mode = "long-arg-zero";
    } else if (argc > 1 && strcmp(argv[1], kBinaryArgZeroMode) == 0) {
        mode = "binary-arg-zero";
    } else if (argc > 1 && strcmp(argv[1], kScriptMode) == 0) {
        mode = "script";
        for (size_t i = 0; envp[i] != nullptr; ++i) {
            if (strncmp(envp[i], "SCRIPT_PATH=", 12) == 0) {
                script = envp[i] + 12;
            }
        }
        if (script.empty()) {
            return 101;
        }
    } else if (envp[0] != nullptr && strcmp(envp[0], kEmptyArgvEnv) == 0) {
        mode = "empty-argv";
    }
    if (!mode.empty()) {
        if (mode == "proc-snapshot") {
            const int result = check_proc_snapshot_and_fork(input_for(mode));
            if (result != 0) {
                return result;
            }
        }
        return check_layout(argc, argv, envp, input_for(mode, script),
                            script.empty() ? self_path : script);
    }
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
