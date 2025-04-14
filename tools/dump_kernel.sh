# 该脚本用于将编译好的内核反编译，并输出到txt文本文件中
# 用于辅助内核调试。出错时可以通过该脚本反编译内核，找到出错的函数的汇编代码
echo "正在反汇编内核..."
objdump -D ../bin/kernel/kernel.elf > ../bin/kernel/kernel.txt
echo "成功反汇编内核到../bin/kernel.txt"