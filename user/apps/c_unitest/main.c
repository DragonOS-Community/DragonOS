// 自检程序：验证 SYS_GETXATTR、SYS_LGETXATTR、SYS_FGETXATTR、
//           SYS_SETXATTR、SYS_FSETXATTR、SYS_LSETXATTR 是否工作正常。
// 覆盖内容：
// - 路径型 setxattr/getxattr
// - 符号链接 lsetxattr/lgetxattr 与跟随链接 getxattr 的差异
// - 文件描述符 fsetxattr/fgetxattr
// - 错误分支：XATTR_CREATE / XATTR_REPLACE，读取不存在属性 ENODATA

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/xattr.h>
#include <unistd.h>
#include <sys/mount.h>

#ifndef ENOATTR
#define ENOATTR ENODATA
#endif

static int g_pass = 0;
static int g_fail = 0;
static int g_skip = 0;

static void pass(const char *msg) {
	printf("PASS: %s\n", msg);
	g_pass++;
}

static void fail(const char *msg) {
	printf("FAIL: %s (errno=%d: %s)\n", msg, errno, strerror(errno));
	g_fail++;
}

static void skip(const char *msg) {
	printf("SKIP: %s\n", msg);
	g_skip++;
}

static bool errno_is(int e1, int e2) {
	// 某些实现可能返回 ENODATA 或 ENOATTR，视为等价
	if ((e1 == ENODATA && e2 == ENOATTR) || (e1 == ENOATTR && e2 == ENODATA)) return true;
	return e1 == e2;
}

static int touch_file(const char *path) {
	int fd = open(path, O_CREAT | O_TRUNC | O_RDWR, 0644);
	if (fd < 0) return -1;
	int saved = errno;
	if (close(fd) < 0) {
		return -1;
	}
	errno = saved;
	return 0;
}

static int ensure_dir(const char *path, mode_t mode) {
	if (mkdir(path, mode) == 0) return 0;
	if (errno == EEXIST) return 0;
	return -1;
}

static int mount_ext4(const char *source, const char *target) {
	// 确保挂载点存在（/mnt 也尝试创建，忽略已存在）
	(void)ensure_dir("/mnt", 0755);
	if (ensure_dir(target, 0755) != 0) {
		return -1;
	}
	const char *fstype = "ext4";
	unsigned long flags = 0; // 不使用 MS_BIND
	const void *data = NULL;
	return mount(source, target, fstype, flags, data);
}

static int umount_ext4(const char *target) {
	return umount(target);
}

static void test_path_get_set(const char *file) {
	const char *name = "user.sgxattr";
	const char *val = "hello";
	const char *val2 = "world";
	char buf[256];

	// setxattr: 正常创建
	if (setxattr(file, name, val, strlen(val), 0) == 0) {
		pass("setxattr(path) create");
	} else if (errno == ENOTSUP || errno == ENOSYS) {
		skip("setxattr(path) not supported by FS or kernel");
		return; // 后续与 xattr 强相关，直接跳出该分组
	} else {
		fail("setxattr(path) create");
		return;
	}

	// getxattr: 正常读取
	ssize_t n = getxattr(file, name, buf, sizeof(buf));
	if (n >= 0 && (size_t)n == strlen(val) && memcmp(buf, val, (size_t)n) == 0) {
		pass("getxattr(path) read back");
	} else {
		fail("getxattr(path) read back");
	}

	// getxattr: 先探测长度
	n = getxattr(file, name, NULL, 0);
	if (n == (ssize_t)strlen(val)) {
		pass("getxattr(path) size probe (NULL buffer)");
	} else {
		fail("getxattr(path) size probe (NULL buffer)");
	}

	// setxattr: XATTR_CREATE（存在则应 EEXIST）
	if (setxattr(file, name, val2, strlen(val2), XATTR_CREATE) == -1 && errno == EEXIST) {
		pass("setxattr(path) XATTR_CREATE -> EEXIST");
	} else {
		fail("setxattr(path) XATTR_CREATE should fail with EEXIST");
	}

	// setxattr: XATTR_REPLACE（替换为 val2）
	if (setxattr(file, name, val2, strlen(val2), XATTR_REPLACE) == 0) {
		pass("setxattr(path) XATTR_REPLACE");
	} else {
		fail("setxattr(path) XATTR_REPLACE");
	}

	// getxattr: 读取替换后的值
	n = getxattr(file, name, buf, sizeof(buf));
	if (n >= 0 && (size_t)n == strlen(val2) && memcmp(buf, val2, (size_t)n) == 0) {
		pass("getxattr(path) read replaced value");
	} else {
		fail("getxattr(path) read replaced value");
	}

	// getxattr: 读取不存在的属性 -> ENODATA/ENOATTR
	errno = 0;
	if (getxattr(file, "user.not_exist", buf, sizeof(buf)) == -1 &&
		(errno_is(errno, ENODATA) || errno_is(errno, ENOATTR))) {
		pass("getxattr(path) non-existent -> ENODATA/ENOATTR");
	} else {
		fail("getxattr(path) non-existent should return ENODATA/ENOATTR");
	}
}

static void test_symlink_get_set(const char *file, const char *symlink_path) {
	const char *name_link = "user.sgxattr_link";
	const char *val_link = "linkval";
	char buf[256];

	unlink(symlink_path);
	if (symlink(file, symlink_path) == 0) {
		pass("create symlink");
	} else {
		fail("create symlink");
		return;
	}

	// lsetxattr: 设置在符号链接对象本身
	if (lsetxattr(symlink_path, name_link, val_link, strlen(val_link), 0) == 0) {
		pass("lsetxattr(symlink)");
	} else if (errno == ENOTSUP || errno == ENOSYS || errno == EPERM) {
		// 许多系统/文件系统策略禁止在符号链接上设置 user.* xattr，返回 EPERM
		skip("lsetxattr(symlink) not permitted/supported");
		return;
	} else {
		fail("lsetxattr(symlink)");
		return;
	}

	// lgetxattr: 读取链接自身属性
	ssize_t n = lgetxattr(symlink_path, name_link, buf, sizeof(buf));
	if (n >= 0 && (size_t)n == strlen(val_link) && memcmp(buf, val_link, (size_t)n) == 0) {
		pass("lgetxattr(symlink) read back");
	} else {
		fail("lgetxattr(symlink) read back");
	}

	// getxattr: 跟随链接读取，目标文件上不存在该属性，应返回 ENODATA/ENOATTR
	errno = 0;
	if (getxattr(symlink_path, name_link, buf, sizeof(buf)) == -1 &&
		(errno_is(errno, ENODATA) || errno_is(errno, ENOATTR))) {
		pass("getxattr(symlink-follow) non-existent on target -> ENODATA/ENOATTR");
	} else {
		fail("getxattr(symlink-follow) should return ENODATA/ENOATTR for link-only attr");
	}

	unlink(symlink_path);
}

static void test_fd_get_set(const char *file) {
	const char *name_fd = "user.sgxattr_fd";
	const char *val_fd = "fdval";
	char buf[256];

	int fd = open(file, O_RDWR);
	if (fd < 0) {
		fail("open file for f*getxattr/f*setxattr");
		return;
	}

	if (fsetxattr(fd, name_fd, val_fd, strlen(val_fd), 0) == 0) {
		pass("fsetxattr(fd)");
	} else if (errno == ENOTSUP || errno == ENOSYS) {
		skip("fsetxattr(fd) not supported");
		close(fd);
		return;
	} else {
		fail("fsetxattr(fd)");
		close(fd);
		return;
	}

	ssize_t n = fgetxattr(fd, name_fd, buf, sizeof(buf));
	if (n >= 0 && (size_t)n == strlen(val_fd) && memcmp(buf, val_fd, (size_t)n) == 0) {
		pass("fgetxattr(fd) read back");
	} else {
		fail("fgetxattr(fd) read back");
	}

	close(fd);
}

int main(int argc, char **argv) {
	// 固定挂载配置与测试路径（无需命令行参数）
	const char *source = "/dev/vdb";
	const char *target = "/mnt/ext4";
	const char *file = "/mnt/ext4/xattr_test_file.txt";
	const char *symlink_path = "/mnt/ext4/xattr_test_link";

	// 先挂载 ext4
	if (mount_ext4(source, target) == 0) {
		pass("mount ext4");
	} else {
		fail("mount ext4");
		goto report;
	}

	// 清理现场（位于挂载点内）
	unlink(symlink_path);
	unlink(file);

	// 1) 创建测试文件（不写入内容，避免某些 FS 写路径未实现导致的干扰）
	if (touch_file(file) == 0) {
		pass("create test file");
	} else {
		fail("create test file");
		goto report;
	}

	// 2) 路径型 set/get 测试
	test_path_get_set(file);

	// 3) 符号链接 l*/get* 语义
	test_symlink_get_set(file, symlink_path);

	// 4) 文件描述符 f* 测试
	test_fd_get_set(file);

	// 清理
	unlink(symlink_path);
	unlink(file);

	// 卸载 ext4
	if (umount_ext4(target) == 0) {
		pass("umount ext4");
	} else {
		fail("umount ext4");
	}

report:
	printf("\nSummary: PASS=%d, FAIL=%d, SKIP=%d\n", g_pass, g_fail, g_skip);
	// 返回非零表示存在失败
	return g_fail == 0 ? 0 : 1;
}

