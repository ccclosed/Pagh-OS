//! Декодирование CPUID для `/proc/cpuinfo`.
//!
//! Это единственная архитектурно-зависимая половина procfs: она читает листья
//! CPUID в простую структуру [`CpuInfoData`], которую рендерит чистый
//! форматтер `vfs::procfs_format`. Такое разделение делает каждый байт
//! `/proc/cpuinfo` host-тестируемым, а сами инструкции CPUID — в одном
//! маленьком, проверяемом месте (`docs/procfs.md` §6.6).
//!
//! Любой лист выше сообщённого максимума защищён проверкой: неподдерживаемый
//! лист возвращает неопределённые данные (обычно нули, но SDM этого не
//! обещает), и незащищённое чтение напечатало бы мусор как факт.

use core::arch::x86_64::{__cpuid, __cpuid_count};

use crate::vfs::procfs_format::CpuInfoData;

/// Декодировать информацию CPUID, нужную `/proc/cpuinfo`.
///
/// Отсутствующие листья оставляют соответствующий `Option` как `None`, и
/// форматтер опускает строку вместо того, чтобы выдумывать значение.
pub fn read() -> CpuInfoData {
    let mut d = CpuInfoData::default();

    // CPUID — непривилегированный архитектурный запрос на x86_64 (intrinsics
    // безопасны именно поэтому); лист 0 определён на каждом CPU x86_64 и даёт
    // строку производителя и максимальный базовый лист.
    let leaf0 = __cpuid(0);
    d.max_basic = leaf0.eax;
    let mut vendor = [0u8; 12];
    vendor[0..4].copy_from_slice(&leaf0.ebx.to_le_bytes());
    vendor[4..8].copy_from_slice(&leaf0.edx.to_le_bytes());
    vendor[8..12].copy_from_slice(&leaf0.ecx.to_le_bytes());
    d.vendor = vendor;

    if d.max_basic >= 1 {
        // Защищено сообщённым максимальным базовым листом.
        let leaf1 = __cpuid(1);
        d.stepping = leaf1.eax & 0xf;
        d.model = (leaf1.eax >> 4) & 0xf;
        d.family = (leaf1.eax >> 8) & 0xf;
        let ext_model = (leaf1.eax >> 16) & 0xf;
        let ext_family = (leaf1.eax >> 20) & 0xff;
        if d.family == 0xf {
            d.family += ext_family;
        }
        if d.family == 0x6 || d.family == 0xf {
            d.model += ext_model << 4;
        }
        d.leaf1_edx = leaf1.edx;
        d.leaf1_ecx = leaf1.ecx;
    }

    if d.max_basic >= 7 {
        // Защищено сообщённым максимальным базовым листом; подлист 0
        // существует везде, где есть лист 7.
        let leaf7 = __cpuid_count(7, 0);
        d.leaf7_ebx = leaf7.ebx;
        d.leaf7_ecx = leaf7.ecx;
    }

    // 0x8000_0000 — максимальный лист расширенных функций; как и лист 0,
    // определён на каждом CPU x86_64.
    let max_ext = __cpuid(0x8000_0000).eax;

    if max_ext >= 0x8000_0001 {
        // Защищено сообщённым максимальным расширенным листом.
        let l = __cpuid(0x8000_0001);
        d.leaf81_edx = l.edx;
        d.leaf81_ecx = l.ecx;
    }

    if max_ext >= 0x8000_0004 {
        let mut brand = [0u8; 48];
        for (i, chunk) in brand.chunks_mut(16).enumerate() {
            // Защищено максимальным расширенным листом; три листа бренда
            // 0x8000_0002..0x8000_0004 непрерывны по определению.
            let l = __cpuid(0x8000_0002 + i as u32);
            chunk[0..4].copy_from_slice(&l.eax.to_le_bytes());
            chunk[4..8].copy_from_slice(&l.ebx.to_le_bytes());
            chunk[8..12].copy_from_slice(&l.ecx.to_le_bytes());
            chunk[12..16].copy_from_slice(&l.edx.to_le_bytes());
        }
        d.brand = brand;
    }

    if max_ext >= 0x8000_0006 {
        // Защищено сообщённым максимальным расширенным листом.
        let l = __cpuid(0x8000_0006);
        let kb = (l.ecx >> 16) & 0xffff;
        if kb > 0 {
            d.cache_kb = Some(kb);
        }
    }

    if max_ext >= 0x8000_0008 {
        // Защищено сообщённым максимальным расширенным листом.
        let l = __cpuid(0x8000_0008);
        d.phys_bits = (l.eax & 0xff) as u8;
        d.virt_bits = ((l.eax >> 8) & 0xff) as u8;
    }

    if d.max_basic >= 0x16 {
        // Защищено сообщённым максимальным базовым листом.
        let l = __cpuid(0x16);
        let mhz = l.eax & 0xffff;
        if mhz > 0 {
            d.mhz = Some(mhz);
        }
    }

    d
}
