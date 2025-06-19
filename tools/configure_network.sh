# Replace with your wired Ethernet interface name
ETH=eno1

sudo modprobe bridge
sudo modprobe br_netfilter

sudo sysctl -w net.bridge.bridge-nf-call-arptables=0
sudo sysctl -w net.bridge.bridge-nf-call-ip6tables=0
sudo sysctl -w net.bridge.bridge-nf-call-iptables=0

sudo ip tuntap add name tap0 mode tap user $USER
sudo brctl addbr br0
sudo brctl addif br0 tap0
sudo brctl addif br0 $ETH
sudo ip link set tap0 up
sudo ip link set $ETH up
sudo ip link set br0 up


# This connects your host system to the internet, so you can use it
# at the same time you run the examples.
sudo dhcpcd br0

sudo mkdir -p /usr/local/etc/qemu
sudo mkdir -p /etc/qemu
sudo sh -c 'echo "allow br0" > /usr/local/etc/qemu/bridge.conf'
sudo sh -c 'echo "allow br0" > /etc/qemu/bridge.conf'