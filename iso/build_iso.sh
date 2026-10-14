#!/usr/bin/env bash
# build_iso.sh — собирает загрузочный ISO с ticros
set -euo pipefail

ISO="ticros.iso"
STAGE="iso/stage"

KERNEL=$(ls /boot/vmlinuz-* 2>/dev/null | tail -1)
if [[ -z "$KERNEL" || ! -f "$KERNEL" ]]; then
    echo "[!] Ядро не найдено в /boot/vmlinuz-*"
    exit 1
fi

if [[ ! -f initramfs.cpio.gz ]]; then
    echo "[!] initramfs.cpio.gz не найден. Сначала запустите iso/build_initramfs.sh"
    exit 1
fi

echo "[*] Ядро: $KERNEL"
echo "[*] Готовим stage..."
rm -rf "$STAGE"
mkdir -p "$STAGE"/boot/grub

cp "$KERNEL" "$STAGE/boot/vmlinuz"
cp initramfs.cpio.gz "$STAGE/boot/"

cat > "$STAGE/boot/grub/grub.cfg" <<'GRUB_EOF'
set timeout=10
set default=0

menuentry "ticros — Victoria-like diagnostic tool" {
    linux  /boot/vmlinuz console=tty0 quiet
    initrd /boot/initramfs.cpio.gz
}

menuentry "ticros (verbose boot)" {
    linux  /boot/vmlinuz console=tty0
    initrd /boot/initramfs.cpio.gz
}

menuentry "ticros (no KASLR, safe)" {
    linux  /boot/vmlinuz console=tty0 nokaslr
    initrd /boot/initramfs.cpio.gz
}
GRUB_EOF

echo "[*] Собираем ISO через grub-mkrescue..."
grub-mkrescue -o "$ISO" "$STAGE" 2>&1 | tail -5

SIZE=$(du -h "$ISO" | cut -f1)
echo "[+] Готово: $ISO ($SIZE)"