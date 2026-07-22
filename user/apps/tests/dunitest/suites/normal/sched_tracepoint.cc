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

    // id 应为数字。
    snprintf(file, sizeof(file), "%s/id", base);
    std::string id = read_all(file);
    EXPECT_FALSE(id.empty());
    char* end = nullptr;
    long idval = strtol(id.c_str(), &end, 10);
    EXPECT_GT(idval, 0) << "invalid id: " << id;

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

int main(int argc, char** argv) {
    // 子模式：被 execve 进来后立即退出 0。
    if (argc >= 2 && strcmp(argv[1], kHelperExec) == 0) {
        _exit(0);
    }
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
