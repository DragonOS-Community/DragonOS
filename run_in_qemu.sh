# ==============检查文件是否齐全================
if [ ! -x "bin/bootloader/boot.bin" ]; then
  echo "bin/bootloader/boot.bin 不存在！"
  exit
fi
if [ ! -x "bin/bootloader/loader.bin" ]; then
  echo "bin/bootloader/loader.bin 不存在！"
  exit
fi
if [ ! -x "bin/boot.img" ]; then
  echo "bin/boot.img 不存在！"
  exit
fi
# ===============文件检查完毕===================


# 将引导程序写入boot.img
dd if=bin/bootloader/boot.bin of=bin/boot.img bs=512 count=1 conv=notrunc

# 判断临时文件夹是否存在，若不存在则创建新的
if [ ! -d "tmp/" ]; then
  mkdir tmp/
  echo "创建了tmp文件夹"
fi

# 挂载boot.img到tmp/boot
  mkdir tmp/boot
  sudo mount bin/boot.img tmp/boot -t vfat -o loop

  # 检查是否挂载成功
  if  mountpoint -q tmp/boot
   then
      echo "成功挂载 boot.img 到 tmp/boot"
      # 把loader.bin复制到boot.img
      sudo cp bin/bootloader/loader.bin tmp/boot
      sync
      sudo umount tmp/boot
  else
    echo "挂载 boot.img 失败！"
  fi



# 运行结束后删除tmp文件夹
sudo rm -rf tmp

# 进行启动前检查
flag_can_run=0

if [ -d "tmp/" ]; then
  flag_can_run=0
  echo "tmp文件夹未删除！"
else
  flag_can_run=1
fi

if [ $flag_can_run -eq 1 ]; then
  qemu-system-x86_64 -s -S -m 2048 -fda bin/boot.img
else
  echo "不满足运行条件"
fi