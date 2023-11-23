#include <arch/arch.h>
#if ARCH(I386) || ARCH(X86_64)

#include <common/idr.h>
#include <mm/slab.h>
/**
 * @brief 更换两个idr_layer指针
 *
 * @param a
 * @param b
 */
static void __swap(struct idr_layer **a, struct idr_layer **b)
{
    struct idr_layer *t = *a;
    *a = *b, *b = t;
}

/**
 * @brief 初始化idr - 你需要保证函数调用之前 free_list指针 为空
 *
 * @param idp
 */
void idr_init(struct idr *idp)
{
    memset(idp, 0, sizeof(struct idr));
    spin_init(&idp->lock);
}

/**
 * @brief 向idr的free_list中添加一个节点(空节点)
 *
 * @param idp
 * @param p
 */
static void __move_to_free_list(struct idr *idp, struct idr_layer *p)
{
    unsigned long flags;
    spin_lock_irqsave(&idp->lock, flags);

    // 插入free_list
    p->ary[0] = idp->free_list;
    io_sfence();
    idp->free_list = p;
    io_sfence();
    ++(idp->id_free_cnt);

    spin_unlock_irqrestore(&idp->lock, flags);
}

/**
 * @brief Get the free_idr_layer from free list object
 *
 * @param idp
 * @return void*
 */
static void *__get_from_free_list(struct idr *idp)
{
    if (idp->id_free_cnt == 0)
    {
        if (idr_preload(idp, 0) != 0)
        {
            kBUG("idr-module find a BUG: get free node fail.(Possible ENOMEM error)");
            return NULL;
        }
    }

    unsigned long flags;
    spin_lock_irqsave(&idp->lock, flags);

    // free_list还有节点
    struct idr_layer *item = idp->free_list;

    if (item == NULL)
    {
        BUG_ON(1);
    }

    io_sfence();
    idp->free_list = idp->free_list->ary[0];
    io_sfence();
    item->ary[0] = NULL; // 记得清空原来的数据
    io_sfence();
    --(idp->id_free_cnt);

    spin_unlock_irqrestore(&idp->lock, flags);

    return item;
}

/**
 * @brief 为idr预分配空间
 *
 * @param idp
 * @param gfp_mask
 * @return int (如果分配成功,将返回0; 否则返回负数 -ENOMEM, 有可能是内存空间不够)
 */
int idr_preload(struct idr *idp, gfp_t gfp_mask)
{
    int timer = 0;
    while (idp->id_free_cnt < IDR_FREE_MAX)
    {
        struct idr_layer *new_one;
        new_one = kzalloc(sizeof(struct idr_layer), gfp_mask); // 默认清空?
        if (unlikely(new_one == NULL))
            return -ENOMEM;

        __move_to_free_list(idp, new_one);
        timer++;
    }
    return 0;
}

/**
 * @brief 释放一个layer的空间
 *
 * @param p
 */
static void __idr_layer_free(struct idr_layer *p)
{
    kfree(p);
}

/**
 * @brief 向上生长一层idr_layer
 *
 * @param idp
 * @return int (0生长成功, 否则返回错误码)
 */
static int __idr_grow(struct idr *idp)
{
    struct idr_layer *new_node = __get_from_free_list(idp);
    if (NULL == new_node)
        return -ENOMEM;

    __swap(&new_node, &idp->top);

    idp->top->ary[0] = new_node;
    idp->top->layer = new_node ? (new_node->layer + 1) : 0; // 注意特判空指针
    idp->top->bitmap = 0;
    idp->top->full = 0; // clear

    if (new_node != NULL) // 设置第0位 = 1, 同时维护树的大小
    {
        idp->top->bitmap = 1;
    }
    if (new_node != NULL && new_node->full == IDR_FULL)
    {
        idp->top->full = 1; // 别忘了初始化 full
    }

    return 0;
}

/**
 * @brief 获取一个没有被占领的ID
 *
 * @param idp
 * @param stk  栈空间
 * @return int (负数表示获取ID失败, [0 <= id && id <= INT_MAX] 则获取ID成功)
 */
static int __idr_get_empty_slot(struct idr *idp, struct idr_layer **stk)
{
    // 注意特判 idp->top == NULL
    while (NULL == idp->top || idp->top->full == IDR_FULL)
        if (__idr_grow(idp) != 0)
            return -ENOMEM;

    int64_t id = 0;
    int layer = idp->top->layer;
    BUG_ON(layer + 1 >= 7);
    stk[layer + 1] = NULL; // 标志为数组末尾

    struct idr_layer *cur_layer = idp->top;
    while (layer >= 0)
    {
        stk[layer] = cur_layer;
        int pos = __lowbit_id(~cur_layer->full);

        if (unlikely(pos < 0))
        {
            kBUG("Value 'cur_layer->full' had been full;"
                 "but __idr_get_empty_slot still try to insert a value.");
        }

        id = (id << IDR_BITS) | pos;
        cur_layer = cur_layer->ary[pos];

        if (layer > 0 && NULL == cur_layer) // 只有非叶子节点才需要开辟儿子节点
        {
            // 初始化儿子节点
            cur_layer = __get_from_free_list(idp);
            if (NULL == cur_layer)
                return -ENOMEM;
            cur_layer->layer = layer - 1; // 儿子节点的layer
            cur_layer->full = 0;
            cur_layer->bitmap = 0;

            stk[layer]->ary[pos] = cur_layer; // 最后别忘了记录儿子节点
        }

        --layer;
    }

    return id;
}

/**
 * @brief 更新full对象 (辅助函数，内部没有边界特判)
 *
 * @param idp
 * @param id
 * @param stk  需要保证stk数组末尾是NULL
 * @param mark 0代表叶子空, 1代表叶子非空但未满, 2代表满
 */
static __always_inline void __idr_mark_full(struct idr *idp, int id, struct idr_layer **stk, int mark)
{
    int64_t __id = (int64_t)id;
    if (unlikely(NULL == stk[0] || NULL == idp->top))
    {
        kBUG("idr-module find a BUG: idp->top can't be NULL.");
        return;
    }

    // 处理叶子节点的full/bitmap标记
    int64_t layer_id = __id & IDR_MASK;
    if (mark == 2)
        stk[0]->full |= (1ull << layer_id);
    if (mark >= 1)
        stk[0]->bitmap |= (1ull << layer_id);

    for (int i = 1; stk[i]; ++i)
    {
        __id >>= IDR_BITS;
        layer_id = __id & IDR_MASK;

        stk[i]->bitmap |= (1ull << layer_id);
        if (stk[i - 1]->full == IDR_FULL)
            stk[i]->full |= (1ull << layer_id);
    }
}

/**
 * @brief 提取一条已存在的路径
 *
 * @param idp
 * @param id
 * @param stk
 * @return int (0表示没有这条路径, 1表示找到这条路径)
 */
static __always_inline int __idr_get_path(struct idr *idp, int id, struct idr_layer **stk)
{
    int64_t __id = (int64_t)id;
    if (unlikely(idp->top == NULL || __id < 0))
    {
        kBUG("idr-module find a BUG: idp->top can't be NULL and id must be non-negative.");
        return 0;
    }

    struct idr_layer *cur_layer = idp->top;
    int layer = cur_layer->layer;
    stk[layer + 1] = NULL; // 标志数组结尾

    if (unlikely((__id >> ((layer + 1ull) * IDR_BITS)) > 0))
    {
        kBUG("idr-module find a BUG: id is invalid.");
        return 0;
    }

    // 提取路径
    while (layer >= 0)
    {
        stk[layer] = cur_layer;
        int64_t layer_id = (__id >> (layer * IDR_BITS)) & IDR_MASK;

        if (unlikely(((cur_layer->bitmap >> layer_id) & 1) == 0))
        {
            kBUG("idr-module find a BUG: no-such son.");
            return 0; // 没有这一个儿子
        }

        cur_layer = cur_layer->ary[layer_id];
        --layer;
    }

    return 1;
}

/**
 * @brief 更新full对象 (辅助函数，内部没有边界特判)
 *
 * @param idp
 * @param id
 * @param stk 需要保证stk数组末尾是NULL
 * @param mark 0代表叶子空, 1代表叶子非空但未满, 2代表满
 */
static __always_inline void __idr_erase_full(struct idr *idp, int id, struct idr_layer **stk, int mark)
{
    int64_t __id = (int64_t)id;
    if (unlikely(NULL == stk[0] || NULL == idp->top))
    {
        kBUG("idr-module find a BUG: idp->top can't be NULL.");
        return;
    }

    // 处理叶子节点的full/bitmap标记
    int64_t layer_id = __id & IDR_MASK;
    if (mark == 0) // 叶子的某个插槽为空
    {
        stk[0]->ary[layer_id] = NULL;
        stk[0]->bitmap ^= (1ull << layer_id);
    }
    if (mark != 2 && ((stk[0]->full >> layer_id) & 1))
        stk[0]->full ^= (1ull << layer_id);

    // 删除节点
    for (int layer = 1; stk[layer]; ++layer)
    {
        __id >>= IDR_BITS;
        layer_id = __id & IDR_MASK;

        if (NULL == stk[layer - 1]->bitmap) // 儿子是空节点
        {
            stk[layer]->ary[layer_id] = NULL;
            stk[layer]->bitmap ^= (1ull << layer_id);

            if ((stk[layer]->full >> layer_id) & 1)
                stk[layer]->full ^= (1ull << layer_id);

            __idr_layer_free(stk[layer - 1]);
            stk[layer - 1] = NULL; // 释放空间记得设置为 NULL
        }
        else if (stk[layer - 1]->full != IDR_FULL)
        {
            if ((stk[layer]->full >> layer_id) & 1)
                stk[layer]->full ^= (1ull << layer_id);
        }
    }

    // 特判根节点是否只剩0号儿子节点 (注意还要layer > 0)
    // (注意,有可能出现idp->top=NULL)
    // bitmap: 1000...000/00.....000
    while (idp->top != NULL && ((idp->top->bitmap <= 1 && idp->top->layer > 0) || // 一条链的情况
                                (idp->top->layer == 0 && idp->top->bitmap == 0))) // 最后一个点的情况
    {
        struct idr_layer *t = idp->top->layer ? idp->top->ary[0] : NULL;
        __idr_layer_free(idp->top);
        idp->top = t;
    }
}

/**
 * @brief 内部的分配ID函数 (辅助函数)
 *
 * @param idp
 * @param ptr
 * @param starting_id 暂时没用
 * @return (0 <= id <= INT_MAX 表示申请的ID；否则是负数错误码, 可能是内存空间不够或者程序逻辑有误)；
 */
static int __idr_get_new_above_int(struct idr *idp, void *ptr, int starting_id)
{
    struct idr_layer *stk[MAX_LEVEL + 1] = {0};

    // kdebug("stk=%#018lx, sizeof_stk=%d", stk, sizeof(stk));
    // memset(stk, 0, sizeof(stk));
    // 你可以选择 memset(stk, 0, sizeof(stk));
    int64_t id = __idr_get_empty_slot(idp, stk);

    if (id >= 0)
    {
        stk[0]->ary[IDR_MASK & id] = ptr;
        __idr_mark_full(idp, id, stk, 2);
    }

    return id;
}

/**
 * @brief 从[0,INT_MAX]区间内返回一个最小的空闲ID
 *
 * @param idp
 * @param ptr     - id 所对应的指针
 * @param int* id - 传入int指针，获取到的NEW_ID存在id里
 * @return int (0表示获取id成功, 负数代表错误 - 可能是内存空间不够)
 */
int idr_alloc(struct idr *idp, void *ptr, int *id)
{
    int rv = __idr_get_new_above_int(idp, ptr, 0);
    if (rv < 0)
        return rv; // error
    *id = rv;
    return 0;
}

/**
 * @brief 删除一个id, 但是不释放对应的ptr指向的空间, 同时返回这个被删除id所对应的ptr
 *
 * @param idp
 * @param id
 * @return void*
 * (如果删除成功，就返回被删除id所对应的ptr；否则返回NULL。注意：如果这个id本来就和NULL绑定，那么也会返回NULL)
 */
void *idr_remove(struct idr *idp, int id)
{
    int64_t __id = (int64_t)id;
    if (unlikely(idp->top == NULL || __id < 0))
        return NULL;

    struct idr_layer *stk[MAX_LEVEL + 1] = {0};

    if (0 == __idr_get_path(idp, __id, stk))
        return NULL; // 找不到路径

    void *ret = stk[0]->ary[__id & IDR_MASK];
    __idr_erase_full(idp, __id, stk, 0);

    return ret;
}

/**
 * @brief 移除IDR中所有的节点,如果free=true,则同时释放所有数据指针的空间(kfree)
 *
 * @param idp
 * @param free
 */
static void __idr_remove_all_with_free(struct idr *idp, bool free)
{
    if (unlikely(NULL == idp->top))
    {
        kBUG("idr-module find a BUG: idp->top can't be NULL.");
        return;
    }

    int sz = sizeof(struct idr_layer);
    struct idr_layer *stk[MAX_LEVEL + 1] = {0};

    struct idr_layer *cur_layer = idp->top;
    int layer = cur_layer->layer;
    BUG_ON(layer + 1 >= 7);
    stk[layer + 1] = NULL; // 标记数组结尾

    while (cur_layer != NULL)
    {
        if (layer > 0 && cur_layer->bitmap) // 非叶子节点
        {
            stk[layer] = cur_layer; // 入栈
            int64_t id = __lowbit_id(cur_layer->bitmap);

            cur_layer->bitmap ^= (1ull << id);
            cur_layer = cur_layer->ary[id];
            stk[layer]->ary[id] = NULL;
            --layer;
        }
        else
        {
            if (free)
            {
                for (int i = 0; i < IDR_SIZE; i++) // 释放数据指针的空间
                {
                    kfree(cur_layer->ary[i]);
                    cur_layer->ary[i] = NULL;
                }
            }

            __idr_layer_free(cur_layer); //  释放空间记得设置为NULL
            ++layer;

            cur_layer = stk[layer]; // 出栈
        }
    }
    idp->top = NULL;
}

/**
 * @brief 删除idr的所有节点，同时释放数据指针的空间，回收free_list的所有空间 - (数据指针指ID所绑定的pointer)
 * @param idp
 */
static void __idr_destroy_with_free(struct idr *idp)
{
    if (likely(idp->top))
        __idr_remove_all_with_free(idp, 1);
    idp->top = NULL;
    while (idp->id_free_cnt)
        __idr_layer_free(__get_from_free_list(idp));
    idp->free_list = NULL;
}

/**
 * @brief 删除所有的ID
 *
 * @param idp
 */
void idr_remove_all(struct idr *idp)
{
    if (unlikely(NULL == idp->top))
        return;

    __idr_remove_all_with_free(idp, 0);
}

/**
 * @brief 释放一个idr占用的所有空间
 *
 * @param idp
 */
void idr_destroy(struct idr *idp)
{
    idr_remove_all(idp);
    idp->top = NULL;
    while (idp->id_free_cnt)
        __idr_layer_free(__get_from_free_list(idp));
    idp->free_list = NULL;
}

/**
 * @brief 返回id对应的数据指针
 *
 * @param idp
 * @param id
 * @return void* (如果id不存在返回NULL；否则返回对应的指针ptr; 注意: 有可能用户的数据本来就是NULL)
 */
void *idr_find(struct idr *idp, int id)
{
    int64_t __id = (int64_t)id;
    if (unlikely(idp->top == NULL || __id < 0))
    {
        // kwarn("idr-find: idp->top == NULL || id < 0.");
        return NULL;
    }

    struct idr_layer *cur_layer = idp->top;
    int layer = cur_layer->layer; // 特判NULL
    barrier();
    // 如果查询的ID的bit数量比layer*IDR_BITS还大, 直接返回NULL
    if ((__id >> ((layer + 1) * IDR_BITS)) > 0)
        return NULL;
    barrier();
    barrier();
    int64_t layer_id = 0;
    while (layer >= 0 && cur_layer != NULL)
    {
        barrier();
        layer_id = (__id >> (IDR_BITS * layer)) & IDR_MASK;
        barrier();
        cur_layer = cur_layer->ary[layer_id];
        --layer;
    }
    return cur_layer;
}

/**
 * @brief  返回id大于 start_id 的数据指针(即非空闲id对应的指针), 如果没有则返回NULL; 可以传入nextid指针，获取下一个id;
 * 时间复杂度O(log_64(n)), 空间复杂度O(log_64(n)) 约为 6;
 *
 * @param idp
 * @param start_id
 * @param nextid
 * @return void* (如果分配,将返回该ID对应的数据指针; 否则返回NULL。注意，
 * 返回NULL不一定代表这ID不存在，有可能该ID就是与空指针绑定。)
 */
void *idr_find_next_getid(struct idr *idp, int64_t start_id, int *nextid)
{
    BUG_ON(nextid == NULL);
    if (unlikely(idp->top == NULL))
    {
        *nextid = -1;
        return NULL;
    }

    ++start_id;
    start_id = max(0, start_id); // 特判负数
    *nextid = 0;

    struct idr_layer *stk[MAX_LEVEL + 1] = {0};

    // memset(stk, 0, sizeof(struct idr_layer *) * (MAX_LEVEL + 1));
    bool state[MAX_LEVEL + 1] = {0}; // 标记是否大于等于]
    int pos_i[MAX_LEVEL + 1] = {0};

    // memset(state, 0, sizeof(state));
    // memset(pos_i, 0, sizeof(pos_i)); // 必须清空

    struct idr_layer *cur_layer = idp->top;
    bool cur_state = false;
    bool init_flag = true;
    int layer = cur_layer->layer;
    BUG_ON(layer + 1 >= 7);
    stk[layer + 1] = NULL; // 标记数组结尾

    // 如果查询的ID的bit数量比layer*IDR_BITS还大, 直接返回NULL
    if ((start_id >> ((layer + 1) * IDR_BITS)) > 0)
    {
        *nextid = -1;
        return NULL;
    }

    while (cur_layer) // layer < top->layer + 1
    {
        BUG_ON(layer < 0);
        if (init_flag) // 第一次入栈
        {
            stk[layer] = cur_layer;
            state[layer] = cur_state;
            pos_i[layer] = cur_state ? 0 : ((start_id >> (layer * IDR_BITS)) & IDR_MASK);
        }
        else
        {
            pos_i[layer]++;
            state[layer] = cur_state = true;
        }

        BUG_ON(pos_i[layer] >= 64);
        unsigned long t_bitmap = (cur_layer->bitmap >> pos_i[layer]);
        if (t_bitmap) // 进一步递归到儿子下面去
        {
            int64_t layer_id = __lowbit_id(t_bitmap) + pos_i[layer];

            // 特别情况
            if ((cur_state == false) && (layer_id > pos_i[layer] > 0))
                cur_state = true;

            pos_i[layer] = layer_id;

            *nextid = (((uint64_t)*nextid) << IDR_BITS) | layer_id; // 更新答案
            if (layer == 0)
            {
                //  找到下一个id: nextid
                return cur_layer->ary[layer_id];
            }

            cur_layer = cur_layer->ary[layer_id];
            init_flag = true; // 儿子节点第一次入栈, 需要init
            --layer;
        }
        else // 子树搜索完毕,向上回溯
        {
            (*nextid) >>= IDR_BITS; // 维护答案

            ++layer;
            cur_layer = stk[layer];
            init_flag = false; // 不是第一次入栈, 不需要init
        }
    }

    *nextid = -1;
    return NULL; // 找不到
}

/**
 * @brief 返回id大于 start_id 的数据指针(即非空闲id对应的指针), 如果没有则返回NULL
 *
 * @param idp
 * @param start_id
 * @return void* (如果分配,将返回该ID对应的数据指针; 否则返回NULL。注意，
 * 返回NULL不一定代表这ID不存在，有可能该ID就是与空指针绑定。)
 */
void *idr_find_next(struct idr *idp, int start_id)
{
    int nextid;
    void *ptr = idr_find_next_getid(idp, start_id, &nextid);

    return ptr; // 当 nextid == -1 时， 出现错误
}

/**
 * @brief 根据id替换指针，你需要保证这个id存在于idr中，否则将会出现错误
 *
 * @param idp
 * @param ptr (要替换旧指针的新指针 - new_ptr)
 * @param id
 * @param old_ptr (返回旧指针, 注意NULL不一定是出现错误，有可能是数据本来就是NULL)
 * @return int (0代表成功，否则就是负数 - 代表错误)
 */
int idr_replace_get_old(struct idr *idp, void *ptr, int id, void **old_ptr)
{
    int64_t __id = (int64_t)id;
    if (unlikely(old_ptr == NULL))
    {
        BUG_ON(1);
        return -EINVAL;
    }
    *old_ptr = NULL;

    if (unlikely(idp->top == NULL || __id < 0))
        return -EDOM; // 参数错误

    struct idr_layer *cur_layer = idp->top;
    int64_t layer = cur_layer->layer;
    // 如果查询的ID的bit数量比layer*IDR_BITS还大, 直接返回NULL
    if ((__id >> ((layer + 1) * IDR_BITS)) > 0)
        return -EDOM;

    while (layer > 0)
    {
        int64_t layer_id = (__id >> (layer * IDR_BITS)) & IDR_MASK;

        if (unlikely(NULL == cur_layer->ary[layer_id]))
            return -ENOMEM;

        cur_layer = cur_layer->ary[layer_id];
        layer--;
    }

    __id &= IDR_MASK;
    *old_ptr = cur_layer->ary[__id];
    cur_layer->ary[__id] = ptr;

    return 0;
}

/**
 * @brief 根据id替换指针，你需要保证这个id存在于idr中，否则将会出现错误
 *
 * @param idp
 * @param ptr (要替换 '旧数据指针' 的 '新数据指针' - new_ptr)
 * @param id
 * @return int (0代表成功，否则就是错误码 - 代表错误)
 */
int idr_replace(struct idr *idp, void *ptr, int id)
{
    int64_t __id = (int64_t)id;
    if (__id < 0)
        return -EDOM;

    void *old_ptr;
    int flags = idr_replace_get_old(idp, ptr, __id, &old_ptr);

    return flags;
}

/**
 * @brief 判断一个idr是否为空
 *
 * @param idp
 * @return true
 * @return false
 */
bool idr_empty(struct idr *idp)
{
    if (idp == NULL || idp->top == NULL || !idp->top->bitmap)
        return true;

    return false;
}

static bool __idr_cnt_pd(struct idr_layer *cur_layer, int layer_id)
{
    // if(layer_id)
    unsigned long flags = ((cur_layer->bitmap) >> layer_id);
    if ((flags % 2) == 0)
    {
        barrier();
        return false; // 没有这一个儿子
    }
    return true;
}

static bool __idr_cnt(int layer, int id, struct idr_layer *cur_layer)
{
    int64_t __id = (int64_t)id;
    while (layer >= 0) // 提取路径
    {
        barrier();

        int64_t layer_id = (__id >> (layer * IDR_BITS)) & IDR_MASK;

        barrier();

        if (__idr_cnt_pd(cur_layer, layer_id) == false)
            return false;

        barrier();

        barrier();
        cur_layer = cur_layer->ary[layer_id];

        barrier();
        --layer;
    }
    return true;
}

/**
 * @brief 这个函数是可以用于判断一个ID是否已经被分配的。
 *
 * @param idp
 * @param id
 * @return true
 * @return false
 */
bool idr_count(struct idr *idp, int id)
{
    int64_t __id = (int64_t)id;
    barrier();
    if (unlikely(idp == NULL || idp->top == NULL || __id < 0))
        return false;

    barrier();
    struct idr_layer *cur_layer = idp->top;
    barrier();
    int layer = cur_layer->layer;

    // 如果查询的ID的bit数量比 layer*IDR_BITS 还大, 直接返回false
    if (unlikely((__id >> ((layer + 1ull) * IDR_BITS)) > 0))
    {
        BUG_ON(1);
        return false;
    }
    barrier();

    return __idr_cnt(layer, id, cur_layer);
}

/********* ****************************************** ida - idr 函数实现分割线
 * **********************************************************/

/**
 * @brief 初始化IDA, 你需要保证调用函数之前, ida的free_list为空, 否则会导致内存泄漏
 * @param ida_p
 */
void ida_init(struct ida *ida_p)
{
    memset(ida_p, 0, sizeof(struct ida));
    idr_init(&ida_p->idr);
}

/**
 * @brief 释放bitmap空间
 *
 */
static void __ida_bitmap_free(struct ida_bitmap *bitmap)
{
    kfree(bitmap);
}

/**
 * @brief 为ida预分配空间
 *
 * @param ida_p
 * @param gfp_mask
 * @return int (如果分配成功,将返回0; 否则返回负数错误码, 有可能是内存空间不够)
 */
int ida_preload(struct ida *ida_p, gfp_t gfp_mask)
{
    if (idr_preload(&ida_p->idr, gfp_mask) != 0)
        return -ENOMEM;

    spin_lock(&ida_p->idr.lock);

    if (NULL == ida_p->free_list)
    {
        struct ida_bitmap *bitmap;
        bitmap = kzalloc(sizeof(struct ida_bitmap), gfp_mask);
        if (NULL == bitmap)
        {
            spin_unlock(&ida_p->idr.lock);
            return -ENOMEM;
        }
        ida_p->free_list = bitmap;
    }

    spin_unlock(&ida_p->idr.lock);
    return 0;
}

/**
 * @brief Get the ida bitmap object
 *
 * @param ida_p
 * @return void*
 */
static void *__get_ida_bitmap(struct ida *ida_p, gfp_t gfp_mask)
{
    if (NULL == ida_p->free_list)
        if (ida_preload(ida_p, gfp_mask) < 0)
        {
            kBUG("error : no memory.");
            return NULL;
        }

    struct ida_bitmap *tmp = ida_p->free_list;
    ida_p->free_list = NULL;
    return tmp;
}

/**
 * @brief 从bitmap中获取id， 并且标记这个ID已经被使用
 * @return int
 */
static int __get_id_from_bitmap(struct ida_bitmap *bmp)
{
    int ret = 0;
    for (int ary_id = 0; ary_id < IDA_BITMAP_LONGS; ary_id++)
    {
        if (bmp->bitmap[ary_id] != IDR_FULL)
        {
            int bmp_id = __lowbit_id(~bmp->bitmap[ary_id]);
            bmp->bitmap[ary_id] |= (1ull << bmp_id);
            bmp->count++; // 注意， 这里已经标记这一位已经使用， 同时更新了ida_count

            if (unlikely((unsigned long long)ary_id * IDA_BMP_SIZE + bmp_id > INT32_MAX))
            {
                BUG_ON(1);
                // kBUG("ida设置id范围为[0, INT32_MAX], 但ida获取的id数值超过INT32_MAX.");
                return -EDOM;
            }

            return ary_id * IDA_BMP_SIZE + bmp_id;
        }
    }

    return -EDOM; // 不合法
}

/**
 * @brief 获取一个ID
 *
 * @param ida_p
 * @param p_id
 * @return int (0表示获取ID成功， 否则是负数 - 错误码)
 */
int ida_alloc(struct ida *ida_p, int *p_id)
{
    BUG_ON(p_id == NULL);
    *p_id = -1;

    struct idr_layer *stk[MAX_LEVEL + 1] = {0}; // 你可以选择memset(0)

    // memset(stk, 0, sizeof(struct idr_layer *) * (MAX_LEVEL + 1));

    io_sfence();
    int64_t idr_id = __idr_get_empty_slot(&ida_p->idr, stk);

    // 如果stk[0]=NULL,可能是idr内部出错/内存空间不够
    if (unlikely(NULL == stk[0]))
        return -ENOMEM;

    if (unlikely(idr_id < 0))
        return idr_id;

    int64_t layer_id = idr_id & IDR_MASK;

    if (NULL == stk[0]->ary[layer_id])
        stk[0]->ary[layer_id] = __get_ida_bitmap(ida_p, 0);

    if (unlikely(NULL == stk[0]->ary[layer_id]))
        return -ENOMEM;

    struct ida_bitmap *bmp = (struct ida_bitmap *)stk[0]->ary[layer_id];
    int low_id = __get_id_from_bitmap(bmp);

    if (unlikely(low_id < 0))
        return low_id;

    *p_id = idr_id * IDA_BITMAP_BITS + low_id;
    __idr_mark_full(&ida_p->idr, idr_id, stk, (bmp->count == IDA_FULL ? 2 : 1));

    return 0;
}

/**
 * @brief 查询ID是否已经被分配
 *
 * @param ida_p
 * @param id
 * @return true
 * @return false
 */
bool ida_count(struct ida *ida_p, int id)
{
    int64_t __id = (int64_t)id;
    if (unlikely(NULL == ida_p || NULL == ida_p->idr.top || id < 0))
        return false;

    int idr_id = __id / IDA_BITMAP_BITS;
    int ary_id = (__id % IDA_BITMAP_BITS) / IDA_BMP_SIZE;
    int bmp_id = (__id % IDA_BITMAP_BITS) % IDA_BMP_SIZE;

    struct ida_bitmap *bmp = idr_find(&ida_p->idr, idr_id);
    if (NULL == bmp)
        return false;

    return ((bmp->bitmap[ary_id] >> bmp_id) & 1);
}

/**
 * @brief 删除一个ID
 *
 * @param ida_p
 * @param id
 */
void ida_remove(struct ida *ida_p, int id)
{
    int64_t __id = (int64_t)id;
    if (unlikely(NULL == ida_p || NULL == ida_p->idr.top || id < 0))
        return;

    int64_t idr_id = __id / IDA_BITMAP_BITS;
    int64_t ary_id = (__id % IDA_BITMAP_BITS) / IDA_BMP_SIZE;
    int64_t bmp_id = (__id % IDA_BITMAP_BITS) % IDA_BMP_SIZE;

    struct idr_layer *stk[MAX_LEVEL + 1] = {0};
    // memset(stk, 0, sizeof(struct idr_layer *) * (MAX_LEVEL + 1));

    if (0 == __idr_get_path(&ida_p->idr, idr_id, stk))
        return;

    struct ida_bitmap *b_p = (struct ida_bitmap *)(stk[0]->ary[idr_id & IDR_MASK]);

    // 不存在这个ID 或者 b_p == NULL
    if (unlikely(NULL == b_p || 0 == ((b_p->bitmap[ary_id] >> bmp_id) & 1)))
        return;

    b_p->count--; // 更新了ida_count
    b_p->bitmap[ary_id] ^= (1ull << bmp_id);

    __idr_erase_full(&ida_p->idr, idr_id, stk, (b_p->count > 0 ? 1 : 0));
    if (0 == b_p->count)
    {
        __ida_bitmap_free(b_p);
        if (stk[0])                                // stk[0] 有可能在 __idr_erase_full 里面已经kfree了
            stk[0]->ary[idr_id & IDR_MASK] = NULL; // 记得设置为空
    }
}

/**
 * @brief 释放所有空间(包括: idr + ida_bitmap + free_list)
 * @param ida_p
 */
void ida_destroy(struct ida *ida_p)
{
    if (unlikely(ida_p == NULL))
    {
        BUG_ON(1);
        return;
    }

    __idr_destroy_with_free(&ida_p->idr);
    ida_p->idr.top = NULL;
    __ida_bitmap_free(ida_p->free_list);
    ida_p->free_list = NULL;
}

/**
 * @brief 判断一个ida是否为空
 *
 * @param ida_p
 * @return true
 * @return false
 */
bool ida_empty(struct ida *ida_p)
{
    if (ida_p == NULL || ida_p->idr.top == NULL || !ida_p->idr.top->bitmap)
        return true;

    return false;
}

#endif