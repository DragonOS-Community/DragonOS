OLD_LIBC_INSTALL_PATH=$(ROOT_PATH)/bin/sysroot/usr/old_libc

all: main.o

	$(LD) -b elf64-x86-64 -z muldefs -o $(tmp_output_dir)/test_kvm  $(shell find . -name "*.o") $(OLD_LIBC_INSTALL_PATH)/lib/libc.a -T link.lds

	$(OBJCOPY) -I elf64-x86-64 -R ".eh_frame" -R ".comment" -O elf64-x86-64 $(tmp_output_dir)/test_kvm $(output_dir)/test_kvm.elf
main.o: main.c
	$(CC) $(CFLAGS) -c main.c  -o main.o
