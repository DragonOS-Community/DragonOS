#include <gtest/gtest.h>

#include <fcntl.h>
#include <linux/bpf.h>
#include <linux/capability.h>
#include <sys/stat.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cerrno>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <functional>
#include <string>
#include <vector>

namespace {

class ScopedFd {
 public:
  explicit ScopedFd(int fd = -1) : fd_(fd) {}
  ~ScopedFd() { Reset(); }
  ScopedFd(const ScopedFd&) = delete;
  ScopedFd& operator=(const ScopedFd&) = delete;
  int get() const { return fd_; }
  void Reset(int fd = -1) {
    if (fd_ >= 0) close(fd_);
    fd_ = fd;
  }

 private:
  int fd_;
};

bool IsDragonOS() {
  struct utsname uts {};
  return uname(&uts) == 0 && std::strstr(uts.release, "dragonos") != nullptr;
}

bool HasCapability(int capability) {
  __user_cap_header_struct header{};
  header.version = _LINUX_CAPABILITY_VERSION_3;
  __user_cap_data_struct data[2]{};
  if (syscall(SYS_capget, &header, data) != 0) return false;
  return data[capability / 32].effective & (uint32_t{1} << (capability % 32));
}

bpf_insn Insn(uint8_t code, uint8_t dst, uint8_t src = 0, int16_t off = 0,
              int32_t imm = 0) {
  bpf_insn result{};
  result.code = code;
  result.dst_reg = dst;
  result.src_reg = src;
  result.off = off;
  result.imm = imm;
  return result;
}

std::vector<bpf_insn> AllowAll() {
  return {Insn(BPF_ALU | BPF_MOV | BPF_K, 0, 0, 0, 1),
          Insn(BPF_JMP | BPF_EXIT, 0)};
}

std::vector<bpf_insn> DenyAll() {
  return {Insn(BPF_ALU | BPF_MOV | BPF_K, 0),
          Insn(BPF_JMP | BPF_EXIT, 0)};
}

// Deny only O_RDONLY access to /dev/null (character device 1:3), leaving
// mknod, F_OK and all unrelated devices permitted. The form mirrors the
// load/compare/return instruction family emitted by opencontainers/cgroups.
std::vector<bpf_insn> DenyNullRead() {
  return {
      Insn(BPF_LDX | BPF_MEM | BPF_W, 2, 1, 0),
      Insn(BPF_ALU | BPF_RSH | BPF_K, 2, 0, 0, 16),
      Insn(BPF_JMP | BPF_JNE | BPF_K, 2, 0, 6, BPF_DEVCG_ACC_READ),
      Insn(BPF_LDX | BPF_MEM | BPF_W, 3, 1, 4),
      Insn(BPF_JMP | BPF_JNE | BPF_K, 3, 0, 4, 1),
      Insn(BPF_LDX | BPF_MEM | BPF_W, 3, 1, 8),
      Insn(BPF_JMP | BPF_JNE | BPF_K, 3, 0, 2, 3),
      Insn(BPF_ALU | BPF_MOV | BPF_K, 0),
      Insn(BPF_JMP | BPF_EXIT, 0),
      Insn(BPF_ALU | BPF_MOV | BPF_K, 0, 0, 0, 1),
      Insn(BPF_JMP | BPF_EXIT, 0),
  };
}

int Load(const std::vector<bpf_insn>& instructions) {
  static constexpr char kLicense[] = "GPL";
  union bpf_attr attr{};
  attr.prog_type = BPF_PROG_TYPE_CGROUP_DEVICE;
  attr.insn_cnt = instructions.size();
  attr.insns = reinterpret_cast<uintptr_t>(instructions.data());
  attr.license = reinterpret_cast<uintptr_t>(kLicense);
  std::memcpy(attr.prog_name, "dkc006_device", sizeof("dkc006_device"));
  return syscall(SYS_bpf, BPF_PROG_LOAD, &attr, sizeof(attr));
}

int Attach(int cgroup_fd, int program_fd, uint32_t flags = BPF_F_ALLOW_MULTI,
           int replace_fd = 0) {
  union bpf_attr attr{};
  attr.target_fd = cgroup_fd;
  attr.attach_bpf_fd = program_fd;
  attr.attach_type = BPF_CGROUP_DEVICE;
  attr.attach_flags = flags;
  attr.replace_bpf_fd = replace_fd;
  return syscall(SYS_bpf, BPF_PROG_ATTACH, &attr, sizeof(attr));
}

int Detach(int cgroup_fd, int program_fd) {
  union bpf_attr attr{};
  attr.target_fd = cgroup_fd;
  attr.attach_bpf_fd = program_fd;
  attr.attach_type = BPF_CGROUP_DEVICE;
  return syscall(SYS_bpf, BPF_PROG_DETACH, &attr, sizeof(attr));
}

int Query(int cgroup_fd, uint32_t* ids, uint32_t* count,
          uint32_t query_flags = 0) {
  union bpf_attr attr{};
  attr.query.target_fd = cgroup_fd;
  attr.query.attach_type = BPF_CGROUP_DEVICE;
  attr.query.query_flags = query_flags;
  attr.query.prog_ids = reinterpret_cast<uintptr_t>(ids);
  attr.query.prog_cnt = *count;
  int result = syscall(SYS_bpf, BPF_PROG_QUERY, &attr, sizeof(attr));
  *count = attr.query.prog_cnt;
  return result;
}

int ProgramById(uint32_t id) {
  union bpf_attr attr{};
  attr.prog_id = id;
  return syscall(SYS_bpf, BPF_PROG_GET_FD_BY_ID, &attr, sizeof(attr));
}

int ProgramInfo(int fd, bpf_prog_info* info) {
  union bpf_attr attr{};
  attr.info.bpf_fd = fd;
  attr.info.info_len = sizeof(*info);
  attr.info.info = reinterpret_cast<uintptr_t>(info);
  return syscall(SYS_bpf, BPF_OBJ_GET_INFO_BY_FD, &attr, sizeof(attr));
}

[[noreturn]] void ChildFail(int detail_fd, const char* step) {
  dprintf(detail_fd, "%s: errno=%d (%s)", step, errno, std::strerror(errno));
  _exit(1);
}

struct ChildResult {
  int status;
  std::string detail;
};

ChildResult RunInCgroup(const std::string& cgroup_path,
                        const std::function<void(int)>& action) {
  int detail_pipe[2];
  if (pipe(detail_pipe) != 0) return {-1, "pipe failed"};
  const pid_t pid = fork();
  if (pid < 0) {
    close(detail_pipe[0]);
    close(detail_pipe[1]);
    return {-1, "fork failed"};
  }
  if (pid == 0) {
    close(detail_pipe[0]);
    const std::string procs = cgroup_path + "/cgroup.procs";
    ScopedFd fd(open(procs.c_str(), O_WRONLY | O_CLOEXEC));
    const std::string own_pid = std::to_string(getpid());
    if (fd.get() < 0 ||
        write(fd.get(), own_pid.data(), own_pid.size()) !=
            static_cast<ssize_t>(own_pid.size())) {
      dprintf(detail_pipe[1], "join cgroup: errno=%d (%s)", errno,
              std::strerror(errno));
      _exit(77);
    }
    action(detail_pipe[1]);
    _exit(0);
  }
  close(detail_pipe[1]);
  std::string detail;
  char buffer[256];
  ssize_t n;
  while ((n = read(detail_pipe[0], buffer, sizeof(buffer))) > 0)
    detail.append(buffer, n);
  close(detail_pipe[0]);
  int status = 0;
  pid_t waited;
  do {
    waited = waitpid(pid, &status, 0);
  } while (waited < 0 && errno == EINTR);
  if (waited != pid) return {-1, "waitpid failed"};
  return {status, detail};
}

class CgroupDeviceBpf : public ::testing::Test {
 protected:
  void SetUp() override {
    guest_ = IsDragonOS();
    if (!guest_ &&
        (!HasCapability(CAP_NET_ADMIN) ||
         (!HasCapability(CAP_SYS_ADMIN) && !HasCapability(CAP_BPF)))) {
      GTEST_SKIP() << "host lacks the capabilities needed for cgroup BPF";
    }
    const char* root = "/sys/fs/cgroup";
    ScopedFd root_fd(open(root, O_RDONLY | O_DIRECTORY | O_CLOEXEC));
    if (root_fd.get() < 0 && !guest_)
      GTEST_SKIP() << "host cgroup2 mount unavailable";
    ASSERT_GE(root_fd.get(), 0) << "cgroup2 mount unavailable: " << errno;

    static unsigned sequence = 0;
    path_ = std::string(root) + "/dkc006-" + std::to_string(getpid()) +
            "-" + std::to_string(++sequence);
    const int mkdir_result = mkdir(path_.c_str(), 0755);
    if (mkdir_result != 0 && !guest_)
      GTEST_SKIP() << "host cgroup2 mount is not writable: " << errno;
    ASSERT_EQ(mkdir_result, 0) << "cannot create test cgroup: " << errno;
    cgroup_fd_.Reset(open(path_.c_str(), O_RDONLY | O_DIRECTORY | O_CLOEXEC));
    ASSERT_GE(cgroup_fd_.get(), 0) << errno;

    ScopedFd probe(Load(AllowAll()));
    if (probe.get() < 0 && !guest_ &&
        (errno == EPERM || errno == EACCES))
      GTEST_SKIP() << "host prohibits BPF_PROG_LOAD";
    ASSERT_GE(probe.get(), 0) << "valid cgroup-device program failed to load: " << errno;
  }

  void TearDown() override {
    cgroup_fd_.Reset();
    if (!path_.empty()) rmdir(path_.c_str());
  }

  void ExpectChildSuccess(const ChildResult& child) {
    if (!guest_ && WIFEXITED(child.status) && WEXITSTATUS(child.status) == 77)
      GTEST_SKIP() << "host disallows cgroup migration: " << child.detail;
    ASSERT_TRUE(WIFEXITED(child.status)) << child.detail;
    ASSERT_EQ(WEXITSTATUS(child.status), 0) << child.detail;
  }

  int cgroup_fd() const { return cgroup_fd_.get(); }
  const std::string& path() const { return path_; }

 private:
  bool guest_ = false;
  ScopedFd cgroup_fd_;
  std::string path_;
};

TEST_F(CgroupDeviceBpf, VerifierRejectsUnsafeInstructions) {
  const std::vector<std::vector<bpf_insn>> invalid = {
      {Insn(BPF_LDX | BPF_MEM | BPF_W, 0, 1, 12),
       Insn(BPF_JMP | BPF_EXIT, 0)},
      {Insn(BPF_STX | BPF_MEM | BPF_W, 1, 0, 0),
       Insn(BPF_JMP | BPF_EXIT, 0)},
      {Insn(BPF_JMP | BPF_CALL, 0, 0, 0, 1),
       Insn(BPF_JMP | BPF_EXIT, 0)},
      {Insn(BPF_ALU | BPF_MOV | BPF_K, 0, 0, 0, 1),
       Insn(BPF_JMP | BPF_JA, 0, 0, -1),
       Insn(BPF_JMP | BPF_EXIT, 0)},
      {Insn(BPF_JMP | BPF_EXIT, 0)},
      {Insn(BPF_LD | BPF_DW | BPF_IMM, 0, 0, 0, 1)},
  };
  for (size_t index = 0; index < invalid.size(); ++index) {
    errno = 0;
    ScopedFd fd(Load(invalid[index]));
    EXPECT_LT(fd.get(), 0) << "unsafe program accepted: " << index;
    EXPECT_NE(errno, 0) << index;
  }
}

TEST_F(CgroupDeviceBpf, UnmappedInstructionPageReturnsEfault) {
  void* inaccessible = mmap(nullptr, 4096, PROT_NONE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  ASSERT_NE(inaccessible, MAP_FAILED) << errno;
  static constexpr char kLicense[] = "GPL";
  union bpf_attr attr{};
  attr.prog_type = BPF_PROG_TYPE_CGROUP_DEVICE;
  attr.insn_cnt = 2;
  attr.insns = reinterpret_cast<uintptr_t>(inaccessible);
  attr.license = reinterpret_cast<uintptr_t>(kLicense);
  errno = 0;
  const int fd = syscall(SYS_bpf, BPF_PROG_LOAD, &attr, sizeof(attr));
  const int load_errno = errno;
  if (fd >= 0) close(fd);
  EXPECT_EQ(fd, -1);
  EXPECT_EQ(load_errno, EFAULT);
  EXPECT_EQ(munmap(inaccessible, 4096), 0);
}

TEST_F(CgroupDeviceBpf, AttachQueryReplaceDetachAndProgramIdentity) {
  if (!IsDragonOS() && !HasCapability(CAP_SYS_ADMIN))
    GTEST_SKIP() << "Linux PROG_GET_FD_BY_ID requires CAP_SYS_ADMIN";
  const auto deny_instructions = DenyAll();
  ScopedFd deny(Load(deny_instructions));
  ScopedFd allow(Load(AllowAll()));
  ASSERT_GE(deny.get(), 0) << errno;
  ASSERT_GE(allow.get(), 0) << errno;
  ASSERT_EQ(Attach(cgroup_fd(), deny.get()), 0) << errno;

  uint32_t count = 0;
  ASSERT_EQ(Query(cgroup_fd(), nullptr, &count), 0) << errno;
  ASSERT_EQ(count, 1u);
  uint32_t first_id = 0;
  ASSERT_EQ(Query(cgroup_fd(), &first_id, &count), 0) << errno;
  ASSERT_EQ(count, 1u);
  ASSERT_NE(first_id, 0u);
  ScopedFd reopened(ProgramById(first_id));
  ASSERT_GE(reopened.get(), 0) << errno;
  bpf_prog_info info{};
  ASSERT_EQ(ProgramInfo(reopened.get(), &info), 0) << errno;
  EXPECT_EQ(info.type, static_cast<uint32_t>(BPF_PROG_TYPE_CGROUP_DEVICE));
  EXPECT_EQ(info.id, first_id);
  EXPECT_STREQ(reinterpret_cast<const char*>(info.name), "dkc006_device");
  ASSERT_EQ(info.xlated_prog_len, deny_instructions.size() * sizeof(bpf_insn));
  std::vector<uint8_t> translated(info.xlated_prog_len);
  bpf_prog_info translated_info{};
  translated_info.xlated_prog_len = translated.size();
  translated_info.xlated_prog_insns =
      reinterpret_cast<uintptr_t>(translated.data());
  ASSERT_EQ(ProgramInfo(reopened.get(), &translated_info), 0) << errno;
  EXPECT_EQ(std::memcmp(translated.data(), deny_instructions.data(),
                        translated.size()), 0);

  union bpf_attr empty_info{};
  empty_info.info.bpf_fd = reopened.get();
  ASSERT_EQ(syscall(SYS_bpf, BPF_OBJ_GET_INFO_BY_FD, &empty_info,
                    sizeof(empty_info)), 0) << errno;
  EXPECT_EQ(empty_info.info.info_len, 0u);

  ASSERT_EQ(Attach(cgroup_fd(), allow.get(), BPF_F_ALLOW_MULTI | BPF_F_REPLACE,
                   deny.get()),
            0)
      << errno;
  uint32_t replacement_id = 0;
  count = 1;
  ASSERT_EQ(Query(cgroup_fd(), &replacement_id, &count), 0) << errno;
  EXPECT_EQ(count, 1u);
  EXPECT_NE(replacement_id, first_id);
  ASSERT_EQ(Detach(cgroup_fd(), allow.get()), 0) << errno;
  count = 0;
  ASSERT_EQ(Query(cgroup_fd(), nullptr, &count), 0) << errno;
  EXPECT_EQ(count, 0u);
}

TEST_F(CgroupDeviceBpf, ClosingProgramFdKeepsPolicyAndMigrationChangesNextOpen) {
  ScopedFd previously_open(open("/dev/null", O_RDONLY | O_CLOEXEC));
  ASSERT_GE(previously_open.get(), 0) << errno;
  ScopedFd filter(Load(DenyNullRead()));
  ASSERT_GE(filter.get(), 0) << errno;
  ASSERT_EQ(Attach(cgroup_fd(), filter.get()), 0) << errno;
  filter.Reset();
  uint32_t id = 0;
  uint32_t count = 1;
  ASSERT_EQ(Query(cgroup_fd(), &id, &count), 0) << errno;
  ASSERT_EQ(count, 1u);
  ASSERT_NE(id, 0u);

  const ChildResult child = RunInCgroup(path(), [&](int detail_fd) {
    char scratch = 0;
    if (read(previously_open.get(), &scratch, 1) != 0)
      ChildFail(detail_fd, "pre-migration open fd remains usable");
    errno = 0;
    ScopedFd denied(open("/dev/null", O_RDONLY | O_CLOEXEC));
    if (denied.get() >= 0 || errno != EPERM)
      ChildFail(detail_fd, "new /dev/null read must be denied");
    ScopedFd unrelated(open("/dev/zero", O_RDONLY | O_CLOEXEC));
    if (unrelated.get() < 0) ChildFail(detail_fd, "unrelated device open");
  });
  ExpectChildSuccess(child);
}

TEST_F(CgroupDeviceBpf, OPathFokAndMknodApplyDifferentDeviceRules) {
  ScopedFd filter(Load(DenyAll()));
  ASSERT_GE(filter.get(), 0) << errno;
  ASSERT_EQ(Attach(cgroup_fd(), filter.get()), 0) << errno;

  char directory[] = "/tmp/dkc006-mknod-XXXXXX";
  ASSERT_NE(mkdtemp(directory), nullptr) << errno;
  const std::string whiteout = std::string(directory) + "/whiteout";
  const std::string real_device = std::string(directory) + "/blocked";
  const ChildResult child = RunInCgroup(path(), [&](int detail_fd) {
    ScopedFd path_fd(open("/dev/null", O_PATH | O_CLOEXEC));
    if (path_fd.get() < 0) ChildFail(detail_fd, "O_PATH bypass");
    errno = 0;
    if (access("/dev/null", F_OK) != -1 || errno != EPERM)
      ChildFail(detail_fd, "access F_OK must check device policy");
    errno = 0;
    if (mknod(whiteout.c_str(), S_IFCHR | 0600, makedev(0, 0)) != 0)
      ChildFail(detail_fd, "whiteout mknod bypass");
    errno = 0;
    if (mknod(real_device.c_str(), S_IFCHR | 0600, makedev(1, 3)) != -1 ||
        errno != EPERM)
      ChildFail(detail_fd, "ordinary device mknod must be denied");
  });
  unlink(whiteout.c_str());
  unlink(real_device.c_str());
  rmdir(directory);
  ExpectChildSuccess(child);
}

}  // namespace

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
