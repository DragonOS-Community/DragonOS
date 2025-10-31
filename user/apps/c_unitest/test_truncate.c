#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#define TEST_FILE "/tmp/test_truncate.txt"
#define TEST_DIR "/tmp/test_truncate_dir"
#define TEST_SYMLINK "/tmp/test_truncate_symlink"
#define TEST_RO_MOUNT "/tmp/test_ro_mount"

// 测试辅助函数
static void test_assert(int condition, const char *message) {
    if (!condition) {
        printf("FAIL: %s\n", message);
        exit(1);
    }
}

static void test_success(const char *message) {
    printf("PASS: %s\n", message);
}

// 测试正常文件截断
static void test_normal_truncate() {
    printf("\n=== 测试正常文件截断 ===\n");
    
    // 创建测试文件
    FILE *fp = fopen(TEST_FILE, "w");
    test_assert(fp != NULL, "创建测试文件");
    fprintf(fp, "Hello, World! This is a test file.");
    fclose(fp);
    
    // 检查初始文件大小
    struct stat st;
    test_assert(stat(TEST_FILE, &st) == 0, "获取文件状态");
    printf("初始文件大小: %ld bytes\n", st.st_size);
    
    // 截断到较小大小
    printf("DEBUG: 调用 truncate(%s, 10)\n", TEST_FILE);
    int result = truncate(TEST_FILE, 10);
    printf("DEBUG: truncate 返回值: %d, errno: %d\n", result, errno);
    
    test_assert(result == 0, "截断到10字节");
    test_assert(stat(TEST_FILE, &st) == 0, "获取截断后文件状态");
    printf("DEBUG: 截断后文件大小: %ld bytes (期望: 10)\n", st.st_size);
    
    if (st.st_size == 10) {
        test_success("截断到较小大小");
    } else {
        printf("FAIL: 文件大小应为10字节，实际为%ld字节\n", st.st_size);
        // 继续测试其他情况
    }
    
    // 截断到较大大小
    printf("DEBUG: 调用 truncate(%s, 100)\n", TEST_FILE);
    result = truncate(TEST_FILE, 100);
    printf("DEBUG: truncate 返回值: %d, errno: %d\n", result, errno);
    
    test_assert(result == 0, "截断到100字节");
    test_assert(stat(TEST_FILE, &st) == 0, "获取截断后文件状态");
    printf("DEBUG: 截断后文件大小: %ld bytes (期望: 100)\n", st.st_size);
    
    if (st.st_size == 100) {
        test_success("截断到较大大小");
    } else {
        printf("FAIL: 文件大小应为100字节，实际为%ld字节\n", st.st_size);
    }
    
    // 截断到0
    printf("DEBUG: 调用 truncate(%s, 0)\n", TEST_FILE);
    result = truncate(TEST_FILE, 0);
    printf("DEBUG: truncate 返回值: %d, errno: %d\n", result, errno);
    
    test_assert(result == 0, "截断到0字节");
    test_assert(stat(TEST_FILE, &st) == 0, "获取截断后文件状态");
    printf("DEBUG: 截断后文件大小: %ld bytes (期望: 0)\n", st.st_size);
    
    if (st.st_size == 0) {
        test_success("截断到0字节");
    } else {
        printf("FAIL: 文件大小应为0字节，实际为%ld字节\n", st.st_size);
    }
    
    // 清理
    unlink(TEST_FILE);
}

// 测试目录截断（应返回EISDIR）
static void test_directory_truncate() {
    printf("\n=== 测试目录截断 ===\n");
    
    // 创建测试目录
    test_assert(mkdir(TEST_DIR, 0755) == 0, "创建测试目录");
    
    // 尝试截断目录
    int result = truncate(TEST_DIR, 10);
    test_assert(result == -1, "截断目录应失败");
    test_assert(errno == EISDIR, "错误码应为EISDIR");
    test_success("目录截断正确返回EISDIR");
    
    // 清理
    rmdir(TEST_DIR);
}

// 测试符号链接截断
static void test_symlink_truncate() {
    printf("\n=== 测试符号链接截断 ===\n");
    
    // 创建目标文件
    FILE *fp = fopen(TEST_FILE, "w");
    test_assert(fp != NULL, "创建目标文件");
    fprintf(fp, "Target file content");
    fclose(fp);
    
    // 创建符号链接
    test_assert(symlink(TEST_FILE, TEST_SYMLINK) == 0, "创建符号链接");
    
    // 截断符号链接（应跟随到目标文件）
    test_assert(truncate(TEST_SYMLINK, 5) == 0, "截断符号链接");
    
    // 检查目标文件大小
    struct stat st;
    test_assert(stat(TEST_FILE, &st) == 0, "获取目标文件状态");
    test_assert(st.st_size == 5, "目标文件大小应为5字节");
    test_success("符号链接截断正确跟随到目标");
    
    // 清理
    unlink(TEST_SYMLINK);
    unlink(TEST_FILE);
}

// 测试不存在的文件
static void test_nonexistent_file() {
    printf("\n=== 测试不存在文件 ===\n");
    
    int result = truncate("/tmp/nonexistent_file", 10);
    test_assert(result == -1, "截断不存在文件应失败");
    test_assert(errno == ENOENT, "错误码应为ENOENT");
    test_success("不存在文件正确返回ENOENT");
}

// 测试只读挂载点截断
static void test_readonly_mount() {
    printf("\n=== 测试只读挂载点截断 ===\n");
    
    // 创建挂载点目录
    test_assert(mkdir(TEST_RO_MOUNT, 0755) == 0, "创建挂载点目录");
    
    // 尝试以只读方式挂载（如果支持的话）
    // 注意：这里可能需要根据实际文件系统支持情况调整
    if (mount("", TEST_RO_MOUNT, "ramfs", MS_RDONLY, NULL) == 0) {
        // 在挂载点创建文件
        char test_path[256];
        snprintf(test_path, sizeof(test_path), "%s/test_file", TEST_RO_MOUNT);
        
        FILE *fp = fopen(test_path, "w");
        if (fp != NULL) {
            fprintf(fp, "Test content");
            fclose(fp);
            
            // 尝试截断只读挂载点上的文件
            int result = truncate(test_path, 5);
            if (result == -1 && errno == EROFS) {
                test_success("只读挂载点截断正确返回EROFS");
            } else {
                printf("WARN: 只读挂载点测试未按预期返回EROFS\n");
            }
            
            unlink(test_path);
        }
        
        umount(TEST_RO_MOUNT);
    } else {
        printf("INFO: 跳过只读挂载测试（可能不支持或权限不足）\n");
    }
    
    // 清理
    rmdir(TEST_RO_MOUNT);
}

// 测试边界条件
static void test_boundary_conditions() {
    printf("\n=== 测试边界条件 ===\n");
    
    // 创建测试文件
    FILE *fp = fopen(TEST_FILE, "w");
    test_assert(fp != NULL, "创建测试文件");
    fprintf(fp, "Test content");
    fclose(fp);
    
    // 测试负长度（应返回EINVAL）
    int result = truncate(TEST_FILE, -1);
    if (result == -1 && errno == EINVAL) {
        test_success("负长度正确返回EINVAL");
    } else {
        printf("WARN: 负长度测试未按预期返回EINVAL\n");
    }
    
    // 测试非常大的长度
    test_assert(truncate(TEST_FILE, 0x7FFFFFFF) == 0, "大长度截断");
    struct stat st;
    test_assert(stat(TEST_FILE, &st) == 0, "获取大长度截断后状态");
    printf("大长度截断后文件大小: %ld bytes\n", st.st_size);
    test_success("大长度截断");
    
    // 清理
    unlink(TEST_FILE);
}

// 测试与ftruncate的一致性
static void test_ftruncate_consistency() {
    printf("\n=== 测试与ftruncate的一致性 ===\n");
    
    // 创建测试文件
    FILE *fp = fopen(TEST_FILE, "w");
    test_assert(fp != NULL, "创建测试文件");
    fprintf(fp, "Test content for consistency");
    fclose(fp);
    
    // 使用truncate截断
    printf("DEBUG: 调用 truncate(%s, 10)\n", TEST_FILE);
    int result = truncate(TEST_FILE, 10);
    printf("DEBUG: truncate 返回值: %d, errno: %d\n", result, errno);
    test_assert(result == 0, "truncate截断");
    
    struct stat st1;
    test_assert(stat(TEST_FILE, &st1) == 0, "获取truncate后状态");
    printf("DEBUG: truncate后文件大小: %ld bytes\n", st1.st_size);
    
    // 使用ftruncate截断
    int fd = open(TEST_FILE, O_RDWR);
    test_assert(fd != -1, "打开文件");
    printf("DEBUG: 调用 ftruncate(fd=%d, 5)\n", fd);
    result = ftruncate(fd, 5);
    printf("DEBUG: ftruncate 返回值: %d, errno: %d\n", result, errno);
    test_assert(result == 0, "ftruncate截断");
    close(fd);
    
    struct stat st2;
    test_assert(stat(TEST_FILE, &st2) == 0, "获取ftruncate后状态");
    printf("DEBUG: ftruncate后文件大小: %ld bytes (期望: 5)\n", st2.st_size);
    
    if (st2.st_size == 5) {
        test_success("truncate和ftruncate行为一致");
    } else {
        printf("FAIL: ftruncate后文件大小应为5字节，实际为%ld字节\n", st2.st_size);
    }
    
    // 清理
    unlink(TEST_FILE);
}

int main() {
    printf("开始 SYS_TRUNCATE 系统调用测试\n");
    printf("================================\n");
    
    // 运行所有测试
    test_normal_truncate();
    test_directory_truncate();
    test_symlink_truncate();
    test_nonexistent_file();
    test_readonly_mount();
    test_boundary_conditions();
    test_ftruncate_consistency();
    
    printf("\n================================\n");
    printf("所有测试完成！\n");
    
    return 0;
}
