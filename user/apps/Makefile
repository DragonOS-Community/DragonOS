
user_apps_sub_dirs=shell about test

all:
	@list='$(user_apps_sub_dirs)'; for subdir in $$list; do \
    		echo "make all in $$subdir";\
    		cd $$subdir;\
    		$(MAKE) all CFLAGS="$(CFLAGS) -I $(shell pwd)";\
    		cd ..;\
	done
