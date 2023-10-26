#include "ktest.h"
#include "ktest_utils.h"

static long ktest_kvm_case0_1(uint64_t arg0, uint64_t arg1){
    kTEST("Testing /dev/kvm device...");
    
}

static ktest_case_table kt_kvm_func_table[] = {
    ktest_kvm_case0_1,
};

int ktest_test_kvm(void* arg)
{
    kTEST("Testing kvm...");
    for (int i = 0; i < sizeof(kt_kvm_func_table) / sizeof(ktest_case_table); ++i)
    {
        kTEST("Testing case %d", i);
        kt_kvm_func_table[i](i, 0);
    }
    kTEST("kvm Test done.");
    return 0;
}
