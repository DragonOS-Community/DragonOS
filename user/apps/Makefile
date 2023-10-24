
user_apps_sub_dirs=shell about test_kvm

ECHO:
	@echo "$@"

$(user_apps_sub_dirs): ECHO

	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" tmp_output_dir="$(tmp_output_dir)" output_dir="$(output_dir)" sys_libs_dir="$(sys_libs_dir)"

all: $(user_apps_sub_dirs)

.PHONY: clean
fmt:
	@echo "格式化代码: user/apps"
