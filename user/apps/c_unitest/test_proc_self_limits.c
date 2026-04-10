/**
 * @file test_proc_self_limits.c
 * @brief 验证 setrlimit(RLIMIT_NOFILE) 后 /proc/self/limits 是否同步更新
 */

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <unistd.h>

#define LIMITS_PATH "/proc/self/limits"
#define LINE_BUF_SZ 512

static void rstrip(char *s)
{
    size_t n = strlen(s);
    while (n > 0 && (s[n - 1] == ' ' || s[n - 1] == '\t' || s[n - 1] == '\n' || s[n - 1] == '\r')) {
        s[--n] = '\0';
    }
}

static void lstrip(char *s)
{
    size_t i = 0;
    while (s[i] == ' ' || s[i] == '\t') {
        i++;
    }
    if (i > 0) {
        memmove(s, s + i, strlen(s + i) + 1);
    }
}

static void trim(char *s)
{
    rstrip(s);
    lstrip(s);
}

static void format_limit_value(rlim_t v, char *out, size_t out_sz)
{
    if (v == RLIM_INFINITY || v == (rlim_t)-1) {
        snprintf(out, out_sz, "unlimited");
        return;
    }
    snprintf(out, out_sz, "%llu", (unsigned long long)v);
}

static int read_nofile_from_proc(char *soft, size_t soft_sz, char *hard, size_t hard_sz)
{
    FILE *fp = fopen(LIMITS_PATH, "r");
    if (!fp) {
        perror("fopen(/proc/self/limits)");
        return -1;
    }

    char line[LINE_BUF_SZ];
    int found = 0;

    while (fgets(line, sizeof(line), fp) != NULL) {
        if (strncmp(line, "Max open files", 14) != 0) {
            continue;
        }

        size_t len = strlen(line);
        if (len < 67) {
            fprintf(stderr, "Malformed limits line: %s\n", line);
            fclose(fp);
            return -1;
        }

        char soft_field[32] = {0};
        char hard_field[32] = {0};

        memcpy(soft_field, line + 26, 20);
        memcpy(hard_field, line + 47, 20);

        trim(soft_field);
        trim(hard_field);

        snprintf(soft, soft_sz, "%s", soft_field);
        snprintf(hard, hard_sz, "%s", hard_field);
        found = 1;
        break;
    }

    fclose(fp);

    if (!found) {
        fprintf(stderr, "Max open files row not found in %s\n", LIMITS_PATH);
        return -1;
    }

    return 0;
}

int main(void)
{
    struct rlimit old_lim;
    if (getrlimit(RLIMIT_NOFILE, &old_lim) != 0) {
        perror("getrlimit(RLIMIT_NOFILE)");
        return 1;
    }

    printf("[INFO] old nofile soft=%llu hard=%llu\n",
           (unsigned long long)old_lim.rlim_cur,
           (unsigned long long)old_lim.rlim_max);

    struct rlimit new_lim = old_lim;
    if (old_lim.rlim_cur == 0 || old_lim.rlim_cur == RLIM_INFINITY) {
        rlim_t candidate = 1024;
        if (old_lim.rlim_max != RLIM_INFINITY && candidate > old_lim.rlim_max) {
            candidate = old_lim.rlim_max;
        }
        new_lim.rlim_cur = candidate;
    } else {
        new_lim.rlim_cur = old_lim.rlim_cur - 1;
    }

    if (setrlimit(RLIMIT_NOFILE, &new_lim) != 0) {
        perror("setrlimit(RLIMIT_NOFILE)");
        return 1;
    }

    printf("[INFO] set nofile soft=%llu hard=%llu\n",
           (unsigned long long)new_lim.rlim_cur,
           (unsigned long long)new_lim.rlim_max);

    char proc_soft[64] = {0};
    char proc_hard[64] = {0};
    if (read_nofile_from_proc(proc_soft, sizeof(proc_soft), proc_hard, sizeof(proc_hard)) != 0) {
        return 1;
    }

    char expect_soft[64] = {0};
    char expect_hard[64] = {0};
    format_limit_value(new_lim.rlim_cur, expect_soft, sizeof(expect_soft));
    format_limit_value(new_lim.rlim_max, expect_hard, sizeof(expect_hard));

    printf("[INFO] /proc/self/limits nofile soft=%s hard=%s\n", proc_soft, proc_hard);
    printf("[INFO] expected soft=%s hard=%s\n", expect_soft, expect_hard);

    int ok = (strcmp(proc_soft, expect_soft) == 0) && (strcmp(proc_hard, expect_hard) == 0);

    if (setrlimit(RLIMIT_NOFILE, &old_lim) != 0) {
        perror("restore setrlimit(RLIMIT_NOFILE)");
    }

    if (!ok) {
        fprintf(stderr, "[FAIL] /proc/self/limits does not reflect setrlimit(RLIMIT_NOFILE)\n");
        return 1;
    }

    printf("[PASS] /proc/self/limits reflects RLIMIT_NOFILE changes\n");
    return 0;
}
