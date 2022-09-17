docker rm -f dragonos-build || echo "No existed container"
p=`pwd`
cpu_count=$(cat /proc/cpuinfo |grep "processor"|wc -l)
docker run --rm --privileged=true --cap-add SYS_ADMIN --cap-add MKNOD -v $p:/data -v /dev:/dev --name dragonos-build -i dragonos/dragonos-dev:v1.0 bash << EOF
cd /data
bash run.sh --current_in_docker
EOF