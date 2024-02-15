sub_dirs = $(wildcard */)

.PHONY: clean
clean:
	@for subdir in $(sub_dirs); do \
		$(MAKE) -C $$subdir clean;\
	done