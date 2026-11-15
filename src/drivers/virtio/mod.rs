// drivers/virtio/mod.rs — virtio-клей транспорта для крейта `virtio-drivers`
// 64-битное ядро ОС x86_64 на Rust (#![no_std])
//
// Модуль хостит небольшие платформенные шимы, нужные крейту `virtio-drivers`.
// `hal` предоставляет `PaghHal` — реализацию `virtio_drivers::Hal`, мостящую DMA
// и phys<->virt к `pmm`/`vmm` pagh. Драйверы устройств (`VirtIOBlk`/`VirtIONet`)
// подключаются позже и параметризуются по `PaghHal`.

pub mod blk;
pub mod hal;
