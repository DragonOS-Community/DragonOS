#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/utsname.h>
#include <unistd.h>

#include <fstream>
#include <map>
#include <sstream>
#include <string>
#include <vector>

namespace {

struct Options {
    std::string mount;
    std::string tag = "hostshare";
    std::string mount_options;
    std::string workload = "all";
    size_t files = 256;
    size_t file_size = 4 * 1024 * 1024;
    size_t block_size = 4096;
    size_t iterations = 4096;
    size_t workers = 4;
};

using StatsMap = std::map<std::string, long long>;

uint64_t now_us() {
    timeval tv = {};
    gettimeofday(&tv, nullptr);
    return static_cast<uint64_t>(tv.tv_sec) * 1000000ULL + tv.tv_usec;
}

std::string path_join(const std::string& a, const std::string& b) {
    if (!a.empty() && a.back() == '/') {
        return a + b;
    }
    return a + "/" + b;
}

const char* env_or_empty(const char* name) {
    const char* value = getenv(name);
    return value ? value : "";
}

const char* result_mount_options(const Options& opt) {
    const char* value = getenv("VIRTIOFS_BENCH_MOUNT_OPTIONS");
    if (value && value[0] != '\0') {
        return value;
    }
    return opt.mount_options.c_str();
}

std::string stats_path() {
    const char* value = getenv("VIRTIOFS_STATS_PATH");
    return value && value[0] != '\0' ? value : "";
}

StatsMap read_stats() {
    StatsMap stats;
    std::string path = stats_path();
    if (path.empty()) {
        return stats;
    }
    std::ifstream in(path);
    if (!in) {
        return stats;
    }

    std::string section;
    std::string line;
    while (std::getline(in, line)) {
        if (line.empty()) {
            continue;
        }
        if (line.front() == '[' && line.back() == ']') {
            section = line.substr(1, line.size() - 2);
            continue;
        }
        std::istringstream iss(line);
        std::string key;
        long long value = 0;
        if (iss >> key >> value) {
            stats[section + "." + key] = value;
        }
    }
    return stats;
}

void emit_stats_delta(const char* workload, const StatsMap& before, const StatsMap& after) {
    std::vector<std::string> active_opcode_prefixes;
    const std::string requests_suffix = "_requests_total";
    for (const auto& item : after) {
        auto old = before.find(item.first);
        if (old == before.end() || item.second <= old->second ||
            item.first.rfind("virtiofs_opcode.opcode_", 0) != 0 ||
            item.first.size() <= requests_suffix.size() ||
            item.first.compare(item.first.size() - requests_suffix.size(), requests_suffix.size(),
                               requests_suffix) != 0) {
            continue;
        }
        active_opcode_prefixes.push_back(
            item.first.substr(0, item.first.size() - requests_suffix.size()));
    }

    for (const auto& item : after) {
        auto old = before.find(item.first);
        if (old == before.end()) {
            continue;
        }
        long long delta = item.second - old->second;
        bool required_zero = item.first == "virtiofs.response_buffer_zero_bytes" ||
                             item.first == "virtiofs.response_buffer_alloc_bytes" ||
                             item.first == "virtiofs.response_buffer_reuse_bytes";
        if (!required_zero) {
            for (const std::string& prefix : active_opcode_prefixes) {
                if (item.first == prefix + "_response_buffer_zero_bytes" ||
                    item.first == prefix + "_response_buffer_alloc_bytes" ||
                    item.first == prefix + "_response_buffer_reuse_bytes") {
                    required_zero = true;
                    break;
                }
            }
        }
        if (delta == 0 && !required_zero) {
            continue;
        }
        printf("stats_delta workload=%s key=%s delta=%lld\n", workload, item.first.c_str(),
               delta);
    }
}

bool write_full(int fd, const void* data, size_t len) {
    const char* p = static_cast<const char*>(data);
    while (len > 0) {
        ssize_t n = write(fd, p, len);
        if (n <= 0) {
            return false;
        }
        p += n;
        len -= static_cast<size_t>(n);
    }
    return true;
}

bool read_full_loop(int fd, std::vector<char>& buf, uint64_t* bytes) {
    for (;;) {
        ssize_t n = read(fd, buf.data(), buf.size());
        if (n < 0) {
            return false;
        }
        if (n == 0) {
            return true;
        }
        *bytes += static_cast<uint64_t>(n);
    }
}

int errno_or_eio() {
    return errno == 0 ? EIO : errno;
}

void record_first_error(int* err, int value) {
    if (*err == 0) {
        *err = value;
    }
}

void fsync_preserve_error(int fd, int* err) {
    if (fsync(fd) != 0) {
        record_first_error(err, errno_or_eio());
    }
}

void close_preserve_error(int fd, int* err) {
    if (close(fd) != 0) {
        record_first_error(err, errno_or_eio());
    }
}

void emit_result(const char* workload, const Options& opt, uint64_t elapsed_us, uint64_t bytes,
                 uint64_t ops, int err) {
    utsname uts = {};
    uname(&uts);
    printf("result workload=%s status=%s errno=%d elapsed_us=%llu bytes=%llu ops=%llu "
           "mount=%s files=%zu file_size=%zu block_size=%zu iterations=%zu workers=%zu "
           "cache_mode=%s mount_options=%s sysname=%s release=%s\n",
           workload, err == 0 ? "ok" : "fail", err,
           static_cast<unsigned long long>(elapsed_us),
           static_cast<unsigned long long>(bytes),
           static_cast<unsigned long long>(ops), opt.mount.c_str(), opt.files, opt.file_size,
           opt.block_size, opt.iterations, opt.workers, env_or_empty("VIRTIOFS_BENCH_CACHE_MODE"),
           result_mount_options(opt), uts.sysname, uts.release);
}

int ensure_dir(const std::string& path) {
    struct stat st = {};
    if (stat(path.c_str(), &st) == 0) {
        return S_ISDIR(st.st_mode) ? 0 : -1;
    }
    return mkdir(path.c_str(), 0755);
}

int prepare_data_file(const Options& opt, const std::string& path, char fill) {
    std::vector<char> buf(opt.block_size, fill);
    int fd = open(path.c_str(), O_CREAT | O_TRUNC | O_RDWR, 0644);
    if (fd < 0) {
        return errno;
    }

    int err = 0;
    for (size_t done = 0; done < opt.file_size; done += buf.size()) {
        size_t n = buf.size();
        if (done + n > opt.file_size) {
            n = opt.file_size - done;
        }
        if (!write_full(fd, buf.data(), n)) {
            err = errno_or_eio();
            break;
        }
    }
    if (err == 0) {
        fsync_preserve_error(fd, &err);
    }
    close_preserve_error(fd, &err);
    return err;
}

int metadata_workload(const Options& opt, const std::string& root) {
    uint64_t start = now_us();
    uint64_t ops = 0;
    int err = 0;
    for (size_t i = 0; i < opt.files; ++i) {
        std::string p = path_join(root, "meta_" + std::to_string(i));
        int fd = open(p.c_str(), O_CREAT | O_TRUNC | O_RDWR, 0644);
        if (fd < 0) {
            err = errno;
            break;
        }
        close_preserve_error(fd, &err);
        if (err != 0) {
            break;
        }
        struct stat st = {};
        if (stat(p.c_str(), &st) != 0) {
            err = errno;
            break;
        }
        if (unlink(p.c_str()) != 0) {
            err = errno;
            break;
        }
        ops += 3;
    }
    emit_result("metadata", opt, now_us() - start, 0, ops, err);
    return err == 0 ? 0 : -1;
}

int sequential_workload(const Options& opt, const std::string& root) {
    std::string p = path_join(root, "seq.dat");
    std::vector<char> buf(opt.block_size, 'D');
    uint64_t bytes = 0;
    uint64_t ops = 0;
    int err = 0;
    uint64_t start = now_us();

    int fd = open(p.c_str(), O_CREAT | O_TRUNC | O_RDWR, 0644);
    if (fd < 0) {
        err = errno;
    } else {
        for (size_t done = 0; err == 0 && done < opt.file_size; done += buf.size()) {
            size_t n = buf.size();
            if (done + n > opt.file_size) {
                n = opt.file_size - done;
            }
            if (!write_full(fd, buf.data(), n)) {
                err = errno_or_eio();
                break;
            }
            bytes += n;
            ++ops;
        }
        if (err == 0) {
            fsync_preserve_error(fd, &err);
        }
        close_preserve_error(fd, &err);
    }
    emit_result("sequential_write", opt, now_us() - start, bytes, ops, err);
    if (err != 0) {
        return -1;
    }

    bytes = 0;
    ops = 1;
    start = now_us();
    fd = open(p.c_str(), O_RDONLY);
    if (fd < 0) {
        err = errno;
    } else {
        if (!read_full_loop(fd, buf, &bytes)) {
            err = errno;
        }
        close(fd);
    }
    emit_result("sequential_read", opt, now_us() - start, bytes, ops, err);
    return err == 0 ? 0 : -1;
}

int random_read_workload(const Options& opt, const std::string& root) {
    std::string p = path_join(root, "random.dat");
    std::vector<char> buf(opt.block_size);
    uint64_t bytes = 0;
    uint64_t ops = 0;
    int err = 0;
    uint64_t start = now_us();

    if (err == 0) {
        int fd = open(p.c_str(), O_RDONLY);
        if (fd < 0) {
            err = errno;
        } else {
            size_t blocks = (opt.file_size + buf.size() - 1) / buf.size();
            if (blocks == 0) {
                blocks = 1;
            }
            for (size_t i = 0; i < opt.iterations; ++i) {
                off_t off = static_cast<off_t>((i * 2654435761ULL) % blocks);
                off *= static_cast<off_t>(buf.size());
                ssize_t n = pread(fd, buf.data(), buf.size(), off);
                if (n < 0) {
                    err = errno;
                    break;
                }
                bytes += static_cast<uint64_t>(n);
                ++ops;
            }
            close(fd);
        }
    }
    emit_result("random_read", opt, now_us() - start, bytes, ops, err);
    return err == 0 ? 0 : -1;
}

int mmap_scan_workload(const Options& opt, const std::string& root) {
    std::string p = path_join(root, "mmap.dat");
    uint64_t start = now_us();
    uint64_t bytes = 0;
    int err = 0;
    volatile uint64_t checksum = 0;

    if (err == 0) {
        int fd = open(p.c_str(), O_RDONLY);
        if (fd < 0) {
            err = errno;
        } else {
            void* map = mmap(nullptr, opt.file_size, PROT_READ, MAP_PRIVATE, fd, 0);
            if (map == MAP_FAILED) {
                err = errno;
            } else {
                const unsigned char* p8 = static_cast<const unsigned char*>(map);
                for (size_t i = 0; i < opt.file_size; i += 4096) {
                    checksum += p8[i];
                    bytes += (opt.file_size - i) < 4096 ? (opt.file_size - i) : 4096;
                }
                munmap(map, opt.file_size);
            }
            close(fd);
        }
    }
    emit_result("mmap_scan", opt, now_us() - start, bytes, checksum, err);
    return err == 0 ? 0 : -1;
}

struct WorkerArg {
    Options opt;
    std::string root;
    size_t id;
    int err;
    uint64_t bytes;
    uint64_t ops;
    bool started;
};

void* worker_main(void* raw) {
    WorkerArg* arg = static_cast<WorkerArg*>(raw);
    std::vector<char> buf(arg->opt.block_size, static_cast<char>('A' + (arg->id % 26)));
    std::string p = path_join(arg->root, "worker_" + std::to_string(arg->id) + ".dat");
    int fd = open(p.c_str(), O_CREAT | O_TRUNC | O_RDWR, 0644);
    if (fd < 0) {
        arg->err = errno;
        return nullptr;
    }
    for (size_t i = 0; i < arg->opt.iterations; ++i) {
        if (!write_full(fd, buf.data(), buf.size())) {
            arg->err = errno_or_eio();
            break;
        }
        arg->bytes += buf.size();
        ++arg->ops;
    }
    if (arg->err == 0) {
        fsync_preserve_error(fd, &arg->err);
    }
    close_preserve_error(fd, &arg->err);
    return nullptr;
}

int concurrent_workload(const Options& opt, const std::string& root) {
    std::vector<pthread_t> threads(opt.workers);
    std::vector<WorkerArg> args(opt.workers);
    uint64_t start = now_us();
    for (size_t i = 0; i < opt.workers; ++i) {
        args[i].opt = opt;
        args[i].root = root;
        args[i].id = i;
        args[i].err = 0;
        args[i].bytes = 0;
        args[i].ops = 0;
        args[i].started = false;
        int rc = pthread_create(&threads[i], nullptr, worker_main, &args[i]);
        if (rc != 0) {
            args[i].err = rc;
        } else {
            args[i].started = true;
        }
    }
    uint64_t bytes = 0;
    uint64_t ops = 0;
    int err = 0;
    for (size_t i = 0; i < opt.workers; ++i) {
        if (args[i].started) {
            int rc = pthread_join(threads[i], nullptr);
            if (rc != 0) {
                record_first_error(&err, rc);
                continue;
            }
        }
        bytes += args[i].bytes;
        ops += args[i].ops;
        if (args[i].err != 0 && err == 0) {
            err = args[i].err;
        }
    }
    emit_result("concurrent_write", opt, now_us() - start, bytes, ops, err);
    return err == 0 ? 0 : -1;
}

void usage(const char* argv0) {
    fprintf(stderr,
            "usage: %s --mount PATH [--workload all|metadata|sequential|random_read|mmap|concurrent] "
            "[--files N] [--file-size N] [--block-size N] [--iterations N] [--workers N]\n",
            argv0);
    fprintf(stderr,
            "       %s [--tag hostshare] [--mount-options OPTIONS] [workload options...]\n",
            argv0);
    fprintf(stderr,
            "If --mount is omitted, the benchmark mounts the virtiofs tag on a temporary "
            "directory and unmounts it before exit.\n");
}

bool parse_size(const char* s, size_t* out) {
    char* end = nullptr;
    errno = 0;
    unsigned long long v = strtoull(s, &end, 10);
    if (errno != 0 || end == s || *end != '\0') {
        return false;
    }
    *out = static_cast<size_t>(v);
    return true;
}

bool parse_args(int argc, char** argv, Options* opt) {
    for (int i = 1; i < argc; ++i) {
        auto need = [&](const char* name) -> const char* {
            if (i + 1 >= argc) {
                fprintf(stderr, "%s requires a value\n", name);
                return nullptr;
            }
            return argv[++i];
        };
        if (strcmp(argv[i], "--mount") == 0) {
            const char* v = need(argv[i]);
            if (!v) return false;
            opt->mount = v;
        } else if (strcmp(argv[i], "--tag") == 0) {
            const char* v = need(argv[i]);
            if (!v) return false;
            opt->tag = v;
        } else if (strcmp(argv[i], "--mount-options") == 0) {
            const char* v = need(argv[i]);
            if (!v) return false;
            opt->mount_options = v;
        } else if (strcmp(argv[i], "--workload") == 0) {
            const char* v = need(argv[i]);
            if (!v) return false;
            opt->workload = v;
        } else if (strcmp(argv[i], "--files") == 0) {
            const char* v = need(argv[i]);
            if (!v || !parse_size(v, &opt->files)) return false;
        } else if (strcmp(argv[i], "--file-size") == 0) {
            const char* v = need(argv[i]);
            if (!v || !parse_size(v, &opt->file_size)) return false;
        } else if (strcmp(argv[i], "--block-size") == 0) {
            const char* v = need(argv[i]);
            if (!v || !parse_size(v, &opt->block_size)) return false;
        } else if (strcmp(argv[i], "--iterations") == 0) {
            const char* v = need(argv[i]);
            if (!v || !parse_size(v, &opt->iterations)) return false;
        } else if (strcmp(argv[i], "--workers") == 0) {
            const char* v = need(argv[i]);
            if (!v || !parse_size(v, &opt->workers)) return false;
        } else {
            return false;
        }
    }
    if (opt->workload != "all" && opt->workload != "metadata" && opt->workload != "sequential" &&
        opt->workload != "random_read" && opt->workload != "mmap" &&
        opt->workload != "concurrent") {
        fprintf(stderr, "unknown workload: %s\n", opt->workload.c_str());
        return false;
    }
    if (opt->tag.empty()) {
        fprintf(stderr, "--tag must not be empty\n");
        return false;
    }
    return opt->file_size != 0 && opt->block_size != 0 && opt->workers != 0;
}

struct AutoMount {
    bool active = false;
    std::string path;

    int mount_if_needed(Options* opt) {
        if (!opt->mount.empty()) {
            return 0;
        }

        path = "/tmp/virtiofs_bench_mount_" + std::to_string(getpid());
        if (ensure_dir(path) != 0) {
            return errno == 0 ? ENOTDIR : errno;
        }

        const char* data = opt->mount_options.empty() ? nullptr : opt->mount_options.c_str();
        if (mount(opt->tag.c_str(), path.c_str(), "virtiofs", 0, data) != 0) {
            int err = errno;
            rmdir(path.c_str());
            return err;
        }

        opt->mount = path;
        active = true;
        return 0;
    }

    void cleanup() {
        if (!active) {
            return;
        }
        if (umount(path.c_str()) != 0) {
            fprintf(stderr, "warning: umount(%s) failed: %s\n", path.c_str(), strerror(errno));
        }
        if (rmdir(path.c_str()) != 0) {
            fprintf(stderr, "warning: rmdir(%s) failed: %s\n", path.c_str(), strerror(errno));
        }
        active = false;
    }

    ~AutoMount() {
        cleanup();
    }
};

int run_with_stats_delta(const char* name, int (*workload)(const Options&, const std::string&),
                         const Options& opt, const std::string& root) {
    StatsMap before = read_stats();
    int rc = workload(opt, root);
    StatsMap after = read_stats();
    emit_stats_delta(name, before, after);
    return rc;
}

int prepare_read_input(const char* workload, const Options& opt, const std::string& path,
                       char fill) {
    int err = prepare_data_file(opt, path, fill);
    if (err != 0) {
        emit_result(workload, opt, 0, 0, 0, err);
        return -1;
    }
    return 0;
}

}  // namespace

int main(int argc, char** argv) {
    Options opt;
    if (!parse_args(argc, argv, &opt)) {
        usage(argv[0]);
        return 2;
    }

    AutoMount automount;
    int mount_err = automount.mount_if_needed(&opt);
    if (mount_err != 0) {
        fprintf(stderr, "failed to mount virtiofs tag '%s': %s\n", opt.tag.c_str(),
                strerror(mount_err));
        return 1;
    }

    std::string root = path_join(opt.mount, ".virtiofs_bench_" + std::to_string(getpid()));
    if (ensure_dir(root) != 0) {
        fprintf(stderr, "failed to create %s: %s\n", root.c_str(), strerror(errno));
        return 1;
    }

    int rc = 0;
    if (opt.workload == "all" || opt.workload == "metadata") {
        rc |= run_with_stats_delta("metadata", metadata_workload, opt, root);
    }
    if (opt.workload == "all" || opt.workload == "sequential") {
        rc |= run_with_stats_delta("sequential", sequential_workload, opt, root);
    }
    if (opt.workload == "all" || opt.workload == "random_read") {
        if (prepare_read_input("random_read", opt, path_join(root, "random.dat"), 'R') == 0) {
            rc |= run_with_stats_delta("random_read", random_read_workload, opt, root);
        } else {
            rc |= -1;
        }
    }
    if (opt.workload == "all" || opt.workload == "mmap") {
        if (prepare_read_input("mmap_scan", opt, path_join(root, "mmap.dat"), 'M') == 0) {
            rc |= run_with_stats_delta("mmap", mmap_scan_workload, opt, root);
        } else {
            rc |= -1;
        }
    }
    if (opt.workload == "all" || opt.workload == "concurrent") {
        rc |= run_with_stats_delta("concurrent", concurrent_workload, opt, root);
    }

    return rc == 0 ? 0 : 1;
}
