all: main.o

	ld -b elf64-x86-64 -z muldefs -o $(tmp_output_dir)/test_signal  $(shell find . -name "*.o") $(shell find $(sys_libs_dir) -name "*.o") -T link.lds

	objcopy -I elf64-x86-64 -R ".eh_frame" -R ".comment" -O elf64-x86-64 $(tmp_output_dir)/test_signal $(output_dir)/test_signal.elf
main.o: main.c
	$(CC) $(CFLAGS) -c main.c  -o main.o
