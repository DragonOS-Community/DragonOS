echo "Creating virtual disk image..."
qemu-img create -f raw disk.img 16M
# 分别输入o、n, 然后按4次回车，直到回到fdisk的默认界面，
# 再输入w即可
# 按顺序输入，并且，每次输入完成后要按下回车）
fdisk disk.img

echo "Successfully created disk image, please make a FAT32 filesystem on it"
sudo mkdir -p ../bin
sudo cp ./disk.img ../bin/
