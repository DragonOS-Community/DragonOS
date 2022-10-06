

#include "ktest.h"
#include "ktest_utils.h"
#include <common/idr.h>

/**
 * @brief 测试idr的构建,预获取空间是否成功
 *
 * 以下函数将被测试:
 * 1. idr_pre_get
 * 2. DECLARE_IDR
 * 3. idr_init
 * 4. idr_destroy
 *
 * 同时还会(间接)测试一些内部函数:
 * 1. move_to_free_list
 *
 * @param arg0
 * @param arg1
 */
static long ktest_idr_case0(uint64_t arg0, uint64_t arg1)
{
    unsigned long bitmap = -1;
    assert((int)(bitmap == IDR_FULL));

    DECLARE_IDR(k_idr);
    assert(k_idr.top == NULL);      // 刚被创建,必须是NULL
    assert(k_idr.id_free_cnt == 0); // 必须是0
    assert(k_idr.free_list == NULL);

    k_idr.id_free_cnt = arg1;
    idr_init(&k_idr);
    assert(k_idr.id_free_cnt == 0);

    assert(idr_pre_get(&k_idr, 0) == 1);
    assert(k_idr.id_free_cnt == IDR_FREE_MAX);

    for (int i = 1; i < 64; i++)
    {
        int id = lowbit_id(i), chk_id = -1;
        for (int j = 0; j < 64; j++)
            if ((i >> j) & 1)
            {
                chk_id = j;
                break;
            }
        assert(id == chk_id);
    }

    // 销毁
    idr_destroy(&k_idr);
    assert(k_idr.id_free_cnt == 0);
    assert(k_idr.free_list == NULL);
    assert(k_idr.top == NULL);

    return 0;
}

/**
 * @brief 测试id的获取，id的删除，id的全体删除, idr的find函数
 *
 * @param arg0
 * @param arg1
 */
static long ktest_idr_case1(uint64_t arg0, uint64_t arg1)
{
    DECLARE_IDR(k_idr);
    int a[128];

    // 获取128个id
    for (int i = 0; i < 128; i++)
    {
        assert(idr_get_new(&k_idr, &a[i], &a[i]) == 0);
        assert(a[i] == i);
    }

    // 查询128个ptr
    for (int i = 0; i < 128; i++)
    {
        int *ptr = idr_find(&k_idr, a[i]);
        assert(ptr == &a[i]);
        assert(ptr != NULL);
        assert(*ptr == a[i]);
    }

    // 倒序：删除64个id
    for (int i = 127; i >= 64; i--)
    {
        idr_remove(&k_idr, a[i]);
        assert(idr_find(&k_idr, a[i]) == NULL);
    }

    // 正序：删除64个id
    for (int i = 0; i <= 63; i++)
    {
        idr_remove(&k_idr, a[i]);
        assert(idr_find(&k_idr, a[i]) == NULL);
    }

    // 重新申请128个id, 值域范围应该仍然是[0,127]
    for (int i = 0; i < 128; i++)
    {
        assert(idr_get_new(&k_idr, &a[i], &a[i]) == 0);
        assert(a[i] == i);
    }

    // 正序：删除32个id
    for (int i = 0; i <= 31; i++)
    {
        idr_remove(&k_idr, a[i]);
        assert(idr_find(&k_idr, a[i]) == NULL);
    }

    // 倒序：删除32个id
    for (int i = 127; i >= 96; i--)
    {
        idr_remove(&k_idr, a[i]);
        assert(idr_find(&k_idr, a[i]) == NULL);
    }

    // 整体删除
    idr_remove_all(&k_idr);
    assert(k_idr.top == NULL);

    // 获取128个id
    for (int i = 0; i < 128; i++)
    {
        assert(idr_get_new(&k_idr, &a[i], &a[i]) == 0);
        assert(a[i] == i);
    }

    // 查询128个ptr
    for (int i = 0; i < 128; i++)
    {
        int *ptr = idr_find(&k_idr, a[i]);
        assert(ptr == &a[i]);
        assert(*ptr == a[i]);
    }

    // 正序：删除64个id
    for (int i = 0; i <= 63; i++)
    {
        idr_remove(&k_idr, a[i]);
        assert(idr_find(&k_idr, a[i]) == NULL);
    }

    // 倒序：删除64个id
    for (int i = 127; i >= 64; i--)
    {
        idr_remove(&k_idr, a[i]);
        assert(idr_find(&k_idr, a[i]) == NULL);
    }

    // 销毁
    idr_destroy(&k_idr);
    assert(k_idr.id_free_cnt == 0);
    assert(k_idr.free_list == NULL);

    return 0;
}

/**
 * @brief case1 的大数据测试
 *
 * @param arg0
 * @param arg1
 */
static long ktest_idr_case2(uint64_t arg0, uint64_t arg1)
{
    DECLARE_IDR(k_idr);

    // 获取 1000‘000 个ID
    const int N = 1e7;
    const int M = 3e6;

    int tmp;
    for (int i = 0; i < N; i++)
    {
        assert(idr_get_new(&k_idr, &tmp, &tmp) == 0);
        assert(tmp == i);

        int *ptr = idr_find(&k_idr, i);
        assert(ptr != NULL);
        assert(*ptr == i);
    }

    // 正向: M 个ID
    for (int i = 0; i < M; i++)
    {
        int *ptr = idr_find(&k_idr, i);
        assert(ptr != NULL);
        assert(*ptr == N - 1);
        idr_remove(&k_idr, i);
        assert(idr_find(&k_idr, i) == NULL);
    }

    // 倒序: N-M 个ID
    for (int i = (N)-1; i >= M; i--)
    {
        int *ptr = idr_find(&k_idr, i);
        assert(*ptr == N - 1);
        idr_remove(&k_idr, i);
        assert(idr_find(&k_idr, i) == NULL);
    }

    // 重新插入数据
    for (int i = 0; i < N; i++)
    {
        assert(idr_get_new(&k_idr, &tmp, &tmp) == 0);
        assert(tmp == i);
        assert(k_idr.top != NULL);

        int *ptr = idr_find(&k_idr, i);
        assert(ptr != NULL);
        assert(*ptr == i);
    }

    assert(k_idr.top != NULL);

    for (int i = 0; i < M; i++)
    {
        assert(idr_replace(&k_idr, NULL, i) == 0);
    }

    // 销毁
    idr_destroy(&k_idr);
    assert(k_idr.id_free_cnt == 0);
    assert(k_idr.free_list == NULL);

    return 0;
}

/**
 * @brief case1 的大数据测试
 *
 * @param arg0
 * @param arg1
 */
static long ktest_idr_case3(uint64_t arg0, uint64_t arg1)
{
    DECLARE_IDR(k_idr);

    const int N = 1949;
    int tmp;

    // 获取ID
    for (int i = 0; i < N; i++)
    {
        assert(idr_get_new(&k_idr, &tmp, &tmp) == 0);
        assert(tmp == i);

        int *ptr = idr_find(&k_idr, i);
        assert(ptr != NULL);
        assert(*ptr == i);
    }

    // 查询 nextid
    for (int i = 1; i <= N; i++)
    {
        int nextid;
        int *ptr = idr_find_next_getid(&k_idr, i - 1, &nextid);
        if (likely(i < N))
        {
            assert(ptr != NULL);
            assert(*ptr == N - 1);
            assert(nextid == i);
        }
        else
        {
            assert(ptr == NULL);
            assert(nextid == -1);
        }
    }

    int sz = N;
    // 删掉某一段
    for (int i = N / 3, j = 2 * (N / 3), k = 0; i <= j; k++, i++)
    {
        int *ptr = idr_find(&k_idr, i);
        assert(ptr != NULL);
        assert(*ptr == N - 1);
        idr_remove(&k_idr, i);

        assert(idr_find(&k_idr, i) == NULL);
        sz--;
        assert(k_idr.top != NULL);
    }

    // 查询 nextid
    for (int i = 1; i <= N; i++)
    {
        int nextid;
        int *ptr = idr_find_next_getid(&k_idr, i - 1, &nextid);
        if (likely(i < N))
        {
            int target = i < N / 3 ? i : max(i, 2 * (N / 3) + 1);
            assert(ptr != NULL);
            assert(*ptr == N - 1);
            assert(nextid == target);
        }
        else
        {
            assert(ptr == NULL);
            assert(nextid == -1);
        }
    }

    // 销毁
    idr_destroy(&k_idr);
    assert(k_idr.id_free_cnt == 0);
    assert(k_idr.free_list == NULL);

    return 0;
}

/**
 * @brief 更加全面覆盖所有函数 - 小数据测试
 *
 * @param arg0
 * @param arg1
 */
static long ktest_idr_case4(uint64_t arg0, uint64_t arg1)
{
    DECLARE_IDR(k_idr);
    idr_init(&k_idr);

    const int N =91173;
    int tmp;

    for (int i = 1; i <= 20; i++)
    {
        int M = N / i, T = M / 3, O = 2 * T;
        for (int j = 0; j < M; j++)
        {
            assert(idr_get_new(&k_idr, &tmp, &tmp) == 0);
            assert(tmp == j);
        }

        for (int j = O; j >= T; j--)
        {
            int *ptr = idr_find(&k_idr, j);
            assert(ptr != NULL);
            assert(*ptr == M - 1);
            idr_remove(&k_idr, j);
        }

        for (int j = O + 1; j < M; j++)
        {
            int *ptr = idr_find(&k_idr, j);
            assert(ptr != NULL);
            assert(*ptr == M - 1);
            idr_remove(&k_idr, j);
        }

        for (int j = T - 1; j >= 0; j--)
        {
            int *ptr = idr_find(&k_idr, j);
            assert(ptr != NULL);
            assert(*ptr == M - 1);
            idr_remove(&k_idr, j);
        }

        assert(k_idr.top == NULL);
    }

    // 销毁
    idr_destroy(&k_idr);
    assert(k_idr.id_free_cnt == 0);
    assert(k_idr.free_list == NULL);

    return 0;
}

/**
 * @brief 测试id的获取，id的删除，id的全体删除, idr的find函数
 *
 * @param arg0
 * @param arg1
 */
static long ktest_idr_case5(uint64_t arg0, uint64_t arg1)
{
    DECLARE_IDR(k_idr);
    const int N = 128;
    int a[N];

    // 获取128个id
    for (int i = 0; i < N; i++)
    {
        assert(idr_get_new(&k_idr, &a[i], &a[i]) == 0);
        assert(a[i] == i);
    }

    // 把id指向的指针向后移动一个单位
    for (int i = 0; i < N; i++)
    {
        int *ptr;
        int flags = idr_replace_get_old(&k_idr, &a[(i + 1) % N], i, &ptr);
        assert(flags == 0); // 0 是成功
        assert(ptr != NULL);
        assert(*ptr == i);

        // 测试是否替换成功
        ptr = idr_find(&k_idr, i);
        assert(ptr != NULL);
        assert(*ptr == (i + 1) % N);
    }

    // 销毁
    idr_destroy(&k_idr);
    assert(k_idr.id_free_cnt == 0);
    assert(k_idr.free_list == NULL);

    // destroy之后，再获取128个id
    for (int i = 0; i < N; i++)
    {
        assert(idr_get_new(&k_idr, &a[i], &a[i]) == 0);
        assert(a[i] == i);
    }

    // 销毁
    idr_destroy(&k_idr);
    assert(k_idr.id_free_cnt == 0);
    assert(k_idr.free_list == NULL);

    return 0;
}

/**
 * @brief 测试ida的插入/删除
 *
 * @param arg0
 * @param arg1
 * @return long
 */
static long ktest_idr_case6(uint64_t arg0, uint64_t arg1)
{
    assert(IDA_BITMAP_LONGS != 0);
    assert(IDA_BMP_SIZE != 0);
    assert(IDA_FULL != 0);
    assert(IDA_BITMAP_BITS != 0);

    DECLARE_IDA(k_ida);
    ida_init(&k_ida);

    const int N = IDA_FULL * IDR_SIZE + 1;

    for (int i = 0; i < N; i++)
    {
        int p_id;
        assert(ida_get_new(&k_ida, &p_id) == 0);
        assert(p_id == i);
    }

    for (int i = 0; i < N; i++)
    {
        assert(ida_count(&k_ida, i) == 1);
    }

    for (int i = N - 1; i >= 0; i--)
    {
        ida_remove(&k_ida, i);
        assert(ida_count(&k_ida, i) == 0);
    }

    assert(k_ida.idr.top == NULL);

    for (int i = 0; i < N; i++)
    {
        int p_id;
        assert(ida_get_new(&k_ida, &p_id) == 0);
        assert(p_id == i);
    }

    assert(k_ida.idr.top != NULL);
    ida_destroy(&k_ida);
    assert(k_ida.idr.top == NULL);
    assert(k_ida.free_list == NULL);

    // 测试destroy之后能否重新获取ID
    for (int i = 0; i < N; i++)
    {
        int p_id;
        assert(ida_get_new(&k_ida, &p_id) == 0);
        assert(p_id == i);
    }

    for (int i = 0; i < N / 3; i++)
    {
        ida_remove(&k_ida, i);
        assert(ida_count(&k_ida, i) == 0);
    }

    for (int i = 2 * N / 3; i < N; i++)
    {
        ida_remove(&k_ida, i);
        assert(ida_count(&k_ida, i) == 0);
    }

    assert(k_ida.idr.top != NULL);
    ida_destroy(&k_ida);
    assert(k_ida.idr.top == NULL);
    assert(k_ida.free_list == NULL);

    return 0;
}

static ktest_case_table kt_idr_func_table[] = {
    ktest_idr_case0,
    ktest_idr_case1,
    // ktest_idr_case2, // 为了加快启动速度, 暂时注释掉这个测试
    ktest_idr_case3,
    ktest_idr_case4,
    ktest_idr_case5,
    ktest_idr_case6,
};

int ktest_test_idr(void* arg)
{
    kTEST("Testing idr...");
    unsigned int sz = sizeof(kt_idr_func_table) / sizeof(ktest_case_table);
    for (int i = 0; i < sz; ++i)
    {
        kTEST("Testing case %d", i);
        kt_idr_func_table[i](i, i + 1);
    }
    kTEST("idr Test done.");
    return 0;
}