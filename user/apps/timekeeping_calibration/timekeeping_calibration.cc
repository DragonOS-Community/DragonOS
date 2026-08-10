#include <errno.h>
#include <inttypes.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

#include <limits>
#include <array>
#include <initializer_list>
#include <string>
#include <unordered_map>
#include <vector>

namespace {

constexpr const char* kProtocol = "TKCAL/2";
constexpr uint64_t kMigrationStride = 4096;
constexpr int kMigrationWaitRounds = 200000;
constexpr size_t kMaxCommandLine = 1024;
constexpr uint64_t kReadsRequired = 10000000;
constexpr uint64_t kTarget10Seconds = 10000000000ULL;
constexpr uint64_t kTarget60Seconds = 60000000000ULL;

using Fields = std::unordered_map<std::string, std::string>;

struct Command {
    std::string verb;
    Fields fields;
};

struct ReadStats {
    uint64_t raw_reads = 0;
    uint64_t mono_reads = 0;
    uint64_t raw_regressions = 0;
    uint64_t mono_regressions = 0;
    uint64_t raw_max_backward_ns = 0;
    uint64_t mono_max_backward_ns = 0;
    uint64_t migrations_requested = 0;
    uint64_t migrations_observed = 0;
    uint64_t cpu_mask_seen = 0;
};

bool ToNs(clockid_t clock_id, uint64_t* value) {
    timespec ts = {};
    if (clock_gettime(clock_id, &ts) != 0 || ts.tv_sec < 0 || ts.tv_nsec < 0 ||
        ts.tv_nsec >= 1000000000L) {
        return false;
    }
    const auto seconds = static_cast<uint64_t>(ts.tv_sec);
    if (seconds > (std::numeric_limits<uint64_t>::max() - static_cast<uint64_t>(ts.tv_nsec)) /
                      1000000000ULL) {
        errno = EOVERFLOW;
        return false;
    }
    *value = seconds * 1000000000ULL + static_cast<uint64_t>(ts.tv_nsec);
    return true;
}

int CurrentCpu() {
    unsigned int cpu = 0;
    if (syscall(SYS_getcpu, &cpu, nullptr, nullptr) != 0) {
        return -1;
    }
    return static_cast<int>(cpu);
}

std::vector<int> AllowedCpus() {
    cpu_set_t set;
    CPU_ZERO(&set);
    std::vector<int> cpus;
    if (sched_getaffinity(0, sizeof(set), &set) != 0) {
        return cpus;
    }
    for (int cpu = 0; cpu < CPU_SETSIZE; ++cpu) {
        if (CPU_ISSET(cpu, &set)) {
            cpus.push_back(cpu);
        }
    }
    return cpus;
}

bool PinCpu(int cpu) {
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(cpu, &set);
    return sched_setaffinity(0, sizeof(set), &set) == 0;
}

bool WaitForCpu(int cpu) {
    for (int i = 0; i < kMigrationWaitRounds; ++i) {
        if (CurrentCpu() == cpu) {
            return true;
        }
        sched_yield();
    }
    errno = ETIMEDOUT;
    return false;
}

bool ParseU64(const std::string& text, uint64_t* value) {
    if (text.empty() || text[0] == '-') {
        return false;
    }
    errno = 0;
    char* end = nullptr;
    const unsigned long long parsed = strtoull(text.c_str(), &end, 10);
    if (errno != 0 || end == text.c_str() || *end != '\0') {
        return false;
    }
    *value = static_cast<uint64_t>(parsed);
    return true;
}

bool SafeToken(const std::string& text) {
    if (text.empty() || text.size() > 128) {
        return false;
    }
    for (unsigned char ch : text) {
        if (!(ch == '_' || ch == '-' || ch == '.' ||
              (ch >= '0' && ch <= '9') || (ch >= 'A' && ch <= 'Z') ||
              (ch >= 'a' && ch <= 'z'))) {
            return false;
        }
    }
    return true;
}

bool ValidRunId(const std::string& text) {
    if (text.size() != 32) {
        return false;
    }
    for (const unsigned char ch : text) {
        if (!((ch >= '0' && ch <= '9') || (ch >= 'a' && ch <= 'f'))) {
            return false;
        }
    }
    return true;
}

bool ParseCommand(const char* line, Command* command) {
    command->verb.clear();
    command->fields.clear();
    std::string input(line);
    while (!input.empty() && (input.back() == '\n' || input.back() == '\r')) {
        input.pop_back();
    }
    size_t cursor = 0;
    while (cursor < input.size()) {
        const size_t next = input.find(' ', cursor);
        const std::string token = input.substr(cursor, next == std::string::npos
                                                          ? std::string::npos
                                                          : next - cursor);
        if (!token.empty()) {
            if (command->verb.empty()) {
                command->verb = token;
            } else {
                const size_t equal = token.find('=');
                if (equal == std::string::npos || equal == 0 || equal + 1 == token.size()) {
                    return false;
                }
                const std::string key = token.substr(0, equal);
                const std::string value = token.substr(equal + 1);
                if (!SafeToken(key) || !SafeToken(value) || !command->fields.emplace(key, value).second) {
                    return false;
                }
            }
        }
        if (next == std::string::npos) {
            break;
        }
        cursor = next + 1;
    }
    return command->verb == "START" || command->verb == "END" || command->verb == "QUIT";
}

bool HasExactFields(const Command& command, std::initializer_list<const char*> names) {
    if (command.fields.size() != names.size()) {
        return false;
    }
    for (const char* name : names) {
        if (command.fields.find(name) == command.fields.end()) {
            return false;
        }
    }
    return true;
}

enum class LineResult { kOk, kEof, kTooLong };

LineResult ReadBoundedLine(std::string* line) {
    std::array<char, kMaxCommandLine + 2> buffer = {};
    if (fgets(buffer.data(), buffer.size(), stdin) == nullptr) {
        return LineResult::kEof;
    }
    const size_t length = strlen(buffer.data());
    const bool has_newline = length > 0 && buffer[length - 1] == '\n';
    if (!has_newline && !feof(stdin)) {
        int ch = 0;
        while ((ch = fgetc(stdin)) != '\n' && ch != EOF) {
        }
        return LineResult::kTooLong;
    }
    if (length > kMaxCommandLine + (has_newline ? 1U : 0U)) {
        return LineResult::kTooLong;
    }
    line->assign(buffer.data(), length);
    return LineResult::kOk;
}

bool ValidWorkload(const std::string& mode, const std::string& affinity, uint64_t target_ns,
                   uint64_t reads) {
    if ((mode == "sleep" || mode == "busy") && affinity == "fixed") {
        return reads == 0 && (target_ns == kTarget10Seconds || target_ns == kTarget60Seconds);
    }
    if (mode == "reads" && (affinity == "fixed" || affinity == "migrate")) {
        return target_ns == 0 && reads == kReadsRequired;
    }
    return false;
}

const std::string* Field(const Command& command, const char* name) {
    const auto iterator = command.fields.find(name);
    return iterator == command.fields.end() ? nullptr : &iterator->second;
}

void EmitReady(const std::string& run_id, uint64_t seq, const std::vector<int>& cpus) {
    printf("%s READY run=%s seq=%" PRIu64 " cpus=", kProtocol, run_id.c_str(), seq);
    if (cpus.empty()) {
        printf("none");
    }
    for (size_t i = 0; i < cpus.size(); ++i) {
        printf("%s%d", i == 0 ? "" : ",", cpus[i]);
    }
    printf("\n");
}

void EmitProtocolError(const std::string& run_id, uint64_t seq, const char* reason) {
    printf("%s ERROR run=%s seq=%" PRIu64 " reason=%s\n", kProtocol, run_id.c_str(), seq,
           reason);
}

bool SleepFor(uint64_t target_ns) {
    timespec request = {static_cast<time_t>(target_ns / 1000000000ULL),
                        static_cast<long>(target_ns % 1000000000ULL)};
    while (true) {
        timespec remaining = {};
        const int result = clock_nanosleep(CLOCK_MONOTONIC, 0, &request, &remaining);
        if (result == 0) {
            return true;
        }
        if (result != EINTR) {
            errno = result;
            return false;
        }
        request = remaining;
    }
}

bool BusyFor(uint64_t start_raw_ns, uint64_t target_ns, uint64_t* checksum) {
    if (target_ns > std::numeric_limits<uint64_t>::max() - start_raw_ns) {
        errno = EOVERFLOW;
        return false;
    }
    const uint64_t deadline = start_raw_ns + target_ns;
    uint64_t now = start_raw_ns;
    uint64_t state = UINT64_C(0x9e3779b97f4a7c15);
    while (now < deadline) {
        for (int i = 0; i < 4096; ++i) {
            state ^= state << 7;
            state ^= state >> 9;
            state *= UINT64_C(0xd6e8feb86659fd93);
        }
        if (!ToNs(CLOCK_MONOTONIC_RAW, &now)) {
            return false;
        }
    }
    *checksum = state;
    return true;
}

void ObserveRead(uint64_t previous, uint64_t current, uint64_t* regressions,
                 uint64_t* max_backward) {
    if (current < previous) {
        ++*regressions;
        const uint64_t backward = previous - current;
        if (backward > *max_backward) {
            *max_backward = backward;
        }
    }
}

bool RunReads(uint64_t reads, bool migrate, const std::vector<int>& cpus, ReadStats* stats,
              const char** reason) {
    if (cpus.empty() || (migrate && cpus.size() < 2)) {
        *reason = migrate ? "requires_two_cpus" : "no_allowed_cpu";
        return false;
    }
    if (!PinCpu(cpus[0]) || !WaitForCpu(cpus[0])) {
        *reason = "initial_affinity_failed";
        return false;
    }

    uint64_t previous_raw = 0;
    uint64_t previous_mono = 0;
    if (!ToNs(CLOCK_MONOTONIC_RAW, &previous_raw) ||
        !ToNs(CLOCK_MONOTONIC, &previous_mono)) {
        *reason = "clock_read_failed";
        return false;
    }
    const int initial_cpu = CurrentCpu();
    if (initial_cpu >= 0 && initial_cpu < 64) {
        stats->cpu_mask_seen |= UINT64_C(1) << initial_cpu;
    }
    int target_index = 0;
    for (uint64_t i = 0; i < reads; ++i) {
        if (migrate && i != 0 && i % kMigrationStride == 0) {
            target_index ^= 1;
            ++stats->migrations_requested;
            if (!PinCpu(cpus[target_index]) || !WaitForCpu(cpus[target_index])) {
                *reason = "migration_timeout";
                return false;
            }
            ++stats->migrations_observed;
            const int cpu = CurrentCpu();
            if (cpu >= 0 && cpu < 64) {
                stats->cpu_mask_seen |= UINT64_C(1) << cpu;
            }
        }
        uint64_t current_raw = 0;
        uint64_t current_mono = 0;
        if (!ToNs(CLOCK_MONOTONIC_RAW, &current_raw) ||
            !ToNs(CLOCK_MONOTONIC, &current_mono)) {
            *reason = "clock_read_failed";
            return false;
        }
        ObserveRead(previous_raw, current_raw, &stats->raw_regressions,
                    &stats->raw_max_backward_ns);
        ObserveRead(previous_mono, current_mono, &stats->mono_regressions,
                    &stats->mono_max_backward_ns);
        previous_raw = current_raw;
        previous_mono = current_mono;
        ++stats->raw_reads;
        ++stats->mono_reads;
    }
    return true;
}

void DisableTerminalEcho(termios* saved, bool* valid) {
    *valid = false;
    if (!isatty(STDIN_FILENO) || tcgetattr(STDIN_FILENO, saved) != 0) {
        return;
    }
    termios current = *saved;
    current.c_lflag &= static_cast<tcflag_t>(~ECHO);
    if (tcsetattr(STDIN_FILENO, TCSANOW, &current) == 0) {
        *valid = true;
    }
}

}  // namespace

int main(int argc, char** argv) {
    if (argc != 3 || strcmp(argv[1], "--run-id") != 0 || !ValidRunId(argv[2])) {
        fprintf(stderr, "usage: %s --run-id 32_LOWERCASE_HEX_DIGITS\n", argv[0]);
        return 2;
    }
    const std::string run_id(argv[2]);
    setvbuf(stdout, nullptr, _IONBF, 0);

    termios saved = {};
    bool restore_terminal = false;
    DisableTerminalEcho(&saved, &restore_terminal);

    const std::vector<int> cpus = AllowedCpus();
    uint64_t expected_seq = 1;
    bool completed = false;
    EmitReady(run_id, 0, cpus);

    std::string line;
    while (true) {
        const LineResult start_line = ReadBoundedLine(&line);
        if (start_line == LineResult::kEof) {
            break;
        }
        if (start_line == LineResult::kTooLong) {
            EmitProtocolError(run_id, expected_seq, "line_too_long");
            continue;
        }
        Command start;
        if (!ParseCommand(line.c_str(), &start)) {
            EmitProtocolError(run_id, expected_seq, "invalid_start");
            continue;
        }
        if (start.verb == "QUIT") {
            const std::string* command_run = Field(start, "run");
            const std::string* seq_text = Field(start, "seq");
            uint64_t seq = 0;
            if (!HasExactFields(start, {"run", "seq"}) || command_run == nullptr ||
                seq_text == nullptr || *command_run != run_id || !ParseU64(*seq_text, &seq) ||
                seq != expected_seq) {
                EmitProtocolError(run_id, expected_seq, "invalid_quit");
                continue;
            }
            if (restore_terminal) {
                tcsetattr(STDIN_FILENO, TCSANOW, &saved);
                restore_terminal = false;
            }
            printf("%s ACK run=%s seq=%" PRIu64 " status=ok\n", kProtocol, run_id.c_str(), seq);
            completed = true;
            break;
        }
        if (start.verb != "START" ||
            !HasExactFields(start, {"run", "seq", "case", "mode", "affinity", "target_ns",
                                    "reads"})) {
            EmitProtocolError(run_id, expected_seq, "invalid_start");
            continue;
        }
        const std::string* command_run = Field(start, "run");
        const std::string* seq_text = Field(start, "seq");
        const std::string* case_id = Field(start, "case");
        const std::string* mode = Field(start, "mode");
        const std::string* affinity = Field(start, "affinity");
        const std::string* target_text = Field(start, "target_ns");
        const std::string* reads_text = Field(start, "reads");
        uint64_t seq = 0;
        uint64_t target_ns = 0;
        uint64_t reads = 0;
        if (command_run == nullptr || seq_text == nullptr || case_id == nullptr || mode == nullptr ||
            affinity == nullptr || target_text == nullptr || reads_text == nullptr ||
            *command_run != run_id || !SafeToken(*case_id) || !ParseU64(*seq_text, &seq) ||
            !ParseU64(*target_text, &target_ns) || !ParseU64(*reads_text, &reads) ||
            seq != expected_seq || !ValidWorkload(*mode, *affinity, target_ns, reads)) {
            EmitProtocolError(run_id, expected_seq, "invalid_fields_or_sequence");
            continue;
        }

        const bool migrate = *affinity == "migrate";
        bool setup_ok = (*affinity == "fixed" || migrate) && !cpus.empty();
        const char* reason = "ok";
        if (!setup_ok) {
            reason = cpus.empty() ? "no_allowed_cpu" : "invalid_affinity";
        }
        if (setup_ok && !migrate) {
            setup_ok = PinCpu(cpus[0]) && WaitForCpu(cpus[0]);
            if (!setup_ok) {
                reason = "initial_affinity_failed";
            }
        }

        uint64_t guest_start_raw = 0;
        uint64_t guest_start_mono = 0;
        setup_ok = setup_ok && ToNs(CLOCK_MONOTONIC_RAW, &guest_start_raw) &&
                   ToNs(CLOCK_MONOTONIC, &guest_start_mono);
        if (!setup_ok && strcmp(reason, "ok") == 0) {
            reason = "start_clock_failed";
        }
        printf("%s START run=%s seq=%" PRIu64 " case=%s guest_raw_ns=%" PRIu64
               " guest_mono_ns=%" PRIu64 " cpu=%d status=%s\n",
               kProtocol, run_id.c_str(), seq, case_id->c_str(), guest_start_raw,
               guest_start_mono, CurrentCpu(), setup_ok ? "ok" : "fail");

        uint64_t checksum = 0;
        ReadStats stats;
        bool workload_ok = setup_ok;
        if (workload_ok && *mode == "sleep") {
            workload_ok = reads == 0 && SleepFor(target_ns);
            if (!workload_ok) reason = "sleep_failed";
        } else if (workload_ok && *mode == "busy") {
            workload_ok = reads == 0 && BusyFor(guest_start_raw, target_ns, &checksum);
            if (!workload_ok) reason = "busy_failed";
        } else if (workload_ok && *mode == "reads") {
            workload_ok = target_ns == 0 && reads > 0 &&
                          RunReads(reads, migrate, cpus, &stats, &reason);
        } else if (workload_ok) {
            workload_ok = false;
            reason = "unsupported_mode";
        }

        uint64_t work_end_raw = 0;
        uint64_t work_end_mono = 0;
        if (!ToNs(CLOCK_MONOTONIC_RAW, &work_end_raw) ||
            !ToNs(CLOCK_MONOTONIC, &work_end_mono)) {
            workload_ok = false;
            reason = "work_end_clock_failed";
        }
        printf("%s WORK_DONE run=%s seq=%" PRIu64 " case=%s status=%s reason=%s"
               " work_end_raw_ns=%" PRIu64 " work_end_mono_ns=%" PRIu64
               " checksum=%016" PRIx64 " raw_reads=%" PRIu64 " mono_reads=%" PRIu64
               " raw_regressions=%" PRIu64 " mono_regressions=%" PRIu64
               " raw_max_backward_ns=%" PRIu64 " mono_max_backward_ns=%" PRIu64
               " migrations_requested=%" PRIu64 " migrations_observed=%" PRIu64
               " cpu_mask_seen=%016" PRIx64 "\n",
               kProtocol, run_id.c_str(), seq, case_id->c_str(), workload_ok ? "ok" : "fail",
               reason, work_end_raw, work_end_mono, checksum, stats.raw_reads, stats.mono_reads,
               stats.raw_regressions, stats.mono_regressions, stats.raw_max_backward_ns,
               stats.mono_max_backward_ns, stats.migrations_requested,
               stats.migrations_observed, stats.cpu_mask_seen);

        const LineResult end_line = ReadBoundedLine(&line);
        if (end_line == LineResult::kEof) {
            break;
        }
        if (end_line == LineResult::kTooLong) {
            EmitProtocolError(run_id, expected_seq, "line_too_long");
            break;
        }
        Command end;
        uint64_t end_seq = 0;
        const bool end_ok = ParseCommand(line.c_str(), &end) && end.verb == "END" &&
                            HasExactFields(end, {"run", "seq"}) &&
                            Field(end, "run") != nullptr && *Field(end, "run") == run_id &&
                            Field(end, "seq") != nullptr && ParseU64(*Field(end, "seq"), &end_seq) &&
                            end_seq == seq;
        uint64_t guest_end_raw = 0;
        uint64_t guest_end_mono = 0;
        const bool end_clock_ok = ToNs(CLOCK_MONOTONIC_RAW, &guest_end_raw) &&
                                  ToNs(CLOCK_MONOTONIC, &guest_end_mono);
        printf("%s END run=%s seq=%" PRIu64 " case=%s guest_raw_ns=%" PRIu64
               " guest_mono_ns=%" PRIu64 " cpu=%d status=%s\n",
               kProtocol, run_id.c_str(), seq, case_id->c_str(), guest_end_raw, guest_end_mono,
               CurrentCpu(), end_ok && end_clock_ok && workload_ok ? "ok" : "fail");
        if (!end_ok) {
            EmitProtocolError(run_id, expected_seq, "invalid_end");
            break;
        }
        ++expected_seq;
        EmitReady(run_id, expected_seq - 1, cpus);
    }

    if (restore_terminal) {
        tcsetattr(STDIN_FILENO, TCSANOW, &saved);
    }
    return completed ? 0 : 1;
}
