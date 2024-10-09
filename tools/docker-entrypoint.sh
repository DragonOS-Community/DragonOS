#!/bin/bash
sudo chown -R ${NEW_USER}:${NEW_GROUP} /home/${NEW_USER}/.cargo/registry
sudo chown -R ${NEW_USER}:${NEW_GROUP} /home/${NEW_USER}/.cargo/git

# 解决kvm权限问题
sudo groupadd kvm || echo "kvm组已存在"
sudo usermod -aG kvm $NEW_USER
sudo chown $NEW_USER /dev/kvm

exec "$@"
