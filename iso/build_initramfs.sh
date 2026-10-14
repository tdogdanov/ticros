#!/usr/bin/env bash
# build_initramfs.sh — собирает initramfs с BusyBox + ticros
set -euo pipefail

ROOTFS="iso/rootfs"
TICROS_BIN="target/x86_64-unknown-linux-musl/release/ticros"

if [[ ! -f "$TICROS_BIN" ]]; then
    echo "[!] ticros не найден: $TICROS_BIN"
    echo "    Соберите: cargo build --release --target x86_64-unknown-linux-musl"
    exit 1
fi

echo "[*] Готовим rootfs..."
rm -rf "$ROOTFS"
mkdir -p "$ROOTFS"/{bin,sbin,dev,proc,sys,tmp,etc,root,mnt,usr/bin,usr/sbin}

echo "[*] Копируем BusyBox..."
cp /bin/busybox "$ROOTFS/bin/"

cd "$ROOTFS/bin"
for cmd in sh ls cat mount umount echo sleep dmesg mkdir mknod mdev chmod ln df free \
           grep sed awk cut tr wc head tail more less vi clear tput reboot poweroff \
           lsblk blkid fdisk hdparm dd sync stty; do
    ln -sf busybox "$cmd" 2>/dev/null || true
done
cd - >/dev/null

echo "[*] Копируем ticros..."
cp "$TICROS_BIN" "$ROOTFS/bin/ticros"
chmod +x "$ROOTFS/bin/ticros"

echo "[*] Создаём init..."
cat > "$ROOTFS/init" <<'INIT_EOF'
#!/bin/sh
mount -t proc none /proc
mount -t sysfs none /sys
mount -t devtmpfs none /dev 2>/dev/null || mdev -s
mkdir -p /dev/pts
mount -t devpts none /dev/pts 2>/dev/null || true

export PATH=/bin:/sbin:/usr/bin:/usr/sbin
export HOME=/root
cd /root

clear 2>/dev/null || printf '\033[2J\033[H'

cat <<'BANNER'

    ████████╗██╗ ██████╗██████╗  ██████╗ ███████╗
    ╚══██╔══╝██║██╔════╝██╔══██╗██╔═══██╗██╔════╝
       ██║   ██║██║     ██████╔╝██║   ██║███████╗
       ██║   ██║██║     ██╔══██╗██║   ██║╚════██║
       ██║   ██║╚██████╗██║  ██║╚██████╔╝███████║
       ╚═╝   ╚═╝ ╚═════╝╚═╝  ╚═╝ ╚═════╝ ╚══════╝

    Victoria-like HDD/SSD diagnostic tool (Rust)
    ============================================

    Диски в системе:

BANNER

for d in /dev/sd? /dev/nvme?n?; do
    [ -e "$d" ] && echo "    $d"
done

cat <<'MENU'

    Команды:
      ticros --device /dev/sda info
      ticros --device /dev/sda smart --full
      ticros --device /dev/sda scan --map
      ticros --device /dev/sda scan --end-mb 1000 --map
      ticros presets

    Помощь: ticros --help

MENU

exec /bin/sh
INIT_EOF

chmod +x "$ROOTFS/init"

echo "[*] Собираем initramfs.cpio.gz..."
cd "$ROOTFS"
find . -print0 | cpio --null -ov --format=newc 2>/dev/null | gzip -9 > ../../initramfs.cpio.gz
cd - >/dev/null

SIZE=$(du -h initramfs.cpio.gz | cut -f1)
echo "[+] Готово: initramfs.cpio.gz ($SIZE)"