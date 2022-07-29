# 内核测试框架

&emsp;&emsp;DragonOS提供了一个测试框架，旨在对内核的一些模块进行自动化测试。内核测试框架位于`ktest/`下。

&emsp;&emsp;我们可以使用这个测试框架，按照规范编写测试代码，然后在合适的地方使用`ktest_start()`创建一个全新的内核线程并发起测试。

## 使用方法

### 创建自动测试程序

&emsp;&emsp;假如您要对kfifo模块进行自动测试，您可以在`ktest/`下，创建一个名为`test-kfifo.c`的测试文件，并编写Makefile。

&emsp;&emsp;在`test-kfifo.c`中，包含`ktest_utils.h`和`ktest.h`这两个头文件。

&emsp;&emsp;您需要像下面这样，在`test-kfifo.c`中，创建一个测试用例函数表，并把测试用例函数填写到其中：
```c
static ktest_case_table kt_kfifo_func_table[] = {
    ktest_kfifo_case0_1,
};
```

&emsp;&emsp;然后创建一个函数，作为kfifo测试的主函数。请注意，您需要将它的声明添加到`ktest.h`中。

```c
uint64_t ktest_test_kfifo(uint64_t arg)
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
```


### 编写测试用例

&emsp;&emsp;您可以创建一个或多个测试用例，命名为：`ktest_kfifo_case_xxxxx`. 在这个例子中，我创建了一个测试用例，命名为：`ktest_kfifo_case0_1`.如下所示：

```c
static long ktest_kfifo_case0_1(uint64_t arg0, uint64_t arg1)
```

&emsp;&emsp;这里最多允许我们传递两个参数到测试函数里面。

&emsp;&emsp;那么，我们该如何编写测试用例呢？

&emsp;&emsp;我们主要是需要设置一些情节，以便能测试到目标组件的每个情况。为了检验模块的行为是否符合预期，我们需要使用`assert(condition)`宏函数，对目标`condition`进行校验。若`condition`为1，则表明测试通过。否则，将会输出一行assert failed信息到屏幕上。

### 发起测试

&emsp;&emsp;我们可以在pid≥1的内核线程中发起测试。由于DragonOS目前尚不完善，您可以在`process/process.c`中的`initial_kernel_thread()`函数内，发起内核自动测试。具体的代码如下：

```c
ktest_start(ktest_test_kfifo, 0);
```

&emsp;&emsp;这样就发起了一个内核测试，它会创建一个新的内核线程进行自动测试，您不必担心第一个内核线程会被阻塞。
&emsp;&emsp;

## API文档

### ktest_start

`pid_t ktest_start(uint64_t (*func)(uint64_t arg), uint64_t arg)`

#### 描述

&emsp;&emsp;开启一个新的内核线程以进行测试

#### 参数

**func**

&emsp;&emsp;测试函数. 新的测试线程将会执行该函数，以进行测试。

**arg**

&emsp;&emsp;传递给测试函数的参数

#### 返回值

&emsp;&emsp;测试线程的pid

### assert

`#define assert(condition)`

#### 描述

&emsp;&emsp;判定condition是否为1，若不为1，则输出一行错误日志信息：

```
[ kTEST FAILED ] Ktest Assertion Failed, file:%s, Line:%d
```

### kTEST

```#define kTEST(...) ```

#### 描述

&emsp;&emsp;格式化输出一行以`[ kTEST ] file:%s, Line:%d`开头的日志信息。


### ktest_case_table

`typedef long (*ktest_case_table)(uint64_t arg0, uint64_t arg1)`

#### 描述
&emsp;&emsp;ktest用例函数的类型定义。
