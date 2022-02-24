#pragma once

#include "mm.h"
#include "../common/glib.h"
#include "../common/printk.h"
#include "../common/kprint.h"

#define SIZEOF_LONG_ALIGN(size) ((size + sizeof(long) - 1) & ~(sizeof(long) - 1))
#define SIZEOF_INT_ALIGN(size) ((size + sizeof(int) - 1) & ~(sizeof(int) - 1))

// SLAB存储池count_using不为空
#define ESLAB_NOTNULL 101
#define ENOT_IN_SLAB 102


struct slab_obj
{
    struct List *list;
    // 当前slab对象所使用的内存页
    struct Page *page;

    ul count_using;
    ul count_free;

    // 当前页面所在的线性地址
    void *vaddr;

    // 位图
    ul bmp_len; // 位图的长度（字节）
    ul bmp_count;   // 位图的有效位数
    ul *bmp;
};

// slab内存池
struct slab
{
    ul size;
    ul count_total_using;
    ul count_total_free;
    // 内存池对象
    struct slab_obj *cache_pool;
    // dma内存池对象
    struct slab_obj *cache_dma_pool;

    // 内存池的构造函数和析构函数
    void *(*constructor)(void *vaddr, ul arg);
    void *(*destructor)(void *vaddr, ul arg);
};

/**
 * @brief 通用内存分配函数
 *
 * @param size 要分配的内存大小
 * @param flags 内存的flag
 * @return void*
 */
void *kmalloc(unsigned long size, unsigned long flags);

/**
 * @brief 通用内存释放函数
 * 
 * @param address 要释放的内存地址
 * @return unsigned long 
 */
unsigned long kfree(void * address);

/**
 * @brief 创建一个内存池
 *
 * @param size 内存池容量大小
 * @param constructor 构造函数
 * @param destructor 析构函数
 * @param arg 参数
 * @return struct slab* 构建好的内存池对象
 */
struct slab *slab_create(ul size, void *(*constructor)(void *vaddr, ul arg), void *(*destructor)(void *vaddr, ul arg), ul arg);

/**
 * @brief 销毁内存池对象
 * 只有当slab对象是空的时候才能销毁
 * @param slab_pool 要销毁的内存池对象
 * @return ul 
 * 
 */
ul slab_destroy(struct slab * slab_pool);

/**
 * @brief 分配SLAB内存池中的内存对象
 * 
 * @param slab_pool slab内存池
 * @param arg 传递给内存对象构造函数的参数
 * @return void* 内存空间的虚拟地址
 */
void* slab_malloc(struct slab *slab_pool, ul arg);

/**
 * @brief 回收slab内存池中的对象
 * 
 * @param slab_pool 对应的内存池
 * @param addr 内存对象的虚拟地址
 * @param arg 传递给虚构函数的参数
 * @return ul 
 */
ul slab_free(struct slab* slab_pool, void* addr, ul arg);