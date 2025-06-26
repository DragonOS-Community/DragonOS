#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/types.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <string.h>
#include <errno.h>

// 测试结果统计
static int test_count = 0;
static int pass_count = 0;

void print_test_result(const char* test_name, int passed) {
    test_count++;
    if (passed) {
        pass_count++;
        printf("[PASS] %s\n", test_name);
    } else {
        printf("[FAIL] %s\n", test_name);
    }
}

void print_final_result() {
    printf("\n=== 测试结果汇总 ===\n");
    printf("总测试数: %d\n", test_count);
    printf("通过数: %d\n", pass_count);
    printf("失败数: %d\n", test_count - pass_count);
    
    if (pass_count == test_count) {
        printf("[ ALL TESTS PASSED ]\n");
    } else {
        printf("[ SOME TESTS FAILED ]\n");
    }
}

// 创建测试文件
int create_test_file(const char* filename, const char* content) {
    FILE *fp = fopen(filename, "w");
    if (!fp) {
        return 0;
    }
    
    if (content) {
        fputs(content, fp);
    }
    fclose(fp);
    return 1;
}

// 获取文件大小
off_t get_file_size(const char* filename) {
    struct stat st;
    if (stat(filename, &st) == -1) {
        return -1;
    }
    return st.st_size;
}

// 测试1: 基本truncate功能 - 截断文件
void test_truncate_shrink() {
    const char* filename = "/tmp/test_truncate_shrink.txt";
    const char* content = "Hello, World! This is a test file for truncate.";
    off_t original_size, new_size;
    
    // 创建测试文件
    if (!create_test_file(filename, content)) {
        print_test_result("基本truncate功能(截断) - 创建文件失败", 0);
        return;
    }
    
    original_size = get_file_size(filename);
    if (original_size == -1) {
        print_test_result("基本truncate功能(截断) - 获取原始大小", 0);
        unlink(filename);
        return;
    }
    
    // 截断到10字节
    if (truncate(filename, 10) == -1) {
        printf("truncate() 失败: %s\n", strerror(errno));
        print_test_result("基本truncate功能(截断) - truncate调用", 0);
        unlink(filename);
        return;
    }
    
    new_size = get_file_size(filename);
    int passed = (new_size == 10);
    
    if (!passed) {
        printf("error: 期望大小: 10, 实际大小: %ld\n", (long)new_size);
    }
    
    print_test_result("基本truncate功能(截断)", passed);
    unlink(filename);
}

// 测试2: 扩展文件
void test_truncate_extend() {
    const char* filename = "/tmp/test_truncate_extend.txt";
    const char* content = "Short";
    off_t original_size, new_size;
    
    if (!create_test_file(filename, content)) {
        print_test_result("扩展文件功能 - 创建文件", 0);
        return;
    }
    
    original_size = get_file_size(filename);
    
    // 扩展到100字节
    if (truncate(filename, 100) == -1) {
        printf("truncate() 扩展失败: %s\n", strerror(errno));
        print_test_result("扩展文件功能 - truncate调用", 0);
        unlink(filename);
        return;
    }
    
    new_size = get_file_size(filename);
    int passed = (new_size == 100);
    
    if (!passed) {
        printf("期望大小: 100, 实际大小: %ld\n", (long)new_size);
    }
    
    print_test_result("扩展文件功能", passed);
    unlink(filename);
}

// 测试3: 截断到0字节
void test_truncate_to_zero() {
    const char* filename = "/tmp/test_truncate_zero.txt";
    const char* content = "This will be emptied";
    off_t new_size;
    
    if (!create_test_file(filename, content)) {
        print_test_result("截断到0字节 - 创建文件", 0);
        return;
    }
    
    if (truncate(filename, 0) == -1) {
        printf("truncate() 到0失败: %s\n", strerror(errno));
        print_test_result("截断到0字节 - truncate调用", 0);
        unlink(filename);
        return;
    }
    
    new_size = get_file_size(filename);
    int passed = (new_size == 0);
    
    if (!passed) {
        printf("期望大小: 0, 实际大小: %ld\n", (long)new_size);
    }
    
    print_test_result("截断到0字节", passed);
    unlink(filename);
}

// 测试4: 对不存在的文件调用truncate (应该失败)
void test_truncate_nonexistent() {
    const char* filename = "/tmp/nonexistent_file.txt";
    
    // 确保文件不存在
    unlink(filename);
    
    int result = truncate(filename, 10);
    int passed = (result == -1 && errno == ENOENT);
    
    if (!passed) {
        printf("期望: truncate失败(ENOENT), 实际: result=%d, errno=%d\n", result, errno);
    }
    
    print_test_result("对不存在文件调用truncate", passed);
}

// 测试5: 对目录调用truncate (应该失败)
void test_truncate_directory() {
    const char* dirname = "/tmp";
    
    int result = truncate(dirname, 10);
    int passed = (result == -1 && errno == EISDIR);
    
    if (!passed) {
        printf("期望: truncate失败(EISDIR), 实际: result=%d, errno=%d\n", result, errno);
    }
    
    print_test_result("对目录调用truncate", passed);
}

// 测试6: 无效参数测试
void test_truncate_invalid_args() {
    const char* filename = "/tmp/test_truncate_invalid.txt";
    
    if (!create_test_file(filename, "test")) {
        print_test_result("无效参数测试 - 创建文件", 0);
        return;
    }
    
    // 测试负数长度 (在某些系统上可能被转换为很大的正数)
    int result = truncate(filename, -1);
    int passed = (result == -1);  // 应该失败
    
    if (!passed) {
        printf("期望: truncate失败, 实际: result=%d\n", result);
    }
    
    print_test_result("无效参数测试(负数长度)", passed);
    unlink(filename);
}

int main(int argc, char *argv[]) {
    printf("=== DragonOS truncate系统调用测试 ===\n\n");
    
    // 如果提供了参数，执行用户指定的测试
    if (argc == 3) {
        const char *filename = argv[1];
        off_t new_size = atoi(argv[2]);
        off_t original_size = get_file_size(filename);
        
        printf("手动测试: %s -> %ld字节\n", filename, (long)new_size);
        
        if (original_size != -1) {
            printf("原始文件大小: %ld字节\n", (long)original_size);
        }
        
        if (truncate(filename, new_size) == -1) {
            printf("[FAIL] truncate()失败: %s\n", strerror(errno));
            return EXIT_FAILURE;
        }
        
        off_t final_size = get_file_size(filename);
        if (final_size == new_size) {
            printf("[PASS] 文件大小成功更改为%ld字节\n", (long)final_size);
            return EXIT_SUCCESS;
        } else {
            printf("[FAIL] 期望大小%ld，实际大小%ld\n", (long)new_size, (long)final_size);
            return EXIT_FAILURE;
        }
    }
    
    // 自动化测试套件
    test_truncate_shrink();
    test_truncate_extend();
    test_truncate_to_zero();
    test_truncate_nonexistent();
    test_truncate_directory();
    test_truncate_invalid_args();
    
    printf("\n");
    print_final_result();
    
    return (pass_count == test_count) ? EXIT_SUCCESS : EXIT_FAILURE;
}
