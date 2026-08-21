// sched_process_exec tracepoint 语义测试。
//
// 验证：
//   1. sched_process_exec 事件在 debugfs 下正确导出（enable/format/id 文件 + format 字段）。
//   2. enable 后执行 execve 会触发该 tracepoint，并在 trace 文件中留下记录。
//
// 对应 issue #2149。

#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <linux/perf_event.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>

namespace {

constexpr char kHelperExec[] = "--sched-tp-helper-exit0";

// 读 path 全部内容（非阻塞，读到 EOF 为止）。
std::string read_all(const char* path) {
    int fd = open(path, O_RDONLY);
    EXPECT_GE(fd, 0) << "open(" << path << ") failed: errno=" << errno << " ("
                     << strerror(errno) << ")";
    if (fd < 0) {
        return {};
    }
    std::string out;
    char buf[256];
    while (true) {
        ssize_t n = read(fd, buf, sizeof(buf));
        if (n <= 0) {
            break;
        }
        out.append(buf, static_cast<size_t>(n));
    }
    close(fd);
    return out;
}

// 向 path 写 data，返回是否写成功。
bool write_file(const char* path, const char* data) {
    int fd = open(path, O_WRONLY);
    if (fd < 0) {
        return false;
    }
    size_t len = strlen(data);
    ssize_t n = write(fd, data, len);
    close(fd);
    return n == static_cast<ssize_t>(len);
}

// mount debugfs 到 root。返回 AssertionResult，供调用处 ASSERT_TRUE 在 mount
// 失败时终止当前 TEST（Thread7：避免 void helper 里的 fatal assertion 只 return helper
// 而让测试继续对着普通目录跑、产生连锁失败）。
::testing::AssertionResult mount_debugfs(const char* root) {
    if (mount("none", root, "debugfs", 0, nullptr) != 0) {
        return ::testing::AssertionFailure()
               << "mount debugfs failed: errno=" << errno << " (" << strerror(errno)
               << ")";
    }
    return ::testing::AssertionSuccess();
}

// RAII guard：持有 debugfs mount 点，保证任意 ASSERT 提前 return 时都恢复测试前
// 状态（Thread8：否则中途 ASSERT 失败会泄漏 enabled 的 static key + mount 点，污染
// 共享 buffer 并给后续无关 exec 永久加开销）。
//
// 构造前提：mount_debugfs 已成功（mount 已生效）。此时析构必须 umount + rmdir。
// arm_enable()：在事件成功 enable 后调用，记录 enable 文件路径；析构时写回 "0"。
// 析构顺序：disable（仅当 arm 过）→ umount → rmdir。不可拷贝/移动（持有路径所有权）。
class DebugfsMount {
 public:
    explicit DebugfsMount(const char* root) : root_(root) {}

    ~DebugfsMount() {
        if (!enable_path_.empty()) {
            write_file(enable_path_.c_str(), "0");
        }
        if (mounted_) {
            umount(root_.c_str());
        }
        rmdir(root_.c_str());
    }

    DebugfsMount(const DebugfsMount&) = delete;
    DebugfsMount& operator=(const DebugfsMount&) = delete;

    // 在事件成功 enable 后调用：析构时把该文件写回 "0"。
    void arm_enable(const char* enable_path) { enable_path_ = enable_path; }
    void mark_mounted() { mounted_ = true; }

 private:
    std::string root_;
    std::string enable_path_;
    bool mounted_ = false;
};

class TemporaryPath {
 public:
    explicit TemporaryPath(const char* path) : path_(path) {}
    ~TemporaryPath() { unlink(path_.c_str()); }
    TemporaryPath(const TemporaryPath&) = delete;
    TemporaryPath& operator=(const TemporaryPath&) = delete;

 private:
    std::string path_;
};

class ScopedAffinity {
 public:
    explicit ScopedAffinity(const cpu_set_t& original) : original_(original) {}

    ~ScopedAffinity() {
        if (sched_setaffinity(0, sizeof(original_), &original_) != 0) {
            ADD_FAILURE() << "failed to restore CPU affinity: " << strerror(errno);
        }
    }

    ScopedAffinity(const ScopedAffinity&) = delete;
    ScopedAffinity& operator=(const ScopedAffinity&) = delete;

 private:
    cpu_set_t original_;
};

std::string record_for_pid(const std::string& trace, pid_t pid) {
    const std::string pid_field = " pid=" + std::to_string(pid) + " old_pid=";
    size_t start = 0;
    while (start < trace.size()) {
        size_t end = trace.find('\n', start);
        if (end == std::string::npos) end = trace.size();
        std::string line = trace.substr(start, end - start);
        if (line.find("sched_process_exec(") != std::string::npos &&
            line.find(pid_field) != std::string::npos) {
            return line;
        }
        start = end + 1;
    }
    return {};
}

[[noreturn]] void helper_exec_exit0();

bool copy_file(const char* source, const char* destination) {
    int in = open(source, O_RDONLY);
    if (in < 0) return false;
    int out = open(destination, O_WRONLY | O_CREAT | O_TRUNC, 0755);
    if (out < 0) {
        close(in);
        return false;
    }
    bool ok = true;
    char buf[4096];
    while (true) {
        ssize_t n = read(in, buf, sizeof(buf));
        if (n == 0) break;
        if (n < 0) {
            ok = false;
            break;
        }
        ssize_t written = 0;
        while (written < n) {
            ssize_t part = write(out, buf + written, static_cast<size_t>(n - written));
            if (part <= 0) {
                ok = false;
                break;
            }
            written += part;
        }
        if (!ok) break;
    }
    close(in);
    close(out);
    return ok;
}

pid_t run_exec_on_cpu(int cpu) {
    pid_t child = fork();
    if (child == 0) {
        cpu_set_t set;
        CPU_ZERO(&set);
        CPU_SET(cpu, &set);
        if (sched_setaffinity(0, sizeof(set), &set) != 0) _exit(126);
        helper_exec_exit0();
    }
    return child;
}

// 子模式：被 execve 进来后立即退出 0。
[[noreturn]] void helper_exec_exit0() {
    char arg0[] = "/proc/self/exe";
    char arg1[] = "--sched-tp-helper-exit0";
    char* const argv[] = {arg0, arg1, nullptr};
    char* const envp[] = {nullptr};
    execve("/proc/self/exe", argv, envp);
    _exit(127);
}

// non-leader exec 辅助：sibling 线程（非 leader）执行 execve。
void* sibling_exec_thread(void* arg) {
    int notify_fd = *static_cast<int*>(arg);
    pid_t tid = static_cast<pid_t>(syscall(SYS_gettid));
    if (write(notify_fd, &tid, sizeof(tid)) != sizeof(tid)) {
        _exit(2);
    }
    close(notify_fd);
    helper_exec_exit0();
    return nullptr;
}

// 子模式：创建 sibling 线程（非 leader）执行 execve，主线程（leader）永久挂起。
// 触发内核 de_thread 的 raw_pid 交换路径（old_pid ≠ pid）。
[[noreturn]] void helper_sibling_exec_exit0(int notify_fd) {
    pthread_t thread;
    if (pthread_create(&thread, nullptr, sibling_exec_thread, &notify_fd) != 0) {
        _exit(1);
    }
    for (;;) {
        pause();
    }
}

}  // namespace

// 事件文件存在且 format 含全部字段。
TEST(SchedProcessExecTp, EventFilesExist) {
    char root[128] = {};
    snprintf(root, sizeof(root), "/tmp/sched_tp_events_%d", getpid());
    ASSERT_EQ(0, mkdir(root, 0755)) << strerror(errno);

    DebugfsMount guard(root);
    ASSERT_TRUE(mount_debugfs(root));  // Thread7：mount 失败终止测试。
    guard.mark_mounted();

    const char* base_rel = "/tracing/events/sched/sched_process_exec";
    char base[256] = {};
    snprintf(base, sizeof(base), "%s%s", root, base_rel);

    struct stat st = {};
    ASSERT_EQ(0, stat(base, &st)) << "missing event dir " << base << ": " << strerror(errno);
    EXPECT_TRUE(S_ISDIR(st.st_mode));

    char file[320] = {};
    for (const char* leaf : {"enable", "format", "id"}) {
        snprintf(file, sizeof(file), "%s/%s", base, leaf);
        ASSERT_EQ(0, stat(file, &st))
            << "missing " << file << ": " << strerror(errno);
    }

    // format 文件应含事件名与全部字段。
    snprintf(file, sizeof(file), "%s/format", base);
    std::string fmt = read_all(file);
    ASSERT_FALSE(fmt.empty());
    for (const char* needle : {"sched_process_exec", "common_pid", "__data_loc char[] filename",
                               "offset:8", "i32 pid", "offset:12", "i32 old_pid",
                               "offset:16", "filename=%s pid=%d old_pid=%d"}) {
        EXPECT_NE(std::string::npos, fmt.find(needle))
            << "format missing \"" << needle << "\"\n"
            << fmt;
    }

    // enable 默认为 "0"（未启用）。
    snprintf(file, sizeof(file), "%s/enable", base);
    std::string enable = read_all(file);
    EXPECT_NE(std::string::npos, enable.find("0")) << "enable not '0' by default: " << enable;

    // id 应为非负整数（DragonOS tracepoint id 从 0 开始递增分配）。
    snprintf(file, sizeof(file), "%s/id", base);
    std::string id = read_all(file);
    EXPECT_FALSE(id.empty());
    char* end = nullptr;
    long idval = strtol(id.c_str(), &end, 10);
    // 确认整个字符串都是数字（允许尾部换行），而非仅前缀可解析。
    ASSERT_NE(end, id.c_str()) << "id not numeric: " << id;
    while (end != nullptr && (*end == '\n' || *end == '\r' || *end == ' ')) ++end;
    EXPECT_EQ(end != nullptr && *end == '\0', true) << "id has trailing garbage: " << id;
    EXPECT_GE(idval, 0) << "invalid id: " << id;

    // 清理由 RAII guard 析构完成（umount + rmdir；本测例未 enable，无需 disable）。
}

TEST(SchedProcessExecTp, PerfCloseQueuesDeferredRelease) {
    char root[128] = {};
    snprintf(root, sizeof(root), "/tmp/sched_tp_perf_close_%d", getpid());
    ASSERT_EQ(0, mkdir(root, 0755)) << strerror(errno);
    DebugfsMount guard(root);
    ASSERT_TRUE(mount_debugfs(root));
    guard.mark_mounted();

    char id_path[320] = {};
    char enable_path[320] = {};
    snprintf(id_path, sizeof(id_path),
             "%s/tracing/events/sched/sched_process_exec/id", root);
    snprintf(enable_path, sizeof(enable_path),
             "%s/tracing/events/sched/sched_process_exec/enable", root);
    const long id = strtol(read_all(id_path).c_str(), nullptr, 10);
    ASSERT_GE(id, 0);

    for (int iteration = 0; iteration < 32; ++iteration) {
        perf_event_attr attr {};
        attr.type = PERF_TYPE_TRACEPOINT;
        attr.size = sizeof(attr);
        attr.config = static_cast<uint64_t>(id);
        int fd = static_cast<int>(syscall(SYS_perf_event_open, &attr, -1, 0, -1, 0));
        ASSERT_GE(fd, 0) << strerror(errno);
        ASSERT_EQ(0, ioctl(fd, PERF_EVENT_IOC_ENABLE, 0)) << strerror(errno);
        EXPECT_NE(std::string::npos, read_all(enable_path).find("0"))
            << "tracefs ownership must remain independent of perf ownership";
        ASSERT_EQ(0, close(fd));
    }
    // This is an enqueue/non-blocking smoke test. Worker completion is an
    // internal lifetime invariant; this user-visible test does not pretend a
    // fixed sleep can prove that the asynchronous queue has drained.
    ASSERT_TRUE(write_file(enable_path, "1"));
    ASSERT_TRUE(write_file(enable_path, "0"));
}

// enable 后 execve 应触发事件，trace 文件留下记录。
TEST(SchedProcessExecTp, FiresOnExecve) {
    char root[128] = {};
    snprintf(root, sizeof(root), "/tmp/sched_tp_fire_%d", getpid());
    ASSERT_EQ(0, mkdir(root, 0755)) << strerror(errno);

    DebugfsMount guard(root);
    ASSERT_TRUE(mount_debugfs(root));  // Thread7：mount 失败终止测试。
    guard.mark_mounted();

    const char* base_rel = "/tracing/events/sched/sched_process_exec";
    char base[256] = {};
    snprintf(base, sizeof(base), "%s%s", root, base_rel);

    // 启用事件。
    char enable_path[320] = {};
    snprintf(enable_path, sizeof(enable_path), "%s/enable", base);
    ASSERT_TRUE(write_file(enable_path, "1")) << "enable write failed";
    guard.arm_enable(enable_path);  // Thread8：任意提前 return 析构都会写回 "0"。

    // 清空 ring buffer：向 trace 写任意字节触发 clear。
    char trace_path[256] = {};
    snprintf(trace_path, sizeof(trace_path), "%s/tracing/trace", root);
    ASSERT_TRUE(write_file(trace_path, "1")) << "trace clear write failed";

    // fork + execve 自身触发 sched_process_exec。
    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: " << strerror(errno);
    if (child == 0) {
        helper_exec_exit0();
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << "waitpid failed: " << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status)) << "child did not exit normally, status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status)) << "helper exit code != 0";

    // 读 trace 快照，断言含 sched_process_exec 记录。
    std::string trace = read_all(trace_path);
    ASSERT_FALSE(trace.empty()) << "trace empty after execve";
    std::string record = record_for_pid(trace, child);
    ASSERT_FALSE(record.empty()) << "no sched_process_exec record for child:\n" << trace;
    EXPECT_NE(std::string::npos, record.find("filename=/proc/self/exe")) << record;
    EXPECT_EQ(std::string::npos, record.find("comm=")) << record;

    // 清理由 RAII guard 析构完成（disable + umount + rmdir）。
}

TEST(SchedProcessExecTp, ShebangReportsOriginalScriptFilename) {
    char root[128] = {};
    snprintf(root, sizeof(root), "/tmp/sched_tp_shebang_mount_%d", getpid());
    ASSERT_EQ(0, mkdir(root, 0755)) << strerror(errno);
    DebugfsMount guard(root);
    ASSERT_TRUE(mount_debugfs(root));
    guard.mark_mounted();

    char base[256] = {};
    snprintf(base, sizeof(base), "%s/tracing/events/sched/sched_process_exec", root);
    char enable_path[320] = {};
    snprintf(enable_path, sizeof(enable_path), "%s/enable", base);
    ASSERT_TRUE(write_file(enable_path, "1"));
    guard.arm_enable(enable_path);
    char trace_path[256] = {};
    snprintf(trace_path, sizeof(trace_path), "%s/tracing/trace", root);
    ASSERT_TRUE(write_file(trace_path, "1"));

    char script_path[160] = {};
    snprintf(script_path, sizeof(script_path), "/tmp/sched_tp_script_%d", getpid());
    TemporaryPath script_cleanup(script_path);
    int script = open(script_path, O_WRONLY | O_CREAT | O_TRUNC, 0755);
    ASSERT_GE(script, 0) << strerror(errno);
    // Use this test binary as the interpreter. The optional shebang argument
    // switches main() into the immediate-exit helper, so the test exercises
    // shebang rewriting without depending on an unrelated shell implementation.
    const char script_body[] = "#!/proc/self/exe --sched-tp-helper-exit0\n";
    ASSERT_EQ(static_cast<ssize_t>(sizeof(script_body) - 1),
              write(script, script_body, sizeof(script_body) - 1));
    close(script);
    ASSERT_EQ(0, chmod(script_path, 0755));

    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        char* const argv[] = {script_path, nullptr};
        char* const envp[] = {nullptr};
        execve(script_path, argv, envp);
        _exit(127);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));

    std::string trace = read_all(trace_path);
    std::string record = record_for_pid(trace, child);
    ASSERT_FALSE(record.empty()) << trace;
    EXPECT_NE(std::string::npos, record.find("filename=" + std::string(script_path))) << record;
    EXPECT_EQ(std::string::npos, record.find("filename=/proc/self/exe")) << record;
}

TEST(SchedProcessExecTp, LongUnicodeExecNameDoesNotCorruptCmdlineCache) {
    char root[128] = {};
    snprintf(root, sizeof(root), "/tmp/sched_tp_unicode_mount_%d", getpid());
    ASSERT_EQ(0, mkdir(root, 0755)) << strerror(errno);
    DebugfsMount guard(root);
    ASSERT_TRUE(mount_debugfs(root));
    guard.mark_mounted();

    char base[256] = {};
    snprintf(base, sizeof(base), "%s/tracing/events/sched/sched_process_exec", root);
    char enable_path[320] = {};
    snprintf(enable_path, sizeof(enable_path), "%s/enable", base);
    ASSERT_TRUE(write_file(enable_path, "1"));
    guard.arm_enable(enable_path);
    char trace_path[256] = {};
    snprintf(trace_path, sizeof(trace_path), "%s/tracing/trace", root);
    ASSERT_TRUE(write_file(trace_path, "1"));

    // Fourteen ASCII bytes place the following three-byte UTF-8 character
    // across the old arbitrary 16-byte cache truncation boundary.
    char executable_path[192] = {};
    snprintf(executable_path, sizeof(executable_path), "/tmp/abcdefghijklmn界_exec_%d", getpid());
    TemporaryPath executable_cleanup(executable_path);
    ASSERT_TRUE(copy_file("/proc/self/exe", executable_path)) << strerror(errno);
    ASSERT_EQ(0, chmod(executable_path, 0755));

    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        char arg1[] = "--sched-tp-helper-exit0";
        char* const argv[] = {executable_path, arg1, nullptr};
        char* const envp[] = {nullptr};
        execve(executable_path, argv, envp);
        _exit(127);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));

    // Reading trace exercises TraceCmdLineCache::get(), the former panic site.
    std::string trace = read_all(trace_path);
    std::string record = record_for_pid(trace, child);
    ASSERT_FALSE(record.empty()) << trace;
    EXPECT_NE(std::string::npos, record.find("filename=" + std::string(executable_path))) << record;
}

// 默认 disabled 时 execve 不应在 trace 留下 sched_process_exec 记录。
// 验证 static-key 门控：未 enable 时 tracepoint 零记录（若门控坏了永远触发，此处可抓住）。
TEST(SchedProcessExecTp, DefaultDisabledNoRecords) {
    char root[128] = {};
    snprintf(root, sizeof(root), "/tmp/sched_tp_disabled_%d", getpid());
    ASSERT_EQ(0, mkdir(root, 0755)) << strerror(errno);

    DebugfsMount guard(root);
    ASSERT_TRUE(mount_debugfs(root));  // Thread7：mount 失败终止测试。
    guard.mark_mounted();

    char trace_path[256] = {};
    snprintf(trace_path, sizeof(trace_path), "%s/tracing/trace", root);

    // 清空 ring buffer。
    ASSERT_TRUE(write_file(trace_path, "1")) << "trace clear write failed";

    // 不 enable（保持默认 disabled 状态），fork + execve 自身触发。
    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: " << strerror(errno);
    if (child == 0) {
        helper_exec_exit0();
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << "waitpid failed: " << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status)) << "child did not exit normally, status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status)) << "helper exit code != 0";

    // 默认 disabled：trace 中不应出现 sched_process_exec 记录。
    std::string trace = read_all(trace_path);
    EXPECT_EQ(std::string::npos, trace.find("sched_process_exec("))
        << "tracepoint fired while disabled (static-key gate broken):\n"
        << trace;

    // 清理由 RAII guard 析构完成（umount + rmdir；本测例未 enable，无需 disable）。
}

// enable 后能触发，disable 后不再触发。验证 enable/disable 状态机真正翻转 static-key。
TEST(SchedProcessExecTp, DisableStopsFiring) {
    char root[128] = {};
    snprintf(root, sizeof(root), "/tmp/sched_tp_disable_%d", getpid());
    ASSERT_EQ(0, mkdir(root, 0755)) << strerror(errno);

    DebugfsMount guard(root);
    ASSERT_TRUE(mount_debugfs(root));  // Thread7：mount 失败终止测试。
    guard.mark_mounted();

    const char* base_rel = "/tracing/events/sched/sched_process_exec";
    char base[256] = {};
    snprintf(base, sizeof(base), "%s%s", root, base_rel);

    char enable_path[320] = {};
    snprintf(enable_path, sizeof(enable_path), "%s/enable", base);
    char trace_path[256] = {};
    snprintf(trace_path, sizeof(trace_path), "%s/tracing/trace", root);

    // 基线：enable 后触发。
    ASSERT_TRUE(write_file(enable_path, "1")) << "enable write failed";
    ASSERT_TRUE(write_file(enable_path, "1")) << "repeated enable write failed";
    guard.arm_enable(enable_path);  // Thread8：任意提前 return 析构都会写回 "0"。
    ASSERT_TRUE(write_file(trace_path, "1")) << "trace clear write failed";
    {
        pid_t child = fork();
        ASSERT_GE(child, 0) << "fork failed: " << strerror(errno);
        if (child == 0) {
            helper_exec_exit0();
        }
        int status = 0;
        ASSERT_EQ(child, waitpid(child, &status, 0)) << "waitpid failed: " << strerror(errno);
        ASSERT_TRUE(WIFEXITED(status)) << "child did not exit normally, status=" << status;
        ASSERT_EQ(0, WEXITSTATUS(status)) << "helper exit code != 0";
    }
    {
        std::string trace = read_all(trace_path);
        ASSERT_FALSE(trace.empty()) << "trace empty after enabled execve";
        ASSERT_NE(std::string::npos, trace.find("sched_process_exec("))
            << "enabled state did not fire (baseline):\n"
            << trace;
    }

    // disable 后清 buffer 再触发：不应再记录。
    ASSERT_TRUE(write_file(enable_path, "0")) << "disable write failed";
    ASSERT_TRUE(write_file(enable_path, "0")) << "repeated disable write failed";
    ASSERT_TRUE(write_file(trace_path, "1")) << "trace clear write failed";
    {
        pid_t child = fork();
        ASSERT_GE(child, 0) << "fork failed: " << strerror(errno);
        if (child == 0) {
            helper_exec_exit0();
        }
        int status = 0;
        ASSERT_EQ(child, waitpid(child, &status, 0)) << "waitpid failed: " << strerror(errno);
        ASSERT_TRUE(WIFEXITED(status)) << "child did not exit normally, status=" << status;
        ASSERT_EQ(0, WEXITSTATUS(status)) << "helper exit code != 0";
    }
    {
        std::string trace = read_all(trace_path);
        EXPECT_EQ(std::string::npos, trace.find("sched_process_exec("))
            << "tracepoint still fired after disable:\n"
            << trace;
    }

    // 清理由 RAII guard 析构完成（disable + umount + rmdir；析构再次写 "0" 对已 disable 状态幂等）。
}

// The control thread and executing task run on different online CPUs. This
// validates tracefs behavior in stable epochs; the kernel static-key selftest
// separately observes the raw branch on a pinned remote CPU without the
// trace_pipe_enabled secondary gate. Fewer than two available CPUs is an unmet
// SMP test environment.
TEST(SchedProcessExecTp, SmpToggleHasStableEpochs) {
    cpu_set_t available;
    CPU_ZERO(&available);
    ASSERT_EQ(0, sched_getaffinity(0, sizeof(available), &available)) << strerror(errno);
    ScopedAffinity affinity_guard(available);
    int control_cpu = -1;
    int worker_cpu = -1;
    for (int cpu = 0; cpu < CPU_SETSIZE; ++cpu) {
        if (!CPU_ISSET(cpu, &available)) continue;
        if (control_cpu < 0) control_cpu = cpu;
        else {
            worker_cpu = cpu;
            break;
        }
    }
    ASSERT_GE(worker_cpu, 0) << "SMP acceptance requires at least two available CPUs";
    cpu_set_t control_set;
    CPU_ZERO(&control_set);
    CPU_SET(control_cpu, &control_set);
    ASSERT_EQ(0, sched_setaffinity(0, sizeof(control_set), &control_set)) << strerror(errno);

    char root[128] = {};
    snprintf(root, sizeof(root), "/tmp/sched_tp_smp_%d", getpid());
    ASSERT_EQ(0, mkdir(root, 0755)) << strerror(errno);
    DebugfsMount guard(root);
    ASSERT_TRUE(mount_debugfs(root));
    guard.mark_mounted();

    char enable_path[320] = {};
    char trace_path[256] = {};
    snprintf(enable_path, sizeof(enable_path),
             "%s/tracing/events/sched/sched_process_exec/enable", root);
    snprintf(trace_path, sizeof(trace_path), "%s/tracing/trace", root);
    guard.arm_enable(enable_path);

    for (int epoch = 0; epoch < 16; ++epoch) {
        ASSERT_TRUE(write_file(enable_path, "1"));
        ASSERT_TRUE(write_file(trace_path, "1"));
        pid_t enabled_child = run_exec_on_cpu(worker_cpu);
        ASSERT_GT(enabled_child, 0);
        int status = 0;
        ASSERT_EQ(enabled_child, waitpid(enabled_child, &status, 0));
        ASSERT_TRUE(WIFEXITED(status));
        ASSERT_EQ(0, WEXITSTATUS(status));
        ASSERT_FALSE(record_for_pid(read_all(trace_path), enabled_child).empty())
            << "remote CPU missed enabled epoch " << epoch;

        ASSERT_TRUE(write_file(enable_path, "0"));
        ASSERT_TRUE(write_file(trace_path, "1"));
        pid_t disabled_child = run_exec_on_cpu(worker_cpu);
        ASSERT_GT(disabled_child, 0);
        status = 0;
        ASSERT_EQ(disabled_child, waitpid(disabled_child, &status, 0));
        ASSERT_TRUE(WIFEXITED(status));
        ASSERT_EQ(0, WEXITSTATUS(status));
        EXPECT_TRUE(record_for_pid(read_all(trace_path), disabled_child).empty())
            << "remote CPU observed stale enabled branch in epoch " << epoch;
    }
}

// non-leader 线程 execve：触发 de_thread 的 raw_pid 交换，old_pid ≠ pid。
// FiresOnExecve 走单线程 leader exec（old_pid == pid），此处覆盖非 leader 路径。
TEST(SchedProcessExecTp, NonLeaderExecFiresWithDistinctOldPid) {
    char root[128] = {};
    snprintf(root, sizeof(root), "/tmp/sched_tp_nonleader_%d", getpid());
    ASSERT_EQ(0, mkdir(root, 0755)) << strerror(errno);

    DebugfsMount guard(root);
    ASSERT_TRUE(mount_debugfs(root));  // Thread7：mount 失败终止测试。
    guard.mark_mounted();

    const char* base_rel = "/tracing/events/sched/sched_process_exec";
    char base[256] = {};
    snprintf(base, sizeof(base), "%s%s", root, base_rel);

    char enable_path[320] = {};
    snprintf(enable_path, sizeof(enable_path), "%s/enable", base);
    char trace_path[256] = {};
    snprintf(trace_path, sizeof(trace_path), "%s/tracing/trace", root);

    ASSERT_TRUE(write_file(enable_path, "1")) << "enable write failed";
    guard.arm_enable(enable_path);  // Thread8：任意提前 return 析构都会写回 "0"。
    ASSERT_TRUE(write_file(trace_path, "1")) << "trace clear write failed";

    int tid_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(tid_pipe)) << "pipe failed: " << strerror(errno);

    // fork child：child 创建 sibling 线程（非 leader）执行 execve，leader 永久挂起。
    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: " << strerror(errno);
    if (child == 0) {
        close(tid_pipe[0]);
        helper_sibling_exec_exit0(tid_pipe[1]);
    }
    close(tid_pipe[1]);
    pid_t sibling_tid = -1;
    ASSERT_EQ(static_cast<ssize_t>(sizeof(sibling_tid)),
              read(tid_pipe[0], &sibling_tid, sizeof(sibling_tid)))
        << "failed to receive sibling tid";
    close(tid_pipe[0]);

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << "waitpid failed: " << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status)) << "child did not exit normally, status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status)) << "helper exit code != 0";

    std::string trace = read_all(trace_path);
    ASSERT_FALSE(trace.empty()) << "trace empty after non-leader execve";

    std::string record = record_for_pid(trace, child);
    ASSERT_FALSE(record.empty())
        << "no sched_process_exec record for non-leader child:\n"
        << trace;

    // 解析 old_pid=N。注意 "old_pid=" 含 "pid=" 子串，先取 old_pid。
    size_t old_pos = record.find("old_pid=");
    ASSERT_NE(std::string::npos, old_pos) << "record missing old_pid=:\n" << record;
    long old_pid_val = strtol(record.c_str() + old_pos + strlen("old_pid="), nullptr, 10);

    // 解析 pid=N：用 " pid="（带前导空格）避免命中 old_pid 内的 pid。
    size_t pid_pos = record.find(" pid=");
    ASSERT_NE(std::string::npos, pid_pos) << "record missing pid=:\n" << record;
    long pid_val = strtol(record.c_str() + pid_pos + strlen(" pid="), nullptr, 10);

    // non-leader exec 触发 de_thread 交换：old_pid（调用 execve 线程原 PID）≠ pid（交换后 leader PID）。
    EXPECT_EQ(child, pid_val) << record;
    EXPECT_EQ(sibling_tid, old_pid_val) << record;

    // 清理由 RAII guard 析构完成（disable + umount + rmdir）。
}

int main(int argc, char** argv) {
    // 子模式：被 execve 进来后立即退出 0。
    if (argc >= 2 && strcmp(argv[1], kHelperExec) == 0) {
        _exit(0);
    }
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
