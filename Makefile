SUBDIRS = kernel

.PHONY: all
all:
	mkdir -p bin/kernel/
	@list='$(SUBDIRS)'; for subdir in $$list; do \
    		echo "make all in $$subdir";\
    		cd $$subdir;\
    		make all;\
    		cd ..;\
    done

.PHONY: clean
clean:
	@list='$(SUBDIRS)'; for subdir in $$list; do \
		echo "Clean in dir: $$subdir";\
		cd $$subdir && make clean;\
		cd .. ;\
	done

gdb:
	gdb -n -x tools/.gdbinit