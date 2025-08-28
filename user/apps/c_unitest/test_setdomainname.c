#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/utsname.h>
#include <errno.h>

// Since setdomainname might not be in libc, we define it manually
#ifdef __x86_64__
#define __NR_setdomainname 171
#elif defined(__riscv)
#define __NR_setdomainname 171
#elif defined(__aarch64__)
#define __NR_setdomainname 171
#else
#define __NR_setdomainname 171
#endif

static inline int sys_setdomainname(const char *name, size_t len)
{
    return syscall(__NR_setdomainname, name, len);
}

int main() {
    struct utsname uts;
    char test_domain1[] = "test.domain.com";
    char test_domain2[] = "dragonos.test";
    char long_domain[256];
    
    printf("=== Testing setdomainname syscall ===\n\n");
    
    // Test 1: Get initial domainname
    printf("Test 1: Get initial domainname\n");
    if (uname(&uts) == 0) {
        printf("Initial domainname: '%s'\n", uts.__domainname);
    } else {
        perror("uname failed");
        return 1;
    }
    printf("\n");
    
    // Test 2: Set a normal domainname
    printf("Test 2: Set normal domainname\n");
    printf("Setting domainname to: '%s'\n", test_domain1);
    if (sys_setdomainname(test_domain1, strlen(test_domain1)) == 0) {
        printf("✓ setdomainname succeeded\n");
        
        // Verify
        if (uname(&uts) == 0) {
            printf("New domainname: '%s'\n", uts.__domainname);
            if (strcmp(uts.__domainname, test_domain1) == 0) {
                printf("✓ Domainname matches!\n");
            } else {
                printf("✗ Domainname doesn't match!\n");
            }
        }
    } else {
        perror("✗ setdomainname failed");
    }
    printf("\n");
    
    // Test 3: Set another domainname
    printf("Test 3: Set different domainname\n");
    printf("Setting domainname to: '%s'\n", test_domain2);
    if (sys_setdomainname(test_domain2, strlen(test_domain2)) == 0) {
        printf("✓ setdomainname succeeded\n");
        
        // Verify
        if (uname(&uts) == 0) {
            printf("New domainname: '%s'\n", uts.__domainname);
            if (strcmp(uts.__domainname, test_domain2) == 0) {
                printf("✓ Domainname matches!\n");
            } else {
                printf("✗ Domainname doesn't match!\n");
            }
        }
    } else {
        perror("✗ setdomainname failed");
    }
    printf("\n");
    
    // Test 4: Test with zero length
    printf("Test 4: Test with zero length\n");
    if (sys_setdomainname("test", 0) == -1 && errno == EINVAL) {
        printf("✓ Correctly returned EINVAL for zero length\n");
    } else {
        printf("✗ Should have failed with EINVAL for zero length\n");
    }
    printf("\n");
    
    // Test 5: Test with NULL pointer
    printf("Test 5: Test with NULL pointer\n");
    if (sys_setdomainname(NULL, 10) == -1 && errno == EFAULT) {
        printf("✓ Correctly returned EFAULT for NULL pointer\n");
    } else {
        printf("✗ Should have failed with EFAULT for NULL pointer\n");
    }
    printf("\n");
    
    // Test 6: Test with long domainname (should fail)
    printf("Test 6: Test with long domainname\n");
    memset(long_domain, 'a', 256);
    long_domain[255] = '\0';
    if (sys_setdomainname(long_domain, 256) == -1 && errno == EINVAL) {
        printf("✓ Correctly returned EINVAL for long domainname\n");
    } else {
        printf("✗ Should have failed with EINVAL for long domainname\n");
    }
    printf("\n");
    
    // Restore original domainname if possible
    printf("Cleanup: Restoring original domainname\n");
    if (sys_setdomainname("(none)", strlen("(none)")) == 0) {
        printf("✓ Restored default domainname\n");
    } else {
        perror("✗ Failed to restore domainname");
    }
    
    printf("\n=== Test completed ===\n");
    return 0;
}