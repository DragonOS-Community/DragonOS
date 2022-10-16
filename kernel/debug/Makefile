
all: traceback.o

CFLAGS += -I .

kallsyms.o: kallsyms.c
	$(CC) -o kallsyms kallsyms.c 
	rm -rf kallsyms.o

traceback.o: traceback/traceback.c
	$(CC) $(CFLAGS) -c traceback/traceback.c -o traceback/traceback.o


# 生成内核栈符号表的汇编文件
generate_kallsyms: kallsyms.o 
	echo "Generating kallsyms..."
	
	nm -n $(kernel_root_path)/kernel | ./kallsyms > kallsyms.S
	$(CC) -c kallsyms.S -o kallsyms.o
	@echo "Kallsyms generated."


clean:
	rm -rf kallsyms