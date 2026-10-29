#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdlib.h>
#include <poll.h>
#include <sys/wait.h>
#include <sys/syscall.h>
#include <time.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/uio.h>
#include <unistd.h>

#include <string>
#include <vector>

namespace {

class PipeWriteFault : public testing::Test {
 protected:
  void SetUp() override {
    page_ = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_, 0);
    ASSERT_EQ(0, pipe2(fds_, O_NONBLOCK | O_CLOEXEC));
    source_ = mmap(nullptr, 2 * page_, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, source_);
    memset(source_, 'x', page_);
    ASSERT_EQ(0, mprotect(static_cast<char*>(source_) + page_, page_, PROT_NONE));
  }
  void TearDown() override {
    if (source_ != MAP_FAILED) munmap(source_, 2 * page_);
    for (int fd : fds_) if (fd >= 0) close(fd);
  }
  char* BeforeFault(size_t prefix) {
    return static_cast<char*>(source_) + page_ - prefix;
  }
  void ExpectContents(const std::string& expected) {
    // A nonblocking read also makes an unexpected empty pipe safe to inspect.
    std::vector<char> bytes(expected.size() + 1);
    if (!expected.empty()) {
      ASSERT_EQ(static_cast<ssize_t>(expected.size()), read(fds_[0], bytes.data(), bytes.size()));
      EXPECT_EQ(expected, std::string(bytes.data(), expected.size()));
    }
    errno = 0;
    EXPECT_EQ(-1, read(fds_[0], bytes.data(), bytes.size()));
    EXPECT_EQ(EAGAIN, errno);
  }
  void SeedTail() { ASSERT_EQ(1, write(fds_[1], "s", 1)); }
  int fds_[2] = {-1, -1};
  long page_ = 0;
  void* source_ = MAP_FAILED;
};

TEST_F(PipeWriteFault, NewPageFaultDoesNotPublishCopiedPrefix) {
  errno = 0;
  EXPECT_EQ(-1, write(fds_[1], BeforeFault(17), 64));
  EXPECT_EQ(EFAULT, errno);
  ExpectContents("");
  // A failed page copy must not expose stale bytes on the next write.
  ASSERT_EQ(1, write(fds_[1], "z", 1));
  ExpectContents("z");
}

TEST_F(PipeWriteFault, EmptyPipeCopiesNullInputOnlyAfterAdmission) {
  errno = 0;
  EXPECT_EQ(-1, syscall(SYS_write, fds_[1], nullptr, 1));
  EXPECT_EQ(EFAULT, errno);
  ExpectContents("");
  iovec null_vector = {nullptr, 1};
  errno = 0;
  EXPECT_EQ(-1, writev(fds_[1], &null_vector, 1));
  EXPECT_EQ(EFAULT, errno);
  ExpectContents("");
}

TEST_F(PipeWriteFault, TailMergeFaultDoesNotExtendVisibleLength) {
  SeedTail();
  errno = 0;
  EXPECT_EQ(-1, write(fds_[1], BeforeFault(17), 64));
  EXPECT_EQ(EFAULT, errno);
  ExpectContents("s");
}

TEST_F(PipeWriteFault, OriginalPageMultipleDoesNotMergeFaultPrefix) {
  SeedTail();
  errno = 0;
  EXPECT_EQ(-1, write(fds_[1], BeforeFault(17), page_));
  EXPECT_EQ(EFAULT, errno);
  ExpectContents("s");
}

TEST_F(PipeWriteFault, CommittedTailMergeSurvivesLaterPageFault) {
  SeedTail();
  EXPECT_EQ(1, write(fds_[1], BeforeFault(1), page_ + 1));
  ExpectContents("sx");
}

TEST_F(PipeWriteFault, OnlyCompletePagesSurviveLaterFault) {
  EXPECT_EQ(page_, write(fds_[1], source_, page_ + 1));
  ExpectContents(std::string(page_, 'x'));
}

TEST_F(PipeWriteFault, WritevFaultWithinPageDoesNotPublishEarlierVector) {
  const char prefix[] = "vector";
  iovec vectors[] = {{const_cast<char*>(prefix), sizeof(prefix) - 1}, {BeforeFault(0), 32}};
  errno = 0;
  EXPECT_EQ(-1, writev(fds_[1], vectors, 2));
  EXPECT_EQ(EFAULT, errno);
  ExpectContents("");
}

TEST_F(PipeWriteFault, WritevOriginalTotalControlsTailMerge) {
  SeedTail();
  iovec vectors[] = {{BeforeFault(17), 17}, {BeforeFault(0), static_cast<size_t>(page_ - 17)}};
  errno = 0;
  EXPECT_EQ(-1, writev(fds_[1], vectors, 2));
  EXPECT_EQ(EFAULT, errno);
  ExpectContents("s");
}

TEST_F(PipeWriteFault, WritevCommitsAcrossVectorBoundaryOnlyAtPageBoundary) {
  iovec vectors[] = {{source_, 17}, {static_cast<char*>(source_) + 17,
                                    static_cast<size_t>(page_ - 17)}, {BeforeFault(0), 1}};
  EXPECT_EQ(page_, writev(fds_[1], vectors, 3));
  ExpectContents(std::string(page_, 'x'));
}

TEST_F(PipeWriteFault, FullPipeRejectsBeforeCopyingBadInput) {
  ASSERT_EQ(page_, fcntl(fds_[1], F_SETPIPE_SZ, page_));
  ASSERT_EQ(page_, write(fds_[1], source_, page_));
  errno = 0;
  EXPECT_EQ(-1, write(fds_[1], BeforeFault(0), 1));
  EXPECT_EQ(EAGAIN, errno);
  errno = 0;
  EXPECT_EQ(-1, syscall(SYS_write, fds_[1], nullptr, 1));
  EXPECT_EQ(EAGAIN, errno);
  iovec null_vector = {nullptr, 1};
  errno = 0;
  EXPECT_EQ(-1, writev(fds_[1], &null_vector, 1));
  EXPECT_EQ(EAGAIN, errno);
  ExpectContents(std::string(page_, 'x'));
}

TEST_F(PipeWriteFault, SmallNonblockingWriteIsNotPartiallyPublished) {
  ASSERT_EQ(page_, fcntl(fds_[1], F_SETPIPE_SZ, page_));
  ASSERT_EQ(page_, write(fds_[1], source_, page_));
  char byte;
  ASSERT_EQ(1, read(fds_[0], &byte, 1));
  errno = 0;
  EXPECT_EQ(-1, write(fds_[1], source_, 64));
  EXPECT_EQ(EAGAIN, errno);
  ExpectContents(std::string(page_ - 1, 'x'));
}

TEST_F(PipeWriteFault, MissingReaderAndZeroLengthPrecedeInputFault) {
  struct sigaction ignore = {};
  struct sigaction saved = {};
  ignore.sa_handler = SIG_IGN;
  sigemptyset(&ignore.sa_mask);
  ASSERT_EQ(0, sigaction(SIGPIPE, &ignore, &saved));
  close(fds_[0]);
  fds_[0] = -1;
  errno = 0;
  const ssize_t result = write(fds_[1], BeforeFault(0), 1);
  const int error = errno;
  const ssize_t empty = write(fds_[1], BeforeFault(0), 0);
  errno = 0;
  const ssize_t null_result = syscall(SYS_write, fds_[1], nullptr, 1);
  const int null_error = errno;
  iovec null_vector = {nullptr, 1};
  errno = 0;
  const ssize_t vector_result = writev(fds_[1], &null_vector, 1);
  const int vector_error = errno;
  const int restored = sigaction(SIGPIPE, &saved, nullptr);
  EXPECT_EQ(0, restored);
  EXPECT_EQ(-1, result);
  EXPECT_EQ(EPIPE, error);
  EXPECT_EQ(0, empty);
  EXPECT_EQ(-1, null_result);
  EXPECT_EQ(EPIPE, null_error);
  EXPECT_EQ(-1, vector_result);
  EXPECT_EQ(EPIPE, vector_error);
}

TEST_F(PipeWriteFault, PacketCopyFaultDoesNotPublishPartialPacket) {
  ASSERT_EQ(0, fcntl(fds_[1], F_SETFL, O_NONBLOCK | O_DIRECT));
  errno = 0;
  EXPECT_EQ(-1, write(fds_[1], BeforeFault(17), 64));
  EXPECT_EQ(EFAULT, errno);
  // Linux reserves the packet slot before copying. The empty packet is
  // observable as one zero-byte read, but no copied prefix becomes visible.
  char byte;
  EXPECT_EQ(0, read(fds_[0], &byte, 1));
  ExpectContents("");
}

TEST_F(PipeWriteFault, PacketWritevKeepsOneLogicalPageAsOnePacket) {
  ASSERT_EQ(0, fcntl(fds_[1], F_SETFL, O_NONBLOCK | O_DIRECT));
  char first[] = "abc";
  char second[] = "def";
  iovec vectors[] = {{first, 3}, {second, 3}};
  ASSERT_EQ(6, writev(fds_[1], vectors, 2));
  ExpectContents("abcdef");
}

TEST_F(PipeWriteFault, EnablingPacketModePreservesExistingMergeableTail) {
  ASSERT_EQ(1, write(fds_[1], "s", 1));
  ASSERT_EQ(0, fcntl(fds_[1], F_SETFL, O_NONBLOCK | O_DIRECT));
  ASSERT_EQ(3, write(fds_[1], "abc", 3));
  char bytes[2];
  ASSERT_EQ(2, read(fds_[0], bytes, sizeof(bytes)));
  EXPECT_EQ("sa", std::string(bytes, sizeof(bytes)));
  ExpectContents("bc");
}

TEST_F(PipeWriteFault, TailMergeRacingFileSpliceKeepsExactContents) {
  constexpr size_t rounds = 512;
  constexpr size_t chunk = 127;
  std::vector<char> expected(rounds * chunk);
  for (size_t offset = 0; offset < expected.size(); ++offset)
    expected[offset] = static_cast<char>((offset * 17 + offset / chunk) & 255);
  char name[] = "/tmp/pipe-copy-splice-XXXXXX";
  const int file = mkstemp(name);
  ASSERT_GE(file, 0);
  unlink(name);
  const pid_t child = fork();
  if (child < 0) {
    close(file);
    FAIL() << strerror(errno);
    return;
  }
  if (child == 0) {
    alarm(8);
    close(fds_[1]);
    if (fcntl(fds_[0], F_SETFL, 0) != 0) _exit(1);
    for (;;) {
      // Small incoming writes merge into a tail that a concurrent splice may
      // currently reserve while copying to its destination outside pipe locks.
      const ssize_t count = splice(fds_[0], nullptr, file, nullptr, page_ + 7, 0);
      if (count == 0) _exit(0);
      if (count < 0 && errno != EINTR) _exit(2);
    }
  }
  size_t completed = 0;
  timespec start = {};
  clock_gettime(CLOCK_MONOTONIC, &start);
  while (completed < expected.size()) {
    timespec now = {};
    clock_gettime(CLOCK_MONOTONIC, &now);
    if (now.tv_sec - start.tv_sec >= 8) break;
    char* source = expected.data() + completed;
    iovec vectors[] = {{source, 19}, {source + 19, 31}, {source + 50, chunk - 50}};
    const ssize_t count = (completed / chunk) % 2
                              ? writev(fds_[1], vectors, 3)
                              : write(fds_[1], source, chunk);
    if (count == static_cast<ssize_t>(chunk)) {
      completed += chunk;
    } else if (count == -1 && errno == EAGAIN) {
      pollfd writable = {fds_[1], POLLOUT, 0};
      poll(&writable, 1, 100);
    } else if (count != -1 || errno != EINTR) {
      break;
    }
  }
  close(fds_[1]);
  fds_[1] = -1;
  if (completed != expected.size()) kill(child, SIGKILL);
  int status = 0;
  pid_t waited;
  do { waited = waitpid(child, &status, 0); } while (waited < 0 && errno == EINTR);
  std::vector<char> received(expected.size() + 1);
  const ssize_t size = pread(file, received.data(), received.size(), 0);
  close(file);
  ASSERT_EQ(expected.size(), completed);
  ASSERT_EQ(child, waited);
  ASSERT_TRUE(WIFEXITED(status)) << "splice consumer timed out, status=" << status;
  EXPECT_EQ(0, WEXITSTATUS(status));
  ASSERT_EQ(static_cast<ssize_t>(expected.size()), size);
  received.resize(expected.size());
  EXPECT_EQ(expected, received);
}

TEST_F(PipeWriteFault, ConcurrentWritevKeepsWholePipeBufRecordsAtomic) {
  ASSERT_EQ(page_, fcntl(fds_[1], F_SETPIPE_SZ, page_));
  ASSERT_EQ(0, fcntl(fds_[1], F_SETFL, 0));
  constexpr size_t records_per_writer = 32;
  const size_t record_size = page_ - 1;
  const size_t expected_size = 2 * records_per_writer * record_size;
  pid_t children[2] = {-1, -1};
  for (size_t index = 0; index < 2; ++index) {
    children[index] = fork();
    if (children[index] == 0) {
      alarm(5);
      const std::vector<char> record(record_size, index ? 'b' : 'a');
      iovec vectors[] = {{const_cast<char*>(record.data()), 7},
                        {const_cast<char*>(record.data()) + 7, record_size / 2},
                        {const_cast<char*>(record.data()) + 7 + record_size / 2,
                         record_size - 7 - record_size / 2}};
      for (size_t count = 0; count < records_per_writer; ++count) {
        if (writev(fds_[1], vectors, 3) != static_cast<ssize_t>(record_size)) _exit(1);
      }
      _exit(0);
    }
    if (children[index] < 0) break;
  }
  // The parent always reaps both children before making any fatal assertion.
  // A finite deadline also covers a write implementation that never wakes.
  std::vector<char> received;
  received.reserve(expected_size);
  timespec start = {};
  clock_gettime(CLOCK_MONOTONIC, &start);
  while (received.size() < expected_size && children[0] > 0 && children[1] > 0) {
    timespec now = {};
    clock_gettime(CLOCK_MONOTONIC, &now);
    if (now.tv_sec - start.tv_sec >= 6) break;
    pollfd readable = {fds_[0], POLLIN, 0};
    if (poll(&readable, 1, 100) <= 0) continue;
    char bytes[3137];
    const ssize_t count = read(fds_[0], bytes, sizeof(bytes));
    if (count > 0) received.insert(received.end(), bytes, bytes + count);
  }
  int statuses[2] = {};
  for (size_t index = 0; index < 2; ++index) {
    if (children[index] <= 0) continue;
    if (received.size() != expected_size) kill(children[index], SIGKILL);
    pid_t result;
    do { result = waitpid(children[index], &statuses[index], 0); } while (result < 0 && errno == EINTR);
    EXPECT_EQ(children[index], result);
  }
  ASSERT_GT(children[0], 0);
  ASSERT_GT(children[1], 0);
  ASSERT_EQ(expected_size, received.size());
  for (int status : statuses) {
    ASSERT_TRUE(WIFEXITED(status)) << "status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status));
  }
  size_t a_records = 0;
  size_t b_records = 0;
  for (size_t offset = 0; offset < received.size(); offset += record_size) {
    const char marker = received[offset];
    ASSERT_TRUE(marker == 'a' || marker == 'b');
    for (size_t byte = 0; byte < record_size; ++byte) {
      ASSERT_EQ(marker, received[offset + byte]) << "record interleaved at " << offset + byte;
    }
    if (marker == 'a') ++a_records;
    else ++b_records;
  }
  EXPECT_EQ(records_per_writer, a_records);
  EXPECT_EQ(records_per_writer, b_records);
}

TEST_F(PipeWriteFault, NonblockingReadAfterPollWaitsForConcurrentWriterCopy) {
  ASSERT_EQ(page_, fcntl(fds_[1], F_SETPIPE_SZ, page_));
  ASSERT_EQ(0, fcntl(fds_[1], F_SETFL, 0));
  constexpr size_t record_size = 257;
  constexpr size_t records = 8192;
  const size_t expected = record_size * records;
  const pid_t writer = fork();
  ASSERT_GE(writer, 0) << strerror(errno);
  if (writer == 0) {
    close(fds_[0]);
    alarm(12);
    const std::string record(record_size, 'm');
    for (size_t index = 0; index < records; ++index) {
      if (write(fds_[1], record.data(), record.size()) !=
          static_cast<ssize_t>(record.size())) _exit(1);
    }
    _exit(0);
  }
  close(fds_[1]);
  fds_[1] = -1;
  // There is exactly one reader, so no other consumer can invalidate POLLIN.
  // A writer's in-progress copy is internal serialization, not an empty pipe.
  // Do not hide a readiness violation by retrying EAGAIN.
  size_t received = 0;
  ssize_t last_read = 0;
  int read_error = 0;
  short last_events = 0;
  bool contents_match = true;
  timespec start = {};
  clock_gettime(CLOCK_MONOTONIC, &start);
  while (received < expected) {
    timespec now = {};
    clock_gettime(CLOCK_MONOTONIC, &now);
    if (now.tv_sec - start.tv_sec >= 15) break;
    pollfd ready = {fds_[0], POLLIN, 0};
    const int count = poll(&ready, 1, 100);
    if (count == 0 || (count < 0 && errno == EINTR)) continue;
    last_events = ready.revents;
    if (count != 1 || !(ready.revents & POLLIN)) break;
    char bytes[113];
    last_read = read(fds_[0], bytes, sizeof(bytes));
    read_error = errno;
    if (last_read <= 0) break;
    for (ssize_t byte = 0; byte < last_read; ++byte) {
      if (bytes[byte] != 'm') contents_match = false;
    }
    received += last_read;
  }
  // Reap before asserting, including the failure path where the producer is
  // blocked on a full pipe. Its alarm also bounds a stuck kernel read call.
  if (received != expected) kill(writer, SIGKILL);
  int status = 0;
  pid_t waited;
  do { waited = waitpid(writer, &status, 0); } while (waited < 0 && errno == EINTR);
  ASSERT_EQ(writer, waited) << strerror(errno);
  ASSERT_EQ(expected, received) << "read=" << last_read << " errno=" << read_error
                               << " (" << strerror(read_error) << ") revents=" << last_events;
  EXPECT_TRUE(contents_match);
  ASSERT_TRUE(WIFEXITED(status)) << "writer status=" << status;
  EXPECT_EQ(0, WEXITSTATUS(status));
}

}  // namespace

int main(int argc, char** argv) {
  testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
