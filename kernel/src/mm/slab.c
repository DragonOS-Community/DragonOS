#include "slab.h"
#include <common/compiler.h>

struct slab kmalloc_cache_group[16] =
    {
        {32, 0, 0, NULL, NULL, NULL, NULL},
        {64, 0, 0, NULL, NULL, NULL, NULL},
        {128, 0, 0, NULL, NULL, NULL, NULL},
        {256, 0, 0, NULL, NULL, NULL, NULL},
        {512, 0, 0, NULL, NULL, NULL, NULL},
        {1024, 0, 0, NULL, NULL, NULL, NULL}, // 1KB
        {2048, 0, 0, NULL, NULL, NULL, NULL},
        {4096, 0, 0, NULL, NULL, NULL, NULL}, // 4KB
        {8192, 0, 0, NULL, NULL, NULL, NULL},
        {16384, 0, 0, NULL, NULL, NULL, NULL},
        {32768, 0, 0, NULL, NULL, NULL, NULL},
        {65536, 0, 0, NULL, NULL, NULL, NULL},
        {131072, 0, 0, NULL, NULL, NULL, NULL}, // 128KB
        {262144, 0, 0, NULL, NULL, NULL, NULL},
        {524288, 0, 0, NULL, NULL, NULL, NULL},
        {1048576, 0, 0, NULL, NULL, NULL, NULL}, // 1MB
};

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
    // 直接分配cache_pool_entry结构体，避免每次访问都要检测是否为NULL，提升效率
    slab_pool->cache_pool_entry = (struct slab_obj *)kmalloc(sizeof(struct slab_obj), 0);

    // BUG
    if (slab_pool->cache_pool_entry == NULL)
    {
        kBUG("slab_create()->kmalloc()->slab->cache_pool_entry == NULL");
        kfree(slab_pool);
        return NULL;
    }
    memset(slab_pool->cache_pool_entry, 0, sizeof(struct slab_obj));

    // dma内存池设置为空
    slab_pool->cache_dma_pool_entry = NULL;

    // 设置构造及析构函数
    slab_pool->constructor = constructor;
    slab_pool->destructor = destructor;

    list_init(&slab_pool->cache_pool_entry->list);

    // 分配属于内存池的内存页
    slab_pool->cache_pool_entry->page = alloc_pages(ZONE_NORMAL, 1, PAGE_KERNEL);

    // BUG
    if (slab_pool->cache_pool_entry->page == NULL)
    {
        kBUG("slab_create()->kmalloc()->slab->cache_pool_entry == NULL");
        kfree(slab_pool->cache_pool_entry);
        kfree(slab_pool);
        return NULL;
    }

    // page_init(slab_pool->cache_pool_entry->page, PAGE_KERNEL);

    slab_pool->cache_pool_entry->count_using = 0;
    slab_pool->cache_pool_entry->count_free = PAGE_2M_SIZE / slab_pool->size;

    slab_pool->count_total_free = slab_pool->cache_pool_entry->count_free;

    slab_pool->cache_pool_entry->vaddr = phys_2_virt(slab_pool->cache_pool_entry->page->addr_phys);

    // bitmap有多少有效位
    slab_pool->cache_pool_entry->bmp_count = slab_pool->cache_pool_entry->count_free;

    // 计算位图所占的空间 占用多少byte（按unsigned long大小的上边缘对齐）
    slab_pool->cache_pool_entry->bmp_len = ((slab_pool->cache_pool_entry->bmp_count + sizeof(ul) * 8 - 1) >> 6) << 3;
    // 初始化位图
    slab_pool->cache_pool_entry->bmp = (ul *)kmalloc(slab_pool->cache_pool_entry->bmp_len, 0);

    // BUG
    if (slab_pool->cache_pool_entry->bmp == NULL)
    {
        kBUG("slab_create()->kmalloc()->slab->cache_pool_entry == NULL");
        free_pages(slab_pool->cache_pool_entry->page, 1);
        kfree(slab_pool->cache_pool_entry);
        kfree(slab_pool);
        return NULL;
    }
    // 将位图清空
    memset(slab_pool->cache_pool_entry->bmp, 0, slab_pool->cache_pool_entry->bmp_len);

    return slab_pool;
}

/**
 * @brief 销毁内存池
 * 只有当slab是空的时候才能销毁
 * @param slab_pool 要销毁的内存池
 * @return ul
 *
 */
ul slab_destroy(struct slab *slab_pool)
{
    struct slab_obj *slab_obj_ptr = slab_pool->cache_pool_entry;
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
    struct slab_obj *slab_obj_ptr = slab_pool->cache_pool_entry;
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

        list_add(&slab_pool->cache_pool_entry->list, &tmp_slab_obj->list);

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

    } while (slab_obj_ptr != slab_pool->cache_pool_entry);

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
    struct slab_obj *slab_obj_ptr = slab_pool->cache_pool_entry;

    do
    {
        // 虚拟地址不在当前内存池对象的管理范围内
        if (!(slab_obj_ptr->vaddr <= addr && addr <= (slab_obj_ptr->vaddr + PAGE_2M_SIZE)))
        {
            slab_obj_ptr = container_of(list_next(&slab_obj_ptr->list), struct slab_obj, list);
        }
        else
        {

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
            if ((slab_obj_ptr->count_using == 0) && ((slab_pool->count_total_free >> 1) >= slab_obj_ptr->count_free) && (slab_obj_ptr != slab_pool->cache_pool_entry))
            {

                list_del(&slab_obj_ptr->list);
                slab_pool->count_total_free -= slab_obj_ptr->count_free;

                kfree(slab_obj_ptr->bmp);
                page_clean(slab_obj_ptr->page);
                free_pages(slab_obj_ptr->page, 1);

                kfree(slab_obj_ptr);
            }
        }

        return 0;
    } while (slab_obj_ptr != slab_pool->cache_pool_entry);

    kwarn("slab_free(): address not in current slab");
    return ENOT_IN_SLAB;
}

/**
 * @brief 初始化内存池组
 * 在初始化通用内存管理单元期间，尚无内存空间分配函数，需要我们手动为SLAB内存池指定存储空间
 * @return ul
 */
ul slab_init()
{
    kinfo("Initializing SLAB...");
    // 将slab的内存池空间放置在mms的后方
    ul tmp_addr = memory_management_struct.end_of_struct;
    for (int i = 0; i < 16; ++i)
    {
        io_mfence();
        spin_init(&kmalloc_cache_group[i].lock);
        // 将slab内存池对象的空间放置在mms的后面，并且预留4个unsigned long 的空间以防止内存越界
        kmalloc_cache_group[i].cache_pool_entry = (struct slab_obj *)memory_management_struct.end_of_struct;

        memory_management_struct.end_of_struct += sizeof(struct slab_obj) + (sizeof(ul) << 2);

        list_init(&kmalloc_cache_group[i].cache_pool_entry->list);

        // 初始化内存池对象
        kmalloc_cache_group[i].cache_pool_entry->count_using = 0;
        kmalloc_cache_group[i].cache_pool_entry->count_free = PAGE_2M_SIZE / kmalloc_cache_group[i].size;
        kmalloc_cache_group[i].cache_pool_entry->bmp_len = (((kmalloc_cache_group[i].cache_pool_entry->count_free + sizeof(ul) * 8 - 1) >> 6) << 3);
        kmalloc_cache_group[i].cache_pool_entry->bmp_count = kmalloc_cache_group[i].cache_pool_entry->count_free;

        // 在slab对象后方放置bmp
        kmalloc_cache_group[i].cache_pool_entry->bmp = (ul *)memory_management_struct.end_of_struct;

        // bmp后方预留4个unsigned long的空间防止内存越界,且按照8byte进行对齐
        memory_management_struct.end_of_struct = (ul)(memory_management_struct.end_of_struct + kmalloc_cache_group[i].cache_pool_entry->bmp_len + (sizeof(ul) << 2)) & (~(sizeof(ul) - 1));
        io_mfence();
        // @todo：此处可优化，直接把所有位设置为0，然后再对部分不存在对应的内存对象的位设置为1
        memset(kmalloc_cache_group[i].cache_pool_entry->bmp, 0xff, kmalloc_cache_group[i].cache_pool_entry->bmp_len);
        for (int j = 0; j < kmalloc_cache_group[i].cache_pool_entry->bmp_count; ++j)
            *(kmalloc_cache_group[i].cache_pool_entry->bmp + (j >> 6)) ^= 1UL << (j % 64);

        kmalloc_cache_group[i].count_total_using = 0;
        kmalloc_cache_group[i].count_total_free = kmalloc_cache_group[i].cache_pool_entry->count_free;
        io_mfence();
    }

    struct Page *page = NULL;

    // 将上面初始化内存池组时，所占用的内存页进行初始化
    ul tmp_page_mms_end = virt_2_phys(memory_management_struct.end_of_struct) >> PAGE_2M_SHIFT;

    ul page_num = 0;
    for (int i = PAGE_2M_ALIGN(virt_2_phys(tmp_addr)) >> PAGE_2M_SHIFT; i <= tmp_page_mms_end; ++i)
    {

        page = memory_management_struct.pages_struct + i;
        page_num = page->addr_phys >> PAGE_2M_SHIFT;
        *(memory_management_struct.bmp + (page_num >> 6)) |= (1UL << (page_num % 64));
        ++page->zone->count_pages_using;
        io_mfence();
        --page->zone->count_pages_free;
        page_init(page, PAGE_KERNEL_INIT | PAGE_KERNEL | PAGE_PGT_MAPPED);
    }
    io_mfence();

    // 为slab内存池对象分配内存空间
    ul *virt = NULL;
    for (int i = 0; i < 16; ++i)
    {
        // 获取一个新的空页并添加到空页表，然后返回其虚拟地址
        virt = (ul *)((memory_management_struct.end_of_struct + PAGE_2M_SIZE * i + PAGE_2M_SIZE - 1) & PAGE_2M_MASK);

        page = Virt_To_2M_Page(virt);

        page_num = page->addr_phys >> PAGE_2M_SHIFT;

        *(memory_management_struct.bmp + (page_num >> 6)) |= (1UL << (page_num % 64));

        ++page->zone->count_pages_using;
        io_mfence(); // 该位置必须加一个mfence，否则O3优化运行时会报错
        --page->zone->count_pages_free;
        page_init(page, PAGE_PGT_MAPPED | PAGE_KERNEL | PAGE_KERNEL_INIT);

        kmalloc_cache_group[i].cache_pool_entry->page = page;

        kmalloc_cache_group[i].cache_pool_entry->vaddr = virt;
    }

    kinfo("SLAB initialized successfully!");

    return 0;
}

/**
 * @brief 在kmalloc中创建slab_obj的函数（与slab_malloc()中的类似)
 *
 * @param size
 * @return struct slab_obj* 创建好的slab_obj
 */

struct slab_obj *kmalloc_create_slab_obj(ul size)
{
    struct Page *page = alloc_pages(ZONE_NORMAL, 1, 0);

    // BUG
    if (page == NULL)
    {
        kBUG("kmalloc_create()->alloc_pages()=>page == NULL");
        return NULL;
    }

    page_init(page, PAGE_KERNEL);

    ul *vaddr = NULL;
    ul struct_size = 0;
    struct slab_obj *slab_obj_ptr;

    // 根据size大小，选择不同的分支来处理
    // 之所以选择512byte为分界点，是因为，此时bmp大小刚好为512byte。显而易见，选择过小的话会导致kmalloc函数与当前函数反复互相调用，最终导致栈溢出
    switch (size)
    {
    // ============ 对于size<=512byte的内存池对象，将slab_obj结构体和bmp放置在物理页的内部 ========
    // 由于这些对象的特征是，bmp占的空间大，而内存块的空间小，这样做的目的是避免再去申请一块内存来存储bmp，减少浪费。
    case 32:
    case 64:
    case 128:
    case 256:
    case 512:
        vaddr = phys_2_virt(page->addr_phys);
        // slab_obj结构体的大小 （本身的大小+bmp的大小）
        struct_size = sizeof(struct slab_obj) + PAGE_2M_SIZE / size / 8;
        // 将slab_obj放置到物理页的末尾
        slab_obj_ptr = (struct slab_obj *)((unsigned char *)vaddr + PAGE_2M_SIZE - struct_size);
        slab_obj_ptr->bmp = (void *)slab_obj_ptr + sizeof(struct slab_obj);

        slab_obj_ptr->count_free = (PAGE_2M_SIZE - struct_size) / size;
        slab_obj_ptr->count_using = 0;
        slab_obj_ptr->bmp_count = slab_obj_ptr->count_free;
        slab_obj_ptr->vaddr = vaddr;
        slab_obj_ptr->page = page;

        list_init(&slab_obj_ptr->list);

        slab_obj_ptr->bmp_len = ((slab_obj_ptr->bmp_count + sizeof(ul) * 8 - 1) >> 6) << 3;

        // @todo：此处可优化，直接把所有位设置为0，然后再对部分不存在对应的内存对象的位设置为1
        memset(slab_obj_ptr->bmp, 0xff, slab_obj_ptr->bmp_len);

        for (int i = 0; i < slab_obj_ptr->bmp_count; ++i)
            *(slab_obj_ptr->bmp + (i >> 6)) ^= 1UL << (i % 64);

        break;
    // ================= 较大的size时，slab_obj和bmp不再放置于当前物理页内部 ============
    // 因为在这种情况下，bmp很短，继续放置在当前物理页内部则会造成可分配的对象少，加剧了内存空间的浪费
    case 1024: // 1KB
    case 2048:
    case 4096: // 4KB
    case 8192:
    case 16384:
    case 32768:
    case 65536:
    case 131072: // 128KB
    case 262144:
    case 524288:
    case 1048576: // 1MB
        slab_obj_ptr = (struct slab_obj *)kmalloc(sizeof(struct slab_obj), 0);

        slab_obj_ptr->count_free = PAGE_2M_SIZE / size;
        slab_obj_ptr->count_using = 0;
        slab_obj_ptr->bmp_count = slab_obj_ptr->count_free;

        slab_obj_ptr->bmp_len = ((slab_obj_ptr->bmp_count + sizeof(ul) * 8 - 1) >> 6) << 3;

        slab_obj_ptr->bmp = (ul *)kmalloc(slab_obj_ptr->bmp_len, 0);

        // @todo：此处可优化，直接把所有位设置为0，然后再对部分不存在对应的内存对象的位设置为1
        memset(slab_obj_ptr->bmp, 0xff, slab_obj_ptr->bmp_len);
        for (int i = 0; i < slab_obj_ptr->bmp_count; ++i)
            *(slab_obj_ptr->bmp + (i >> 6)) ^= 1UL << (i % 64);

        slab_obj_ptr->vaddr = phys_2_virt(page->addr_phys);
        slab_obj_ptr->page = page;
        list_init(&slab_obj_ptr->list);
        break;
    // size 错误
    default:
        kerror("kamlloc_create(): Wrong size%d", size);
        free_pages(page, 1);
        return NULL;
        break;
    }

    return slab_obj_ptr;
}

/**
 * @brief 通用内存分配函数
 *
 * @param size 要分配的内存大小
 * @param gfp 内存的flag
 * @return void* 内核内存虚拟地址
 */
void *kmalloc(unsigned long size, gfp_t gfp)
{
    void *result = NULL;
    if (size > 1048576)
    {
        kwarn("kmalloc(): Can't alloc such memory: %ld bytes, because it is too large.", size);
        return NULL;
    }
    int index;
    for (int i = 0; i < 16; ++i)
    {
        if (kmalloc_cache_group[i].size >= size)
        {
            index = i;
            break;
        }
    }
    // 对当前内存池加锁
    spin_lock(&kmalloc_cache_group[index].lock);

    struct slab_obj *slab_obj_ptr = kmalloc_cache_group[index].cache_pool_entry;

    // 内存池没有可用的内存对象，需要进行扩容
    if (unlikely(kmalloc_cache_group[index].count_total_free == 0))
    {
        // 创建slab_obj
        slab_obj_ptr = kmalloc_create_slab_obj(kmalloc_cache_group[index].size);

        // BUG
        if (unlikely(slab_obj_ptr == NULL))
        {
            kBUG("kmalloc()->kmalloc_create_slab_obj()=>slab == NULL");
            goto failed;
        }

        kmalloc_cache_group[index].count_total_free += slab_obj_ptr->count_free;
        list_add(&kmalloc_cache_group[index].cache_pool_entry->list, &slab_obj_ptr->list);
    }
    else // 内存对象充足
    {
        do
        {
            // 跳转到下一个内存池对象
            if (slab_obj_ptr->count_free == 0)
                slab_obj_ptr = container_of(list_next(&slab_obj_ptr->list), struct slab_obj, list);
            else
                break;
        } while (slab_obj_ptr != kmalloc_cache_group[index].cache_pool_entry);
    }
    // 寻找一块可用的内存对象
    int md;
    for (int i = 0; i < slab_obj_ptr->bmp_count; ++i)
    {

        // 当前bmp全部被使用
        if (*(slab_obj_ptr->bmp + (i >> 6)) == 0xffffffffffffffffUL)
        {
            i += 63;
            continue;
        }
        md = i % 64;
        // 找到相应的内存对象
        if ((*(slab_obj_ptr->bmp + (i >> 6)) & (1UL << md)) == 0)
        {
            *(slab_obj_ptr->bmp + (i >> 6)) |= (1UL << md);
            ++(slab_obj_ptr->count_using);
            --(slab_obj_ptr->count_free);

            --kmalloc_cache_group[index].count_total_free;
            ++kmalloc_cache_group[index].count_total_using;
            // 放锁
            spin_unlock(&kmalloc_cache_group[index].lock);
            // 返回内存对象
            result = (void *)((char *)slab_obj_ptr->vaddr + kmalloc_cache_group[index].size * i);
            goto done;
        }
    }
    goto failed;
done:;
    if (gfp & __GFP_ZERO)
        memset(result, 0, size);
    return result;
failed:;
    spin_unlock(&kmalloc_cache_group[index].lock);
    kerror("kmalloc(): Cannot alloc more memory: %d bytes", size);
    return NULL;
}

/**
 * @brief 通用内存释放函数
 *
 * @param address 要释放的内存线性地址
 * @return unsigned long
 */
unsigned long kfree(void *address)
{
    if (unlikely(address == NULL))
        return 0;
    struct slab_obj *slab_obj_ptr = NULL;

    // 将线性地址按照2M物理页对齐, 获得所在物理页的起始线性地址
    void *page_base_addr = (void *)((ul)address & PAGE_2M_MASK);

    int index;

    for (int i = 0; i < 16; ++i)
    {
        slab_obj_ptr = kmalloc_cache_group[i].cache_pool_entry;

        do
        {
            // 不属于当前slab_obj的管理范围
            if (likely(slab_obj_ptr->vaddr != page_base_addr))
            {
                slab_obj_ptr = container_of(list_next(&slab_obj_ptr->list), struct slab_obj, list);
            }
            else
            {
                // 对当前内存池加锁
                spin_lock(&kmalloc_cache_group[i].lock);
                // 计算地址属于哪一个内存对象
                index = (address - slab_obj_ptr->vaddr) / kmalloc_cache_group[i].size;

                // 复位bmp
                *(slab_obj_ptr->bmp + (index >> 6)) ^= 1UL << (index % 64);

                ++(slab_obj_ptr->count_free);
                --(slab_obj_ptr->count_using);
                ++kmalloc_cache_group[i].count_total_free;
                --kmalloc_cache_group[i].count_total_using;

                // 回收空闲的slab_obj
                // 条件：当前slab_obj_ptr的使用为0、总空闲内存对象>=当前slab_obj的总对象的2倍 且当前slab_pool不为起始slab_obj
                if ((slab_obj_ptr->count_using == 0) && (kmalloc_cache_group[i].count_total_free >= ((slab_obj_ptr->bmp_count) << 1)) && (kmalloc_cache_group[i].cache_pool_entry != slab_obj_ptr))
                {
                    switch (kmalloc_cache_group[i].size)
                    {
                    case 32:
                    case 64:
                    case 128:
                    case 256:
                    case 512:
                        // 在这种情况下，slab_obj是被安放在page内部的
                        list_del(&slab_obj_ptr->list);

                        kmalloc_cache_group[i].count_total_free -= slab_obj_ptr->bmp_count;
                        page_clean(slab_obj_ptr->page);
                        free_pages(slab_obj_ptr->page, 1);
                        break;

                    default:
                        // 在这种情况下，slab_obj是被安放在额外获取的内存对象中的
                        list_del(&slab_obj_ptr->list);
                        kmalloc_cache_group[i].count_total_free -= slab_obj_ptr->bmp_count;

                        kfree(slab_obj_ptr->bmp);

                        page_clean(slab_obj_ptr->page);
                        free_pages(slab_obj_ptr->page, 1);

                        kfree(slab_obj_ptr);
                        break;
                    }
                }
                // 放锁
                spin_unlock(&kmalloc_cache_group[i].lock);
                return 0;
            }

        } while (slab_obj_ptr != kmalloc_cache_group[i].cache_pool_entry);
    }
    kBUG("kfree(): Can't free memory. address=%#018lx", address);
    return ECANNOT_FREE_MEM;
}