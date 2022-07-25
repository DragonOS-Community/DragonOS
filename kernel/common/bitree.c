#include "bitree.h"
#include <mm/slab.h>
#include <common/errno.h>
#include <debug/bug.h>

#define smaller(root, a, b) (root->cmp(a, b) == -1)
#define equal(root, a, b) (root->cmp(a, b) == 0)
#define greater(root, a, b) (root->cmp(a, b) == 1)

/**
 * @brief 创建二叉搜索树
 *
 * @param node 根节点
 * @param cmp 比较函数
 * @return struct bt_root_t* 树根结构体
 */
struct bt_root_t *bt_create_tree(struct bt_node_t *node, int (*cmp)(struct bt_node_t *a, struct bt_node_t *b))
{
    if (node == NULL || cmp == NULL)
        return -EINVAL;

    struct bt_root_t *root = (struct bt_root_t *)kmalloc(sizeof(struct bt_root_t), 0);
    memset((void *)root, 0, sizeof(struct bt_root_t));
    root->bt_node = node;
    root->cmp = cmp;

    return root;
}

/**
 * @brief 创建结点
 *
 * @param left 左子节点
 * @param right 右子节点
 * @param value 当前节点的值
 * @return struct bt_node_t*
 */
struct bt_node_t *bt_create_node(struct bt_node_t *left, struct bt_node_t *right, struct bt_node_t *parent, void *value)
{
    struct bt_node_t *node = (struct bt_node_t *)kmalloc(sizeof(struct bt_node_t), 0);
    FAIL_ON_TO(node == NULL, nomem);
    memset((void *)node, 0, sizeof(struct bt_node_t));

    node->left = left;
    node->right = right;
    node->value = value;
    node->parent = parent;

    return node;
nomem:;
    return -ENOMEM;
}
/**
 * @brief 插入结点
 *
 * @param root 树根结点
 * @param value 待插入结点的值
 * @return int 返回码
 */
int bt_insert(struct bt_root_t *root, void *value)
{
    if (root == NULL)
        return -EINVAL;

    struct bt_node_t *this_node = root->bt_node;
    struct bt_node_t *last_node = NULL;
    struct bt_node_t *insert_node = bt_create_node(NULL, NULL, NULL, value);
    FAIL_ON_TO((uint64_t)insert_node == (uint64_t)(-ENOMEM), failed);

    while (this_node != NULL)
    {
        last_node = this_node;
        if (smaller(root, insert_node, this_node))
            this_node = this_node->left;
        else
            this_node = this_node->right;
    }

    insert_node->parent = last_node;
    if (unlikely(last_node == NULL))
        root->bt_node = insert_node;
    else
    {
        if (smaller(root, insert_node, last_node))
            last_node->left = insert_node;
        else
            last_node->right = insert_node;
    }

    return 0;

failed:;
    return -ENOMEM;
}

/**
 * @brief 搜索值为value的结点
 *
 * @param value 值
 * @param ret_addr 返回的结点基地址
 * @return int 错误码
 */
int bt_query(struct bt_root_t *root, void *value, uint64_t *ret_addr)
{
    struct bt_node_t *this_node = root->bt_node;
    struct bt_node_t tmp_node = {0};
    tmp_node.value = value;
    while (this_node != NULL && !equal(root, &this_node, &tmp_node))
    {
        if (smaller(root, &tmp_node, this_node))
            this_node = this_node->left;
        else
            this_node = this_node->right;
    }

    *ret_addr = (uint64_t)ret_addr;
    return 0;
}