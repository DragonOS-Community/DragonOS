docker rm -f dragonos-build || echo "No existed container"
p=`pwd`
cpu_count=$(cat /proc/cpuinfo |grep "processor"|wc -l)
docker run --rm --privileged=true --cap-add SYS_ADMIN --cap-add MKNOD -v $p:/data -v /dev:/dev -v dragonos-build-cargo:/root/.cargo/registry --name dragonos-build -i dragonos/dragonos-dev:v1.1.0-beta3 bash << EOF
source ~/.cargo/env
cd /data
# Change rust src
bash tools/change_rust_src.sh
make all -j $cpu_count && make write_diskimage
EOF