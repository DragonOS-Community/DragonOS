
all: 
	@echo ""

CFLAGS += -I .

# 请注意，这个不能使用raw的gcc来编译。
kallsyms.o: kallsyms.c
	gcc -o kallsyms kallsyms.c
	rm -rf kallsyms.o

# 生成内核栈符号表的汇编文件
generate_kallsyms: kallsyms.o 
	echo "Generating kallsyms..."
# 请注意，这个不能使用raw的nm来处理
	nm -n -C $(kernel_root_path)/kernel | ./kallsyms > kallsyms.S
	$(CC) -c kallsyms.S -o kallsyms.o
	@echo "Kallsyms generated."


clean:
	rm -rf kallsyms