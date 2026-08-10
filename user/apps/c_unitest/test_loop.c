#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

// ===================================================================
// 测试框架定义
// ===================================================================

#define TEST_PASS 0
#define TEST_FAIL 1

// 测试结果统计
static int g_tests_total = 0;
static int g_tests_passed = 0;
static int g_tests_failed = 0;

// 测试输出宏
#define TEST_BEGIN(name)                                                       \
  do {                                                                         \
    g_tests_total++;                                                           \
    printf("\n");                                                              \
    printf("========================================\n");                      \
    printf("[TEST %d] %s\n", g_tests_total, name);                             \
    printf("========================================\n");                      \
  } while (0)

#define TEST_END_PASS(name)                                                    \
  do {                                                                         \
    g_tests_passed++;                                                          \
    printf("----------------------------------------\n");                      \
    printf("[PASS] %s\n", name);                                               \
    printf("----------------------------------------\n");                      \
  } while (0)

#define TEST_END_FAIL(name, reason)                                            \
  do {                                                                         \
    g_tests_failed++;                                                          \
    printf("----------------------------------------\n");                      \
    printf("[FAIL] %s: %s\n", name, reason);                                   \
    printf("----------------------------------------\n");                      \
  } while (0)

#define LOG_INFO(fmt, ...) printf("[INFO] " fmt "\n", ##__VA_ARGS__)
#define LOG_ERROR(fmt, ...) fprintf(stderr, "[ERROR] " fmt "\n", ##__VA_ARGS__)
#define LOG_STEP(fmt, ...) printf("  -> " fmt "\n", ##__VA_ARGS__)

// 打印最终测试汇总
static void print_test_summary(void) {
  printf("\n");
  printf("+========================================+\n");
  printf("|           TEST SUMMARY                 |\n");
  printf("+========================================+\n");
  printf("|  Total:  %3d                           |\n", g_tests_total);
  printf("|  Passed: %3d                           |\n", g_tests_passed);
  printf("|  Failed: %3d                           |\n", g_tests_failed);
  printf("+========================================+\n");
  if (g_tests_failed == 0) {
    printf("|  Result: ALL TESTS PASSED              |\n");
  } else {
    printf("|  Result: SOME TESTS FAILED             |\n");
  }
  printf("+========================================+\n");
}

// ===================================================================
// Loop 设备常量定义
// ===================================================================

// 控制命令常量
#define LOOP_CTL_ADD 0x4C80
#define LOOP_CTL_REMOVE 0x4C81
#define LOOP_CTL_GET_FREE 0x4C82
#define LOOP_SET_FD 0x4C00
#define LOOP_CLR_FD 0x4C01
#define LOOP_SET_STATUS64 0x4C04
#define LOOP_GET_STATUS64 0x4C05
#define LOOP_CHANGE_FD 0x4C06
#define LOOP_SET_CAPACITY 0x4C07

// 设备路径和测试参数
#define LOOP_DEVICE_CONTROL "/dev/loop-control"
#define LO_FLAGS_READ_ONLY 0x1
#define TEST_FILE_NAME "test_image.img"
#define TEST_FILE_NAME_2 "test_image_2.img"
#define TEST_SYNC_FILE_NAME "test_sync_image.img"
#define TEST_FILE_SIZE (1024 * 1024)  // 1MB
#define TEST_FILE_SIZE_2 (1024 * 1024) // 1MB

// Loop 状态结构体
// 必须与 Linux UAPI `include/uapi/linux/loop.h` 的 `struct loop_info64` 一致，
// 否则 LOOP_SET_STATUS64/LOOP_GET_STATUS64 会发生字段错位。
struct loop_status64 {
  uint64_t lo_device;
  uint64_t lo_inode;
  uint64_t lo_rdevice;
  uint64_t lo_offset;
  uint64_t lo_sizelimit;
  uint32_t lo_number;
  uint32_t lo_encrypt_type;
  uint32_t lo_encrypt_key_size;
  uint32_t lo_flags;
  uint8_t lo_file_name[64];
  uint8_t lo_crypt_name[64];
  uint8_t lo_encrypt_key[32];
  uint64_t lo_init[2];
};

// ===================================================================
// 全局测试资源
// ===================================================================

static int g_control_fd = -1;
static int g_backing_fd_1 = -1;
static int g_backing_fd_2 = -1;

// ===================================================================
// 辅助函数
// ===================================================================

// 创建测试镜像文件
static int create_test_file(const char *filename, int size) {
  LOG_STEP("Creating test file: %s (%d bytes)", filename, size);

  int fd = open(filename, O_CREAT | O_TRUNC | O_RDWR, 0644);
  if (fd < 0) {
    LOG_ERROR("Failed to create test file: %s", strerror(errno));
    return -1;
  }

  char zero_block[512] = {0};
  for (int i = 0; i < size / 512; ++i) {
    if (write(fd, zero_block, 512) != 512) {
      LOG_ERROR("Failed to write to test file: %s", strerror(errno));
      close(fd);
      return -1;
    }
  }

  close(fd);
  LOG_STEP("Test file created successfully");
  return 0;
}

// Allocate a new registered loop device atomically. LOOP_CTL_GET_FREE returns
// an already registered reusable device and must not be followed by ADD of
// the same minor.
static int create_loop_device(int control_fd, int *out_minor) {
  int minor = ioctl(control_fd, LOOP_CTL_ADD, UINT32_MAX);
  if (minor < 0) {
    LOG_ERROR("Failed to add loop device: %s", strerror(errno));
    return -1;
  }

  *out_minor = minor;
  return 0;
}

// ===================================================================
// 并发测试辅助结构
// ===================================================================

struct io_thread_args {
  char loop_dev_path[64];
  int duration_seconds;
  volatile int should_stop;
  int io_count;
  int error_count;
};

struct delete_thread_args {
  int control_fd;
  int loop_minor;
  int result;
  int error_code;
};

static void *io_worker_thread(void *arg) {
  struct io_thread_args *args = (struct io_thread_args *)arg;
  char buffer[512];
  time_t start_time = time(NULL);

  while (!args->should_stop &&
         (time(NULL) - start_time) < args->duration_seconds) {
    int fd = open(args->loop_dev_path, O_RDWR);
    if (fd < 0) {
      if (errno == ENODEV || errno == ENOENT) {
        break;
      }
      args->error_count++;
      usleep(10000);
      continue;
    }

    if (read(fd, buffer, sizeof(buffer)) < 0) {
      if (errno != ENODEV) {
        args->error_count++;
      }
    } else {
      args->io_count++;
    }

    close(fd);
    usleep(1000);
  }

  return NULL;
}

static void *delete_worker_thread(void *arg) {
  struct delete_thread_args *args = (struct delete_thread_args *)arg;
  usleep(50000); // 50ms 延迟确保 I/O 线程已启动

  args->result = ioctl(args->control_fd, LOOP_CTL_REMOVE, args->loop_minor);
  args->error_code = errno;

  return NULL;
}

// ===================================================================
// 测试用例
// ===================================================================

// 测试 1: 基本读写测试
static int test_basic_read_write(int loop_fd, const char *loop_path,
                                 struct loop_status64 *status) {
  const char *test_name = "Basic Read/Write";
  TEST_BEGIN(test_name);

  char write_buf[512] = "Hello Loop Device!";
  char read_buf[512] = {0};
  char verify_buf[512] = {0};

  // 写入测试
  LOG_STEP("Writing data to loop device...");
  if (lseek(loop_fd, 0, SEEK_SET) < 0) {
    TEST_END_FAIL(test_name, "lseek failed before write");
    return TEST_FAIL;
  }

  if (write(loop_fd, write_buf, sizeof(write_buf)) != sizeof(write_buf)) {
    TEST_END_FAIL(test_name, "write failed");
    return TEST_FAIL;
  }
  LOG_STEP("Write successful: '%s'", write_buf);

  // 验证后端文件
  LOG_STEP("Verifying backing file content...");
  int verify_fd = open(TEST_FILE_NAME, O_RDONLY);
  if (verify_fd < 0) {
    TEST_END_FAIL(test_name, "cannot open backing file for verification");
    return TEST_FAIL;
  }

  if (lseek(verify_fd, (off_t)status->lo_offset, SEEK_SET) < 0 ||
      read(verify_fd, verify_buf, sizeof(write_buf)) != sizeof(write_buf)) {
    close(verify_fd);
    TEST_END_FAIL(test_name, "cannot read backing file");
    return TEST_FAIL;
  }
  close(verify_fd);

  if (memcmp(write_buf, verify_buf, sizeof(write_buf)) != 0) {
    TEST_END_FAIL(test_name, "backing file content mismatch");
    return TEST_FAIL;
  }
  LOG_STEP("Backing file verification passed");

  // 读取测试
  LOG_STEP("Reading data from loop device...");
  if (lseek(loop_fd, 0, SEEK_SET) < 0) {
    TEST_END_FAIL(test_name, "lseek failed before read");
    return TEST_FAIL;
  }

  if (read(loop_fd, read_buf, sizeof(read_buf)) != sizeof(read_buf)) {
    TEST_END_FAIL(test_name, "read failed");
    return TEST_FAIL;
  }
  LOG_STEP("Read successful: '%s'", read_buf);

  if (strcmp(write_buf, read_buf) != 0) {
    TEST_END_FAIL(test_name, "read data mismatch");
    return TEST_FAIL;
  }

  TEST_END_PASS(test_name);
  return TEST_PASS;
}

// 同步写和 fsync 必须使用 LOOP_SET_FD 时保留的 open file description；
// 关闭用户传入的原始 backing fd 后仍应可用。
static int test_sync_io_retains_backing_file(void) {
  const char *test_name = "O_SYNC/O_DSYNC/fsync Retain Backing File";
  TEST_BEGIN(test_name);

  int minor = -1;
  int backing_fd = -1;
  int sync_fd = -1;
  int dsync_fd = -1;
  int verify_fd = -1;
  char path[64];
  char sync_data[512] = "loop O_SYNC retained backing";
  char dsync_data[512] = "loop O_DSYNC retained backing";
  char observed[1024] = {0};

  if (create_test_file(TEST_SYNC_FILE_NAME, TEST_FILE_SIZE) < 0) {
    goto fail;
  }
  backing_fd = open(TEST_SYNC_FILE_NAME, O_RDWR);
  if (backing_fd < 0 || create_loop_device(g_control_fd, &minor) < 0) {
    goto fail;
  }
  sprintf(path, "/dev/loop%d", minor);
  sync_fd = open(path, O_RDWR | O_SYNC);
  if (sync_fd < 0 || ioctl(sync_fd, LOOP_SET_FD, backing_fd) < 0) {
    goto fail;
  }

  close(backing_fd);
  backing_fd = -1;

  dsync_fd = open(path, O_RDWR | O_DSYNC);
  if (dsync_fd < 0 ||
      pwrite(sync_fd, sync_data, sizeof(sync_data), 0) != sizeof(sync_data) ||
      pwrite(dsync_fd, dsync_data, sizeof(dsync_data), 512) !=
          sizeof(dsync_data) ||
      fsync(sync_fd) != 0 || fdatasync(dsync_fd) != 0) {
    goto fail;
  }

  verify_fd = open(TEST_SYNC_FILE_NAME, O_RDONLY);
  if (verify_fd < 0 ||
      pread(verify_fd, observed, sizeof(observed), 0) != sizeof(observed) ||
      memcmp(observed, sync_data, sizeof(sync_data)) != 0 ||
      memcmp(observed + 512, dsync_data, sizeof(dsync_data)) != 0) {
    goto fail;
  }

  close(verify_fd);
  close(dsync_fd);
  ioctl(sync_fd, LOOP_CLR_FD, 0);
  close(sync_fd);
  ioctl(g_control_fd, LOOP_CTL_REMOVE, minor);
  unlink(TEST_SYNC_FILE_NAME);
  TEST_END_PASS(test_name);
  return TEST_PASS;

fail:
  if (verify_fd >= 0)
    close(verify_fd);
  if (dsync_fd >= 0)
    close(dsync_fd);
  if (sync_fd >= 0) {
    ioctl(sync_fd, LOOP_CLR_FD, 0);
    close(sync_fd);
  }
  if (backing_fd >= 0)
    close(backing_fd);
  if (minor >= 0)
    ioctl(g_control_fd, LOOP_CTL_REMOVE, minor);
  unlink(TEST_SYNC_FILE_NAME);
  TEST_END_FAIL(test_name, "retained backing synchronous I/O failed");
  return TEST_FAIL;
}

// 测试 2: status ioctl 不得修改绑定时确定的 READ_ONLY
static int test_status_preserves_read_only(int loop_fd,
                                           struct loop_status64 *status) {
  const char *test_name = "Status Preserves Bind-Time Read-Only";
  TEST_BEGIN(test_name);

  LOG_STEP("Attempting to set READ_ONLY through LOOP_SET_STATUS64...");
  status->lo_flags |= LO_FLAGS_READ_ONLY;
  if (ioctl(loop_fd, LOOP_SET_STATUS64, status) < 0) {
    TEST_END_FAIL(test_name, "LOOP_SET_STATUS64 failed");
    return TEST_FAIL;
  }

  struct loop_status64 observed = {0};
  if (ioctl(loop_fd, LOOP_GET_STATUS64, &observed) < 0) {
    TEST_END_FAIL(test_name, "LOOP_GET_STATUS64 failed");
    return TEST_FAIL;
  }
  if (observed.lo_flags & LO_FLAGS_READ_ONLY) {
    TEST_END_FAIL(test_name, "status ioctl changed bind-time READ_ONLY");
    return TEST_FAIL;
  }
  status->lo_flags &= ~LO_FLAGS_READ_ONLY;

  TEST_END_PASS(test_name);
  return TEST_PASS;
}

// 测试 3: writable loop 必须拒绝 LOOP_CHANGE_FD
static int test_writable_change_fd_rejected(int loop_fd) {
  const char *test_name = "Writable LOOP_CHANGE_FD Rejected";
  TEST_BEGIN(test_name);

  errno = 0;
  if (ioctl(loop_fd, LOOP_CHANGE_FD, g_backing_fd_2) >= 0 ||
      errno != EINVAL) {
    TEST_END_FAIL(test_name, "writable change did not fail with EINVAL");
    return TEST_FAIL;
  }

  TEST_END_PASS(test_name);
  return TEST_PASS;
}

// 测试 4: read-only、同容量 backing 可以 CHANGE_FD
static int test_read_only_equal_size_change_fd(void) {
  const char *test_name = "Read-Only Equal-Size LOOP_CHANGE_FD";
  TEST_BEGIN(test_name);

  int minor = -1;
  int loop_fd = -1;
  int rw_loop_fd = -1;
  char path[64];
  char expected[512] = "second backing generation";
  char observed[512] = {0};

  if (pwrite(g_backing_fd_2, expected, sizeof(expected), 0) !=
      sizeof(expected)) {
    TEST_END_FAIL(test_name, "cannot initialize second backing");
    return TEST_FAIL;
  }
  if (create_loop_device(g_control_fd, &minor) < 0) {
    TEST_END_FAIL(test_name, "cannot create loop device");
    return TEST_FAIL;
  }
  sprintf(path, "/dev/loop%d", minor);
  loop_fd = open(path, O_RDONLY);
  if (loop_fd < 0 || ioctl(loop_fd, LOOP_SET_FD, g_backing_fd_1) < 0) {
    goto fail;
  }
  if (ioctl(loop_fd, LOOP_CHANGE_FD, g_backing_fd_2) < 0) {
    goto fail;
  }
  if (pread(loop_fd, observed, sizeof(observed), 0) != sizeof(observed) ||
      memcmp(expected, observed, sizeof(expected)) != 0) {
    goto fail;
  }

  struct loop_status64 status = {0};
  if (ioctl(loop_fd, LOOP_GET_STATUS64, &status) < 0 ||
      !(status.lo_flags & LO_FLAGS_READ_ONLY)) {
    goto fail;
  }
  status.lo_flags = 0;
  if (ioctl(loop_fd, LOOP_SET_STATUS64, &status) < 0 ||
      ioctl(loop_fd, LOOP_GET_STATUS64, &status) < 0 ||
      !(status.lo_flags & LO_FLAGS_READ_ONLY)) {
    goto fail;
  }

  rw_loop_fd = open(path, O_RDWR);
  errno = 0;
  if (rw_loop_fd < 0 ||
      write(rw_loop_fd, expected, sizeof(expected)) >= 0 || errno != EROFS) {
    goto fail;
  }

  close(rw_loop_fd);
  ioctl(loop_fd, LOOP_CLR_FD, 0);
  close(loop_fd);
  ioctl(g_control_fd, LOOP_CTL_REMOVE, minor);
  TEST_END_PASS(test_name);
  return TEST_PASS;

fail:
  if (rw_loop_fd >= 0)
    close(rw_loop_fd);
  if (loop_fd >= 0) {
    ioctl(loop_fd, LOOP_CLR_FD, 0);
    close(loop_fd);
  }
  if (minor >= 0)
    ioctl(g_control_fd, LOOP_CTL_REMOVE, minor);
  TEST_END_FAIL(test_name, "read-only equal-size change semantics failed");
  return TEST_FAIL;
}

// 测试 5: LOOP_SET_CAPACITY
static int test_set_capacity(int loop_fd, struct loop_status64 *status) {
  const char *test_name = "LOOP_SET_CAPACITY";
  TEST_BEGIN(test_name);

  // 扩大后端文件
  LOG_STEP("Resizing backing file to %d bytes...", TEST_FILE_SIZE * 2);
  int resize_fd = open(TEST_FILE_NAME, O_RDWR);
  if (resize_fd < 0) {
    TEST_END_FAIL(test_name, "cannot open backing file for resize");
    return TEST_FAIL;
  }

  int new_size = TEST_FILE_SIZE * 2;
  if (ftruncate(resize_fd, new_size) < 0) {
    close(resize_fd);
    TEST_END_FAIL(test_name, "ftruncate failed");
    return TEST_FAIL;
  }
  close(resize_fd);
  LOG_STEP("Backing file resized successfully");

  // 清除 sizelimit 以便看到扩展效果
  LOG_STEP("Clearing sizelimit...");
  status->lo_sizelimit = 0;
  if (ioctl(loop_fd, LOOP_SET_STATUS64, status) < 0) {
    TEST_END_FAIL(test_name, "failed to clear sizelimit");
    return TEST_FAIL;
  }

  // 调用 LOOP_SET_CAPACITY
  LOG_STEP("Calling LOOP_SET_CAPACITY...");
  if (ioctl(loop_fd, LOOP_SET_CAPACITY, 0) < 0) {
    TEST_END_FAIL(test_name, "LOOP_SET_CAPACITY failed");
    return TEST_FAIL;
  }
  LOG_STEP("LOOP_SET_CAPACITY successful");

  // 获取新状态
  struct loop_status64 new_status = {0};
  if (ioctl(loop_fd, LOOP_GET_STATUS64, &new_status) < 0) {
    TEST_END_FAIL(test_name, "failed to get status after capacity change");
    return TEST_FAIL;
  }
  LOG_STEP("New status - offset: %llu, sizelimit: %llu",
           (unsigned long long)new_status.lo_offset,
           (unsigned long long)new_status.lo_sizelimit);

  // 尝试写入扩展区域
  LOG_STEP("Writing to extended region...");
  char extended_buf[512] = "Extended Data!";
  if (lseek(loop_fd, TEST_FILE_SIZE, SEEK_SET) < 0 ||
      write(loop_fd, extended_buf, sizeof(extended_buf)) !=
          sizeof(extended_buf)) {
    TEST_END_FAIL(test_name, "write to extended region failed");
    return TEST_FAIL;
  }
  LOG_STEP("Write to extended region successful");

  // 验证扩展区域内容
  LOG_STEP("Verifying extended region content...");
  char verify_buf[512] = {0};
  int verify_fd = open(TEST_FILE_NAME, O_RDONLY);
  if (verify_fd < 0) {
    TEST_END_FAIL(test_name, "cannot open backing file for verification");
    return TEST_FAIL;
  }

  off_t verify_offset = (off_t)status->lo_offset + TEST_FILE_SIZE;
  if (lseek(verify_fd, verify_offset, SEEK_SET) < 0 ||
      read(verify_fd, verify_buf, sizeof(extended_buf)) !=
          sizeof(extended_buf)) {
    close(verify_fd);
    TEST_END_FAIL(test_name, "cannot read extended region");
    return TEST_FAIL;
  }
  close(verify_fd);

  if (memcmp(extended_buf, verify_buf, sizeof(extended_buf)) != 0) {
    TEST_END_FAIL(test_name, "extended region content mismatch");
    return TEST_FAIL;
  }

  TEST_END_PASS(test_name);
  return TEST_PASS;
}

// 已绑定且仍有 opener 的 loop 设备必须拒绝 LOOP_CTL_REMOVE。
static int test_concurrent_io_deletion(void) {
  const char *test_name = "Bound Loop Removal During Concurrent I/O";
  TEST_BEGIN(test_name);

#define NUM_IO_THREADS 4

  // 创建测试用 loop 设备
  int test_minor;
  if (create_loop_device(g_control_fd, &test_minor) < 0) {
    TEST_END_FAIL(test_name, "failed to create test loop device");
    return TEST_FAIL;
  }
  LOG_STEP("Created loop device loop%d", test_minor);

  char test_path[64];
  sprintf(test_path, "/dev/loop%d", test_minor);

  int test_fd = open(test_path, O_RDWR);
  if (test_fd < 0) {
    ioctl(g_control_fd, LOOP_CTL_REMOVE, test_minor);
    TEST_END_FAIL(test_name, "failed to open test loop device");
    return TEST_FAIL;
  }

  if (ioctl(test_fd, LOOP_SET_FD, g_backing_fd_1) < 0) {
    close(test_fd);
    ioctl(g_control_fd, LOOP_CTL_REMOVE, test_minor);
    TEST_END_FAIL(test_name, "failed to bind test loop device");
    return TEST_FAIL;
  }
  LOG_STEP("Bound backing file to test device");

  // 启动 I/O 线程
  pthread_t io_threads[NUM_IO_THREADS];
  struct io_thread_args io_args[NUM_IO_THREADS];

  LOG_STEP("Starting %d I/O threads...", NUM_IO_THREADS);
  for (int i = 0; i < NUM_IO_THREADS; i++) {
    strcpy(io_args[i].loop_dev_path, test_path);
    io_args[i].duration_seconds = 5;
    io_args[i].should_stop = 0;
    io_args[i].io_count = 0;
    io_args[i].error_count = 0;

    if (pthread_create(&io_threads[i], NULL, io_worker_thread, &io_args[i]) !=
        0) {
      for (int j = 0; j < i; j++) {
        io_args[j].should_stop = 1;
        pthread_join(io_threads[j], NULL);
      }
      ioctl(test_fd, LOOP_CLR_FD, 0);
      close(test_fd);
      ioctl(g_control_fd, LOOP_CTL_REMOVE, test_minor);
      TEST_END_FAIL(test_name, "failed to create I/O thread");
      return TEST_FAIL;
    }
  }

  // 保持主 fd 打开，并在其他 opener 正在执行 I/O 时尝试删除。
  LOG_STEP("Starting removal attempt...");
  pthread_t delete_thread;
  struct delete_thread_args delete_args = {.control_fd = g_control_fd,
                                           .loop_minor = test_minor,
                                           .result = 0,
                                           .error_code = 0};

  if (pthread_create(&delete_thread, NULL, delete_worker_thread,
                     &delete_args) != 0) {
    for (int i = 0; i < NUM_IO_THREADS; i++) {
      io_args[i].should_stop = 1;
      pthread_join(io_threads[i], NULL);
    }
    ioctl(test_fd, LOOP_CLR_FD, 0);
    close(test_fd);
    ioctl(g_control_fd, LOOP_CTL_REMOVE, test_minor);
    TEST_END_FAIL(test_name, "failed to create delete thread");
    return TEST_FAIL;
  }

  // 等待删除完成
  pthread_join(delete_thread, NULL);
  LOG_STEP("Deletion completed with result: %d (errno: %d)", delete_args.result,
           delete_args.error_code);

  // 停止并等待 I/O 线程
  for (int i = 0; i < NUM_IO_THREADS; i++) {
    io_args[i].should_stop = 1;
  }

  int total_io = 0, total_errors = 0;
  for (int i = 0; i < NUM_IO_THREADS; i++) {
    pthread_join(io_threads[i], NULL);
    total_io += io_args[i].io_count;
    total_errors += io_args[i].error_count;
  }
  LOG_STEP("I/O statistics: %d successful, %d errors", total_io, total_errors);

  if (delete_args.result >= 0 || delete_args.error_code != EBUSY) {
    ioctl(test_fd, LOOP_CLR_FD, 0);
    close(test_fd);
    ioctl(g_control_fd, LOOP_CTL_REMOVE, test_minor);
    TEST_END_FAIL(test_name, "bound device removal did not fail with EBUSY");
    return TEST_FAIL;
  }

  // 失败的删除不得影响已绑定设备；清除绑定后才能真正删除。
  char verify_buf[512];
  if (pread(test_fd, verify_buf, sizeof(verify_buf), 0) != sizeof(verify_buf) ||
      ioctl(test_fd, LOOP_CLR_FD, 0) < 0) {
    close(test_fd);
    ioctl(g_control_fd, LOOP_CTL_REMOVE, test_minor);
    TEST_END_FAIL(test_name, "failed removal damaged bound device");
    return TEST_FAIL;
  }
  errno = 0;
  if (ioctl(g_control_fd, LOOP_CTL_REMOVE, test_minor) >= 0 || errno != EBUSY) {
    close(test_fd);
    TEST_END_FAIL(test_name,
                  "unbound device removal ignored the remaining opener");
    return TEST_FAIL;
  }
  close(test_fd);
  if (ioctl(g_control_fd, LOOP_CTL_REMOVE, test_minor) < 0) {
    TEST_END_FAIL(test_name, "unbound device removal failed");
    return TEST_FAIL;
  }

  int verify_fd = open(test_path, O_RDWR);
  if (verify_fd >= 0) {
    close(verify_fd);
    TEST_END_FAIL(test_name, "device still accessible after successful removal");
    return TEST_FAIL;
  }

  TEST_END_PASS(test_name);
  return TEST_PASS;

#undef NUM_IO_THREADS
}

// 测试 6: 删除未绑定的设备
static int test_delete_unbound_device(void) {
  const char *test_name = "Delete Unbound Device";
  TEST_BEGIN(test_name);

  int minor;
  if (create_loop_device(g_control_fd, &minor) < 0) {
    TEST_END_FAIL(test_name, "failed to create loop device");
    return TEST_FAIL;
  }
  LOG_STEP("Created unbound loop device loop%d", minor);

  // 立即删除
  LOG_STEP("Deleting unbound device...");
  if (ioctl(g_control_fd, LOOP_CTL_REMOVE, minor) < 0) {
    TEST_END_FAIL(test_name, "failed to delete unbound device");
    return TEST_FAIL;
  }

  TEST_END_PASS(test_name);
  return TEST_PASS;
}

// 测试 7: 重复删除设备
static int test_duplicate_deletion(void) {
  const char *test_name = "Duplicate Deletion";
  TEST_BEGIN(test_name);

  int minor;
  if (create_loop_device(g_control_fd, &minor) < 0) {
    TEST_END_FAIL(test_name, "failed to create loop device");
    return TEST_FAIL;
  }
  LOG_STEP("Created loop device loop%d", minor);

  // 第一次删除
  LOG_STEP("First deletion...");
  if (ioctl(g_control_fd, LOOP_CTL_REMOVE, minor) < 0) {
    TEST_END_FAIL(test_name, "first deletion failed");
    return TEST_FAIL;
  }
  LOG_STEP("First deletion successful");

  // 第二次删除（应该失败）
  LOG_STEP("Second deletion (should fail)...");
  errno = 0;
  int ret = ioctl(g_control_fd, LOOP_CTL_REMOVE, minor);
  if (ret >= 0) {
    TEST_END_FAIL(test_name, "second deletion should have failed");
    return TEST_FAIL;
  }

  if (errno != ENODEV && errno != EINVAL) {
    char msg[64];
    snprintf(msg, sizeof(msg), "unexpected errno: %d", errno);
    TEST_END_FAIL(test_name, msg);
    return TEST_FAIL;
  }
  LOG_STEP("Second deletion correctly failed with errno %d", errno);

  TEST_END_PASS(test_name);
  return TEST_PASS;
}

// 测试 8: 文件描述符泄漏检测
static int test_fd_leak_detection(void) {
  const char *test_name = "FD Leak Detection";
  TEST_BEGIN(test_name);

#define LEAK_TEST_COUNT 10

  int minors[LEAK_TEST_COUNT];
  int fds[LEAK_TEST_COUNT];

  LOG_STEP("Creating %d loop devices...", LEAK_TEST_COUNT);
  for (int i = 0; i < LEAK_TEST_COUNT; i++) {
    minors[i] = -1;
    fds[i] = -1;

    if (create_loop_device(g_control_fd, &minors[i]) < 0) {
      // 清理已创建的设备
      for (int j = 0; j < i; j++) {
        if (fds[j] >= 0) {
          ioctl(fds[j], LOOP_CLR_FD, 0);
          close(fds[j]);
        }
        if (minors[j] >= 0)
          ioctl(g_control_fd, LOOP_CTL_REMOVE, minors[j]);
      }
      TEST_END_FAIL(test_name, "failed to create loop device");
      return TEST_FAIL;
    }

    char path[64];
    sprintf(path, "/dev/loop%d", minors[i]);
    fds[i] = open(path, O_RDWR);
    if (fds[i] < 0) {
      // 清理已创建的设备
      for (int j = 0; j < i; j++) {
        if (fds[j] >= 0) {
          ioctl(fds[j], LOOP_CLR_FD, 0);
          close(fds[j]);
        }
        if (minors[j] >= 0)
          ioctl(g_control_fd, LOOP_CTL_REMOVE, minors[j]);
      }
      ioctl(g_control_fd, LOOP_CTL_REMOVE, minors[i]);
      TEST_END_FAIL(test_name, "failed to open loop device");
      return TEST_FAIL;
    }

    // 立即绑定后端文件，这样下次 GET_FREE 会返回不同的 minor
    if (ioctl(fds[i], LOOP_SET_FD, g_backing_fd_1) < 0) {
      // 清理已创建的设备
      for (int j = 0; j <= i; j++) {
        if (fds[j] >= 0) {
          ioctl(fds[j], LOOP_CLR_FD, 0);
          close(fds[j]);
        }
        if (minors[j] >= 0)
          ioctl(g_control_fd, LOOP_CTL_REMOVE, minors[j]);
      }
      TEST_END_FAIL(test_name, "failed to bind loop device");
      return TEST_FAIL;
    }
  }
  LOG_STEP("Created %d devices successfully", LEAK_TEST_COUNT);

  // 删除所有设备
  LOG_STEP("Deleting all devices...");
  int success_count = 0;
  for (int i = 0; i < LEAK_TEST_COUNT; i++) {
    if (fds[i] >= 0) {
      ioctl(fds[i], LOOP_CLR_FD, 0);
      close(fds[i]);
    }
    if (ioctl(g_control_fd, LOOP_CTL_REMOVE, minors[i]) == 0) {
      success_count++;
    }
  }

  LOG_STEP("Deleted %d/%d devices", success_count, LEAK_TEST_COUNT);

  if (success_count != LEAK_TEST_COUNT) {
    TEST_END_FAIL(test_name, "not all devices deleted");
    return TEST_FAIL;
  }

  TEST_END_PASS(test_name);
  return TEST_PASS;

#undef LEAK_TEST_COUNT
}

// 测试 9: 删除后设备不可访问
static int test_device_inaccessible_after_deletion(void) {
  const char *test_name = "Device Inaccessible After Deletion";
  TEST_BEGIN(test_name);

  int minor;
  if (create_loop_device(g_control_fd, &minor) < 0) {
    TEST_END_FAIL(test_name, "failed to create loop device");
    return TEST_FAIL;
  }

  char path[64];
  sprintf(path, "/dev/loop%d", minor);
  LOG_STEP("Created loop device %s", path);

  int fd = open(path, O_RDWR);
  if (fd < 0) {
    ioctl(g_control_fd, LOOP_CTL_REMOVE, minor);
    TEST_END_FAIL(test_name, "failed to open loop device");
    return TEST_FAIL;
  }

  if (ioctl(fd, LOOP_SET_FD, g_backing_fd_1) < 0) {
    close(fd);
    ioctl(g_control_fd, LOOP_CTL_REMOVE, minor);
    TEST_END_FAIL(test_name, "failed to bind loop device");
    return TEST_FAIL;
  }
  LOG_STEP("Bound backing file");

  // 执行一次成功的 I/O
  char buf[512] = "Test data";
  if (write(fd, buf, sizeof(buf)) != sizeof(buf)) {
    ioctl(fd, LOOP_CLR_FD, 0);
    close(fd);
    ioctl(g_control_fd, LOOP_CTL_REMOVE, minor);
    TEST_END_FAIL(test_name, "initial write failed");
    return TEST_FAIL;
  }
  LOG_STEP("Initial I/O successful");

  // Linux rejects LOOP_CTL_REMOVE while the device is still bound. Detach the
  // backing file first, then close the final opener before removing the node.
  if (ioctl(fd, LOOP_CLR_FD, 0) < 0) {
    close(fd);
    ioctl(g_control_fd, LOOP_CTL_REMOVE, minor);
    TEST_END_FAIL(test_name, "failed to clear loop backing");
    return TEST_FAIL;
  }
  close(fd);

  // 删除设备
  LOG_STEP("Deleting device...");
  if (ioctl(g_control_fd, LOOP_CTL_REMOVE, minor) < 0) {
    TEST_END_FAIL(test_name, "deletion failed");
    return TEST_FAIL;
  }

  // 尝试重新打开
  LOG_STEP("Attempting to reopen deleted device...");
  errno = 0;
  int reopen_fd = open(path, O_RDWR);
  if (reopen_fd >= 0) {
    close(reopen_fd);
    TEST_END_FAIL(test_name, "device still accessible after deletion");
    return TEST_FAIL;
  }

  if (errno != ENODEV && errno != ENOENT) {
    char msg[64];
    snprintf(msg, sizeof(msg), "unexpected errno: %d", errno);
    TEST_END_FAIL(test_name, msg);
    return TEST_FAIL;
  }
  LOG_STEP("Device correctly inaccessible (errno: %d)", errno);

  TEST_END_PASS(test_name);
  return TEST_PASS;
}

// ===================================================================
// 主函数
// ===================================================================

int main(void) {
  printf("+========================================+\n");
  printf("|     Loop Device Test Suite             |\n");
  printf("+========================================+\n");

  // 初始化测试环境
  LOG_INFO("Initializing test environment...");

  if (create_test_file(TEST_FILE_NAME, TEST_FILE_SIZE) < 0 ||
      create_test_file(TEST_FILE_NAME_2, TEST_FILE_SIZE_2) < 0) {
    LOG_ERROR("Failed to create test files");
    return EXIT_FAILURE;
  }

  g_backing_fd_1 = open(TEST_FILE_NAME, O_RDWR);
  g_backing_fd_2 = open(TEST_FILE_NAME_2, O_RDWR);
  if (g_backing_fd_1 < 0 || g_backing_fd_2 < 0) {
    LOG_ERROR("Failed to open backing files");
    goto cleanup_files;
  }

  g_control_fd = open(LOOP_DEVICE_CONTROL, O_RDWR);
  if (g_control_fd < 0) {
    LOG_ERROR("Failed to open loop control device: %s", strerror(errno));
    goto cleanup_backing;
  }
  LOG_INFO("Test environment initialized");

  // 创建主测试 loop 设备
  int main_minor;
  if (create_loop_device(g_control_fd, &main_minor) < 0) {
    LOG_ERROR("Failed to create main loop device");
    goto cleanup_control;
  }

  char main_loop_path[64];
  sprintf(main_loop_path, "/dev/loop%d", main_minor);
  LOG_INFO("Created main loop device: %s", main_loop_path);

  int main_loop_fd = open(main_loop_path, O_RDWR);
  if (main_loop_fd < 0) {
    LOG_ERROR("Failed to open main loop device");
    ioctl(g_control_fd, LOOP_CTL_REMOVE, main_minor);
    goto cleanup_control;
  }

  if (ioctl(main_loop_fd, LOOP_SET_FD, g_backing_fd_1) < 0) {
    LOG_ERROR("Failed to bind main loop device");
    close(main_loop_fd);
    ioctl(g_control_fd, LOOP_CTL_REMOVE, main_minor);
    goto cleanup_control;
  }

  // 配置偏移和大小限制
  struct loop_status64 status = {
      .lo_offset = 512,
      .lo_sizelimit = TEST_FILE_SIZE - 512,
      .lo_flags = 0,
  };

  if (ioctl(main_loop_fd, LOOP_SET_STATUS64, &status) < 0) {
    LOG_ERROR("Failed to configure main loop device");
    close(main_loop_fd);
    ioctl(g_control_fd, LOOP_CTL_REMOVE, main_minor);
    goto cleanup_control;
  }
  LOG_INFO("Main loop device configured (offset: %llu, sizelimit: %llu)",
           (unsigned long long)status.lo_offset,
           (unsigned long long)status.lo_sizelimit);

  // ===================================================================
  // 运行测试用例
  // ===================================================================

  // 基本功能测试
  test_basic_read_write(main_loop_fd, main_loop_path, &status);
  test_status_preserves_read_only(main_loop_fd, &status);
  test_writable_change_fd_rejected(main_loop_fd);
  test_read_only_equal_size_change_fd();
  test_sync_io_retains_backing_file();
  test_set_capacity(main_loop_fd, &status);

  // 资源回收测试
  test_concurrent_io_deletion();
  test_delete_unbound_device();
  test_duplicate_deletion();
  test_fd_leak_detection();
  test_device_inaccessible_after_deletion();

  // ===================================================================
  // 清理
  // ===================================================================

  LOG_INFO("Cleaning up main loop device...");
  ioctl(main_loop_fd, LOOP_CLR_FD, 0);
  close(main_loop_fd);
  ioctl(g_control_fd, LOOP_CTL_REMOVE, main_minor);

cleanup_control:
  if (g_control_fd >= 0)
    close(g_control_fd);

cleanup_backing:
  if (g_backing_fd_1 >= 0)
    close(g_backing_fd_1);
  if (g_backing_fd_2 >= 0)
    close(g_backing_fd_2);

cleanup_files:
  unlink(TEST_FILE_NAME);
  unlink(TEST_FILE_NAME_2);

  // 打印测试汇总
  print_test_summary();

  return (g_tests_failed == 0) ? EXIT_SUCCESS : EXIT_FAILURE;
}
