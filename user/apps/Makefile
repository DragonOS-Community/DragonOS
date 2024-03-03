sub_dirs = $(wildcard */)

.PHONY: clean
clean:
	@for subdir in $(sub_dirs); do \
		$(MAKE) -C $$subdir clean;\
	done

fmt:
	@for subdir in $(sub_dirs); do \
		FMT_CHECK=$(FMT_CHECK) $(MAKE) -C $$subdir fmt;\
	done
