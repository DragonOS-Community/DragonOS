#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#include <vector>

namespace {

class PipePair {
 public:
  ~PipePair() { for (int fd : fds_) if (fd >= 0) close(fd); }
  bool Open(int flags = 0) { return pipe2(fds_, O_CLOEXEC | O_NONBLOCK | flags) == 0; }
  int reader() const { return fds_[0]; }
  int writer() const { return fds_[1]; }
 private:
  int fds_[2] = {-1, -1};
};

class PipeEmptySlot : public testing::Test {
 protected:
  void SetUp() override {
    page_ = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_, 0);
    bad_ = mmap(nullptr, page_, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, bad_);
  }
  void TearDown() override { if (bad_ != MAP_FAILED) munmap(bad_, page_); }
  void AddEmptySlot(const PipePair& pipe) {
    // A page-multiple request skips tail merging, reserves a fresh slot, then
    // faults before publishing any bytes. The slot still consumes capacity.
    errno = 0;
    ASSERT_EQ(-1, write(pipe.writer(), bad_, page_));
    ASSERT_EQ(EFAULT, errno);
  }
  void ExpectReady(int fd, short events, bool ready) {
    pollfd item = {fd, events, 0};
    EXPECT_EQ(ready ? 1 : 0, poll(&item, 1, 0));
    EXPECT_EQ(ready, (item.revents & events) != 0);
  }
  void ExpectNoBytes(const PipePair& pipe) {
    int available = -1;
    ASSERT_EQ(0, ioctl(pipe.reader(), FIONREAD, &available));
    EXPECT_EQ(0, available);
  }
  void ExpectEmptyStreamRead(const PipePair& pipe) {
    char byte;
    errno = 0;
    EXPECT_EQ(-1, read(pipe.reader(), &byte, 1));
    EXPECT_EQ(EAGAIN, errno);
  }
  long page_ = 0;
  void* bad_ = MAP_FAILED;
};

TEST_F(PipeEmptySlot, PollReportsReservedSlotEvenWithZeroAvailableBytes) {
  PipePair pipe;
  ASSERT_TRUE(pipe.Open());
  ASSERT_EQ(page_, fcntl(pipe.writer(), F_SETPIPE_SZ, page_));
  AddEmptySlot(pipe);
  ExpectNoBytes(pipe);
  ExpectReady(pipe.reader(), POLLIN, true);
  ExpectReady(pipe.writer(), POLLOUT, false);
  ExpectEmptyStreamRead(pipe);
  ExpectReady(pipe.reader(), POLLIN, false);
  ExpectReady(pipe.writer(), POLLOUT, true);
}

TEST_F(PipeEmptySlot, ShrinkingCapacityCountsSlotsRatherThanBytes) {
  PipePair pipe;
  ASSERT_TRUE(pipe.Open());
  ASSERT_EQ(2 * page_, fcntl(pipe.writer(), F_SETPIPE_SZ, 2 * page_));
  AddEmptySlot(pipe);
  AddEmptySlot(pipe);
  ExpectNoBytes(pipe);
  errno = 0;
  EXPECT_EQ(-1, fcntl(pipe.writer(), F_SETPIPE_SZ, page_));
  EXPECT_EQ(EBUSY, errno);
  ExpectEmptyStreamRead(pipe);
  EXPECT_EQ(page_, fcntl(pipe.writer(), F_SETPIPE_SZ, page_));
}

TEST_F(PipeEmptySlot, PacketReadConsumesOneEmptySlotAndReturnsZero) {
  PipePair pipe;
  ASSERT_TRUE(pipe.Open(O_DIRECT));
  ASSERT_EQ(2 * page_, fcntl(pipe.writer(), F_SETPIPE_SZ, 2 * page_));
  AddEmptySlot(pipe);
  AddEmptySlot(pipe);
  ExpectNoBytes(pipe);
  char byte;
  EXPECT_EQ(0, read(pipe.reader(), &byte, 1));
  ExpectReady(pipe.reader(), POLLIN, true);
  EXPECT_EQ(0, read(pipe.reader(), &byte, 1));
  ExpectReady(pipe.reader(), POLLIN, false);
  ExpectEmptyStreamRead(pipe);
}

TEST_F(PipeEmptySlot, PipeToFileSpliceDiscardsEmptySlotsBeforeEagain) {
  PipePair pipe;
  ASSERT_TRUE(pipe.Open(O_DIRECT));
  ASSERT_EQ(page_, fcntl(pipe.writer(), F_SETPIPE_SZ, page_));
  AddEmptySlot(pipe);
  char name[] = "/tmp/pipe-empty-splice-XXXXXX";
  const int file = mkstemp(name);
  ASSERT_GE(file, 0);
  unlink(name);
  errno = 0;
  const ssize_t result = splice(pipe.reader(), nullptr, file, nullptr, 1, SPLICE_F_NONBLOCK);
  const int error = errno;
  const off_t position = lseek(file, 0, SEEK_CUR);
  close(file);
  EXPECT_EQ(-1, result);
  EXPECT_EQ(EAGAIN, error);
  EXPECT_EQ(0, position);
  ExpectReady(pipe.reader(), POLLIN, false);
  const std::vector<char> bytes(page_, 'w');
  EXPECT_EQ(page_, write(pipe.writer(), bytes.data(), bytes.size()));
}

TEST_F(PipeEmptySlot, PipeToPipeSpliceMovesEmptySlotBeforeEagain) {
  PipePair input;
  PipePair output;
  ASSERT_TRUE(input.Open(O_DIRECT));
  ASSERT_TRUE(output.Open());
  AddEmptySlot(input);
  errno = 0;
  EXPECT_EQ(-1, splice(input.reader(), nullptr, output.writer(), nullptr, 1,
                       SPLICE_F_NONBLOCK));
  EXPECT_EQ(EAGAIN, errno);
  ExpectReady(input.reader(), POLLIN, false);
  ExpectReady(output.reader(), POLLIN, true);
  ExpectNoBytes(output);
  // The moved slot retains packet semantics even though the output write
  // descriptor itself was opened as a stream.
  char byte;
  EXPECT_EQ(0, read(output.reader(), &byte, 1));
  ExpectEmptyStreamRead(output);
}

TEST_F(PipeEmptySlot, TeeCopiesEmptySlotOnceWithoutConsumingInput) {
  PipePair input;
  PipePair output;
  ASSERT_TRUE(input.Open(O_DIRECT));
  ASSERT_TRUE(output.Open());
  ASSERT_EQ(page_, fcntl(output.writer(), F_SETPIPE_SZ, page_));
  AddEmptySlot(input);
  EXPECT_EQ(0, tee(input.reader(), output.writer(), 1, SPLICE_F_NONBLOCK));
  ExpectReady(input.reader(), POLLIN, true);
  ExpectReady(output.reader(), POLLIN, true);
  ExpectReady(output.writer(), POLLOUT, false);
  ExpectNoBytes(input);
  ExpectNoBytes(output);
  char byte;
  EXPECT_EQ(0, read(input.reader(), &byte, 1));
  EXPECT_EQ(0, read(output.reader(), &byte, 1));
  ExpectEmptyStreamRead(input);
  ExpectEmptyStreamRead(output);
}

TEST_F(PipeEmptySlot, StreamReadSkipsEmptySlotAndReturnsFollowingData) {
  PipePair pipe;
  ASSERT_TRUE(pipe.Open());
  ASSERT_EQ(2 * page_, fcntl(pipe.writer(), F_SETPIPE_SZ, 2 * page_));
  AddEmptySlot(pipe);
  const std::vector<char> expected(page_, 'm');
  ASSERT_EQ(page_, write(pipe.writer(), expected.data(), expected.size()));
  std::vector<char> bytes(page_);
  EXPECT_EQ(page_, read(pipe.reader(), bytes.data(), bytes.size()));
  EXPECT_EQ(expected, bytes);
  ExpectEmptyStreamRead(pipe);
}

TEST_F(PipeEmptySlot, PacketReadStopsAtEmptySlotBeforeFollowingData) {
  PipePair pipe;
  ASSERT_TRUE(pipe.Open(O_DIRECT));
  ASSERT_EQ(2 * page_, fcntl(pipe.writer(), F_SETPIPE_SZ, 2 * page_));
  AddEmptySlot(pipe);
  ASSERT_EQ(1, write(pipe.writer(), "m", 1));
  char byte;
  EXPECT_EQ(0, read(pipe.reader(), &byte, 1));
  EXPECT_EQ(1, read(pipe.reader(), &byte, 1));
  EXPECT_EQ('m', byte);
  ExpectEmptyStreamRead(pipe);
}

TEST_F(PipeEmptySlot, OutputFaultConsumesOnlyLeadingEmptySlot) {
  PipePair pipe;
  ASSERT_TRUE(pipe.Open());
  ASSERT_EQ(2 * page_, fcntl(pipe.writer(), F_SETPIPE_SZ, 2 * page_));
  AddEmptySlot(pipe);
  const std::vector<char> expected(page_, 'm');
  ASSERT_EQ(page_, write(pipe.writer(), expected.data(), expected.size()));
  errno = 0;
  EXPECT_EQ(-1, read(pipe.reader(), bad_, page_));
  EXPECT_EQ(EFAULT, errno);
  int available = -1;
  ASSERT_EQ(0, ioctl(pipe.reader(), FIONREAD, &available));
  EXPECT_EQ(page_, available);
  // Shrinking is possible only if the leading empty slot really retired.
  EXPECT_EQ(page_, fcntl(pipe.writer(), F_SETPIPE_SZ, page_));
  std::vector<char> bytes(page_);
  EXPECT_EQ(page_, read(pipe.reader(), bytes.data(), bytes.size()));
  EXPECT_EQ(expected, bytes);
}

TEST_F(PipeEmptySlot, ZeroLengthReadDoesNotConsumeEmptyPacket) {
  PipePair pipe;
  ASSERT_TRUE(pipe.Open(O_DIRECT));
  AddEmptySlot(pipe);
  EXPECT_EQ(0, read(pipe.reader(), bad_, 0));
  ExpectReady(pipe.reader(), POLLIN, true);
  char byte;
  EXPECT_EQ(0, read(pipe.reader(), &byte, 1));
  ExpectReady(pipe.reader(), POLLIN, false);
}

TEST_F(PipeEmptySlot, RemovingEmptyStreamSlotMakesBlockedWriterRunnable) {
  PipePair pipe;
  PipePair completion;
  ASSERT_TRUE(pipe.Open());
  ASSERT_TRUE(completion.Open());
  ASSERT_EQ(page_, fcntl(pipe.writer(), F_SETPIPE_SZ, page_));
  AddEmptySlot(pipe);
  ASSERT_EQ(0, fcntl(pipe.writer(), F_SETFL, 0));
  const pid_t child = fork();
  ASSERT_GE(child, 0);
  if (child == 0) {
    alarm(3);
    const std::vector<char> bytes(page_, 'w');
    const ssize_t written = write(pipe.writer(), bytes.data(), bytes.size());
    if (written != page_) _exit(1);
    const char done = 'd';
    _exit(write(completion.writer(), &done, 1) == 1 ? 0 : 2);
  }
  // No user data can complete while the sole ring slot is occupied.
  pollfd done = {completion.reader(), POLLIN, 0};
  const int premature = poll(&done, 1, 50);
  char byte;
  errno = 0;
  const ssize_t drained = read(pipe.reader(), &byte, 1);
  const int drain_error = errno;
  done.revents = 0;
  const int ready = poll(&done, 1, 2000);
  int status = 0;
  pid_t waited;
  do { waited = waitpid(child, &status, 0); } while (waited < 0 && errno == EINTR);
  EXPECT_EQ(0, premature);
  EXPECT_EQ(-1, drained);
  EXPECT_EQ(EAGAIN, drain_error);
  EXPECT_EQ(1, ready);
  ASSERT_EQ(child, waited);
  ASSERT_TRUE(WIFEXITED(status)) << "writer did not wake, status=" << status;
  EXPECT_EQ(0, WEXITSTATUS(status));
  std::vector<char> bytes(page_);
  EXPECT_EQ(page_, read(pipe.reader(), bytes.data(), bytes.size()));
  EXPECT_EQ(std::vector<char>(page_, 'w'), bytes);
}

}  // namespace

int main(int argc, char** argv) {
  testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
