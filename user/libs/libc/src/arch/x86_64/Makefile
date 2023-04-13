libc_arch_objs:= $(shell find ./*.c)

ECHO:
	@echo "$@"


$(libc_arch_objs): ECHO
	$(CC) $(CFLAGS) -c $@ -o $@.o

# 由于目前使用的是raw的gcc，所以不需要crti.o和crtn.o（待更换为x86_64-dragonos-gcc后再改这里）
# all: $(libc_arch_objs) crti.o crtn.o
all: $(libc_arch_objs)
	mv crt0.c.o crt0.o

# crti.o: crti.S
# 	$(CC) -E crti.S > _crti.s # 预处理
# 	$(AS) $(ASFLAGS) -o crti.o _crti.s

# crtn.o: crtn.S
# 	$(CC) -E crtn.S > _crtn.s # 预处理
# 	$(AS) $(ASFLAGS) -o crtn.o _crtn.s

clean:
	
	echo "Done."