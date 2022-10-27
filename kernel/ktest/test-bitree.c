#include "ktest.h"
#include <ktest/ktest_utils.h>

#include <common/unistd.h>
#include <common/kprint.h>
#include <common/bitree.h>
#include <common/errno.h>

#include <mm/slab.h>

struct test_value_t
{
    uint64_t tv;
};
static int compare(void *a, void *b)
{
    if (((struct test_value_t *)a)->tv > ((struct test_value_t *)b)->tv)
        return 1;
    else if (((struct test_value_t *)a)->tv == ((struct test_value_t *)b)->tv)
        return 0;
    else
        return -1;
}

static int release(void *value)
{
    // kdebug("release");
    return 0;
}

/**
 * @brief 测试创建二叉树
 *
 * @return int
 */
static long ktest_bitree_case1(uint64_t arg0, uint64_t arg1)
{
    int val;
    // ========== 测试创建树
    struct test_value_t *tv1 = (struct test_value_t *)kmalloc(sizeof(struct test_value_t), 0);
    tv1->tv = 20;
    struct bt_node_t *rn = bt_create_node(NULL, NULL, NULL, tv1);

    assert(rn != NULL);
    assert((int64_t)rn != (-EINVAL));
    assert(rn->value == tv1);

    struct bt_root_t *tree = bt_create_tree(rn, compare, release);
    assert(tree != NULL);
    assert(tree->bt_node == rn);
    assert(tree->cmp == compare);
    assert(tree->release == release);
    assert(tree->size == 1);

    // ========= 向树中插入数据10、30
    struct test_value_t *tv2 = (struct test_value_t *)kmalloc(sizeof(struct test_value_t), 0);
    assert(tv2 != NULL);
    tv2->tv = 10;
    {
        int last_size = tree->size;
        val = bt_insert(tree, tv2);
        assert(val == 0);
        assert(last_size + 1 == tree->size);
    }
    struct test_value_t *tv3 = (struct test_value_t *)kmalloc(sizeof(struct test_value_t), 0);
    assert(tv3 != NULL);
    tv3->tv = 30;
    {
        int last_size = tree->size;
        val = bt_insert(tree, tv3);
        assert(val == 0);
        assert(last_size + 1 == tree->size);
    }

    // 检测树的形状
    assert(((struct test_value_t *)tree->bt_node->left->value)->tv == tv2->tv);
    assert(((struct test_value_t *)tree->bt_node->right->value)->tv == tv3->tv);

    // ========= 查询结点
    // 查询值为tv2的结点
    struct bt_node_t *node2;
    assert(bt_query(tree, tv2, (uint64_t*)(&node2)) == 0);
    assert(node2 != NULL);
    assert(node2->value == tv2);

    // ========= 插入第4个结点：15
    struct test_value_t *tv4 = (struct test_value_t *)kmalloc(sizeof(struct test_value_t), 0);
    assert(tv4 != NULL);
    tv4->tv = 15;
    {
        int last_size = tree->size;
        val = bt_insert(tree, tv4);
        assert(val == 0);
        assert(last_size + 1 == tree->size);
    }

    assert(((struct test_value_t *)node2->right->value)->tv == tv4->tv);

    // ======= 查询不存在的值
    struct bt_node_t *node_not_exists;
    struct test_value_t *tv_not_exists = (struct test_value_t *)kmalloc(sizeof(struct test_value_t), 0);
    assert(tv_not_exists != NULL);
    tv_not_exists->tv = 100;
    assert(bt_query(tree, tv_not_exists, (uint64_t*)(&node_not_exists)) == -1);
    // kdebug("node_not_exists.val=%d", ((struct test_value_t*)node_not_exists->value)->tv);
    assert(node_not_exists == NULL);

    // 删除根节点
    assert(bt_delete(tree, rn->value) == 0);
    assert(((struct test_value_t *)tree->bt_node->value)->tv != 20);
    assert(tree->bt_node->right == NULL);

    // 删除树
    assert(bt_destroy_tree(tree) == 0);

    return 0;
}

static ktest_case_table kt_bitree_func_table[] = {
    ktest_bitree_case1,
};

int ktest_test_bitree(void* arg)
{
    kTEST("Testing bitree...");
    for (int i = 0; i < sizeof(kt_bitree_func_table) / sizeof(ktest_case_table); ++i)
    {
        kTEST("Testing case %d", i);
        kt_bitree_func_table[i](0, 0);
    }
    kTEST("bitree Test done.");
    return 0;
}