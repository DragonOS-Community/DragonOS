all: shell.o cmd.o cmd_help.o cmd_test.o

	$(LD) -b elf64-x86-64 -z muldefs -o $(tmp_output_dir)/shell  $(shell find . -name "*.o") $(ROOT_PATH)/bin/sysroot/usr/lib/libc.a -T shell.lds

	$(OBJCOPY) -I elf64-x86-64 -R ".eh_frame" -R ".comment" -O elf64-x86-64 $(tmp_output_dir)/shell $(output_dir)/shell.elf
shell.o: shell.c
	$(CC) $(CFLAGS) -c shell.c  -o shell.o

cmd.o: cmd.c
	$(CC) $(CFLAGS) -c cmd.c  -o cmd.o

cmd_test.o: cmd_test.c
	$(CC) $(CFLAGS) -c cmd_test.c  -o cmd_test.o

cmd_help.o: cmd_help.c
	$(CC) $(CFLAGS) -c cmd_help.c  -o cmd_help.o
