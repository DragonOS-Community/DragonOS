#include "slab.h"

/**
 * @brief 创建一个内存池
 *
 * @param size 内存池容量大小
 * @param constructor 构造函数
 * @param destructor 析构函数
 * @param arg 参数
 * @return struct slab* 构建好的内存池对象
 */
struct slab *slab_create(ul size, void *(*constructor)(void *vaddr, ul arg), void *(*destructor)(void *vaddr, ul arg), ul arg)
{
    struct slab *slab_pool = (struct slab *)kmalloc(sizeof(struct slab), 0);

    // BUG
    if (slab_pool == NULL)
    {
        kBUG("slab_create()->kmalloc()->slab == NULL");
        return NULL;
    }

    memset(slab_pool, 0, sizeof(struct slab));

    slab_pool->size = SIZEOF_LONG_ALIGN(size);
    slab_pool->count_total_using = 0;
    slab_pool->count_total_free = 0;
    // 直接分配cache_pool结构体，避免每次访问都要检测是否为NULL，提升效率
    slab_pool->cache_pool = (struct slab_obj *)kmalloc(sizeof(struct slab_obj), 0);

    // BUG
    if (slab_pool->cache_pool == NULL)
    {
        kBUG("slab_create()->kmalloc()->slab->cache_pool == NULL");
        kfree(slab_pool);
        return NULL;
    }
    memset(slab_pool->cache_pool, 0, sizeof(struct slab_obj));

    // dma内存池设置为空
    slab_pool->cache_dma_pool = NULL;

    // 设置构造及析构函数
    slab_pool->constructor = constructor;
    slab_pool->destructor = destructor;

    list_init(&slab_pool->cache_pool->list);

    // 分配属于内存池的内存页
    slab_pool->cache_pool->page = alloc_pages(ZONE_NORMAL, 1, PAGE_KERNEL);

    // BUG
    if (slab_pool->cache_pool->page == NULL)
    {
        kBUG("slab_create()->kmalloc()->slab->cache_pool == NULL");
        kfree(slab_pool->cache_pool);
        kfree(slab_pool);
        return NULL;
    }

    // page_init(slab_pool->cache_pool->page, PAGE_KERNEL);

    slab_pool->cache_pool->count_using = 0;
    slab_pool->cache_pool->count_free = PAGE_2M_SIZE / slab_pool->size;

    slab_pool->count_total_free = slab_pool->cache_pool->count_free;

    slab_pool->cache_pool->vaddr = phys_2_virt(slab_pool->cache_pool->page->addr_phys);

    // bitmap有多少有效位
    slab_pool->cache_pool->bmp_count = slab_pool->cache_pool->count_free;

    // 计算位图所占的空间 占用多少byte（按unsigned long大小的上边缘对齐）
    slab_pool->cache_pool->bmp_len = ((slab_pool->cache_pool->bmp_count + sizeof(ul) * 8 - 1) >> 6) << 3;
    // 初始化位图
    slab_pool->cache_pool->bmp = (ul *)kmalloc(slab_pool->cache_pool->bmp_len, 0);

    // BUG
    if (slab_pool->cache_pool->bmp == NULL)
    {
        kBUG("slab_create()->kmalloc()->slab->cache_pool == NULL");
        free_pages(slab_pool->cache_pool->page, 1);
        kfree(slab_pool->cache_pool);
        kfree(slab_pool);
        return NULL;
    }
    // 将位图清空
    memset(slab_pool->cache_pool->bmp, 0, slab_pool->cache_pool->bmp_len);

    return slab_pool;
}

/**
 * @brief 销毁内存池对象
 * 只有当slab对象是空的时候才能销毁
 * @param slab_pool 要销毁的内存池对象
 * @return ul
 *
 */
ul slab_destroy(struct slab *slab_pool)
{
    struct slab_obj *slab_obj_ptr = slab_pool->cache_pool;
    if (slab_pool->count_total_using)
    {
        kBUG("slab_cache->count_total_using != 0");
        return ESLAB_NOTNULL;
    }

    struct slab_obj *tmp_slab_obj = NULL;
    while (!list_empty(&slab_obj_ptr->list))
    {
        tmp_slab_obj = slab_obj_ptr;
        // 获取下一个slab_obj的起始地址
        slab_obj_ptr = container_of(list_next(&slab_obj_ptr->list), struct slab_obj, list);

        list_del(&tmp_slab_obj->list);

        kfree(tmp_slab_obj->bmp);

        page_clean(tmp_slab_obj->page);

        free_pages(tmp_slab_obj->page, 1);

        kfree(tmp_slab_obj);
    }

    kfree(slab_obj_ptr->bmp);
    page_clean(slab_obj_ptr->page);
    free_pages(slab_obj_ptr->page, 1);
    kfree(slab_obj_ptr);
    kfree(slab_pool);

    return 0;
}

/**
 * @brief 分配SLAB内存池中的内存对象
 *
 * @param slab_pool slab内存池
 * @param arg 传递给内存对象构造函数的参数
 * @return void* 内存空间的虚拟地址
 */
void *slab_malloc(struct slab *slab_pool, ul arg)
{
    struct slab_obj *slab_obj_ptr = slab_pool->cache_pool;
    struct slab_obj *tmp_slab_obj = NULL;

    // slab内存池中已经没有空闲的内存对象，进行扩容
    if (slab_pool->count_total_free == 0)
    {
        tmp_slab_obj = (struct slab_obj *)kmalloc(sizeof(struct slab_obj), 0);

        // BUG
        if (tmp_slab_obj == NULL)
        {
            kBUG("slab_malloc()->kmalloc()->slab->tmp_slab_obj == NULL");
            return NULL;
        }

        memset(tmp_slab_obj, 0, sizeof(struct slab_obj));
        list_init(&tmp_slab_obj->list);

        tmp_slab_obj->page = alloc_pages(ZONE_NORMAL, 1, PAGE_KERNEL);

        // BUG
        if (tmp_slab_obj->page == NULL)
        {
            kBUG("slab_malloc()->kmalloc()=>tmp_slab_obj->page == NULL");
            kfree(tmp_slab_obj);
            return NULL;
        }

        tmp_slab_obj->count_using = 0;
        tmp_slab_obj->count_free = PAGE_2M_SIZE / slab_pool->size;
        tmp_slab_obj->vaddr = phys_2_virt(tmp_slab_obj->page->addr_phys);
        tmp_slab_obj->bmp_count = tmp_slab_obj->count_free;
        // 计算位图所占的空间 占用多少byte（按unsigned long大小的上边缘对齐）
        tmp_slab_obj->bmp_len = ((tmp_slab_obj->bmp_count + sizeof(ul) * 8 - 1) >> 6) << 3;
        tmp_slab_obj->bmp = (ul *)kmalloc(tmp_slab_obj->bmp_len, 0);

        // BUG
        if (tmp_slab_obj->bmp == NULL)
        {
            kBUG("slab_malloc()->kmalloc()=>tmp_slab_obj->bmp == NULL");
            free_pages(tmp_slab_obj->page, 1);
            kfree(tmp_slab_obj);
            return NULL;
        }

        memset(tmp_slab_obj->bmp, 0, tmp_slab_obj->bmp_len);

        list_add(&slab_pool->cache_pool->list, tmp_slab_obj);

        slab_pool->count_total_free += tmp_slab_obj->count_free;

        slab_obj_ptr = tmp_slab_obj;
    }

    // 扩容完毕或无需扩容，开始分配内存对象
    int tmp_md;
    do
    {
        if (slab_obj_ptr->count_free == 0)
        {
            slab_obj_ptr = container_of(list_next(&slab_obj_ptr->list), struct slab_obj, list);
            continue;
        }

        for (int i = 0; i < slab_obj_ptr->bmp_count; ++i)
        {
            // 当前bmp对应的内存对象都已经被分配
            if (*(slab_obj_ptr->bmp + (i >> 6)) == 0xffffffffffffffffUL)
            {
                i += 63;
                continue;
            }

            // 第i个内存对象是空闲的
            tmp_md = i % 64;
            if ((*(slab_obj_ptr->bmp + (i >> 6)) & (1UL << tmp_md)) == 0)
            {
                // 置位bmp
                *(slab_obj_ptr->bmp + (i >> 6)) |= (1UL << tmp_md);

                // 更新当前slab对象的计数器
                ++(slab_obj_ptr->count_using);
                --(slab_obj_ptr->count_free);
                // 更新slab内存池的计数器
                ++(slab_pool->count_total_using);
                --(slab_pool->count_total_free);

                if (slab_pool->constructor != NULL)
                {
                    // 返回内存对象指针（要求构造函数返回内存对象指针）
                    return slab_pool->constructor((char *)slab_obj_ptr->vaddr + slab_pool->size * i, arg);
                }
                // 返回内存对象指针
                else
                    return (void *)((char *)slab_obj_ptr->vaddr + slab_pool->size * i);
            }
        }

    } while (slab_obj_ptr != slab_pool->cache_pool);

    // should not be here

    kBUG("slab_malloc() ERROR: can't malloc");

    // 释放内存
    if (tmp_slab_obj != NULL)
    {
        list_del(&tmp_slab_obj->list);
        kfree(tmp_slab_obj->bmp);
        page_clean(tmp_slab_obj->page);
        free_pages(tmp_slab_obj->page, 1);
        kfree(tmp_slab_obj);
    }
    return NULL;
}

/**
 * @brief 回收slab内存池中的对象
 *
 * @param slab_pool 对应的内存池
 * @param addr 内存对象的虚拟地址
 * @param arg 传递给虚构函数的参数
 * @return ul
 */
ul slab_free(struct slab *slab_pool, void *addr, ul arg)
{
    struct slab_obj *slab_obj_ptr = slab_pool->cache_pool;

    do
    {
        // 虚拟地址不在当前内存池对象的管理范围内
        if (!(slab_obj_ptr->vaddr <= addr && addr <= (slab_obj_ptr->vaddr + PAGE_2M_SIZE)))
        {
            slab_obj_ptr = container_of(list_next(&slab_obj_ptr->list), struct slab_obj, list);
            continue;
        }

        // 计算出给定内存对象是第几个
        int index = (addr - slab_obj_ptr->vaddr) / slab_pool->size;

        // 复位位图中对应的位
        *(slab_obj_ptr->bmp + (index >> 6)) ^= (1UL << index % 64);

        ++(slab_obj_ptr->count_free);
        --(slab_obj_ptr->count_using);

        ++(slab_pool->count_total_free);
        --(slab_pool->count_total_using);

        // 有对应的析构函数，调用析构函数
        if (slab_pool->destructor != NULL)
            slab_pool->destructor((char *)slab_obj_ptr->vaddr + slab_pool->size * index, arg);
        
        // 当前内存对象池的正在使用的内存对象为0，且内存池的空闲对象大于当前对象池的2倍，则销毁当前对象池，以减轻系统内存压力
        if((slab_obj_ptr->count_using==0)&&((slab_pool->count_total_free>>1)>=slab_obj_ptr->count_free))
        {
            // 防止删除了slab_pool的cache_pool入口
            if(slab_pool->cache_pool==slab_obj_ptr)
                slab_pool->cache_pool = container_of(list_next(&slab_obj_ptr->list), struct slab_obj, list);
            
            list_del(&slab_obj_ptr->list);
            slab_pool->count_total_free -= slab_obj_ptr->count_free;
            
            kfree(slab_obj_ptr->bmp);
            page_clean(slab_obj_ptr->page);
            free_pages(slab_obj_ptr->page,1);
            kfree(slab_obj_ptr);
            
        }

        return 0;
    } while (slab_obj_ptr != slab_pool->cache_pool);

    kwarn("slab_free(): address not in current slab");
    return ENOT_IN_SLAB;
}

/**
 * @brief 通用内存分配函数
 *
 * @param size 要分配的内存大小
 * @param flags 内存的flag
 * @return void*
 */
void *kmalloc(unsigned long size, unsigned long flags)
{
    // @todo: 内存分配函数
}

/**
 * @brief 通用内存释放函数
 *
 * @param address 要释放的内存地址
 * @return unsigned long
 */
unsigned long kfree(void *address)
{
    // @todo: 通用内存释放函数
}