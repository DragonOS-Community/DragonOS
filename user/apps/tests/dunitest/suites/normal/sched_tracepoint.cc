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
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
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

// mount debugfs 到 root。
void mount_debugfs(const char* root) {
    ASSERT_EQ(0, mount("none", root, "debugfs", 0, nullptr))
        << "mount debugfs failed: errno=" << errno << " (" << strerror(errno) << ")";
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
void* sibling_exec_thread(void*) {
    helper_exec_exit0();
    return nullptr;
}

// 子模式：创建 sibling 线程（非 leader）执行 execve，主线程（leader）永久挂起。
// 触发内核 de_thread 的 raw_pid 交换路径（old_pid ≠ pid）。
[[noreturn]] void helper_sibling_exec_exit0() {
    pthread_t thread;
    if (pthread_create(&thread, nullptr, sibling_exec_thread, nullptr) != 0) {
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

    mount_debugfs(root);

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
    for (const char* needle :
         {"sched_process_exec", "common_pid", "comm", "pid", "old_pid"}) {
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

    EXPECT_EQ(0, umount(root)) << strerror(errno);
    EXPECT_EQ(0, rmdir(root)) << strerror(errno);
}

// enable 后 execve 应触发事件，trace 文件留下记录。
TEST(SchedProcessExecTp, FiresOnExecve) {
    char root[128] = {};
    snprintf(root, sizeof(root), "/tmp/sched_tp_fire_%d", getpid());
    ASSERT_EQ(0, mkdir(root, 0755)) << strerror(errno);

    mount_debugfs(root);

    const char* base_rel = "/tracing/events/sched/sched_process_exec";
    char base[256] = {};
    snprintf(base, sizeof(base), "%s%s", root, base_rel);

    // 启用事件。
    char enable_path[320] = {};
    snprintf(enable_path, sizeof(enable_path), "%s/enable", base);
    ASSERT_TRUE(write_file(enable_path, "1")) << "enable write failed";

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
    EXPECT_NE(std::string::npos, trace.find("sched_process_exec("))
        << "no sched_process_exec record in trace:\n"
        << trace;
    // TP_printk 输出的字段。
    EXPECT_NE(std::string::npos, trace.find("comm="))
        << "trace missing comm= field:\n"
        << trace;

    // 关闭事件并清理。
    write_file(enable_path, "0");
    EXPECT_EQ(0, umount(root)) << strerror(errno);
    EXPECT_EQ(0, rmdir(root)) << strerror(errno);
}

// 默认 disabled 时 execve 不应在 trace 留下 sched_process_exec 记录。
// 验证 static-key 门控：未 enable 时 tracepoint 零记录（若门控坏了永远触发，此处可抓住）。
TEST(SchedProcessExecTp, DefaultDisabledNoRecords) {
    char root[128] = {};
    snprintf(root, sizeof(root), "/tmp/sched_tp_disabled_%d", getpid());
    ASSERT_EQ(0, mkdir(root, 0755)) << strerror(errno);

    mount_debugfs(root);

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

    EXPECT_EQ(0, umount(root)) << strerror(errno);
    EXPECT_EQ(0, rmdir(root)) << strerror(errno);
}

// enable 后能触发，disable 后不再触发。验证 enable/disable 状态机真正翻转 static-key。
TEST(SchedProcessExecTp, DisableStopsFiring) {
    char root[128] = {};
    snprintf(root, sizeof(root), "/tmp/sched_tp_disable_%d", getpid());
    ASSERT_EQ(0, mkdir(root, 0755)) << strerror(errno);

    mount_debugfs(root);

    const char* base_rel = "/tracing/events/sched/sched_process_exec";
    char base[256] = {};
    snprintf(base, sizeof(base), "%s%s", root, base_rel);

    char enable_path[320] = {};
    snprintf(enable_path, sizeof(enable_path), "%s/enable", base);
    char trace_path[256] = {};
    snprintf(trace_path, sizeof(trace_path), "%s/tracing/trace", root);

    // 基线：enable 后触发。
    ASSERT_TRUE(write_file(enable_path, "1")) << "enable write failed";
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

    EXPECT_EQ(0, umount(root)) << strerror(errno);
    EXPECT_EQ(0, rmdir(root)) << strerror(errno);
}

// non-leader 线程 execve：触发 de_thread 的 raw_pid 交换，old_pid ≠ pid。
// FiresOnExecve 走单线程 leader exec（old_pid == pid），此处覆盖非 leader 路径。
TEST(SchedProcessExecTp, NonLeaderExecFiresWithDistinctOldPid) {
    char root[128] = {};
    snprintf(root, sizeof(root), "/tmp/sched_tp_nonleader_%d", getpid());
    ASSERT_EQ(0, mkdir(root, 0755)) << strerror(errno);

    mount_debugfs(root);

    const char* base_rel = "/tracing/events/sched/sched_process_exec";
    char base[256] = {};
    snprintf(base, sizeof(base), "%s%s", root, base_rel);

    char enable_path[320] = {};
    snprintf(enable_path, sizeof(enable_path), "%s/enable", base);
    char trace_path[256] = {};
    snprintf(trace_path, sizeof(trace_path), "%s/tracing/trace", root);

    ASSERT_TRUE(write_file(enable_path, "1")) << "enable write failed";
    ASSERT_TRUE(write_file(trace_path, "1")) << "trace clear write failed";

    // fork child：child 创建 sibling 线程（非 leader）执行 execve，leader 永久挂起。
    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: " << strerror(errno);
    if (child == 0) {
        helper_sibling_exec_exit0();
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << "waitpid failed: " << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status)) << "child did not exit normally, status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status)) << "helper exit code != 0";

    std::string trace = read_all(trace_path);
    ASSERT_FALSE(trace.empty()) << "trace empty after non-leader execve";

    // 找到包含 sched_process_exec 的记录行。
    size_t rec = trace.find("sched_process_exec");
    ASSERT_NE(std::string::npos, rec)
        << "no sched_process_exec record after non-leader execve:\n"
        << trace;
    size_t line_end = trace.find('\n', rec);
    if (line_end == std::string::npos) {
        line_end = trace.size();
    }
    std::string record = trace.substr(rec, line_end - rec);

    // 解析 old_pid=N。注意 "old_pid=" 含 "pid=" 子串，先取 old_pid。
    size_t old_pos = record.find("old_pid=");
    ASSERT_NE(std::string::npos, old_pos) << "record missing old_pid=:\n" << record;
    long old_pid_val = strtol(record.c_str() + old_pos + strlen("old_pid="), nullptr, 10);

    // 解析 pid=N：用 " pid="（带前导空格）避免命中 old_pid 内的 pid。
    size_t pid_pos = record.find(" pid=");
    ASSERT_NE(std::string::npos, pid_pos) << "record missing pid=:\n" << record;
    long pid_val = strtol(record.c_str() + pid_pos + strlen(" pid="), nullptr, 10);

    // non-leader exec 触发 de_thread 交换：old_pid（调用 execve 线程原 PID）≠ pid（交换后 leader PID）。
    EXPECT_NE(old_pid_val, pid_val)
        << "non-leader exec should produce distinct old_pid vs pid:\n"
        << record;

    write_file(enable_path, "0");
    EXPECT_EQ(0, umount(root)) << strerror(errno);
    EXPECT_EQ(0, rmdir(root)) << strerror(errno);
}

int main(int argc, char** argv) {
    // 子模式：被 execve 进来后立即退出 0。
    if (argc >= 2 && strcmp(argv[1], kHelperExec) == 0) {
        _exit(0);
    }
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
