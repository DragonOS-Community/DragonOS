// Regression coverage for DKC-005.
//
// `BPF_PROG_LOAD` must apply the same instruction-layout and map-reference
// rules as Linux: a malformed `LD_DW_IMM` pair, an unknown opcode, an invalid
// map descriptor or an out-of-range direct value offset all have to fail with
// the Linux errno instead of publishing a program fd, and the program has to
// keep every map it references alive.
#include <gtest/gtest.h>

#include <fcntl.h>
#include <linux/bpf.h>
#include <linux/capability.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <cerrno>
#include <cstdint>
#include <cstring>
#include <vector>

namespace {

// `ENOTSUPP` (524) is not in libc's errno list, so it is spelled out here.
constexpr int kEnotsupp = 524;
constexpr uint32_t kVarOffLimit = 1u << 29;

class ScopedFd {
 public:
  explicit ScopedFd(int fd = -1) : fd_(fd) {}
  ~ScopedFd() { Reset(); }
  ScopedFd(const ScopedFd&) = delete;
  ScopedFd& operator=(const ScopedFd&) = delete;
  ScopedFd(ScopedFd&& other) noexcept : fd_(other.fd_) { other.fd_ = -1; }
  ScopedFd& operator=(ScopedFd&& other) noexcept {
    Reset(other.fd_);
    other.fd_ = -1;
    return *this;
  }
  int get() const { return fd_; }
  void Reset(int fd = -1) {
    if (fd_ >= 0) close(fd_);
    fd_ = fd;
  }

 private:
  int fd_;
};

bool HasCapability(int capability) {
  __user_cap_header_struct header{};
  header.version = _LINUX_CAPABILITY_VERSION_3;
  __user_cap_data_struct data[2]{};
  if (syscall(SYS_capget, &header, data) != 0) return false;
  return (data[capability / 32].effective & (uint32_t{1} << (capability % 32))) != 0;
}

bool CanLoadBpfPrograms() {
  return HasCapability(CAP_BPF) || HasCapability(CAP_SYS_ADMIN) || HasCapability(CAP_NET_ADMIN);
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

// The first half of a pseudo `BPF_LD | BPF_IMM | BPF_DW` load.
bpf_insn LdDwImm(uint8_t src, int32_t imm, int16_t off = 0) {
  return Insn(BPF_LD | BPF_IMM | BPF_DW, 1, src, off, imm);
}

// The reserved second half of the same instruction.
bpf_insn LdDwImmHigh(int32_t imm) { return Insn(0, 0, 0, 0, imm); }

bpf_insn Exit() { return Insn(BPF_JMP | BPF_EXIT, 0); }

bpf_insn MovR0(int32_t imm) { return Insn(BPF_ALU64 | BPF_MOV | BPF_K, 0, 0, 0, imm); }

// A load result: the returned fd (or -1) and the errno the kernel reported.
class LoadedProgram {
 public:
  explicit LoadedProgram(std::vector<bpf_insn> instructions) {
    static constexpr char kLicense[] = "GPL";
    union bpf_attr attr{};
    attr.prog_type = BPF_PROG_TYPE_SOCKET_FILTER;
    attr.insn_cnt = static_cast<uint32_t>(instructions.size());
    attr.insns = reinterpret_cast<uintptr_t>(instructions.data());
    attr.license = reinterpret_cast<uintptr_t>(kLicense);
    errno = 0;
    fd_ = static_cast<int>(syscall(SYS_bpf, BPF_PROG_LOAD, &attr, sizeof(attr)));
    err_ = errno;
  }
  ~LoadedProgram() {
    if (fd_ >= 0) close(fd_);
  }
  LoadedProgram(const LoadedProgram&) = delete;
  LoadedProgram& operator=(const LoadedProgram&) = delete;

  bool ok() const { return fd_ >= 0; }
  int fd() const { return fd_; }
  int err() const { return err_; }

 private:
  int fd_;
  int err_;
};

int CreateMap(uint32_t type, uint32_t key_size, uint32_t value_size, uint32_t max_entries) {
  union bpf_attr attr{};
  attr.map_type = type;
  attr.key_size = key_size;
  attr.value_size = value_size;
  attr.max_entries = max_entries;
  errno = 0;
  return static_cast<int>(syscall(SYS_bpf, BPF_MAP_CREATE, &attr, sizeof(attr)));
}

int CreateArrayMap(uint32_t value_size, uint32_t max_entries) {
  return CreateMap(BPF_MAP_TYPE_ARRAY, 4, value_size, max_entries);
}

int CreateHashMap(uint32_t value_size, uint32_t max_entries) {
  return CreateMap(BPF_MAP_TYPE_HASH, 4, value_size, max_entries);
}

// `BPF_PROG_LOAD` failure paths are expected; success paths need a live map.
class BpfProgLoadTest : public ::testing::Test {
 protected:
  void SetUp() override {
    if (!CanLoadBpfPrograms()) {
      GTEST_SKIP() << "BPF_PROG_LOAD requires CAP_BPF/CAP_SYS_ADMIN/CAP_NET_ADMIN";
    }
  }
};

// Linux returns the same errno from any kernel; the value is part of the ABI.
template <typename T>
void ExpectFailure(const T& result, int expected_errno) {
  EXPECT_FALSE(result.ok());
  EXPECT_EQ(result.err(), expected_errno);
}

TEST_F(BpfProgLoadTest, TrailingLdImm64FailsClosed) {
  // The original DKC-005 reproducer: a single half instruction used to walk
  // one slot past the end of the program.
  ExpectFailure(LoadedProgram({LdDwImm(0, 0)}), EINVAL);
  // A trailing half instruction after a valid one is equally malformed.
  ExpectFailure(LoadedProgram({MovR0(1), LdDwImm(0, 0x11223344)}), EINVAL);
}

TEST_F(BpfProgLoadTest, MalformedSecondSlotIsRejected) {
  ExpectFailure(LoadedProgram({LdDwImm(0, 0), Insn(0x95, 0), Exit()}), EINVAL);
  ExpectFailure(LoadedProgram({LdDwImm(0, 0), Insn(0, 1), Exit()}), EINVAL);
  ExpectFailure(LoadedProgram({LdDwImm(0, 0), Insn(0, 0, 1), Exit()}), EINVAL);
  ExpectFailure(LoadedProgram({LdDwImm(0, 0), Insn(0, 0, 0, 4), Exit()}), EINVAL);
  // A reserved offset in the first slot is invalid too (`check_ld_imm()`).
  ExpectFailure(LoadedProgram({LdDwImm(0, 0, 7), LdDwImmHigh(0), Exit()}), EINVAL);
}

TEST_F(BpfProgLoadTest, GenericImm64IsAccepted) {
  LoadedProgram program({LdDwImm(0, 0x11223344), LdDwImmHigh(0x55667788), MovR0(0), Exit()});
  EXPECT_TRUE(program.ok()) << "errno=" << program.err();
}

TEST_F(BpfProgLoadTest, UnknownPseudoRegisterIsRejected) {
  ExpectFailure(LoadedProgram({LdDwImm(7, 0), LdDwImmHigh(0), MovR0(0), Exit()}), EINVAL);
}

TEST_F(BpfProgLoadTest, UnknownOpcodesAreRejected) {
  // `0x00` is the cBPF `LD | IMM | W` form; eBPF only defines `DW`.
  ExpectFailure(LoadedProgram({Insn(0x00, 0), Exit()}), EINVAL);
  // `ST` without a memory-mode address is not an eBPF instruction.
  ExpectFailure(LoadedProgram({Insn(BPF_ST, 0), Exit()}), EINVAL);
}

TEST_F(BpfProgLoadTest, LdxReservedFieldsAreRejected) {
  // `LDX` with a non-zero immediate.
  ExpectFailure(LoadedProgram({Insn(BPF_LDX | BPF_MEM | BPF_W, 0, 0, 0, 1), MovR0(0), Exit()}),
                EINVAL);
  // `LDX` with a mode other than `MEM`/`MEMSX`.
  ExpectFailure(LoadedProgram({Insn(BPF_LDX | BPF_ABS | BPF_W, 0), MovR0(0), Exit()}), EINVAL);
  // The two legal forms are still accepted.
  LoadedProgram mem({Insn(BPF_LDX | BPF_MEM | BPF_W, 0, 1), MovR0(0), Exit()});
  EXPECT_TRUE(mem.ok()) << "errno=" << mem.err();
  LoadedProgram memsx({Insn(BPF_LDX | BPF_MEMSX | BPF_W, 0, 1), MovR0(0), Exit()});
  EXPECT_TRUE(memsx.ok()) << "errno=" << memsx.err();
}

TEST_F(BpfProgLoadTest, MapFdResolution) {
  ScopedFd map(CreateArrayMap(8, 1));
  ASSERT_GE(map.get(), 0) << "errno=" << errno;

  LoadedProgram good({LdDwImm(BPF_PSEUDO_MAP_FD, map.get()), LdDwImmHigh(0), MovR0(0), Exit()});
  EXPECT_TRUE(good.ok()) << "errno=" << good.err();

  // An unopened descriptor is reported before anything else.
  ExpectFailure(LoadedProgram({LdDwImm(BPF_PSEUDO_MAP_FD, 0x7ffffff), LdDwImmHigh(0), MovR0(0), Exit()}),
                EBADF);

  // A descriptor that does not refer to a map.
  ScopedFd not_a_map(open("/dev/null", O_RDONLY | O_CLOEXEC));
  ASSERT_GE(not_a_map.get(), 0);
  ExpectFailure(LoadedProgram({LdDwImm(BPF_PSEUDO_MAP_FD, not_a_map.get()), LdDwImmHigh(0), MovR0(0), Exit()}),
                EINVAL);

  // A non-zero high immediate is a malformed `MAP_FD` load, even for a good fd.
  ExpectFailure(LoadedProgram({LdDwImm(BPF_PSEUDO_MAP_FD, map.get()), LdDwImmHigh(1), MovR0(0), Exit()}),
                EINVAL);
}

TEST_F(BpfProgLoadTest, MapValueOffsetBounds) {
  ScopedFd map(CreateArrayMap(8, 1));
  ASSERT_GE(map.get(), 0) << "errno=" << errno;

  auto load_with_offset = [&](int32_t offset) {
    return LoadedProgram(
        {LdDwImm(BPF_PSEUDO_MAP_VALUE, map.get()), LdDwImmHigh(offset), MovR0(0), Exit()});
  };

  EXPECT_TRUE(load_with_offset(0).ok());
  EXPECT_TRUE(load_with_offset(7).ok());
  // `value_size` is the unrounded request, not the slot-aligned element size.
  ExpectFailure(load_with_offset(8), EINVAL);
  ExpectFailure(load_with_offset(-1), EINVAL);
  ExpectFailure(load_with_offset(static_cast<int32_t>(kVarOffLimit)), EINVAL);
}

TEST_F(BpfProgLoadTest, MapValueRequiresDirectValueSupport) {
  ScopedFd one_entry(CreateArrayMap(8, 1));
  ScopedFd two_entries(CreateArrayMap(8, 2));
  ScopedFd hash_one(CreateHashMap(8, 1));
  ScopedFd hash_two(CreateHashMap(8, 2));
  ASSERT_GE(one_entry.get(), 0);
  ASSERT_GE(two_entries.get(), 0);
  ASSERT_GE(hash_one.get(), 0);
  ASSERT_GE(hash_two.get(), 0);

  auto load_value = [&](int fd, int32_t offset) {
    return LoadedProgram({LdDwImm(BPF_PSEUDO_MAP_VALUE, fd), LdDwImmHigh(offset), MovR0(0), Exit()});
  };

  EXPECT_TRUE(load_value(one_entry.get(), 0).ok());
  // Multi-entry arrays report `ENOTSUPP` before the offset is even looked at.
  ExpectFailure(load_value(two_entries.get(), 1), kEnotsupp);
  ExpectFailure(load_value(two_entries.get(), kVarOffLimit), EINVAL);
  // Hash maps have no direct value address at all, for any entry count.
  ExpectFailure(load_value(hash_one.get(), 0), EINVAL);
  ExpectFailure(load_value(hash_two.get(), 0), EINVAL);
}

TEST_F(BpfProgLoadTest, MapValueResolvesDescriptorBeforeOffset) {
  // Linux resolves the map descriptor first, so a bad fd wins over a bad
  // offset and the errno stays `EBADF`.
  ExpectFailure(LoadedProgram({LdDwImm(BPF_PSEUDO_MAP_VALUE, 0x7ffffff), LdDwImmHigh(kVarOffLimit),
                               MovR0(0), Exit()}),
                EBADF);
  ExpectFailure(LoadedProgram({LdDwImm(BPF_PSEUDO_MAP_VALUE, 0x7ffffff), LdDwImmHigh(5), MovR0(0), Exit()}),
                EBADF);
}

TEST_F(BpfProgLoadTest, MapIdxWithoutFdArrayIsProtocolError) {
  ScopedFd map(CreateArrayMap(8, 1));
  ASSERT_GE(map.get(), 0);

  ExpectFailure(LoadedProgram({LdDwImm(BPF_PSEUDO_MAP_IDX, map.get()), LdDwImmHigh(0), MovR0(0), Exit()}),
                EPROTO);
  ExpectFailure(LoadedProgram({LdDwImm(BPF_PSEUDO_MAP_IDX_VALUE, map.get()), LdDwImmHigh(0), MovR0(0), Exit()}),
                EPROTO);
  // A non-zero high immediate is malformed before the fd_array check.
  ExpectFailure(LoadedProgram({LdDwImm(BPF_PSEUDO_MAP_IDX, map.get()), LdDwImmHigh(5), MovR0(0), Exit()}),
                EINVAL);
}

TEST_F(BpfProgLoadTest, UsedMapsAreDeduplicatedAndBounded) {
  ScopedFd map(CreateArrayMap(8, 1));
  ASSERT_GE(map.get(), 0);

  std::vector<bpf_insn> twice;
  for (int i = 0; i < 2; i++) {
    twice.push_back(LdDwImm(BPF_PSEUDO_MAP_FD, map.get()));
    twice.push_back(LdDwImmHigh(0));
  }
  twice.push_back(MovR0(0));
  twice.push_back(Exit());
  LoadedProgram dedup(twice);
  EXPECT_TRUE(dedup.ok()) << "errno=" << dedup.err();

  for (int count : {64, 65}) {
    std::vector<ScopedFd> maps;
    std::vector<bpf_insn> program;
    for (int i = 0; i < count; i++) {
      ScopedFd fd(CreateArrayMap(8, 1));
      ASSERT_GE(fd.get(), 0) << "errno=" << errno;
      maps.push_back(std::move(fd));
      program.push_back(LdDwImm(BPF_PSEUDO_MAP_FD, maps.back().get()));
      program.push_back(LdDwImmHigh(0));
    }
    program.push_back(MovR0(0));
    program.push_back(Exit());

    LoadedProgram loaded(std::move(program));
    if (count == 64) {
      EXPECT_TRUE(loaded.ok()) << "errno=" << loaded.err();
    } else {
      // `MAX_USED_MAPS` is 64; the 65th distinct map is `E2BIG`.
      ExpectFailure(loaded, E2BIG);
    }
  }
}

TEST_F(BpfProgLoadTest, ArrayMapAllocationBounds) {
  // Linux `array_map_alloc_check()` caps `value_size` at `INT_MAX` with
  // `E2BIG`; a larger request must not be reported as an allocation failure.
  errno = 0;
  int fd = CreateArrayMap(0xffffffffu, 1);
  int err = errno;
  EXPECT_EQ(fd, -1);
  EXPECT_EQ(err, E2BIG);

  errno = 0;
  fd = CreateArrayMap(0x80000000u, 1);
  err = errno;
  EXPECT_EQ(fd, -1);
  EXPECT_EQ(err, E2BIG);

  // A size that overflows the address space fails as an allocation failure
  // rather than producing an undersized map.
  errno = 0;
  fd = CreateArrayMap(8, 0xffffffffu);
  err = errno;
  EXPECT_EQ(fd, -1);
  EXPECT_EQ(err, ENOMEM);
}

TEST_F(BpfProgLoadTest, ProgramOutlivesMapDescriptor) {
  // A published program owns one reference per referenced map, so the order in
  // which the two descriptors are closed must not matter: dropping the map
  // descriptor first has to leave the program (and the map it points at) alive.
  ScopedFd map(CreateArrayMap(8, 1));
  ASSERT_GE(map.get(), 0) << "errno=" << errno;

  LoadedProgram program(
      {LdDwImm(BPF_PSEUDO_MAP_VALUE, map.get()), LdDwImmHigh(0), MovR0(0), Exit()});
  ASSERT_TRUE(program.ok()) << "errno=" << program.err();

  map.Reset();
  EXPECT_NE(fcntl(program.fd(), F_GETFD), -1) << "errno=" << errno;
}

}  // namespace

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
