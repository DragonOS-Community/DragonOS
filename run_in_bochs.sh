# ======检查是否以sudo运行=================
uid=`id -u`
if [ ! $uid == "0" ];then
  echo "请以sudo权限运行"
  exit
fi

# 第一个参数如果是--notbuild 那就不构建，直接运行
if [ ! "$1" == "--nobuild" ]; then
    echo "开始构建..."
    make all
    make clean
fi

# ==============检查文件是否齐全================

bins[0]=bin/bootloader/boot.bin
bins[1]=bin/bootloader/loader.bin
bins[2]=bin/boot.img
bins[3]=bin/kernel/kernel.bin

for file in ${bins[*]};do
if [ ! -x $file ]; then
  echo "$file 不存在！"
  exit
  fi
done

# ===============文件检查完毕===================


# =========将引导程序写入boot.img=============
dd if=bin/bootloader/boot.bin of=bin/boot.img bs=512 count=1 conv=notrunc

# =========创建临时文件夹==================
# 判断临时文件夹是否存在，若不存在则创建新的
if [ ! -d "tmp/" ]; then
  mkdir tmp/
  echo "创建了tmp文件夹"
fi

# ==============挂载boot.img=============
  mkdir tmp/boot
  mount bin/boot.img tmp/boot -t vfat -o loop

  # 检查是否挂载成功
  if  mountpoint -q tmp/boot
   then
      echo "成功挂载 boot.img 到 tmp/boot"
      # ========把loader.bin复制到boot.img==========
      cp bin/bootloader/loader.bin tmp/boot
      # ========把内核程序复制到boot.img======
      cp bin/kernel/kernel.bin tmp/boot
      sync
      # 卸载磁盘
      umount tmp/boot
  else
    echo "挂载 boot.img 失败！"
  fi



# 运行结束后删除tmp文件夹
rm -rf tmp

# 进行启动前检查
flag_can_run=0

if [ -d "tmp/" ]; then
  flag_can_run=0
  echo "tmp文件夹未删除！"
else
  flag_can_run=1
fi

if [ $flag_can_run -eq 1 ]; then
  bochs -f ./bochsrc -q
else
  echo "不满足运行条件"
fi