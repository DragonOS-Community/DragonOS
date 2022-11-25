#pragma once
#include <common/glib.h>

struct bt_node_t
{
    struct bt_node_t *left;
    struct bt_node_t *right;
    struct bt_node_t *parent;
    void *value; // 数据

} __attribute__((aligned(sizeof(long))));

struct bt_root_t
{
    struct bt_node_t *bt_node;
    int32_t size;                 // 树中的元素个数
    int (*cmp)(void *a, void *b); // 比较函数   a>b 返回1， a==b返回0, a<b返回-1
    /**
     * @brief 释放结点的value的函数
     * @param value 结点的值
     */
    int (*release)(void *value);
};

/**
 * @brief 创建二叉搜索树
 *
 * @param node 根节点
 * @param cmp 比较函数
 * @param release 用来释放结点的value的函数
 * @return struct bt_root_t* 树根结构体
 */
struct bt_root_t *bt_create_tree(struct bt_node_t *node, int (*cmp)(void *a, void *b), int (*release)(void *value));

/**
 * @brief 创建结点
 *
 * @param left 左子节点
 * @param right 右子节点
 * @param value 当前节点的值
 * @return struct bt_node_t*
 */
struct bt_node_t *bt_create_node(struct bt_node_t *left, struct bt_node_t *right, struct bt_node_t *parent, void *value);

/**
 * @brief 插入结点
 *
 * @param root 树根结点
 * @param value 待插入结点的值
 * @return int 返回码
 */
int bt_insert(struct bt_root_t *root, void *value);

/**
 * @brief 搜索值为value的结点
 *
 * @param root 树根结点
 * @param value 值
 * @param ret_addr 返回的结点基地址
 * @return int 错误码
 */
int bt_query(struct bt_root_t *root, void *value, uint64_t *ret_addr);

/**
 * @brief 删除结点
 *
 * @param root 树根
 * @param value 待删除结点的值
 * @return int 返回码
 */
int bt_delete(struct bt_root_t *root, void *value);

/**
 * @brief 释放整个二叉搜索树
 *
 * @param root
 * @return int
 */
int bt_destroy_tree(struct bt_root_t *root);