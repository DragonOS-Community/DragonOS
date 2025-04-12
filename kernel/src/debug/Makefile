
all: 
	@echo ""

CFLAGS += -I .

# 生成内核栈符号表的汇编文件
generate_kallsyms:
	@echo "Generating kallsyms..."
# 请注意，这个不能使用raw的nm来处理
	nm -n -C $(kernel_root_path)/kernel | $(kernel_root_path)/../../build-scripts/target/release/gen_kallsyms > kallsyms.S
	$(CC) -c kallsyms.S -o kallsyms.o
	@echo "Kallsyms generated."


clean:
	rm -rf gen_kallsyms