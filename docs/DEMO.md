# Демо-медиа и однокомандный бандл

`docs/demo.gif` / `docs/demo.mp4` — **настоящие снимки framebuffer** этого ядра под QEMU/OVMF: каждый кадр — это `screendump` гостевого framebuffer 1280×800, а ввод инжектится через QEMU monitor (`sendkey`, `mouse_move`/`mouse_button`). Ничего не композитится и не постановочно готовится на хосте: консоль, окна, canvas `paint` и курсор рисует само ядро.

Запись длится 24 с, 15 fps, от загрузки до панели `settings` с темой `synth` и живыми обоями.

**Что именно записано.** Это **консольный вариант A** на ядре 2.18.12 (shell, `paint`,
`nano+`, `settings`); медиа не переснималось после появления desktop-варианта B
(userspace-композитор, [`desktop-b.md`](desktop-b.md)), и текущий demo-бандл грузит уже
desktop. Кадры не постановочные — устарел только сюжет, а не способ съёмки.

**Канал релиза.** Demo — третий, нестабильный канал: тег содержит `-demo` (сейчас
`2.24.0-demo`) либо образ собран из ветки `integration/demo`, и такая запись **обязана**
быть GitHub **Pre-release** (`prerelease: true`), никогда не Latest и не описываться как
production-ready; Latest остаётся за stable `vX.Y.Z` из `main` (правило #154, чек-лист —
`RELEASING.md`). Старый `2.18.12-demo` опубликован без флага — это ровно та ошибка, против
которой правило.

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

Опубликованный архив распаковывается в каталог `pagh-demo-x86_64/` (поэтому в README
команда `tar xzf … && cd pagh-demo-x86_64`), в нём — `run.sh` и готовый `pagh-boot.img`
(образ с ESP и системным томом; самодостаточный, тулчейн Rust не нужен). Лаунчер бандла
ищет объединённый `OVMF.fd` или сплит-пару `OVMF_CODE.fd`/`OVMF_VARS.fd`, при первом запуске
копирует vars-шаблон рядом с собой и грузит QEMU (`-machine pc -cpu max -m 2048M`) с образом
как диском; `PAGH_HEADLESS=1 ./run.sh` грузится без окна и пишет `pagh-serial.log`,
`./run.sh --iso` грузит ISO-вариант вместо USB-образа. Desktop стартует сам: рецепт
`/mnt/etc/autorun` уже внутри тома (см. `--autorun` у `tools/mkimage.py`).

Собрать такой бандл из исходников (после `python3 tools/build.py stage --release`):

```sh
# ESP с ядром и конфигом Limine
dd if=/dev/zero of=pagh-boot.img bs=1M count=64
mkfs.vfat -F 32 -n PAGH pagh-boot.img
mcopy -i pagh-boot.img -s iso_root/* ::
# верхний каталог — ровно как в опубликованном архиве
mkdir -p pagh-demo-x86_64 && cp pagh-boot.img run.sh pagh-demo-x86_64/
tar czf pagh-demo-x86_64.tar.gz pagh-demo-x86_64/
```

Репозиторный `run.sh` — не тот же файл, что в бандле: в бандле лежит самостоятельный
лаунчер (только QEMU + OVMF, без сборки). Системный том с desktop собирается
`python3 tools/mkimage.py` (`--make-disk`/`--usb`/`--iso`) плюс `--autorun
rust-apps/pagh-desktop/desktop-autorun`: без рецепта образ загрузится в консольный shell
без desktop, и это ожидаемо.