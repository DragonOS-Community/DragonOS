(_lockref)=
# lockref

&emsp;&emsp;lockref是将自旋锁与引用计数变量融合在连续、对齐的8字节内的一种技术。

## lockref结构

```c
struct lockref
{
    union
    {
#ifdef __LOCKREF_ENABLE_CMPXCHG__
        aligned_u64 lock_count; // 通过该变量的声明，使得整个lockref的地址按照8字节对齐
#endif
        struct
        {
            spinlock_t lock;
            int count;
        };
    };
};
```
## 特性描述
&emsp;&emsp;由于在高负载的情况下，系统会频繁的执行“锁定-改变引用变量-解锁”的操作，这期间很可能出现spinlock和引用计数跨缓存行的情况，这将会大大降低性能。lockref通过强制对齐，尽可能的降低缓存行的占用数量，使得性能得到提升。

&emsp;&emsp;并且，在x64体系结构下，还通过cmpxchg()指令，实现了无锁快速路径。不需要对自旋锁加锁即可更改引用计数的值，进一步提升性能。当快速路径不存在（对于未支持的体系结构）或者尝试超时后，将会退化成“锁定-改变引用变量-解锁”的操作。此时由于lockref强制对齐，只涉及到1个缓存行，因此性能比原先的spinlock+ref_count的模式要高。

## 关于cmpxchg_loop

&emsp;&emsp;在改变引用计数时，cmpxchg先确保没有别的线程持有锁，然后改变引用计数，同时通过`lock cmpxchg`指令验证在更改发生时，没有其他线程持有锁，并且当前的目标lockref的值与old变量中存储的一致，从而将新值存储到目标lockref。这种无锁操作能极大的提升性能。如果不符合上述条件，在多次尝试后，将退化成传统的加锁方式来更改引用计数。

## 参考资料

&emsp;&emsp;[Introducing lockrefs - LWN.net, Jonathan Corbet](https://lwn.net/Articles/565734/)
