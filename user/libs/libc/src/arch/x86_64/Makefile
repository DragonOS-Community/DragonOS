libc_arch_objs:= $(shell find ./*.c)

ECHO:
	@echo "$@"


$(libc_arch_objs): ECHO
	$(CC) $(CFLAGS) -c $@ -o $@.o

all: $(libc_arch_objs) crti.o crtn.o
	mv crt0.c.o crt0.o

crti.o: crti.S
	$(CC) -E crti.S > _crti.s # 预处理
	$(AS) $(ASFLAGS) -o crti.o _crti.s

crtn.o: crtn.S
	$(CC) -E crtn.S > _crtn.s # 预处理
	$(AS) $(ASFLAGS) -o crtn.o _crtn.s

clean:
	
	echo "Done."