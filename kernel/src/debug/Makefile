
all: 
	@echo ""

CFLAGS += -I .

gen_kallsyms: gen_kallsyms.rs
	rustc gen_kallsyms.rs

# 生成内核栈符号表的汇编文件
generate_kallsyms: gen_kallsyms
	@echo "Generating kallsyms..."
# 请注意，这个不能使用raw的nm来处理
	nm -n -C $(kernel_root_path)/kernel | ./gen_kallsyms > kallsyms.S
	$(CC) -c kallsyms.S -o kallsyms.o
	@echo "Kallsyms generated."


clean:
	rm -rf gen_kallsyms