# Demo media and the one-command bundle

`docs/demo.gif` / `docs/demo.mp4` are **real framebuffer captures** of this kernel
running under QEMU/OVMF — every frame is a `screendump` of the guest's 1280×800
framebuffer, with input injected through the QEMU monitor (`sendkey`,
`mouse_move`/`mouse_button`). Nothing is composited or staged on the host: the
kernel draws the console, the windows, the paint canvas and the cursor itself.

The capture is 24 s long, 15 fps, from the boot sequence to the `settings` panel
with the `synth` theme and a live wallpaper.

## How it was recorded

1. Build and stage the kernel exactly as usual:

   ```sh
   python3 tools/build.py stage --release
   ```

2. Boot it headless with a QEMU monitor socket and inject input while dumping frames:

   ```sh
   qemu-system-x86_64 -machine pc -cpu max -m 1024M \
     -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE.fd \
     -drive if=pflash,format=raw,file=OVMF_VARS.fd \
     -drive file=fat:rw:iso_root,format=raw \
     -drive file=disk.img,format=raw,if=none,id=hd0 \
     -device virtio-blk-pci,drive=hd0 \
     -netdev user,id=net0 -device e1000,netdev=net0 \
     -device qemu-xhci,id=xhci -device usb-mouse,bus=xhci.0 \
     -vga std -display none -serial file:serial.log \
     -monitor unix:/tmp/pagh_mon.sock,server,nowait -no-reboot
   ```

   Frames come from monitor `screendump … -f png`; the sequence drives `clear`,
   `ls /mnt`, `ifconfig`, `paint` (mouse strokes), `nano /mnt/pagh-log.txt`, then
   `settings` (theme + wallpaper cycling). Frames captured while the kernel was
   mid-repaint (the shadow-buffer→VGA copy takes a few milliseconds) are dropped,
   so the video only shows fully rendered frames.

3. Encode:

   ```sh
   ffmpeg -framerate 15 -i o%05d.png -c:v libopenh264 -b:v 6M -pix_fmt yuv420p docs/demo.mp4
   ffmpeg -framerate 15 -i o%05d.png -vf "fps=12,scale=960:-1:flags=lanczos,split[a][b];\
     [a]palettegen=max_colors=192:stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=3" \
     -loop 0 docs/demo.gif
   ```

`paint` is driven by a **USB HID mouse** in the capture (`-device qemu-xhci
-device usb-mouse`) because synthetic PS/2 deltas are lossy at scripted rates.
Note that 2.18.12 moves a USB mouse vertically inverted on the PS/2 fallback path;
the capture script compensates on the host side and the fix itself is a separate
commit. The default PS/2 mouse under QEMU is unaffected.

## The `pagh-demo-x86_64.tar.gz` bundle

The release bundle is just the staged kernel plus the bootloader on a FAT image,
and a launcher:

```sh
# from the repository root, after `python3 tools/build.py stage --release`
dd if=/dev/zero of=pagh-boot.img bs=1M count=64
mkfs.vfat -F 32 -n PAGH pagh-boot.img
mcopy -i pagh-boot.img -s iso_root/* ::
tar czf pagh-demo-x86_64.tar.gz pagh-boot.img run.sh TRY-IT.md README.txt
```

The launcher resolves a combined `OVMF.fd` or a split `OVMF_CODE.fd`/`OVMF_VARS.fd`
pair, creates a 256 MiB `disk.img` on first run, and boots QEMU with the image as
the UEFI boot disk plus a virtio-blk data disk. `PAGH_HEADLESS=1 ./run.sh` boots
without a window and writes `pagh-serial.log`.
