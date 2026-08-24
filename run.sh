#!/usr/bin/env bash
# Build, stage, and boot pagh with QEMU/OVMF on Linux.
set -Eeuo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

PROFILE_ARGS=()
FEATURE_ARGS=()
HEADLESS=0

usage() {
  cat <<'EOF'
Usage: ./run.sh [--release] [--features LIST] [--headless]

Environment overrides:
  LIMINE_DIR   Limine directory containing BOOTX64.EFI
  LIMINE_EFI   Exact BOOTX64.EFI path
  OVMF         Combined OVMF.fd firmware path
  OVMF_CODE    Split OVMF_CODE.fd path
  OVMF_VARS    Split OVMF_VARS.fd template path
  PAGH_DISK    Data disk path (default: disk.img)
  PAGH_CPU     QEMU CPU model (default: max; use host with KVM)
EOF
}

while (($#)); do
  case "$1" in
    --release) PROFILE_ARGS=(--release); shift ;;
    --features) FEATURE_ARGS=(--features "${2:?--features requires a value}"); shift 2 ;;
    --headless) HEADLESS=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

for tool in qemu-system-x86_64 qemu-img; do
  command -v "$tool" >/dev/null || { echo "error: $tool not found" >&2; exit 1; }
done

"$ROOT/build.sh" "${PROFILE_ARGS[@]}" "${FEATURE_ARGS[@]}" --stage

DISK="${PAGH_DISK:-$ROOT/disk.img}"
CPU_MODEL="${PAGH_CPU:-max}"
if [[ ! -f "$DISK" ]]; then
  echo "==> Creating 1 GiB data disk: $DISK"
  qemu-img create -f raw "$DISK" 1G
fi

qemu_args=(
  -cpu "$CPU_MODEL"
  -drive "file=fat:rw:$ROOT/iso_root,format=raw"
  -drive "file=$DISK,format=raw,if=none,id=hd0"
  -device virtio-blk-pci,drive=hd0
  -netdev user,id=net0,hostfwd=tcp::5555-:7,hostfwd=udp::5555-:7
  -device e1000,netdev=net0
  -m 1024M
  -serial stdio
  -no-reboot
  -no-shutdown
  -d int,cpu_reset,guest_errors
  -D "$ROOT/qemu_debug.log"
)

# Prefer an explicitly supplied combined image, then a project-local OVMF.fd.
COMBINED="${OVMF:-}"
[[ -n "$COMBINED" ]] || [[ ! -f "$ROOT/OVMF.fd" ]] || COMBINED="$ROOT/OVMF.fd"

if [[ -n "$COMBINED" ]]; then
  [[ -f "$COMBINED" ]] || { echo "error: OVMF firmware not found: $COMBINED" >&2; exit 1; }
  qemu_args=(-bios "$COMBINED" "${qemu_args[@]}")
else
  CODE="${OVMF_CODE:-}"
  VARS_TEMPLATE="${OVMF_VARS:-}"
  for p in /usr/share/OVMF/OVMF_CODE.fd /usr/share/edk2/x64/OVMF_CODE.fd /usr/share/edk2/ovmf/OVMF_CODE.fd; do
    [[ -n "$CODE" ]] || [[ ! -f "$p" ]] || CODE="$p"
  done
  for p in /usr/share/OVMF/OVMF_VARS.fd /usr/share/edk2/x64/OVMF_VARS.fd /usr/share/edk2/ovmf/OVMF_VARS.fd; do
    [[ -n "$VARS_TEMPLATE" ]] || [[ ! -f "$p" ]] || VARS_TEMPLATE="$p"
  done
  [[ -f "$CODE" && -f "$VARS_TEMPLATE" ]] || {
    echo "error: OVMF not found; set OVMF or OVMF_CODE + OVMF_VARS" >&2
    exit 1
  }
  mkdir -p "$ROOT/.cache"
  VARS_RUNTIME="$ROOT/.cache/OVMF_VARS.fd"
  [[ -f "$VARS_RUNTIME" ]] || cp "$VARS_TEMPLATE" "$VARS_RUNTIME"
  qemu_args=(
    -drive "if=pflash,format=raw,readonly=on,file=$CODE"
    -drive "if=pflash,format=raw,file=$VARS_RUNTIME"
    "${qemu_args[@]}"
  )
fi

if ((HEADLESS)); then
  qemu_args=(-display none "${qemu_args[@]}")
fi

echo "==> Starting QEMU; exit with Ctrl-A, then X"
exec qemu-system-x86_64 "${qemu_args[@]}"
