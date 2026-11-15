# Демо-медиа и однокомандный бандл

`docs/demo.gif` / `docs/demo.mp4` — **настоящие снимки framebuffer** этого ядра под QEMU/OVMF: каждый кадр — это `screendump` гостевого framebuffer 1280×800, а ввод инжектится через QEMU monitor (`sendkey`, `mouse_move`/`mouse_button`). Ничего не композитится и не постановочно готовится на хосте: консоль, окна, canvas `paint` и курсор рисует само ядро.

Запись длится 24 с, 15 fps, от загрузки до панели `settings` с темой `synth` и живыми обоями.

## Как это записывалось

1. Собрать и застейджить ядро как обычно:

   ```sh
   python3 tools/build.py stage --release
   ```

2. Загрузить его headless с сокетом QEMU monitor и инжектить ввод, сбрасывая кадры:

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

   Кадры берутся из monitor-команды `screendump … -f png`; последовательность прогоняет `clear`, `ls /mnt`, `paint` (штрихи мышью), `ifconfig` (к этому моменту DHCP-lease уже живой), `nano /mnt/pagh-log.txt`, затем `settings` (перебор темы и обоев). Кадры, снятые в момент перерисовки ядра (копирование shadow-buffer→VGA занимает несколько миллисекунд), отбрасываются, поэтому видео показывает только полностью отрисованные кадры.

3. Закодировать:

   ```sh
   ffmpeg -framerate 15 -i o%05d.png -c:v libopenh264 -b:v 6M -pix_fmt yuv420p docs/demo.mp4
   ffmpeg -framerate 15 -i o%05d.png -vf "fps=12,scale=960:-1:flags=lanczos,split[a][b];\
     [a]palettegen=max_colors=192:stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=3" \
     -loop 0 docs/demo.gif
   ```

`paint` в записи управляется **USB HID-мышью** (`-device qemu-xhci
-device usb-mouse`), потому что синтетические PS/2-дельты теряются на скриптовых скоростях. Запись сделана на ядре с исправлением Y-знака USB-HID (`1a54396`), поэтому хостовой компенсации не нужно; штатная PS/2-мышь под QEMU в любом случае не затронута.

## Бандл `pagh-demo-x86_64.tar.gz`

Релизный бандл — это просто staged-ядро с загрузчиком на FAT-образе и лаунчер:

```sh
# из корня репозитория, после `python3 tools/build.py stage --release`
dd if=/dev/zero of=pagh-boot.img bs=1M count=64
mkfs.vfat -F 32 -n PAGH pagh-boot.img
mcopy -i pagh-boot.img -s iso_root/* ::
tar czf pagh-demo-x86_64.tar.gz pagh-boot.img run.sh TRY-IT.md README.txt
```

Лаунчер находит объединённый `OVMF.fd` или сплит-пару `OVMF_CODE.fd`/`OVMF_VARS.fd`, создаёт при первом запуске `disk.img` на 256 MiB и грузит QEMU с образом как UEFI boot-диском и virtio-blk data-диском. `PAGH_HEADLESS=1 ./run.sh` грузится без окна и пишет `pagh-serial.log`.