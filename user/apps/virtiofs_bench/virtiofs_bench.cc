#include <errno.h>
#include <limits.h>
#include <dirent.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/utsname.h>
#include <time.h>
#include <unistd.h>

#include <algorithm>
#include <atomic>
#include <fstream>
#include <limits>
#include <map>
#include <new>
#include <sstream>
#include <string>
#include <vector>

struct VirtiofsBenchLastSyscall {
    std::atomic<uint64_t> sequence{0};
    std::atomic<uint64_t> ordinal{0};
    std::atomic<uint64_t> offset{0};
    std::atomic<uint64_t> requested{0};
    std::atomic<int64_t> returned{0};
    std::atomic<int32_t> error{0};
    std::atomic<uint32_t> state{0};  // 1 entering, 2 returned
};

// Stable symbol for a timeout-side GDB snapshot. The sequence is odd while a
// writer updates fields and even when readers may trust a double-read.
extern "C" VirtiofsBenchLastSyscall virtiofs_bench_last_syscall;
VirtiofsBenchLastSyscall virtiofs_bench_last_syscall;

namespace {

void publish_last_syscall(uint64_t ordinal, uint64_t offset, uint64_t requested,
                          int64_t returned, int error, uint32_t state) {
    auto& slot = virtiofs_bench_last_syscall;
    slot.sequence.fetch_add(1, std::memory_order_acq_rel);
    slot.ordinal.store(ordinal, std::memory_order_relaxed);
    slot.offset.store(offset, std::memory_order_relaxed);
    slot.requested.store(requested, std::memory_order_relaxed);
    slot.returned.store(returned, std::memory_order_relaxed);
    slot.error.store(error, std::memory_order_relaxed);
    slot.state.store(state, std::memory_order_relaxed);
    slot.sequence.fetch_add(1, std::memory_order_release);
}

enum class WorkloadSpec {
    All,
    Metadata,
    Readdir,
    ReaddirPrepare,
    ReaddirScan,
    ReaddirCleanup,
    Sequential,
    Prepare,
    SequentialWrite,
    SequentialRead,
    Cleanup,
    RandomRead,
    Mmap,
    Concurrent,
};

const char* workload_name(WorkloadSpec workload) {
    switch (workload) {
        case WorkloadSpec::All:
            return "all";
        case WorkloadSpec::Metadata:
            return "metadata";
        case WorkloadSpec::Readdir:
            return "readdir";
        case WorkloadSpec::ReaddirPrepare:
            return "readdir_prepare";
        case WorkloadSpec::ReaddirScan:
            return "readdir_scan";
        case WorkloadSpec::ReaddirCleanup:
            return "readdir_cleanup";
        case WorkloadSpec::Sequential:
            return "sequential";
        case WorkloadSpec::Prepare:
            return "prepare";
        case WorkloadSpec::SequentialWrite:
            return "sequential_write";
        case WorkloadSpec::SequentialRead:
            return "sequential_read";
        case WorkloadSpec::Cleanup:
            return "cleanup";
        case WorkloadSpec::RandomRead:
            return "random_read";
        case WorkloadSpec::Mmap:
            return "mmap";
        case WorkloadSpec::Concurrent:
            return "concurrent";
    }
    return "unknown";
}

bool parse_workload(const std::string& value, WorkloadSpec* workload) {
    static const struct {
        const char* name;
        WorkloadSpec workload;
    } specs[] = {{"all", WorkloadSpec::All},
                 {"metadata", WorkloadSpec::Metadata},
                 {"readdir", WorkloadSpec::Readdir},
                 {"readdir_prepare", WorkloadSpec::ReaddirPrepare},
                 {"readdir_scan", WorkloadSpec::ReaddirScan},
                 {"readdir_cleanup", WorkloadSpec::ReaddirCleanup},
                 {"sequential", WorkloadSpec::Sequential},
                 {"prepare", WorkloadSpec::Prepare},
                 {"sequential_write", WorkloadSpec::SequentialWrite},
                 {"sequential_read", WorkloadSpec::SequentialRead},
                 {"cleanup", WorkloadSpec::Cleanup},
                 {"random_read", WorkloadSpec::RandomRead},
                 {"mmap", WorkloadSpec::Mmap},
                 {"concurrent", WorkloadSpec::Concurrent}};
    for (const auto& spec : specs) {
        if (value == spec.name) {
            *workload = spec.workload;
            return true;
        }
    }
    return false;
}

struct Options {
    std::string mount;
    std::string tag = "hostshare";
    std::string mount_options;
    std::string expect_dax;
    WorkloadSpec workload = WorkloadSpec::All;
    std::string path;
    uint64_t seed = 0x445241474f4e4f53ULL;
    size_t files = 256;
    size_t file_size = 4 * 1024 * 1024;
    size_t block_size = 4096;
    size_t iterations = 4096;
    size_t workers = 4;
    bool iterations_explicit = false;
};

using StatsMap = std::map<std::string, long long>;

uint64_t now_us() {
    timespec ts = {};
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
        return 0;
    }
    return static_cast<uint64_t>(ts.tv_sec) * 1000000ULL +
           static_cast<uint64_t>(ts.tv_nsec) / 1000ULL;
}

uint64_t process_cpu_us() {
    timespec ts = {};
    if (clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &ts) != 0) {
        return 0;
    }
    return static_cast<uint64_t>(ts.tv_sec) * 1000000ULL +
           static_cast<uint64_t>(ts.tv_nsec) / 1000ULL;
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

StatsMap read_stats_at(const std::string& path) {
    StatsMap stats;
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

StatsMap read_stats() {
    return read_stats_at(stats_path());
}

std::string quiescence_path() {
    const char* value = getenv("VIRTIOFS_QUIESCENCE_PATH");
    if (value && value[0] != '\0') {
        return value;
    }
    return stats_path();
}

bool wait_for_quiescence(const char* workload, const char* stage) {
    const std::string path = quiescence_path();
    if (path.empty()) {
        return true;
    }
    const char* current_keys[] = {"fuse.request_queue_current",
                                  "fuse.dispatch_current",
                                  "fuse.processing_current",
                                  "fuse.background_inflight_current",
                                  "fuse.read_reservation_current",
                                  "virtiofs.inflight_current",
                                  "virtiofs.hiprio_inflight_current",
                                  "virtiofs.request_inflight_current",
                                  "virtiofs.queue_full_blocked_current",
                                  "virtiofs.reply_retained_current"};
    const char* total_keys[] = {"fuse.requests_queued_total",
                                "fuse.requests_dequeued_total",
                                "fuse.requests_replied_ok_total",
                                "fuse.requests_replied_err_total",
                                "fuse.requests_aborted_total",
                                "virtiofs.bridge_submitted_total",
                                "virtiofs.bridge_completed_total",
                                "virtiofs.direct_read_requested_bytes_total",
                                "virtiofs.direct_read_completed_bytes_total"};
    unsigned int stable = 0;
    StatsMap previous;
    for (unsigned int sample = 1; sample <= 200; ++sample) {
        StatsMap stats = read_stats_at(path);
        bool zero = !stats.empty();
        for (const char* key : current_keys) {
            auto value = stats.find(key);
            if (value == stats.end() || value->second != 0) {
                zero = false;
                break;
            }
        }
        bool unchanged = zero && !previous.empty();
        for (const char* key : total_keys) {
            auto value = stats.find(key);
            auto old = previous.find(key);
            if (value == stats.end() || old == previous.end() || value->second != old->second) {
                unchanged = false;
                break;
            }
        }
        stable = unchanged ? stable + 1 : 0;
        previous = std::move(stats);
        if (stable >= 2) {
            printf("quiescence workload=%s stage=%s status=ok samples=%u run_id=%s\n", workload,
                   stage, sample, env_or_empty("VIRTIOFS_BENCH_RUN_ID"));
            fflush(stdout);
            return true;
        }
        usleep(10000);
    }
    printf("quiescence workload=%s stage=%s status=timeout samples=200 run_id=%s\n", workload,
           stage, env_or_empty("VIRTIOFS_BENCH_RUN_ID"));
    fflush(stdout);
    return false;
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

bool stats_delta(const StatsMap& before, const StatsMap& after, const std::string& key,
                 long long* delta) {
    auto old = before.find(key);
    auto current = after.find(key);
    if (old == before.end() || current == after.end()) {
        return false;
    }
    *delta = current->second - old->second;
    return true;
}

bool require_readdir_no_nplusone(const char* workload, const StatsMap& before,
                                 const StatsMap& after) {
    long long lookup = 0;
    long long forget = 0;
    long long getattr = 0;
    long long readdir = 0;
    long long readdirplus = 0;
    bool present =
        stats_delta(before, after, "virtiofs_opcode.opcode_1_requests_total", &lookup) &&
        stats_delta(before, after, "virtiofs_opcode.opcode_2_requests_total", &forget) &&
        stats_delta(before, after, "virtiofs_opcode.opcode_3_requests_total", &getattr) &&
        stats_delta(before, after, "virtiofs_opcode.opcode_28_requests_total", &readdir) &&
        stats_delta(before, after, "virtiofs_opcode.opcode_44_requests_total", &readdirplus);
    bool passed = present && lookup == 0 && forget == 0 && getattr == 0 &&
                  readdir + readdirplus > 0;
    printf("stats_assert workload=%s check=readdir_nplusone status=%s present=%d "
           "lookup=%lld forget=%lld getattr=%lld readdir=%lld readdirplus=%lld\n",
           workload, passed ? "ok" : "fail", present ? 1 : 0, lookup, forget, getattr,
           readdir, readdirplus);
    return passed;
}

bool require_zero_copy_transfer(const char* workload, const StatsMap& before,
                                const StatsMap& after, int opcode) {
    const std::string prefix =
        "virtiofs_opcode.opcode_" + std::to_string(opcode);
    long long requests = 0;
    long long transfer_count = 0;
    long long transfer_bytes = 0;
    long long copy_bytes = 0;
    bool present =
        stats_delta(before, after, prefix + "_requests_total", &requests) &&
        stats_delta(before, after, prefix + "_reply_payload_transfer_count", &transfer_count) &&
        stats_delta(before, after, prefix + "_reply_payload_transfer_bytes", &transfer_bytes) &&
        stats_delta(before, after, prefix + "_reply_payload_copy_bytes", &copy_bytes);
    bool passed = present && requests > 0 && transfer_count > 0 && transfer_bytes > 0 &&
                  copy_bytes == 0;
    if (!passed) {
        fprintf(stderr,
                "stats_assert workload=%s opcode=%d status=fail present=%d requests=%lld "
                "transfer_count=%lld transfer_bytes=%lld copy_bytes=%lld\n",
                workload, opcode, present ? 1 : 0, requests, transfer_count, transfer_bytes,
                copy_bytes);
    }
    return passed;
}

bool stats_delta_any(const StatsMap& before, const StatsMap& after,
                     const std::vector<std::string>& keys, long long* delta) {
    for (const auto& key : keys) {
        if (stats_delta(before, after, key, delta)) {
            return true;
        }
    }
    return false;
}

bool require_cached_read_data_path(const char* workload, size_t expected_bytes,
                                   const StatsMap& before, const StatsMap& after) {
    const std::string prefix = "virtiofs_opcode.opcode_15";
    long long requests = 0;
    long long opcode_requests = 0;
    long long transfer_count = 0;
    long long transfer_bytes = 0;
    long long copy_bytes = 0;
    long long direct_dma_requested_requests = 0;
    long long direct_dma_requested_bytes = 0;
    long long direct_dma_requests = 0;
    long long direct_dma_bytes = 0;
    bool read_requests_present =
        stats_delta(before, after, "virtiofs.read_requested_requests_total", &requests);
    bool opcode_requests_present =
        stats_delta(before, after, prefix + "_requests_total", &opcode_requests);
    bool detailed_opcode_active = opcode_requests_present && opcode_requests > 0;
    bool copy_present =
        stats_delta(before, after, prefix + "_reply_payload_copy_bytes", &copy_bytes);
    bool transfer_present =
        stats_delta(before, after, prefix + "_reply_payload_transfer_count", &transfer_count) &&
        stats_delta(before, after, prefix + "_reply_payload_transfer_bytes", &transfer_bytes);
    bool direct_requested_present =
        stats_delta(before, after, "virtiofs.direct_read_requested_requests_total",
                    &direct_dma_requested_requests) &&
        stats_delta(before, after, "virtiofs.direct_read_requested_bytes_total",
                    &direct_dma_requested_bytes);
    bool direct_present = stats_delta_any(
                              before, after,
                              {"virtiofs.direct_read_completed_requests_total",
                               "fuse.cached_read_direct_dma_requests_total",
                               "fuse.cached_read_direct_dma_count_total",
                               "virtiofs.cached_read_direct_dma_requests_total"},
                              &direct_dma_requests) &&
                          stats_delta_any(before, after,
                                          {"virtiofs.direct_read_completed_bytes_total",
                                           "fuse.cached_read_direct_dma_bytes_total",
                                           "virtiofs.cached_read_direct_dma_bytes_total"},
                                          &direct_dma_bytes);
    // A formal cached-read sample must not merely contain some direct-DMA
    // traffic.  Every daemon READ must use the reserved page-cache SG path,
    // and its request/completion byte accounting must exactly match the
    // workload.  Zero transfer/copy deltas exclude a mixed fallback path.
    // Per-opcode transport counters exist in detailed mode only.  When they
    // are present they must exclude a mixed fallback; in light mode strict
    // READ/direct-DMA request and byte conservation provides that guarantee.
    bool transport_metrics_clean =
        (!copy_present && !transfer_present) ||
        (copy_present && transfer_present && copy_bytes == 0 && transfer_count == 0 &&
         transfer_bytes == 0);
    bool dma_bytes_conserved = direct_dma_requested_bytes == direct_dma_bytes &&
                               direct_dma_bytes >= 0 &&
                               direct_dma_bytes <= static_cast<long long>(expected_bytes) &&
                               ((requests == 0) == (direct_dma_bytes == 0));
    bool passed = read_requests_present && requests >= 0 &&
                  (!detailed_opcode_active || opcode_requests == requests) &&
                  transport_metrics_clean &&
                  direct_requested_present && direct_present &&
                  direct_dma_requested_requests == requests && direct_dma_requests == requests &&
                  dma_bytes_conserved;
    if (!passed) {
        fprintf(stderr,
                "stats_assert workload=%s opcode=15 status=fail read_requests_present=%d "
                "requests=%lld opcode_requests_present=%d opcode_requests=%lld "
                "transfer_count=%lld transfer_bytes=%lld direct_dma_requested_present=%d "
                "direct_dma_requested_requests=%lld direct_dma_requested_bytes=%lld "
                "direct_dma_present=%d direct_dma_requests=%lld direct_dma_bytes=%lld "
                "expected_bytes=%zu copy_bytes=%lld\n",
                workload, read_requests_present ? 1 : 0, requests,
                opcode_requests_present ? 1 : 0, opcode_requests, transfer_count, transfer_bytes,
                direct_requested_present ? 1 : 0, direct_dma_requested_requests,
                direct_dma_requested_bytes, direct_present ? 1 : 0, direct_dma_requests,
                direct_dma_bytes, expected_bytes, copy_bytes);
    }
    return passed;
}

bool validate_zero_copy_metrics(WorkloadSpec workload, const Options& opt,
                                const StatsMap& before, const StatsMap& after) {
    const char* name = workload_name(workload);
    if (workload == WorkloadSpec::Metadata) {
        return require_zero_copy_transfer(name, before, after, 35);  // CREATE
    }
    // The combined sequential workload writes and publishes its manifest
    // inside the measurement window, so control-plane reads may legitimately
    // contribute READ bytes.  Formal transport conservation uses the split,
    // preflighted sequential_read phase only.
    if (workload == WorkloadSpec::SequentialRead) {
        return require_cached_read_data_path(name, opt.file_size, before, after);
    }
    if (workload == WorkloadSpec::Readdir || workload == WorkloadSpec::ReaddirScan) {
        long long readdir_requests = 0;
        long long readdirplus_requests = 0;
        stats_delta(before, after, "virtiofs_opcode.opcode_28_requests_total",
                    &readdir_requests);
        stats_delta(before, after, "virtiofs_opcode.opcode_44_requests_total",
                    &readdirplus_requests);
        int opcode = readdirplus_requests > 0 ? 44 : 28;
        return require_zero_copy_transfer(name, before, after, opcode);
    }
    return true;
}

bool is_dax_data_workload(WorkloadSpec workload) {
    return workload == WorkloadSpec::Sequential || workload == WorkloadSpec::SequentialWrite ||
           workload == WorkloadSpec::SequentialRead || workload == WorkloadSpec::RandomRead ||
           workload == WorkloadSpec::Mmap || workload == WorkloadSpec::Concurrent;
}

bool validate_dax_metrics(WorkloadSpec workload, const Options& opt, const StatsMap& before,
                          const StatsMap& after) {
    if (opt.expect_dax.empty() || !is_dax_data_workload(workload)) {
        return true;
    }

    long long setup_requests = 0;
    long long mappings_created = 0;
    bool present = stats_delta(before, after, "virtiofs_opcode.opcode_48_requests_total",
                               &setup_requests) &&
                   stats_delta(before, after, "virtiofs.dax_mapping_created_total",
                               &mappings_created);
    bool expected = opt.expect_dax == "always";
    bool passed = present && (expected ? setup_requests > 0 && mappings_created > 0
                                       : setup_requests == 0 && mappings_created == 0);
    if (!passed) {
        fprintf(stderr,
                "dax_assert workload=%s status=fail expectation=%s present=%d "
                "setup_requests=%lld mappings_created=%lld\n",
                workload_name(workload), opt.expect_dax.c_str(), present ? 1 : 0, setup_requests,
                mappings_created);
    }
    return passed;
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

struct IoCounters {
    uint64_t syscalls = 0;
    uint64_t short_io = 0;
    uint64_t eintr = 0;
};

struct WritePhaseTimings {
    uint64_t data_loop_us = 0;
    uint64_t fsync_us = 0;
    uint64_t close_us = 0;
    uint64_t end_to_end_us = 0;
};

void emit_result(const char* workload, const Options& opt, uint64_t elapsed_us, uint64_t bytes,
                 uint64_t ops, int err, const IoCounters& io = {}, uint64_t checksum = 0,
                 const WritePhaseTimings& write_timings = {}, uint64_t cpu_us = 0) {
    utsname uts = {};
    uname(&uts);
    printf("result workload=%s status=%s errno=%d elapsed_us=%llu bytes=%llu ops=%llu "
           "syscalls=%llu short_io=%llu eintr=%llu checksum=%016llx "
           "data_loop_us=%llu fsync_us=%llu close_us=%llu end_to_end_us=%llu "
           "process_cpu_us=%llu "
           "mount=%s dataset=%s seed=%llu files=%zu file_size=%zu block_size=%zu "
           "iterations=%zu workers=%zu run_id=%s "
           "cache_mode=%s mount_options=%s expect_dax=%s sysname=%s release=%s\n",
           workload, err == 0 ? "ok" : "fail", err,
           static_cast<unsigned long long>(elapsed_us),
           static_cast<unsigned long long>(bytes),
           static_cast<unsigned long long>(ops),
           static_cast<unsigned long long>(io.syscalls),
           static_cast<unsigned long long>(io.short_io),
           static_cast<unsigned long long>(io.eintr),
           static_cast<unsigned long long>(checksum),
           static_cast<unsigned long long>(write_timings.data_loop_us),
           static_cast<unsigned long long>(write_timings.fsync_us),
           static_cast<unsigned long long>(write_timings.close_us),
           static_cast<unsigned long long>(write_timings.end_to_end_us),
           static_cast<unsigned long long>(cpu_us), opt.mount.c_str(),
           opt.path.empty() ? "ephemeral" : opt.path.c_str(),
           static_cast<unsigned long long>(opt.seed), opt.files, opt.file_size, opt.block_size,
           opt.iterations, opt.workers, env_or_empty("VIRTIOFS_BENCH_RUN_ID"),
           env_or_empty("VIRTIOFS_BENCH_CACHE_MODE"),
           result_mount_options(opt), opt.expect_dax.empty() ? "unspecified" : opt.expect_dax.c_str(),
           uts.sysname, uts.release);
}

int ensure_dir(const std::string& path) {
    struct stat st = {};
    if (stat(path.c_str(), &st) == 0) {
        return S_ISDIR(st.st_mode) ? 0 : -1;
    }
    return mkdir(path.c_str(), 0755);
}

constexpr const char* kDatasetFile = "seq.dat";
constexpr const char* kManifestFile = "manifest.v1";
constexpr const char* kManifestMagic = "VIRTIOFS_BENCH_DATASET_V1";
constexpr const char* kSeqTempPrefix = ".seq.tmp.";
constexpr const char* kManifestTempPrefix = ".manifest.tmp.";
constexpr const char* kSeqBackupPrefix = ".seq.backup.";
constexpr const char* kManifestBackupPrefix = ".manifest.backup.";
constexpr const char* kDatasetLockPrefix = ".virtiofs_bench_lock_";

class DatasetWriteLock {
public:
    DatasetWriteLock() = default;
    DatasetWriteLock(const DatasetWriteLock&) = delete;
    DatasetWriteLock& operator=(const DatasetWriteLock&) = delete;

    ~DatasetWriteLock() {
        if (fd_ >= 0) {
            // Closing the descriptor releases a classic fcntl record lock,
            // including when the process exits or crashes.
            close(fd_);
        }
    }

    bool acquire(const Options& opt, int* err) {
        int mount_fd = open(opt.mount.c_str(), O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW);
        if (mount_fd < 0) {
            *err = errno_or_eio();
            return false;
        }
        const std::string name = std::string(kDatasetLockPrefix) + opt.path;
        fd_ = openat(mount_fd, name.c_str(),
                     O_CREAT | O_RDWR | O_CLOEXEC | O_NOFOLLOW, 0644);
        const int open_error = fd_ < 0 ? errno_or_eio() : 0;
        close(mount_fd);
        if (open_error != 0) {
            *err = open_error;
            return false;
        }

        struct flock lock = {};
        lock.l_type = F_WRLCK;
        lock.l_whence = SEEK_SET;
        for (;;) {
            if (fcntl(fd_, F_SETLKW, &lock) == 0) {
                return true;
            }
            if (errno != EINTR) {
                *err = errno_or_eio();
                close(fd_);
                fd_ = -1;
                return false;
            }
        }
    }

private:
    int fd_ = -1;
};

struct DatasetManifest {
    uint64_t size = 0;
    uint64_t block_size = 0;
    uint64_t seed = 0;
    uint64_t hash = 0;
};

void phase_marker(const Options& opt, const char* workload, const char* phase, const char* event,
                  uint64_t offset, uint64_t requested, int64_t returned, int err) {
    const char* enabled = getenv("VIRTIOFS_BENCH_PHASE_MARKERS");
    if (enabled && strcmp(enabled, "0") == 0) {
        return;
    }
    // DragonOS's current stderr formatter truncates at 64-bit printf
    // conversions. Build the record from decimal strings so each phase remains
    // a single, shell-tokenizable line without losing timing or I/O context.
    static std::map<std::string, uint64_t> phase_starts;
    const uint64_t timestamp_us = now_us();
    const std::string key = std::string(workload) + ":" + phase;
    uint64_t elapsed_us = 0;
    if (strcmp(event, "begin") == 0) {
        phase_starts[key] = timestamp_us;
    } else {
        auto start = phase_starts.find(key);
        if (start != phase_starts.end()) {
            elapsed_us = timestamp_us >= start->second ? timestamp_us - start->second : 0;
            phase_starts.erase(start);
        }
    }

    std::string record =
        "phase workload=" + std::string(workload) +
        " dataset=" + (opt.path.empty() ? std::string("ephemeral") : opt.path) +
        " phase=" + phase + " event=" + event + " pid=" + std::to_string(getpid()) +
        " monotonic_us=" + std::to_string(timestamp_us) +
        " elapsed_us=" + std::to_string(elapsed_us) +
        " offset=" + std::to_string(offset) + " requested=" + std::to_string(requested) +
        " returned=" + std::to_string(returned) + " errno=" + std::to_string(err) +
        " run_id=" + env_or_empty("VIRTIOFS_BENCH_RUN_ID") + "\n";
    fputs(record.c_str(), stderr);
    fflush(stderr);
}

void emit_io_summary(const char* workload, const IoCounters& io, uint64_t checksum,
                     uint64_t verify_us) {
    printf("io_summary workload=%s syscalls=%llu short_io=%llu eintr=%llu checksum=%016llx "
           "run_id=%s "
           "verify_us=%llu\n",
           workload, static_cast<unsigned long long>(io.syscalls),
           static_cast<unsigned long long>(io.short_io),
           static_cast<unsigned long long>(io.eintr),
           static_cast<unsigned long long>(checksum), env_or_empty("VIRTIOFS_BENCH_RUN_ID"),
           static_cast<unsigned long long>(verify_us));
}

uint64_t mix64(uint64_t value) {
    value += 0x9e3779b97f4a7c15ULL;
    value = (value ^ (value >> 30)) * 0xbf58476d1ce4e5b9ULL;
    value = (value ^ (value >> 27)) * 0x94d049bb133111ebULL;
    return value ^ (value >> 31);
}

unsigned char pattern_byte(uint64_t seed, uint64_t offset) {
    uint64_t word = mix64(seed ^ (offset >> 3));
    return static_cast<unsigned char>(word >> ((offset & 7U) * 8U));
}

void fill_pattern(unsigned char* data, size_t len, uint64_t seed, uint64_t offset) {
    for (size_t i = 0; i < len; ++i) {
        data[i] = pattern_byte(seed, offset + i);
    }
}

uint64_t hash_bytes(const unsigned char* data, size_t len) {
    uint64_t hash = 1469598103934665603ULL;
    for (size_t i = 0; i < len; ++i) {
        hash ^= data[i];
        hash *= 1099511628211ULL;
    }
    return hash;
}

int open_dataset_dir(const Options& opt, bool create) {
    int mount_fd = open(opt.mount.c_str(), O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW);
    if (mount_fd < 0) {
        return -1;
    }
    std::string dirname = ".virtiofs_bench_" + opt.path;
    if (create && mkdirat(mount_fd, dirname.c_str(), 0755) != 0 && errno != EEXIST) {
        int saved = errno;
        close(mount_fd);
        errno = saved;
        return -1;
    }
    int root_fd = openat(mount_fd, dirname.c_str(),
                         O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW);
    int saved = errno;
    close(mount_fd);
    errno = saved;
    return root_fd;
}

bool write_all_counted(int fd, const unsigned char* data, size_t len, IoCounters* io) {
    size_t done = 0;
    while (done < len) {
        size_t requested = len - done;
        ++io->syscalls;
        ssize_t n = write(fd, data + done, requested);
        if (n < 0 && errno == EINTR) {
            ++io->eintr;
            continue;
        }
        if (n <= 0) {
            return false;
        }
        if (static_cast<size_t>(n) != requested) {
            ++io->short_io;
        }
        done += static_cast<size_t>(n);
    }
    return true;
}

bool write_text_file_at(int dir_fd, const std::string& temp_name, const std::string& text,
                        int* err) {
    int fd = openat(dir_fd, temp_name.c_str(),
                    O_CREAT | O_EXCL | O_WRONLY | O_CLOEXEC | O_NOFOLLOW, 0644);
    if (fd < 0) {
        *err = errno_or_eio();
        return false;
    }
    if (!write_full(fd, text.data(), text.size())) {
        *err = errno_or_eio();
    }
    if (*err == 0) {
        fsync_preserve_error(fd, err);
    }
    close_preserve_error(fd, err);
    if (*err != 0) {
        unlinkat(dir_fd, temp_name.c_str(), 0);
    }
    return *err == 0;
}

bool write_manifest_temp(int dir_fd, const std::string& temp, const DatasetManifest& manifest,
                         int* err) {
    std::ostringstream out;
    out << kManifestMagic << "\n"
        << "size " << manifest.size << "\n"
        << "block_size " << manifest.block_size << "\n"
        << "seed " << manifest.seed << "\n"
        << "hash " << manifest.hash << "\n";
    return write_text_file_at(dir_fd, temp, out.str(), err);
}

bool parse_manifest_u64(const std::string& token, uint64_t* value) {
    if (token.empty()) {
        return false;
    }
    uint64_t parsed = 0;
    for (unsigned char c : token) {
        if (c < '0' || c > '9') {
            return false;
        }
        const uint64_t digit = c - '0';
        if (parsed > (std::numeric_limits<uint64_t>::max() - digit) / 10) {
            return false;
        }
        parsed = parsed * 10 + digit;
    }
    *value = parsed;
    return true;
}

bool read_manifest(int dir_fd, DatasetManifest* manifest, int* err) {
    int fd = openat(dir_fd, kManifestFile, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0) {
        *err = errno_or_eio();
        return false;
    }
    std::string text;
    char buffer[512];
    for (;;) {
        ssize_t n = read(fd, buffer, sizeof(buffer));
        if (n < 0 && errno == EINTR) {
            continue;
        }
        if (n < 0) {
            *err = errno_or_eio();
            break;
        }
        if (n == 0) {
            break;
        }
        if (text.size() + static_cast<size_t>(n) > 4096) {
            *err = EOVERFLOW;
            break;
        }
        text.append(buffer, static_cast<size_t>(n));
    }
    close_preserve_error(fd, err);
    if (*err != 0) {
        return false;
    }
    std::istringstream in(text);
    std::string magic;
    std::string size_key, size_value;
    std::string block_key, block_value;
    std::string seed_key, seed_value;
    std::string hash_key, hash_value;
    std::string trailing;
    if (!(in >> magic >> size_key >> size_value >> block_key >> block_value >> seed_key >>
          seed_value >> hash_key >> hash_value) ||
        magic != kManifestMagic || size_key != "size" || block_key != "block_size" ||
        seed_key != "seed" || hash_key != "hash" || (in >> trailing) ||
        !parse_manifest_u64(size_value, &manifest->size) ||
        !parse_manifest_u64(block_value, &manifest->block_size) ||
        !parse_manifest_u64(seed_value, &manifest->seed) ||
        !parse_manifest_u64(hash_value, &manifest->hash) || manifest->size == 0 ||
        manifest->block_size == 0 || manifest->block_size > static_cast<uint64_t>(SSIZE_MAX)) {
        *err = EINVAL;
        return false;
    }
    return true;
}

bool rename_existing_to_backup(int dir_fd, const char* current, const std::string& backup,
                               bool* backed_up, int* err) {
    struct stat st = {};
    if (fstatat(dir_fd, current, &st, AT_SYMLINK_NOFOLLOW) != 0) {
        if (errno == ENOENT) {
            return true;
        }
        record_first_error(err, errno_or_eio());
        return false;
    }
    if (renameat(dir_fd, current, dir_fd, backup.c_str()) != 0) {
        record_first_error(err, errno_or_eio());
        return false;
    }
    *backed_up = true;
    return true;
}

void restore_backup(int dir_fd, const std::string& backup, const char* current, bool backed_up,
                    bool published) {
    if (published) {
        unlinkat(dir_fd, current, 0);
    }
    if (backed_up) {
        // Rollback is best-effort only when the filesystem itself has already
        // reported an error. Do not replace the original failure code.
        renameat(dir_fd, backup.c_str(), dir_fd, current);
    }
}

bool publish_dataset(int dir_fd, const std::string& data_temp,
                     const std::string& manifest_temp, int* err) {
    const std::string suffix = std::to_string(getpid());
    const std::string data_backup = std::string(kSeqBackupPrefix) + suffix;
    const std::string manifest_backup = std::string(kManifestBackupPrefix) + suffix;
    bool data_backed_up = false;
    bool manifest_backed_up = false;
    bool data_published = false;
    bool manifest_published = false;

    if (!rename_existing_to_backup(dir_fd, kDatasetFile, data_backup, &data_backed_up, err) ||
        !rename_existing_to_backup(dir_fd, kManifestFile, manifest_backup,
                                   &manifest_backed_up, err)) {
        restore_backup(dir_fd, data_backup, kDatasetFile, data_backed_up, false);
        return false;
    }
    if (renameat(dir_fd, data_temp.c_str(), dir_fd, kDatasetFile) != 0) {
        record_first_error(err, errno_or_eio());
    } else {
        data_published = true;
    }

    // Deterministic fault injection for the host transcript test. It models a
    // failure between the two renames without weakening the production path.
    const char* fault = getenv("VIRTIOFS_BENCH_TEST_FAULT");
    if (*err == 0 && fault && strcmp(fault, "delay_before_manifest_publish") == 0) {
        // Deterministically expose the two-rename transaction window to the
        // host concurrency test. The dataset lock remains held during it.
        usleep(1000000);
    }
    if (*err == 0 && fault && strcmp(fault, "before_manifest_publish") == 0) {
        *err = EIO;
    }
    if (*err == 0 && renameat(dir_fd, manifest_temp.c_str(), dir_fd, kManifestFile) != 0) {
        record_first_error(err, errno_or_eio());
    } else if (*err == 0) {
        manifest_published = true;
    }
    if (*err == 0) {
        fsync_preserve_error(dir_fd, err);
    }
    if (*err != 0) {
        restore_backup(dir_fd, manifest_backup, kManifestFile, manifest_backed_up,
                       manifest_published);
        restore_backup(dir_fd, data_backup, kDatasetFile, data_backed_up, data_published);
        fsync(dir_fd);
        return false;
    }

    // The new pair is now the durable committed dataset. Backup cleanup is
    // intentionally not part of the transaction; cleanup_workload also
    // removes leftovers after a crash or a failed unlink.
    if (data_backed_up) {
        unlinkat(dir_fd, data_backup.c_str(), 0);
    }
    if (manifest_backed_up) {
        unlinkat(dir_fd, manifest_backup.c_str(), 0);
    }
    fsync(dir_fd);
    return true;
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

int readdir_workload(const Options& opt, const std::string& root) {
    const std::string prefix = "readdir_";
    uint64_t start = now_us();
    uint64_t ops = 0;
    int err = 0;
    for (size_t i = 0; i < opt.files; ++i) {
        std::string path = path_join(root, prefix + std::to_string(i));
        int fd = open(path.c_str(), O_CREAT | O_TRUNC | O_WRONLY, 0644);
        if (fd < 0) {
            err = errno_or_eio();
            break;
        }
        close_preserve_error(fd, &err);
        if (err != 0) {
            break;
        }
    }

    size_t found = 0;
    if (err == 0) {
        DIR* dir = opendir(root.c_str());
        if (!dir) {
            err = errno_or_eio();
        } else {
            errno = 0;
            while (dirent* entry = readdir(dir)) {
                if (strncmp(entry->d_name, prefix.c_str(), prefix.size()) == 0) {
                    ++found;
                }
            }
            if (errno != 0) {
                err = errno;
            }
            if (closedir(dir) != 0 && err == 0) {
                err = errno_or_eio();
            }
        }
    }
    if (err == 0 && found != opt.files) {
        err = EIO;
    }
    for (size_t i = 0; i < opt.files; ++i) {
        std::string path = path_join(root, prefix + std::to_string(i));
        if (unlink(path.c_str()) == 0) {
            ++ops;
        } else if (err == 0) {
            err = errno_or_eio();
        }
    }
    emit_result("readdir", opt, now_us() - start, 0, ops + found, err);
    return err == 0 ? 0 : -1;
}

constexpr const char* kReaddirEntryPrefix = "entry_";
constexpr size_t kReaddirEntryDigits = 8;
constexpr size_t kReaddirBufferSize = 64 * 1024;

struct LinuxDirent64 {
    uint64_t ino;
    int64_t off;
    uint16_t reclen;
    uint8_t type;
    char name[1];
};

std::string readdir_entry_name(size_t index) {
    std::string name = kReaddirEntryPrefix;
    name.resize(name.size() + kReaddirEntryDigits, '0');
    for (size_t pos = name.size(); pos > sizeof("entry_") - 1; --pos) {
        name[pos - 1] = static_cast<char>('0' + index % 10);
        index /= 10;
    }
    return name;
}

bool readdir_entry_index(const char* name, size_t len, size_t files, size_t* index) {
    constexpr size_t prefix_len = sizeof("entry_") - 1;
    if (len != prefix_len + kReaddirEntryDigits ||
        memcmp(name, kReaddirEntryPrefix, prefix_len) != 0) {
        return false;
    }
    size_t value = 0;
    for (size_t i = prefix_len; i < len; ++i) {
        const unsigned char c = static_cast<unsigned char>(name[i]);
        if (c < '0' || c > '9') {
            return false;
        }
        value = value * 10 + static_cast<size_t>(c - '0');
    }
    if (value >= files) {
        return false;
    }
    *index = value;
    return true;
}

uint64_t readdir_dataset_checksum(size_t files) {
    uint64_t hash = 1469598103934665603ULL;
    for (size_t i = 0; i < files; ++i) {
        const std::string name = readdir_entry_name(i);
        for (unsigned char c : name) {
            hash ^= c;
            hash *= 1099511628211ULL;
        }
        hash ^= 0xff;
        hash *= 1099511628211ULL;
    }
    return hash;
}

struct ReaddirScanCounters {
    uint64_t bytes = 0;
    uint64_t getdents_calls = 0;
};

struct ReaddirScanScratch {
    explicit ReaddirScanScratch(size_t files)
        : seen(files, false), buffer(kReaddirBufferSize) {}

    void reset() {
        std::fill(seen.begin(), seen.end(), false);
    }

    std::vector<bool> seen;
    std::vector<unsigned char> buffer;
};

int scan_readdir_once(int fd, const Options& opt, ReaddirScanScratch* scratch,
                      ReaddirScanCounters* counters) {
    bool seen_dot = false;
    bool seen_dotdot = false;
    bool have_terminal_cookie = false;
    int64_t terminal_cookie = 0;

    for (;;) {
        ++counters->getdents_calls;
        long nread = syscall(SYS_getdents64, fd, scratch->buffer.data(), scratch->buffer.size());
        if (nread < 0 && errno == EINTR) {
            continue;
        }
        if (nread < 0) {
            return errno_or_eio();
        }
        if (nread == 0) {
            break;
        }
        counters->bytes += static_cast<uint64_t>(nread);
        size_t offset = 0;
        int64_t this_terminal_cookie = terminal_cookie;
        while (offset < static_cast<size_t>(nread)) {
            constexpr size_t header = offsetof(LinuxDirent64, name);
            if (static_cast<size_t>(nread) - offset < header) {
                return EIO;
            }
            const auto* entry =
                reinterpret_cast<const LinuxDirent64*>(scratch->buffer.data() + offset);
            if (entry->reclen < header + 1 || entry->reclen % sizeof(uint64_t) != 0 ||
                entry->reclen > static_cast<size_t>(nread) - offset) {
                return EIO;
            }
            const size_t name_capacity = entry->reclen - header;
            const void* terminator = memchr(entry->name, '\0', name_capacity);
            if (!terminator) {
                return EIO;
            }
            const size_t name_len = static_cast<const char*>(terminator) - entry->name;
            if (name_len == 0 || memchr(entry->name, '/', name_len)) {
                return EIO;
            }
            if (name_len == 1 && entry->name[0] == '.') {
                if (seen_dot) {
                    return EIO;
                }
                seen_dot = true;
            } else if (name_len == 2 && entry->name[0] == '.' && entry->name[1] == '.') {
                if (seen_dotdot) {
                    return EIO;
                }
                seen_dotdot = true;
            } else {
                size_t index = 0;
                if (!readdir_entry_index(entry->name, name_len, opt.files, &index) ||
                    scratch->seen[index] || entry->ino == 0 || entry->type != DT_REG) {
                    return EIO;
                }
                scratch->seen[index] = true;
            }
            this_terminal_cookie = entry->off;
            offset += entry->reclen;
        }
        if (offset != static_cast<size_t>(nread) ||
            (have_terminal_cookie && this_terminal_cookie == terminal_cookie)) {
            return EIO;
        }
        terminal_cookie = this_terminal_cookie;
        have_terminal_cookie = true;
    }
    if (seen_dot != seen_dotdot ||
        std::find(scratch->seen.begin(), scratch->seen.end(), false) != scratch->seen.end()) {
        return EIO;
    }
    return 0;
}

int readdir_prepare_workload(const Options& opt, const std::string&) {
    const char* label = workload_name(WorkloadSpec::ReaddirPrepare);
    const uint64_t start = now_us();
    int err = 0;
    uint64_t ops = 0;
    phase_marker(opt, label, "prepare", "begin", 0, opt.files, 0, 0);
    DatasetWriteLock lock;
    if (!lock.acquire(opt, &err)) {
        phase_marker(opt, label, "prepare", "end", 0, opt.files, -1, err);
        emit_result(label, opt, now_us() - start, 0, 0, err);
        return -1;
    }
    int root_fd = open_dataset_dir(opt, true);
    if (root_fd < 0) {
        err = errno_or_eio();
    }
    for (size_t i = 0; err == 0 && i < opt.files; ++i) {
        const std::string name = readdir_entry_name(i);
        int fd = openat(root_fd, name.c_str(), O_CREAT | O_EXCL | O_WRONLY | O_CLOEXEC | O_NOFOLLOW,
                        0644);
        if (fd < 0 && errno == EEXIST) {
            struct stat st = {};
            if (fstatat(root_fd, name.c_str(), &st, AT_SYMLINK_NOFOLLOW) != 0) {
                err = errno_or_eio();
            } else if (!S_ISREG(st.st_mode) || st.st_size != 0) {
                err = EINVAL;
            }
        } else if (fd < 0) {
            err = errno_or_eio();
        } else {
            close_preserve_error(fd, &err);
        }
        if (err == 0) {
            ++ops;
        }
    }
    ReaddirScanCounters scan = {};
    ReaddirScanScratch scratch(opt.files);
    scratch.reset();
    if (err == 0 && lseek(root_fd, 0, SEEK_SET) < 0) {
        err = errno_or_eio();
    }
    if (err == 0) {
        err = scan_readdir_once(root_fd, opt, &scratch, &scan);
    }
    if (root_fd >= 0) {
        close_preserve_error(root_fd, &err);
    }
    phase_marker(opt, label, "prepare", "end", ops, opt.files, err == 0 ? 0 : -1, err);
    emit_result(label, opt, now_us() - start, 0, ops, err, {},
                err == 0 ? readdir_dataset_checksum(opt.files) : 0);
    return err == 0 ? 0 : -1;
}

int readdir_scan_workload(const Options& opt, const std::string&) {
    const char* label = workload_name(WorkloadSpec::ReaddirScan);
    int err = 0;
    int root_fd = open_dataset_dir(opt, false);
    if (root_fd < 0) {
        err = errno_or_eio();
        emit_result(label, opt, 0, 0, 0, err);
        return -1;
    }
    const bool verify_stats = !stats_path().empty();
    if (!wait_for_quiescence(label, "before")) {
        err = ETIMEDOUT;
    }
    StatsMap before;
    if (err == 0) {
        before = read_stats();
        if (verify_stats && before.empty()) {
            err = EIO;
        }
    }
    if (err != 0) {
        close_preserve_error(root_fd, &err);
        emit_result(label, opt, 0, 0, 0, err);
        return -1;
    }

    phase_marker(opt, label, "scan", "begin", 0, opt.files, 0, 0);
    ReaddirScanCounters scan = {};
    ReaddirScanScratch scratch(opt.files);
    uint64_t elapsed_us = 0;
    uint64_t cpu_us = 0;
    for (size_t iteration = 0; err == 0 && iteration < opt.iterations; ++iteration) {
        if (iteration != 0 && lseek(root_fd, 0, SEEK_SET) < 0) {
            err = errno_or_eio();
            break;
        }
        scratch.reset();
        const uint64_t cpu_start = process_cpu_us();
        const uint64_t wall_start = now_us();
        err = scan_readdir_once(root_fd, opt, &scratch, &scan);
        elapsed_us += now_us() - wall_start;
        const uint64_t cpu_end = process_cpu_us();
        if (cpu_end >= cpu_start) {
            cpu_us += cpu_end - cpu_start;
        }
    }
    phase_marker(opt, label, "scan", "end", scan.bytes, opt.files,
                 err == 0 ? static_cast<int64_t>(opt.files * opt.iterations) : -1, err);

    if (!wait_for_quiescence(label, "after")) {
        err = err == 0 ? ETIMEDOUT : err;
    }
    StatsMap after = read_stats();
    emit_stats_delta(label, before, after);
    if (err == 0 && verify_stats &&
        (after.empty() ||
         !validate_zero_copy_metrics(WorkloadSpec::ReaddirScan, opt, before, after) ||
         !require_readdir_no_nplusone(label, before, after))) {
        err = EIO;
    }
    close_preserve_error(root_fd, &err);

    IoCounters io = {};
    io.syscalls = scan.getdents_calls;
    const uint64_t ops = opt.files * opt.iterations;
    emit_result(label, opt, elapsed_us, scan.bytes, ops, err, io,
                err == 0 ? readdir_dataset_checksum(opt.files) : 0, {}, cpu_us);
    return err == 0 ? 0 : -1;
}

int readdir_cleanup_workload(const Options& opt, const std::string&) {
    const char* label = workload_name(WorkloadSpec::ReaddirCleanup);
    const uint64_t start = now_us();
    int err = 0;
    uint64_t ops = 0;
    phase_marker(opt, label, "cleanup", "begin", 0, opt.files, 0, 0);
    DatasetWriteLock lock;
    if (!lock.acquire(opt, &err)) {
        emit_result(label, opt, now_us() - start, 0, 0, err);
        return -1;
    }
    int root_fd = open_dataset_dir(opt, false);
    if (root_fd < 0) {
        if (errno != ENOENT) {
            err = errno_or_eio();
        }
    } else {
        for (size_t i = 0; i < opt.files; ++i) {
            const std::string name = readdir_entry_name(i);
            if (unlinkat(root_fd, name.c_str(), 0) == 0) {
                ++ops;
            } else if (errno != ENOENT) {
                record_first_error(&err, errno_or_eio());
            }
        }
        close_preserve_error(root_fd, &err);
    }
    int mount_fd = open(opt.mount.c_str(), O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW);
    if (mount_fd < 0) {
        record_first_error(&err, errno_or_eio());
    } else {
        const std::string dirname = ".virtiofs_bench_" + opt.path;
        if (unlinkat(mount_fd, dirname.c_str(), AT_REMOVEDIR) != 0 && errno != ENOENT) {
            record_first_error(&err, errno_or_eio());
        }
        close_preserve_error(mount_fd, &err);
    }
    phase_marker(opt, label, "cleanup", "end", ops, opt.files, err == 0 ? 0 : -1, err);
    emit_result(label, opt, now_us() - start, 0, ops, err);
    return err == 0 ? 0 : -1;
}

int sequential_write_phase(const Options& opt, WorkloadSpec workload) {
    const char* label = workload_name(workload);
    std::vector<unsigned char> data;
    try {
        data.resize(opt.file_size);
    } catch (const std::bad_alloc&) {
        emit_result(label, opt, 0, 0, 0, ENOMEM);
        return -1;
    }
    fill_pattern(data.data(), data.size(), opt.seed, 0);
    uint64_t checksum = hash_bytes(data.data(), data.size());

    int err = 0;
    DatasetWriteLock dataset_lock;
    if (!dataset_lock.acquire(opt, &err)) {
        emit_result(label, opt, 0, 0, 0, err);
        return -1;
    }

    int root_fd = open_dataset_dir(opt, true);
    if (root_fd < 0) {
        emit_result(label, opt, 0, 0, 0, errno_or_eio());
        return -1;
    }

    std::string temp = std::string(kSeqTempPrefix) + std::to_string(getpid());
    std::string manifest_temp = std::string(kManifestTempPrefix) + std::to_string(getpid());
    phase_marker(opt, label, "open", "begin", 0, 0, 0, 0);
    int fd = openat(root_fd, temp.c_str(),
                    O_CREAT | O_EXCL | O_WRONLY | O_CLOEXEC | O_NOFOLLOW, 0644);
    err = fd < 0 ? errno_or_eio() : 0;
    phase_marker(opt, label, "open", "end", 0, 0, fd, err);

    uint64_t bytes = 0;
    IoCounters io;
    uint64_t elapsed_us = 0;
    WritePhaseTimings write_timings;
    if (fd >= 0) {
        phase_marker(opt, label, "data_loop", "begin", 0, opt.file_size, 0, 0);
        const uint64_t end_to_end_start = now_us();
        uint64_t start = end_to_end_start;
        while (err == 0 && bytes < opt.file_size) {
            size_t requested = std::min(opt.block_size, opt.file_size - static_cast<size_t>(bytes));
            if (!write_all_counted(fd, data.data() + bytes, requested, &io)) {
                err = errno_or_eio();
                break;
            }
            bytes += requested;
        }
        elapsed_us = now_us() - start;
        write_timings.data_loop_us = elapsed_us;
        phase_marker(opt, label, "data_loop", "end", bytes, 0,
                     err == 0 ? static_cast<int64_t>(bytes) : -1, err);

        if (err == 0) {
            phase_marker(opt, label, "fsync", "begin", bytes, 0, 0, 0);
            start = now_us();
            fsync_preserve_error(fd, &err);
            write_timings.fsync_us = now_us() - start;
            phase_marker(opt, label, "fsync", "end", bytes, 0, err == 0 ? 0 : -1, err);
        }
        phase_marker(opt, label, "close", "begin", bytes, 0, 0, 0);
        start = now_us();
        close_preserve_error(fd, &err);
        const uint64_t close_end = now_us();
        write_timings.close_us = close_end - start;
        write_timings.end_to_end_us = close_end - end_to_end_start;
        phase_marker(opt, label, "close", "end", bytes, 0, err == 0 ? 0 : -1, err);
    }

    if (err == 0) {
        DatasetManifest manifest = {opt.file_size, opt.block_size, opt.seed, checksum};
        phase_marker(opt, label, "manifest", "begin", bytes, 0, 0, 0);
        if (write_manifest_temp(root_fd, manifest_temp, manifest, &err)) {
            publish_dataset(root_fd, temp, manifest_temp, &err);
        }
        phase_marker(opt, label, "manifest", "end", bytes, 0, err == 0 ? 0 : -1, err);
    }
    if (err != 0) {
        unlinkat(root_fd, temp.c_str(), 0);
        unlinkat(root_fd, manifest_temp.c_str(), 0);
    }
    close(root_fd);
    emit_result(label, opt, elapsed_us, bytes, io.syscalls, err, io, checksum, write_timings);
    emit_io_summary(label, io, checksum, 0);
    return err == 0 ? 0 : -1;
}

int sequential_read_phase(const Options& opt, const DatasetManifest* prepared_manifest = nullptr) {
    const char* label = workload_name(WorkloadSpec::SequentialRead);
    int err = 0;
    int root_fd = open_dataset_dir(opt, false);
    if (root_fd < 0) {
        emit_result(label, opt, 0, 0, 0, errno_or_eio());
        return -1;
    }

    DatasetManifest manifest;
    if (prepared_manifest) {
        manifest = *prepared_manifest;
    }
    if ((!prepared_manifest && !read_manifest(root_fd, &manifest, &err)) ||
        manifest.size != opt.file_size || manifest.size > std::numeric_limits<size_t>::max()) {
        if (err == 0) {
            err = EINVAL;
        }
        close(root_fd);
        emit_result(label, opt, 0, 0, 0, err);
        return -1;
    }

    std::vector<unsigned char> data;
    try {
        data.resize(static_cast<size_t>(manifest.size));
    } catch (const std::bad_alloc&) {
        close(root_fd);
        emit_result(label, opt, 0, 0, 0, ENOMEM);
        return -1;
    }

    phase_marker(opt, label, "open", "begin", 0, 0, 0, 0);
    int fd = openat(root_fd, kDatasetFile, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    err = fd < 0 ? errno_or_eio() : 0;
    if (fd >= 0) {
        struct stat st = {};
        if (fstat(fd, &st) != 0) {
            err = errno_or_eio();
        } else if (!S_ISREG(st.st_mode) || st.st_size < 0 ||
                   static_cast<uint64_t>(st.st_size) != manifest.size) {
            err = EINVAL;
        }
    }
    phase_marker(opt, label, "open", "end", 0, 0, fd, err);

    IoCounters io;
    uint64_t bytes = 0;
    uint64_t elapsed_us = 0;
    if (fd >= 0 && err == 0) {
        phase_marker(opt, label, "data_loop", "begin", 0, manifest.size, 0, 0);
        uint64_t start = now_us();
        while (err == 0 && bytes < manifest.size) {
            size_t requested = std::min(
                opt.block_size, static_cast<size_t>(manifest.size - bytes));
            ++io.syscalls;
            publish_last_syscall(io.syscalls, bytes, requested, 0, 0, 1);
            ssize_t n = read(fd, data.data() + bytes, requested);
            int read_error = n < 0 ? errno : 0;
            publish_last_syscall(io.syscalls, bytes, requested, n, read_error, 2);
            if (n < 0 && errno == EINTR) {
                ++io.eintr;
                continue;
            }
            if (n < 0) {
                err = errno_or_eio();
                break;
            }
            if (n == 0) {
                err = EIO;
                break;
            }
            if (static_cast<size_t>(n) != requested) {
                ++io.short_io;
            }
            bytes += static_cast<uint64_t>(n);
        }
        if (err == 0) {
            unsigned char extra = 0;
            for (;;) {
                ++io.syscalls;
                publish_last_syscall(io.syscalls, bytes, 1, 0, 0, 1);
                ssize_t n = read(fd, &extra, 1);
                int read_error = n < 0 ? errno : 0;
                publish_last_syscall(io.syscalls, bytes, 1, n, read_error, 2);
                if (n < 0 && errno == EINTR) {
                    ++io.eintr;
                    continue;
                }
                if (n < 0) {
                    err = errno_or_eio();
                } else if (n != 0) {
                    err = EOVERFLOW;
                }
                break;
            }
        }
        elapsed_us = now_us() - start;
        phase_marker(opt, label, "data_loop", "end", bytes, 0,
                     err == 0 ? static_cast<int64_t>(bytes) : -1, err);
    }
    if (fd >= 0) {
        phase_marker(opt, label, "close", "begin", bytes, 0, 0, 0);
        close_preserve_error(fd, &err);
        phase_marker(opt, label, "close", "end", bytes, 0, err == 0 ? 0 : -1, err);
    }
    close(root_fd);

    uint64_t checksum = 0;
    uint64_t verify_us = 0;
    if (err == 0) {
        phase_marker(opt, label, "verify", "begin", 0, manifest.size, 0, 0);
        uint64_t start = now_us();
        checksum = hash_bytes(data.data(), data.size());
        for (size_t i = 0; i < data.size(); ++i) {
            if (data[i] != pattern_byte(manifest.seed, i)) {
                err = EILSEQ;
                break;
            }
        }
        if (err == 0 && checksum != manifest.hash) {
            err = EILSEQ;
        }
        verify_us = now_us() - start;
        phase_marker(opt, label, "verify", "end", bytes, 0, err == 0 ? 0 : -1, err);
    }

    emit_result(label, opt, elapsed_us, bytes, io.syscalls, err, io, checksum);
    emit_io_summary(label, io, checksum, verify_us);
    return err == 0 ? 0 : -1;
}

int cleanup_workload(const Options& opt, const std::string&) {
    const char* label = workload_name(WorkloadSpec::Cleanup);
    int err = 0;
    phase_marker(opt, label, "cleanup", "begin", 0, 0, 0, 0);
    DatasetWriteLock dataset_lock;
    if (!dataset_lock.acquire(opt, &err)) {
        phase_marker(opt, label, "cleanup", "end", 0, 0, -1, err);
        emit_result(label, opt, 0, 0, 0, err);
        return -1;
    }
    int root_fd = open_dataset_dir(opt, false);
    if (root_fd < 0) {
        if (errno != ENOENT) {
            err = errno_or_eio();
        }
    } else {
        int scan_fd = dup(root_fd);
        DIR* dir = scan_fd < 0 ? nullptr : fdopendir(scan_fd);
        if (!dir) {
            if (scan_fd >= 0) {
                close(scan_fd);
            }
            record_first_error(&err, errno_or_eio());
        } else {
            for (;;) {
                errno = 0;
                dirent* entry = readdir(dir);
                if (!entry) {
                    if (errno != 0) {
                        record_first_error(&err, errno_or_eio());
                    }
                    break;
                }
                const std::string name = entry->d_name;
                const bool known = name == kDatasetFile || name == kManifestFile ||
                                   name.rfind(kSeqTempPrefix, 0) == 0 ||
                                   name.rfind(kManifestTempPrefix, 0) == 0 ||
                                   name.rfind(kSeqBackupPrefix, 0) == 0 ||
                                   name.rfind(kManifestBackupPrefix, 0) == 0;
                if (known && unlinkat(root_fd, name.c_str(), 0) != 0 && errno != ENOENT) {
                    record_first_error(&err, errno_or_eio());
                }
            }
            if (closedir(dir) != 0) {
                record_first_error(&err, errno_or_eio());
            }
        }
        close_preserve_error(root_fd, &err);
    }

    int mount_fd = open(opt.mount.c_str(), O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW);
    if (mount_fd < 0) {
        record_first_error(&err, errno_or_eio());
    } else {
        std::string dirname = ".virtiofs_bench_" + opt.path;
        if (unlinkat(mount_fd, dirname.c_str(), AT_REMOVEDIR) != 0 && errno != ENOENT) {
            record_first_error(&err, errno_or_eio());
        }
        close_preserve_error(mount_fd, &err);
    }
    phase_marker(opt, label, "cleanup", "end", 0, 0, err == 0 ? 0 : -1, err);
    emit_result(label, opt, 0, 0, 0, err);
    return err == 0 ? 0 : -1;
}

int prepare_workload(const Options& opt, const std::string&) {
    return sequential_write_phase(opt, WorkloadSpec::Prepare);
}

int sequential_write_workload(const Options& opt, const std::string&) {
    return sequential_write_phase(opt, WorkloadSpec::SequentialWrite);
}

int sequential_read_workload(const Options& opt, const std::string&) {
    return sequential_read_phase(opt);
}

bool preflight_sequential_read(const Options& opt, DatasetManifest* manifest) {
    int err = 0;
    int root_fd = open_dataset_dir(opt, false);
    if (root_fd < 0) {
        return false;
    }
    const bool valid = read_manifest(root_fd, manifest, &err) &&
                       manifest->size == opt.file_size &&
                       manifest->size <= std::numeric_limits<size_t>::max();
    close(root_fd);
    return valid && err == 0;
}

int sequential_workload(const Options& opt, const std::string&) {
    if (sequential_write_phase(opt, WorkloadSpec::SequentialWrite) != 0) {
        return -1;
    }
    return sequential_read_phase(opt);
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
    emit_result("mmap_scan", opt, now_us() - start, bytes, checksum, err, {}, checksum);
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
    bool read_only;
};

void* worker_main(void* raw) {
    WorkerArg* arg = static_cast<WorkerArg*>(raw);
    std::vector<char> buf(arg->opt.block_size, static_cast<char>('A' + (arg->id % 26)));
    std::string p = arg->read_only
                        ? path_join(arg->root, "concurrent_read.dat")
                        : path_join(arg->root, "worker_" + std::to_string(arg->id) + ".dat");
    int fd = open(p.c_str(), arg->read_only ? O_RDONLY : (O_CREAT | O_TRUNC | O_RDWR), 0644);
    if (fd < 0) {
        arg->err = errno;
        return nullptr;
    }
    for (size_t i = 0; i < arg->opt.iterations; ++i) {
        if (arg->read_only) {
            size_t span = arg->opt.file_size > buf.size() ? arg->opt.file_size - buf.size() : 0;
            off_t off = span == 0 ? 0 : static_cast<off_t>((i * buf.size()) % span);
            ssize_t n = pread(fd, buf.data(), buf.size(), off);
            if (n < 0) {
                arg->err = errno;
                break;
            }
            arg->bytes += static_cast<uint64_t>(n);
        } else {
            if (!write_full(fd, buf.data(), buf.size())) {
                arg->err = errno_or_eio();
                break;
            }
            arg->bytes += buf.size();
        }
        ++arg->ops;
    }
    if (!arg->read_only && arg->err == 0) {
        fsync_preserve_error(fd, &arg->err);
    }
    close_preserve_error(fd, &arg->err);
    return nullptr;
}

static int run_concurrent_phase(const Options& opt, const std::string& root, bool read_only,
                                const char* label) {
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
        args[i].read_only = read_only;
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
    emit_result(label, opt, now_us() - start, bytes, ops, err);
    return err == 0 ? 0 : -1;
}

int concurrent_workload(const Options& opt, const std::string& root) {
    int rc = run_concurrent_phase(opt, root, false, "concurrent_write");
    std::string path = path_join(root, "concurrent_read.dat");
    std::vector<char> block(opt.block_size, 'R');
    int fd = open(path.c_str(), O_CREAT | O_TRUNC | O_WRONLY, 0644);
    if (fd < 0)
        return -1;
    for (size_t done = 0; done < opt.file_size;) {
        size_t len = std::min(block.size(), opt.file_size - done);
        if (!write_full(fd, block.data(), len)) {
            close(fd);
            return -1;
        }
        done += len;
    }
    fsync(fd);
    close(fd);
    rc |= run_concurrent_phase(opt, root, true, "concurrent_read");
    return rc;
}

void usage(const char* argv0) {
    fprintf(stderr,
            "usage: %s --mount PATH [--workload all|metadata|readdir|readdir_prepare|"
            "readdir_scan|readdir_cleanup|sequential|prepare|sequential_write|"
            "sequential_read|cleanup|random_read|mmap|concurrent] "
            "[--path RELATIVE_DATASET] [--seed N] [--expect-dax always|never] [--files N] "
            "[--file-size N] [--block-size N] [--iterations N] [--workers N]\n",
            argv0);
    fprintf(stderr,
            "       %s [--tag hostshare] [--mount-options OPTIONS] [workload options...]\n",
            argv0);
    fprintf(stderr,
            "If --mount is omitted, the benchmark mounts the virtiofs tag on a temporary "
            "directory and unmounts it before exit.\n");
}

constexpr size_t kMaxFiles = 1000000;
constexpr size_t kMaxFileSize = 1024ULL * 1024 * 1024;
constexpr size_t kMaxBlockSize = 16ULL * 1024 * 1024;
constexpr size_t kMaxIterations = 100000000;
constexpr size_t kMaxWorkers = 1024;

bool parse_size(const char* name, const char* s, size_t maximum, size_t* out) {
    if (!s || s[0] == '\0' || s[0] == '+' || s[0] == '-') {
        fprintf(stderr, "%s must be an unsigned decimal integer no greater than %zu\n", name,
                maximum);
        return false;
    }
    for (const unsigned char* p = reinterpret_cast<const unsigned char*>(s); *p; ++p) {
        if (*p < '0' || *p > '9') {
            fprintf(stderr, "%s must be an unsigned decimal integer no greater than %zu\n",
                    name, maximum);
            return false;
        }
    }
    char* end = nullptr;
    errno = 0;
    unsigned long long v = strtoull(s, &end, 10);
    if (errno != 0 || end == s || *end != '\0' || v > maximum ||
        v > static_cast<unsigned long long>(std::numeric_limits<size_t>::max())) {
        fprintf(stderr, "%s must be an unsigned decimal integer no greater than %zu\n", name,
                maximum);
        return false;
    }
    *out = static_cast<size_t>(v);
    return true;
}

bool parse_u64(const char* s, uint64_t* out) {
    if (!s || s[0] == '\0' || s[0] == '+' || s[0] == '-') {
        return false;
    }
    char* end = nullptr;
    errno = 0;
    unsigned long long value = strtoull(s, &end, 0);
    if (errno != 0 || end == s || *end != '\0') {
        return false;
    }
    *out = static_cast<uint64_t>(value);
    return true;
}

bool valid_dataset_component(const std::string& path) {
    if (path.empty() || path == "." || path == ".." || path.size() > 128) {
        return false;
    }
    for (unsigned char c : path) {
        if (!((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') ||
              (c >= '0' && c <= '9') || c == '.' || c == '_' || c == '-')) {
            return false;
        }
    }
    return true;
}

bool is_dataset_lifecycle_workload(WorkloadSpec workload) {
    return workload == WorkloadSpec::Prepare || workload == WorkloadSpec::SequentialWrite ||
           workload == WorkloadSpec::SequentialRead || workload == WorkloadSpec::Cleanup ||
           workload == WorkloadSpec::ReaddirPrepare || workload == WorkloadSpec::ReaddirScan ||
           workload == WorkloadSpec::ReaddirCleanup;
}

bool mount_options_contain(const std::string& options, const std::string& expected) {
    std::istringstream stream(options);
    std::string option;
    while (std::getline(stream, option, ',')) {
        if (option == expected) {
            return true;
        }
    }
    return false;
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
        } else if (strcmp(argv[i], "--expect-dax") == 0) {
            const char* v = need(argv[i]);
            if (!v) return false;
            opt->expect_dax = v;
        } else if (strcmp(argv[i], "--workload") == 0) {
            const char* v = need(argv[i]);
            if (!v) return false;
            if (!parse_workload(v, &opt->workload)) {
                fprintf(stderr, "unknown workload: %s\n", v);
                return false;
            }
        } else if (strcmp(argv[i], "--path") == 0) {
            const char* v = need(argv[i]);
            if (!v) return false;
            opt->path = v;
        } else if (strcmp(argv[i], "--seed") == 0) {
            const char* v = need(argv[i]);
            if (!v || !parse_u64(v, &opt->seed)) return false;
        } else if (strcmp(argv[i], "--files") == 0) {
            const char* v = need(argv[i]);
            if (!v || !parse_size("--files", v, kMaxFiles, &opt->files)) return false;
        } else if (strcmp(argv[i], "--file-size") == 0) {
            const char* v = need(argv[i]);
            if (!v || !parse_size("--file-size", v, kMaxFileSize, &opt->file_size)) return false;
        } else if (strcmp(argv[i], "--block-size") == 0) {
            const char* v = need(argv[i]);
            if (!v || !parse_size("--block-size", v, kMaxBlockSize, &opt->block_size)) return false;
        } else if (strcmp(argv[i], "--iterations") == 0) {
            const char* v = need(argv[i]);
            if (!v || !parse_size("--iterations", v, kMaxIterations, &opt->iterations)) return false;
            opt->iterations_explicit = true;
        } else if (strcmp(argv[i], "--workers") == 0) {
            const char* v = need(argv[i]);
            if (!v || !parse_size("--workers", v, kMaxWorkers, &opt->workers)) return false;
        } else {
            return false;
        }
    }
    if (!opt->path.empty() && !valid_dataset_component(opt->path)) {
        fprintf(stderr, "--path must be one safe relative dataset component\n");
        return false;
    }
    if (is_dataset_lifecycle_workload(opt->workload) && opt->path.empty()) {
        fprintf(stderr, "%s requires --path RELATIVE_DATASET\n", workload_name(opt->workload));
        return false;
    }
    if (opt->workload == WorkloadSpec::ReaddirScan && !opt->iterations_explicit) {
        opt->iterations = 1;
    }
    if ((opt->workload == WorkloadSpec::ReaddirPrepare ||
         opt->workload == WorkloadSpec::ReaddirScan ||
         opt->workload == WorkloadSpec::ReaddirCleanup) &&
        (opt->files == 0 || opt->iterations == 0)) {
        fprintf(stderr, "%s requires non-zero --files and --iterations\n",
                workload_name(opt->workload));
        return false;
    }
    if (opt->tag.empty()) {
        fprintf(stderr, "--tag must not be empty\n");
        return false;
    }
    if (!opt->expect_dax.empty() && opt->expect_dax != "always" &&
        opt->expect_dax != "never") {
        fprintf(stderr, "--expect-dax must be always or never\n");
        return false;
    }
    if (!opt->expect_dax.empty() && opt->workload != WorkloadSpec::All &&
        !is_dax_data_workload(opt->workload)) {
        fprintf(stderr, "--expect-dax requires all or a data workload\n");
        return false;
    }
    if (!opt->expect_dax.empty() && stats_path().empty()) {
        fprintf(stderr, "--expect-dax requires VIRTIOFS_STATS_PATH\n");
        return false;
    }
    if (!opt->expect_dax.empty() && opt->mount.empty() &&
        !mount_options_contain(opt->mount_options, "dax=" + opt->expect_dax)) {
        fprintf(stderr,
                "automatic mounts with --expect-dax require matching dax=always|never in "
                "--mount-options\n");
        return false;
    }
    return opt->file_size != 0 && opt->block_size != 0 && opt->block_size <= SSIZE_MAX &&
           opt->workers != 0;
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

int run_with_stats_delta(WorkloadSpec spec, int (*workload)(const Options&, const std::string&),
                         const Options& opt, const std::string& root) {
    const char* name = workload_name(spec);
    const bool verify_stats = !stats_path().empty();
    DatasetManifest prepared_manifest;
    // The manifest is control-plane input, not measured file payload.  Load
    // and validate it before the counter baseline so its first FUSE_READ does
    // not contaminate sequential-read request-size evidence.
    if (spec == WorkloadSpec::SequentialRead &&
        !preflight_sequential_read(opt, &prepared_manifest)) {
        fprintf(stderr, "preflight workload=%s status=fail reason=manifest_invalid\n", name);
        return -1;
    }
    if (!wait_for_quiescence(name, "before")) {
        return -1;
    }
    StatsMap before = read_stats();
    if (verify_stats && before.empty()) {
        fprintf(stderr, "stats_assert workload=%s status=fail reason=baseline_unavailable\n", name);
        return -1;
    }
    int rc = spec == WorkloadSpec::SequentialRead
                 ? sequential_read_phase(opt, &prepared_manifest)
                 : workload(opt, root);
    if (!wait_for_quiescence(name, "after")) {
        rc = -1;
    }
    StatsMap after = read_stats();
    emit_stats_delta(name, before, after);
    if (rc == 0 && verify_stats) {
        bool skip_read_transport_assert =
            opt.expect_dax == "always" && is_dax_data_workload(spec);
        if (after.empty() ||
            (!skip_read_transport_assert && !validate_zero_copy_metrics(spec, opt, before, after)) ||
            !validate_dax_metrics(spec, opt, before, after)) {
            return -1;
        }
    }
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

    if (opt.path.empty()) {
        opt.path = std::to_string(getpid());
    }
    std::string root = path_join(opt.mount, ".virtiofs_bench_" + opt.path);
    if (opt.workload != WorkloadSpec::Cleanup &&
        opt.workload != WorkloadSpec::ReaddirCleanup &&
        opt.workload != WorkloadSpec::ReaddirScan) {
        int root_fd = open_dataset_dir(opt, true);
        if (root_fd < 0) {
            fprintf(stderr, "failed to create safe dataset %s: %s\n", root.c_str(),
                    strerror(errno));
            return 1;
        }
        close(root_fd);
    }

    int rc = 0;
    if (opt.workload == WorkloadSpec::All || opt.workload == WorkloadSpec::Metadata) {
        rc |= run_with_stats_delta(WorkloadSpec::Metadata, metadata_workload, opt, root);
    }
    if (opt.workload == WorkloadSpec::All || opt.workload == WorkloadSpec::Readdir) {
        rc |= run_with_stats_delta(WorkloadSpec::Readdir, readdir_workload, opt, root);
    }
    if (opt.workload == WorkloadSpec::ReaddirPrepare) {
        rc |= run_with_stats_delta(WorkloadSpec::ReaddirPrepare, readdir_prepare_workload, opt,
                                   root);
    }
    if (opt.workload == WorkloadSpec::ReaddirScan) {
        rc |= readdir_scan_workload(opt, root);
    }
    if (opt.workload == WorkloadSpec::ReaddirCleanup) {
        rc |= run_with_stats_delta(WorkloadSpec::ReaddirCleanup, readdir_cleanup_workload, opt,
                                   root);
    }
    if (opt.workload == WorkloadSpec::All || opt.workload == WorkloadSpec::Sequential) {
        rc |= run_with_stats_delta(WorkloadSpec::Sequential, sequential_workload, opt, root);
    }
    if (opt.workload == WorkloadSpec::Prepare) {
        rc |= run_with_stats_delta(WorkloadSpec::Prepare, prepare_workload, opt, root);
    }
    if (opt.workload == WorkloadSpec::SequentialWrite) {
        rc |= run_with_stats_delta(WorkloadSpec::SequentialWrite, sequential_write_workload, opt,
                                   root);
    }
    if (opt.workload == WorkloadSpec::SequentialRead) {
        rc |= run_with_stats_delta(WorkloadSpec::SequentialRead, sequential_read_workload, opt,
                                   root);
    }
    if (opt.workload == WorkloadSpec::Cleanup) {
        rc |= run_with_stats_delta(WorkloadSpec::Cleanup, cleanup_workload, opt, root);
    }
    if (opt.workload == WorkloadSpec::All || opt.workload == WorkloadSpec::RandomRead) {
        if (prepare_read_input("random_read", opt, path_join(root, "random.dat"), 'R') == 0) {
            rc |= run_with_stats_delta(WorkloadSpec::RandomRead, random_read_workload, opt, root);
        } else {
            rc |= -1;
        }
    }
    if (opt.workload == WorkloadSpec::All || opt.workload == WorkloadSpec::Mmap) {
        if (prepare_read_input("mmap_scan", opt, path_join(root, "mmap.dat"), 'M') == 0) {
            rc |= run_with_stats_delta(WorkloadSpec::Mmap, mmap_scan_workload, opt, root);
        } else {
            rc |= -1;
        }
    }
    if (opt.workload == WorkloadSpec::All || opt.workload == WorkloadSpec::Concurrent) {
        rc |= run_with_stats_delta(WorkloadSpec::Concurrent, concurrent_workload, opt, root);
    }

    return rc == 0 ? 0 : 1;
}
