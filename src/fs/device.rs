//! Помощники уровня устройства, общие для boot-пути и shell-меню `disk`.
//!
//! Само *решение* «можно ли форматировать устройство?» — чистое и живёт в
//! [`crate::fs::format_policy`], который host-тесты включают напрямую. Чтение
//! устройства для этого решения — уже I/O, поэтому оно здесь: одна реализация на
//! оба вызывающих, иначе меню классифицировало бы диск иначе, чем boot-путь, и
//! предлагало стереть ровно те диски, которые boot-путь трогать отказывается.

/// Наибольшее смещение в байтах, которое осматривает зонд.
///
/// Один MiB — минимальное окно, вмещающее всё, что должен распознать
/// классификатор: суперблок ext2 по смещению 1024, MBR в конце сектора 0 и
/// заголовок GPT с массивом записей разделов в LBA 1..33.
const PROBE_LIMIT: u64 = 1024 * 1024;

/// Шаг зонда в байтах: один блок ФС (8 секторов устройства).
const PROBE_CHUNK: u64 = 4096;

/// Классифицирует первый MiB `dev` для [`crate::fs::format_policy`].
///
/// Читает шагами по [`PROBE_CHUNK`] и останавливается, как только разметка
/// определена: размеченное или непустое устройство стоит одного чтения; до конца
/// [`PROBE_LIMIT`] сканируется только устройство, притворяющееся пустым. Ошибка
/// чтения оставляет классификацию `Foreign` (отказ), поэтому нечитаемое
/// устройство тоже никогда не форматируется — в том числе устройство с одним
/// битым сектором в окне: его нельзя отформатировать, пока оно не обнулено.
pub fn probe_boot_area(dev: &dyn crate::drivers::BlockDevice) -> crate::fs::format_policy::Probe {
    use crate::fs::format_policy::Probe;

    const SECTORS_PER_CHUNK: u64 = PROBE_CHUNK / 512;
    let mut buf = alloc::vec![0u8; PROBE_CHUNK as usize];
    let mut probe = match dev.read_block(0, &mut buf) {
        Ok(_) => Probe::new(&buf),
        Err(()) => return Probe::unreadable(),
    };
    let mut sector = SECTORS_PER_CHUNK;
    while !probe.is_decided() && sector * 512 < PROBE_LIMIT {
        match dev.read_block(sector, &mut buf) {
            Ok(_) => probe.feed(&buf),
            Err(()) => return Probe::unreadable(),
        }
        sector += SECTORS_PER_CHUNK;
    }
    probe
}
