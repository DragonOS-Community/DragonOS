all: about.o

	ld -b elf64-x86-64 -z muldefs -o $(tmp_output_dir)/about  $(shell find . -name "*.o") $(shell find $(sys_libs_dir) -name "*.o") -T about.lds

	objcopy -I elf64-x86-64 -R ".eh_frame" -R ".comment" -O elf64-x86-64 $(tmp_output_dir)/about $(output_dir)/about.elf
about.o: about.c
	$(CC) $(CFLAGS) -c about.c  -o about.o
