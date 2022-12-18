# 获得当前git提交的sha1,并截取前8位
GIT_COMMIT_SHA1=$(shell git log -n 1 | head -n 1 | cut -d ' ' -f 2 | cut -c1-8)

all: about.o

	$(LD) -b elf64-x86-64 -z muldefs -o $(tmp_output_dir)/about  $(shell find . -name "*.o") $(shell find $(sys_libs_dir) -name "*.o") -T about.lds

	objcopy -I elf64-x86-64 -R ".eh_frame" -R ".comment" -O elf64-x86-64 $(tmp_output_dir)/about $(output_dir)/about.elf

about.o: version_header about.c
	$(CC) $(CFLAGS) -c about.c  -o about.o

# 生成版本头文件sys_version.h
version_header: about.c
	@echo "#define DRAGONOS_GIT_COMMIT_SHA1 \"$(GIT_COMMIT_SHA1)\"" > sys_version.h
