/**
 * @file test_shebang.c
 * @brief Test shebang (#!) script execution
 *
 * This program tests the kernel's shebang parsing functionality by:
 * 1. Creating a shell script with shebang
 * 2. Executing it via execve
 * 3. Verifying the script runs correctly
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <errno.h>

#define TEST_SCRIPT_PATH "/tmp/test_shebang.sh"
#define TEST_SCRIPT_WITH_ARG_PATH "/tmp/test_shebang_arg.sh"
#define TEST_OUTPUT_PATH "/tmp/test_shebang_output.txt"

// Simple shell script content
static const char *simple_script =
    "#!/bin/sh\n"
    "echo \"Shebang test: Hello from shell script!\"\n"
    "echo \"argc=$#\"\n"
    "for arg in \"$@\"; do\n"
    "    echo \"arg: $arg\"\n"
    "done\n"
    "exit 0\n";

// Script with interpreter argument (#!/usr/local/bin/env sh)
// Note: On DragonOS, env is at /usr/local/bin/env, not /usr/bin/env
static const char *env_script =
    "#!/usr/local/bin/env sh\n"
    "echo \"Shebang with env: Hello!\"\n"
    "echo \"Script path: $0\"\n"
    "exit 0\n";

static int create_script(const char *path, const char *content) {
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0755);
    if (fd < 0) {
        perror("Failed to create script");
        return -1;
    }

    size_t len = strlen(content);
    ssize_t written = write(fd, content, len);
    close(fd);

    if (written != (ssize_t)len) {
        perror("Failed to write script content");
        return -1;
    }

    return 0;
}

static int test_simple_shebang(void) {
    printf("\n=== Test 1: Simple shebang (#!/bin/sh) ===\n");

    if (create_script(TEST_SCRIPT_PATH, simple_script) < 0) {
        return -1;
    }

    pid_t pid = fork();
    if (pid < 0) {
        perror("fork failed");
        return -1;
    }

    if (pid == 0) {
        // Child process: execute the script
        char *argv[] = {TEST_SCRIPT_PATH, "arg1", "arg2", "arg3", NULL};
        char *envp[] = {NULL};

        printf("[Child] Executing script: %s\n", TEST_SCRIPT_PATH);
        execve(TEST_SCRIPT_PATH, argv, envp);

        // If execve returns, it failed
        perror("execve failed");
        exit(1);
    }

    // Parent process: wait for child
    int status;
    waitpid(pid, &status, 0);

    if (WIFEXITED(status)) {
        int exit_code = WEXITSTATUS(status);
        printf("[Parent] Child exited with code: %d\n", exit_code);
        if (exit_code == 0) {
            printf("Test 1 PASSED\n");
            return 0;
        }
    } else if (WIFSIGNALED(status)) {
        printf("[Parent] Child killed by signal: %d\n", WTERMSIG(status));
    }

    printf("Test 1 FAILED\n");
    return -1;
}

static int test_env_shebang(void) {
    printf("\n=== Test 2: Shebang with env (#!/usr/local/bin/env sh) ===\n");

    if (create_script(TEST_SCRIPT_WITH_ARG_PATH, env_script) < 0) {
        return -1;
    }

    pid_t pid = fork();
    if (pid < 0) {
        perror("fork failed");
        return -1;
    }

    if (pid == 0) {
        // Child process: execute the script
        char *argv[] = {TEST_SCRIPT_WITH_ARG_PATH, NULL};
        char *envp[] = {"PATH=/bin:/usr/bin", NULL};

        printf("[Child] Executing script: %s\n", TEST_SCRIPT_WITH_ARG_PATH);
        execve(TEST_SCRIPT_WITH_ARG_PATH, argv, envp);

        // If execve returns, it failed
        perror("execve failed");
        exit(1);
    }

    // Parent process: wait for child
    int status;
    waitpid(pid, &status, 0);

    if (WIFEXITED(status)) {
        int exit_code = WEXITSTATUS(status);
        printf("[Parent] Child exited with code: %d\n", exit_code);
        if (exit_code == 0) {
            printf("Test 2 PASSED\n");
            return 0;
        }
    } else if (WIFSIGNALED(status)) {
        printf("[Parent] Child killed by signal: %d\n", WTERMSIG(status));
    }

    printf("Test 2 FAILED\n");
    return -1;
}

static int test_nonexistent_interpreter(void) {
    printf("\n=== Test 3: Non-existent interpreter ===\n");

    const char *bad_script =
        "#!/nonexistent/interpreter\n"
        "echo \"This should not run\"\n";

    const char *bad_script_path = "/tmp/test_bad_shebang.sh";

    if (create_script(bad_script_path, bad_script) < 0) {
        return -1;
    }

    pid_t pid = fork();
    if (pid < 0) {
        perror("fork failed");
        return -1;
    }

    if (pid == 0) {
        char *argv[] = {(char *)bad_script_path, NULL};
        char *envp[] = {NULL};

        execve(bad_script_path, argv, envp);

        // execve should fail with ENOENT
        int err = errno;
        printf("[Child] execve failed as expected, errno=%d (%s)\n", err, strerror(err));
        exit(err == ENOENT ? 0 : 1);
    }

    int status;
    waitpid(pid, &status, 0);

    if (WIFEXITED(status) && WEXITSTATUS(status) == 0) {
        printf("Test 3 PASSED (correctly rejected non-existent interpreter)\n");
        return 0;
    }

    printf("Test 3 FAILED\n");
    return -1;
}

static int test_direct_binary(void) {
    printf("\n=== Test 4: Direct binary execution (no shebang) ===\n");

    pid_t pid = fork();
    if (pid < 0) {
        perror("fork failed");
        return -1;
    }

    if (pid == 0) {
        // Use /bin/sh -c since /bin/echo may not exist on DragonOS
        char *argv[] = {"/bin/sh", "-c", "echo 'Direct binary execution works!'", NULL};
        char *envp[] = {NULL};

        execve("/bin/sh", argv, envp);
        perror("execve failed");
        exit(1);
    }

    int status;
    waitpid(pid, &status, 0);

    if (WIFEXITED(status) && WEXITSTATUS(status) == 0) {
        printf("Test 4 PASSED\n");
        return 0;
    }

    printf("Test 4 FAILED\n");
    return -1;
}

static void cleanup(void) {
    unlink(TEST_SCRIPT_PATH);
    unlink(TEST_SCRIPT_WITH_ARG_PATH);
    unlink("/tmp/test_bad_shebang.sh");
    unlink(TEST_OUTPUT_PATH);
}

int main(int argc, char *argv[]) {
    printf("========================================\n");
    printf("   Shebang (#!) Execution Test Suite   \n");
    printf("========================================\n");

    int passed = 0;
    int failed = 0;

    // Test 1: Simple shebang
    if (test_simple_shebang() == 0) {
        passed++;
    } else {
        failed++;
    }

    // Test 2: Shebang with interpreter argument (env)
    if (test_env_shebang() == 0) {
        passed++;
    } else {
        failed++;
    }

    // Test 3: Non-existent interpreter
    if (test_nonexistent_interpreter() == 0) {
        passed++;
    } else {
        failed++;
    }

    // Test 4: Direct binary (sanity check)
    if (test_direct_binary() == 0) {
        passed++;
    } else {
        failed++;
    }

    // Cleanup
    cleanup();

    printf("\n========================================\n");
    printf("   Test Results: %d passed, %d failed   \n", passed, failed);
    printf("========================================\n");

    return failed > 0 ? 1 : 0;
}
