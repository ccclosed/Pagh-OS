// drivers/virtio/mod.rs — virtio transport glue for the `virtio-drivers` crate
// 64-bit x86_64 OS kernel in Rust (#![no_std])
//
// This module hosts the small platform shims the `virtio-drivers` crate needs.
// `hal` provides `PaghHal`, the `virtio_drivers::Hal` implementation that
// bridges DMA and phys<->virt to pagh's `pmm`/`vmm`. Device drivers
// (`VirtIOBlk`/`VirtIONet`) are attached in later milestones and parameterize
// over `PaghHal`.

pub mod hal;
pub mod blk;
