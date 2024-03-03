sub_dirs = $(wildcard */)

.PHONY: clean
clean:
	@list='$(sub_dirs)'; for subdir in $$list; do \
		$(MAKE) -C $$subdir clean;\
	done

.PHONY: fmt
fmt:
	@list='$(sub_dirs)'; for subdir in $$list; do \
		FMT_CHECK=$(FMT_CHECK) $(MAKE) -C $$subdir fmt;\
	done
