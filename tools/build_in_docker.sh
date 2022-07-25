docker rm -f dragonos-build
p=`pwd`
cpu_count=$(cat /proc/cpuinfo |grep "processor"|wc -l)
docker run -v $p:/data --name dragonos-build -i dragonos/dragonos-dev:v1.0 bash << EOF
cd /data
bash run.sh --current_in_docker
EOF