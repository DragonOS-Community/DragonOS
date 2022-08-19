all: test.o

	ld -b elf64-x86-64 -z muldefs -o $(tmp_output_dir)/test  $(shell find . -name "*.o") $(shell find $(sys_libs_dir) -name "*.o") -T test.lds

	objcopy -I elf64-x86-64 -R ".eh_frame" -R ".comment" -O elf64-x86-64 $(tmp_output_dir)/test $(output_dir)/test.elf
test.o: test.c
	gcc $(CFLAGS) -c test.c  -o test.o
