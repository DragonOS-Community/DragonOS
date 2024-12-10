#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#define TEST_ASSERT(left, right, success_msg, fail_msg)                        \
  do {                                                                         \
    if ((left) == (right)) {                                                   \
      printf("[PASS] %s\n", success_msg);                                      \
    } else {                                                                   \
      printf("[FAIL] %s: Expected %d, but got %d\n", fail_msg, (right),        \
             (left));                                                          \
    }                                                                          \
  } while (0)

#define FIFO_PATH "/bin/test_fifo" // 使用 /tmp 目录避免权限问题

typedef struct {
  int fd;
  int error_code;
} FifoWriteResult;

// 信号处理函数
void sigpipe_handler(int signo) {
  if (signo == SIGPIPE) {
    printf("Received SIGPIPE signal. Write operation failed.\n");
  }
}

const char *scenarios[] = {"No readers (FIFO never had readers)",
                           "Reader exists but disconnects",
                           "Active reader exists"};

FifoWriteResult test_fifo_write(int scenario_index, int nonblocking) {
  FifoWriteResult result = {.fd = -1, .error_code = 0};
  int fd;
  const char *data = "Hello, FIFO!";

  // Set write mode and non-blocking flag
  int flags = O_WRONLY;
  if (nonblocking) {
    flags |= O_NONBLOCK;
  }

  // Open the FIFO write end
  fd = open(FIFO_PATH, flags);
  if (fd == -1) {
    result.fd = fd;
    result.error_code = errno;

    if (errno == ENXIO) {
      printf("Result: Failed to open FIFO for writing (ENXIO: No readers).\n");
    } else {
      perror("Failed to open FIFO for writing");
    }
    return result; // Return early with error details
  }

  // Write data
  ssize_t bytes_written = write(fd, data, strlen(data));
  if (bytes_written == -1) {
    result.error_code = errno;

    if (bytes_written == -1) {
      if (errno == EPIPE) {
        printf("Result: Write failed with EPIPE (no readers available).\n");
      } else if (errno == ENXIO) {
        printf("Result: Write failed with ENXIO (FIFO never had readers).\n");
      } else if (errno == EAGAIN) {
        printf("Result: Write failed with EAGAIN (nonblocking write, pipe full "
               "or no readers).\n");
      } else {
        perror("Write failed with an unexpected error");
      }
    } else {
      printf("Result: Write succeeded. Bytes written: %zd\n", bytes_written);
    }

    result.fd = fd;
    close(fd);
    return result; // Return with fd and error_code
  }
}

void test_case1(int nonblocking) {
  // Case 1: No readers (FIFO never had readers)
  FifoWriteResult result = test_fifo_write(0, nonblocking);

  char buffer[100];
  sprintf(buffer, "Fail with unexpected error %d", result.error_code);
  TEST_ASSERT(result.error_code, ENXIO, "write(2) fails with the error ENXIO",
              buffer);
}

void test_case2(int nonblocking) {
  pid_t reader_pid;

  // Case 2: Reader exists but disconnects
  reader_pid = fork();
  if (reader_pid == 0) {
    // Child process acts as a reader
    int reader_fd = open(FIFO_PATH, O_RDONLY);
    if (reader_fd == -1) {
      perror("Reader failed to open FIFO");
      exit(EXIT_FAILURE);
    }
    sleep(2); // Simulate a brief existence of the reader
    close(reader_fd);
    exit(EXIT_SUCCESS);
  }

  sleep(5); // Ensure the reader has opened the FIFO
  FifoWriteResult result = test_fifo_write(1, nonblocking);
  waitpid(reader_pid, NULL, 0); // Wait for the reader process to exit

  if (nonblocking) {
    TEST_ASSERT(result.error_code, EPIPE,
                "Non-Blocking Write failed with EPIPE",
                "Non-Blocking Write failed with wrong error type");
  } else {
    TEST_ASSERT(result.error_code, EPIPE, "Blocking Write failed with EPIPE",
                "Blocking Write failed with wrong error type");
  }
}

void test_case3(int nonblocking) {
  pid_t reader_pid;

  // Case 3: Active reader exists
  reader_pid = fork();
  if (reader_pid == 0) {
    // Child process acts as a reader
    int reader_fd = open(FIFO_PATH, O_RDONLY);
    if (reader_fd == -1) {
      perror("Reader failed to open FIFO");
      exit(EXIT_FAILURE);
    }
    sleep(5); // Keep the reader active
    close(reader_fd);
    exit(EXIT_SUCCESS);
  }

  sleep(1); // Ensure the reader has opened the FIFO
  FifoWriteResult result = test_fifo_write(2, nonblocking);

  waitpid(reader_pid, NULL, 0); // Wait for the reader process to exit

  TEST_ASSERT(result.error_code, 0, "write succeed", "write failed");
}

void run_tests(int nonblocking) {
  for (int i = 0; i < 3; i++) {
    printf("\n--- Testing: %s (nonblocking=%d) ---\n", scenarios[i],
           nonblocking);
    switch (i) {
    case 0:
    //   test_case1(nonblocking);
      break;
    case 1:
      test_case2(nonblocking);
      break;
    case 2:
    //   test_case3(nonblocking);
      break;
    }
  }
}

void test_blocking() {
  // 创建 FIFO
  if (mkfifo(FIFO_PATH, 0666) == -1 && errno != EEXIST) {
    perror("mkfifo failed");
    exit(EXIT_FAILURE);
  }

  // 测试阻塞模式下的三种情况
  printf("========== Testing Blocking Mode ==========\n");
  run_tests(0); // 阻塞模式
  // 删除 FIFO
  unlink(FIFO_PATH);
}

void test_non_blocking() {
  // 创建 FIFO
  if (mkfifo(FIFO_PATH, 0666) == -1 && errno != EEXIST) {
    perror("mkfifo failed");
    exit(EXIT_FAILURE);
  }
  // 测试非阻塞模式下的三种情况
  printf("\n========== Testing Nonblocking Mode ==========\n");
  run_tests(1); // 非阻塞模式
  // 删除 FIFO
  unlink(FIFO_PATH);
}

int main() {
  // 设置 SIGPIPE 信号处理
  signal(SIGPIPE, sigpipe_handler);

//   test_blocking();
  test_non_blocking();

  printf("\nAll tests completed.\n");
  return 0;
}