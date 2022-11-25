#include "ktest.h"
#include "ktest_utils.h"
#include <common/kfifo.h>
#include <common/kprint.h>
#include <mm/slab.h>

static long ktest_kfifo_case0_1(uint64_t arg0, uint64_t arg1)
{
    const int fifo_size = 256;
    // 创建kfifo（由kfifo申请内存）
    struct kfifo_t fifo;
    if (arg0 == 0)
        assert(kfifo_alloc(&fifo, fifo_size, 0) == 0);
    else
    {
        void *buf = kmalloc(fifo_size, 0);
        kfifo_init(&fifo, buf, fifo_size);
    }

    assert(fifo.buffer != NULL);
    assert(fifo.total_size == fifo_size);
    assert(kfifo_total_size(&fifo) == fifo_size);
    assert(fifo.size == 0);
    assert(kfifo_size(&fifo) == 0);
    assert(fifo.in_offset == 0);
    assert(fifo.out_offset == 0);
    assert(kfifo_empty(&fifo) == 1);
    assert(kfifo_full(&fifo) == 0);

    // 循环增加10个uint64_t
    for (int i = 1; i <= 10; ++i)
    {
        uint64_t tmp = i;
        assert(kfifo_in(&fifo, &tmp, sizeof(uint64_t)) == sizeof(uint64_t));
    }
    assert(fifo.in_offset == 10 * sizeof(uint64_t));
    assert(fifo.out_offset == 0);
    assert(fifo.size == 10 * sizeof(uint64_t));
    assert(fifo.total_size == fifo_size);

    // 循环删除这10个uint64_t
    for (int i = 1; i <= 10; ++i)
    {
        uint64_t tmp = 0;
        assert(kfifo_out(&fifo, &tmp, sizeof(uint64_t)) == sizeof(uint64_t));
        assert(tmp == i);
        assert(fifo.size == (10 - i) * sizeof(uint64_t));
        assert(fifo.in_offset == 10 * sizeof(uint64_t));
        assert(fifo.out_offset == i * sizeof(uint64_t));
    }

    assert(fifo.in_offset == 10 * sizeof(uint64_t));
    assert(fifo.out_offset == 10 * sizeof(uint64_t));
    assert(fifo.in_offset == fifo.out_offset);
    assert(kfifo_empty(&fifo) == 1);

    // reset
    kfifo_reset(&fifo);
    assert(fifo.in_offset == 0);
    assert(fifo.out_offset == 0);
    assert(fifo.size == 0);

    // 测试插入31个元素
    for (int i = 1; i <= 31; ++i)
    {
        uint64_t tmp = i;
        assert(kfifo_in(&fifo, &tmp, sizeof(uint64_t)) == sizeof(uint64_t));
    }

    assert(fifo.size == 31 * sizeof(uint64_t));
    assert(fifo.in_offset == 31 * sizeof(uint64_t));
    assert(fifo.out_offset == 0);

    // 然后再尝试插入一个大小为2*sizeof(uint64_t)的元素
    {
        __int128_t tmp = 100;
        assert(kfifo_in(&fifo, &tmp, sizeof(__int128_t)) == 0);
        assert(fifo.size == 31 * sizeof(uint64_t));
        assert(fifo.in_offset == 31 * sizeof(uint64_t));
        assert(fifo.out_offset == 0);
    }
    // 插入一个uint64, 队列满
    {
        uint64_t tmp = 32;
        assert(kfifo_in(&fifo, &tmp, sizeof(uint64_t)) == sizeof(uint64_t));
        assert(kfifo_full(&fifo));
        assert(kfifo_empty(&fifo) == 0);
        assert(fifo.size == fifo.total_size);
        assert(fifo.in_offset == fifo_size);
        assert(fifo.out_offset == 0);
    }

    // 取出之前的20个元素
    for (int i = 1; i <= 20; ++i)
    {
        uint64_t tmp = 0;
        assert(kfifo_out(&fifo, &tmp, sizeof(uint64_t)) == sizeof(uint64_t));
    }
    assert(fifo.size == (fifo.total_size - 20 * sizeof(uint64_t)));
    assert(fifo.in_offset == fifo_size);
    assert(fifo.out_offset == 20 * sizeof(uint64_t));

    // 插入10个元素,剩余10个空位
    {
        uint64_t tmp = 99;

        assert(kfifo_in(&fifo, &tmp, sizeof(uint64_t)) == sizeof(uint64_t));
        assert(fifo.in_offset == 1 * sizeof(uint64_t));

        for (int i = 1; i <= 9; ++i)
        {
            assert(kfifo_in(&fifo, &tmp, sizeof(uint64_t)) == sizeof(uint64_t));
        }
        assert(fifo.in_offset == 10 * sizeof(uint64_t));
        assert(fifo.size == 22 * sizeof(uint64_t));
    }

    {
        // 取出20个
        char tmp[20 * sizeof(uint64_t)];
        assert(kfifo_out(&fifo, &tmp, 20 * sizeof(uint64_t)) == 20 * sizeof(uint64_t));
        assert(fifo.out_offset == 8 * sizeof(uint64_t));
        assert(fifo.size == 2 * (sizeof(uint64_t)));
    }

    {
        // 插入25个
        char tmp[25 * sizeof(uint64_t)];
        assert(kfifo_in(&fifo, &tmp, 25 * sizeof(uint64_t)) == 25 * sizeof(uint64_t));
        assert(fifo.out_offset == 8 * sizeof(uint64_t));
        assert(fifo.size == 27 * sizeof(uint64_t));
        assert(fifo.in_offset == 3 * sizeof(uint64_t));
    }

    // 测试reset out
    uint32_t prev_in_offset = fifo.in_offset;
    kfifo_reset_out(&fifo);
    assert(fifo.size == 0);
    assert(fifo.total_size == fifo_size);
    assert(fifo.in_offset == prev_in_offset);
    assert(fifo.out_offset == prev_in_offset);

    // 测试释放
    if (arg0 == 0)
    {
        kfifo_free_alloc(&fifo);
        assert(fifo.buffer == NULL);
    }
    return 0;
}

static ktest_case_table kt_kfifo_func_table[] = {
    ktest_kfifo_case0_1,
};

int ktest_test_kfifo(void* arg)
{
    kTEST("Testing kfifo...");
    for (int i = 0; i < sizeof(kt_kfifo_func_table) / sizeof(ktest_case_table); ++i)
    {
        kTEST("Testing case %d", i);
        kt_kfifo_func_table[i](i, 0);
    }
    kTEST("kfifo Test done.");
    return 0;
}
