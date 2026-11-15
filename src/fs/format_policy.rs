//! Когда pagh вправе форматировать блочное устройство.
//!
//! Политика намеренно **чистая** (`core`-only, без I/O): устройство зондирует
//! вызывающий и передаёт байты сюда, поэтому host-tests проверяют само решение
//! (свойство P50) на реальных boot-областях GPT/MBR/ext2, не полагаясь на QEMU.
//!
//! Правила по порядку:
//!
//! 1. *монтируемый* суперблок ext2 — это ФС, не форматировать никогда, что бы
//!    ещё ни лежало на устройстве (`Refusal::ExistingFilesystem`);
//! 2. целиком нулевая зондируемая boot-область действительно пуста, и только
//!    её pagh форматирует сам;
//! 3. `allow_destructive` (явный opt-in) — единственный путь мимо чужой
//!    разметки;
//! 4. всё остальное отвергается с указанием, что найдено.

/// Смещение суперблока ext2 в байтах внутри первого блока ФС.
pub const SUPERBLOCK_OFFSET: usize = 1024;

/// Смещение `s_magic` **внутри** суперблока (`0x38`, как у
/// `Ext2SuperBlock::s_magic`; смещение проверяется в `ext2::structs`).
///
/// Магия ext-семейства лежит по `SUPERBLOCK_OFFSET + SB_MAGIC_OFFSET`
/// (1024 + 0x38), а не по 1024; константу экспортируют и детектор, и host-тест
/// P50.
pub const SB_MAGIC_OFFSET: usize = 0x38;

/// `s_magic` ext2/ext4 в том виде, в каком лежит на диске (little-endian `0xEF53`).
pub const EXT2_MAGIC_LE: [u8; 2] = [0x53, 0xEF];

/// Сигнатура заголовка GPT в LBA 1 (байт 512).
pub const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";

/// Boot-сигнатура MBR в конце LBA 0 (байт 510).
pub const MBR_SIGNATURE: [u8; 2] = [0x55, 0xAA];

/// Минимальная длина зонда, при которой распознаётся любая разметка выше
/// (магия ext2).
pub const MIN_PROBE: usize = SUPERBLOCK_OFFSET + SB_MAGIC_OFFSET + 2;

/// Явный opt-in: оператор пишет этот маркер в сектор 0, разрешая pagh забрать
/// устройство, на котором уже есть данные.
///
/// ```text
/// printf 'PAGH-FORMAT' | sudo dd of=/dev/nvme0n1 bs=1 conv=notrunc
/// ```
///
/// Единственный способ разрешить форматирование устройства с данными, и сам по
/// себе он **разрушителен**: маркер ложится в первые 11 байт сектора 0 —
/// bootstrap-область размеченного диска. Без маркера устройство форматируется,
/// только когда зондируемая область целиком нулевая.
pub const OPT_IN_MARKER_NAME: &str = "PAGH-FORMAT";

/// Тот же маркер байтами — для зонда. Выводится из имени, поэтому разойтись
/// они не могут.
pub const OPT_IN_MARKER: &[u8] = OPT_IN_MARKER_NAME.as_bytes();

/// Как выглядит зондируемая boot-область устройства.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// Присутствует магия суперблока ext2/ext4.
    Ext2,
    /// Присутствует таблица разделов MBR или GPT.
    PartitionTable,
    /// Все зондируемые байты нулевые.
    Blank,
    /// Ненулевые данные, не подходящие под варианты выше.
    Foreign,
}

impl Layout {
    /// Короткое имя для строки boot-лога.
    pub fn name(self) -> &'static str {
        match self {
            Layout::Ext2 => "ext2-like",
            Layout::PartitionTable => "partitioned",
            Layout::Blank => "blank",
            Layout::Foreign => "foreign data",
        }
    }
}

/// Причина, по которой устройство нельзя форматировать.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Есть валидный суперблок ext2: настоящая ФС, которая не смонтировалась.
    ExistingFilesystem,
    /// На устройстве есть таблица разделов (MBR или GPT).
    PartitionTable,
    /// Магия ext2 есть, но суперблок не проходит валидацию.
    Ext2Like,
    /// Ненулевые байты — ни таблица разделов, ни суперблок ext2.
    ForeignData,
}

impl Refusal {
    /// Однострочное объяснение для serial-лога.
    pub fn reason(self) -> &'static str {
        match self {
            Refusal::ExistingFilesystem => {
                "existing ext2 mount failed; refusing destructive reformat"
            }
            Refusal::PartitionTable => "device carries an MBR/GPT partition table",
            Refusal::Ext2Like => "device looks like ext2 but its superblock does not validate",
            Refusal::ForeignData => "device is neither blank nor ext2 (unknown on-disk data)",
        }
    }
}

/// Инкрементальный классификатор boot-области.
///
/// Создаётся по первому блоку ФС (≥ [`MIN_PROBE`] байт, на практике 4 KiB),
/// затем `feed` до тех пор, пока [`Probe::is_decided`] ложно. I/O — на
/// вызывающем: он может остановиться сразу после определения разметки; до конца
/// устройство сканируется, только если притворяется пустым.
///
/// Fail-closed по построению: зонд короче [`MIN_PROBE`] — это
/// [`Layout::Foreign`] (отказ), а не `Blank`.
#[derive(Clone, Copy, Debug)]
pub struct Probe {
    layout: Layout,
    /// Пока `Blank`: первый же ненулевой байт дальше делает его `Foreign`.
    provisional_blank: bool,
    /// В секторе 0 найден [`OPT_IN_MARKER`] оператора.
    marked: bool,
}

impl Probe {
    /// Классифицирует первый блок ФС.
    pub fn new(first_block: &[u8]) -> Self {
        let marked = first_block.len() >= OPT_IN_MARKER.len()
            && &first_block[..OPT_IN_MARKER.len()] == OPT_IN_MARKER;
        if first_block.len() < MIN_PROBE {
            return Self {
                layout: Layout::Foreign,
                provisional_blank: false,
                marked,
            };
        }
        let layout = if is_ext2_superblock(first_block) {
            Layout::Ext2
        } else if is_partition_table(first_block) {
            Layout::PartitionTable
        } else if is_zero(first_block) {
            Layout::Blank
        } else {
            Layout::Foreign
        };
        Self {
            layout,
            provisional_blank: layout == Layout::Blank,
            marked,
        }
    }

    /// Зонд не удался (ошибка I/O). Отказ, форматирования не будет.
    pub fn unreadable() -> Self {
        Self {
            layout: Layout::Foreign,
            provisional_blank: false,
            marked: false,
        }
    }

    /// Передаёт следующий фрагмент boot-области.
    pub fn feed(&mut self, chunk: &[u8]) {
        if self.provisional_blank && !is_zero(chunk) {
            self.layout = Layout::Foreign;
            self.provisional_blank = false;
        }
    }

    /// Разметка, определённая на текущий момент.
    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// Найден ли явный [`OPT_IN_MARKER`]? Это значение передаётся как
    /// `allow_destructive`.
    pub fn is_marked(&self) -> bool {
        self.marked
    }

    /// `true`, когда разметка уже не изменится (дальнейший зонд не нужен).
    pub fn is_decided(&self) -> bool {
        !self.provisional_blank
    }
}

/// Единственное решение `boot::init_fs`: можно ли форматировать устройство?
///
/// `has_valid_superblock` — результат `Ext2Fs::has_valid_superblock`
/// (*разбираемый* суперблок, не просто магия), `layout` — результат зонда
/// boot-области.
///
/// Приоритет: разбираемый суперблок ext2 отказывает безусловно; пустое
/// устройство форматируется без opt-in; [`OPT_IN_MARKER`] снимает любой отказ
/// *по разметке*; иначе возвращается отказ самой разметки.
pub fn format_allowed(
    layout: Layout,
    has_valid_superblock: bool,
    allow_destructive: bool,
) -> Result<(), Refusal> {
    if has_valid_superblock {
        // Настоящая ФС, не прошедшая mount, — это ошибка, а не разрешение
        // стирать пользовательские данные. Маркер этот отказ не снимает: том
        // достаточно цел, чтобы его опознали. Кто действительно хочет вернуть
        // такое устройство, сначала обнуляет суперблок — тогда зонд видит
        // `Blank`, и маркер не нужен.
        return Err(Refusal::ExistingFilesystem);
    }
    if layout == Layout::Blank {
        return Ok(());
    }
    // Маркер — письменное согласие оператора, поэтому снимает отказы по
    // разметке: таблицу разделов, магию ext2 с невалидным суперблоком, любые
    // чужие данные. Проверка здесь, до превращения разметки в отказ, чтобы
    // приоритет был виден в одном месте, а не следовал из веток match.
    if allow_destructive {
        return Ok(());
    }
    Err(match layout {
        Layout::PartitionTable => Refusal::PartitionTable,
        Layout::Ext2 => Refusal::Ext2Like,
        // `Blank` вернулся выше; ветка нужна для полноты.
        Layout::Blank | Layout::Foreign => Refusal::ForeignData,
    })
}

/// Магия суперблока ext2/ext4 по [`SUPERBLOCK_OFFSET`].
fn is_ext2_superblock(buf: &[u8]) -> bool {
    // Магия лежит по 1024 + 0x38, а не по 1024: см. `SB_MAGIC_OFFSET`.
    buf.len() >= MIN_PROBE && buf[SUPERBLOCK_OFFSET + SB_MAGIC_OFFSET..MIN_PROBE] == EXT2_MAGIC_LE
}

/// Заголовок GPT в LBA 1 или boot-сигнатура MBR в конце LBA 0.
///
/// Защитный MBR (GPT) несёт обе, поэтому для отказа хватило бы одной; проверяются
/// обе, чтобы поймать и hybrid-, и чисто MBR-разметку.
fn is_partition_table(buf: &[u8]) -> bool {
    let gpt = buf.len() >= 520 && &buf[512..520] == GPT_SIGNATURE;
    let mbr = buf.len() >= 512 && buf[510..512] == MBR_SIGNATURE;
    gpt || mbr
}

/// Ни одного ненулевого байта во всём срезе (пустой срез тривиально нулевой).
fn is_zero(buf: &[u8]) -> bool {
    buf.iter().all(|&b| b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpt_boot_block() -> [u8; 4096] {
        let mut b = [0u8; 4096];
        // Защитный MBR: одна запись раздела 0xEE, затем boot-сигнатура.
        b[446] = 0x00; // не загрузочный
        b[450] = 0xEE; // тип: GPT protective
        b[454] = 0x01; // первый LBA 1 (little-endian u32)
        b[510] = 0x55;
        b[511] = 0xAA;
        b[512..520].copy_from_slice(GPT_SIGNATURE);
        b
    }

    fn ext2_boot_block() -> [u8; 4096] {
        let mut b = [0u8; 4096];
        // По `SUPERBLOCK_OFFSET + SB_MAGIC_OFFSET` — там, где магия у настоящего
        // тома. Тест и детектор обязаны использовать одну и ту же константу.
        b[SUPERBLOCK_OFFSET + SB_MAGIC_OFFSET..MIN_PROBE].copy_from_slice(&EXT2_MAGIC_LE);
        b
    }

    #[test]
    fn blank_probe_is_blank_and_decided_only_after_the_scan() {
        let mut p = Probe::new(&[0u8; 4096]);
        assert_eq!(p.layout(), Layout::Blank);
        assert!(
            !p.is_decided(),
            "blank stays provisional while chunks remain"
        );
        p.feed(&[0u8; 4096]);
        assert_eq!(p.layout(), Layout::Blank);
    }

    #[test]
    fn later_non_zero_chunk_makes_a_blank_probe_foreign() {
        let mut p = Probe::new(&[0u8; 4096]);
        p.feed(&[0u8; 4096]);
        let mut dirty = [0u8; 4096];
        dirty[4095] = 1;
        p.feed(&dirty);
        assert_eq!(p.layout(), Layout::Foreign);
        assert!(p.is_decided());
    }

    #[test]
    fn layouts_are_recognised() {
        assert_eq!(
            Probe::new(&gpt_boot_block()).layout(),
            Layout::PartitionTable
        );
        assert_eq!(Probe::new(&ext2_boot_block()).layout(), Layout::Ext2);
        assert_eq!(Probe::new(&[0u8; 4096]).layout(), Layout::Blank);
        let mut junk = [0u8; 4096];
        junk[0] = 0xEB; // jump x86-загрузчика, например NTFS/XFS/LUKS
        assert_eq!(Probe::new(&junk).layout(), Layout::Foreign);
    }

    #[test]
    fn short_probe_fails_closed() {
        let p = Probe::new(&[0u8; 16]);
        assert_eq!(p.layout(), Layout::Foreign);
        assert!(p.is_decided());
        assert_eq!(Probe::unreadable().layout(), Layout::Foreign);
    }

    #[test]
    fn only_a_blank_device_is_formatted_by_default() {
        assert_eq!(format_allowed(Layout::Blank, false, false), Ok(()));
        assert_eq!(
            format_allowed(Layout::PartitionTable, false, false),
            Err(Refusal::PartitionTable)
        );
        assert_eq!(
            format_allowed(Layout::Ext2, false, false),
            Err(Refusal::Ext2Like)
        );
        assert_eq!(
            format_allowed(Layout::Foreign, false, false),
            Err(Refusal::ForeignData)
        );
        assert_eq!(
            format_allowed(Layout::Foreign, false, true),
            Ok(()),
            "an explicit opt-in is the only way past a foreign layout"
        );
    }

    #[test]
    fn a_mountable_superblock_is_never_formatted() {
        for layout in [
            Layout::Blank,
            Layout::Ext2,
            Layout::PartitionTable,
            Layout::Foreign,
        ] {
            for allow in [false, true] {
                assert_eq!(
                    format_allowed(layout, true, allow),
                    Err(Refusal::ExistingFilesystem),
                    "layout {layout:?} allow={allow}"
                );
            }
        }
    }

    #[test]
    fn the_opt_in_marker_is_recognised_only_where_written() {
        let mut marked = gpt_boot_block();
        marked[..OPT_IN_MARKER.len()].copy_from_slice(OPT_IN_MARKER);
        let p = Probe::new(&marked);
        assert!(p.is_marked());
        // Маркер разрешает ровно один класс разметки, а таблица разделов под
        // ним всё равно распознаётся.
        assert_eq!(p.layout(), Layout::PartitionTable);
        assert_eq!(format_allowed(p.layout(), false, p.is_marked()), Ok(()));

        assert!(!Probe::new(&gpt_boot_block()).is_marked());
        assert!(!Probe::new(&[0u8; 4096]).is_marked());
        assert!(!Probe::unreadable().is_marked());

        // Маркер дальше в окне — не маркер: он обязан быть в секторе 0.
        let mut late = [0u8; 4096];
        late[2048..2048 + OPT_IN_MARKER.len()].copy_from_slice(OPT_IN_MARKER);
        assert!(!Probe::new(&late).is_marked());
        assert_eq!(Probe::new(&late).layout(), Layout::Foreign);
    }

    #[test]
    fn the_marker_does_not_override_a_mountable_filesystem() {
        let mut marked = ext2_boot_block();
        marked[..OPT_IN_MARKER.len()].copy_from_slice(OPT_IN_MARKER);
        let p = Probe::new(&marked);
        assert!(p.is_marked());
        assert_eq!(
            format_allowed(p.layout(), true, p.is_marked()),
            Err(Refusal::ExistingFilesystem)
        );
    }

    #[test]
    fn the_marker_lifts_every_layout_refusal() {
        // Маркер — согласие на *содержимое этого устройства*, как бы зонд его ни
        // назвал: таблица разделов, невалидная магия ext2, нераспознанные данные.
        // Выше него только разбираемый суперблок.
        for layout in [
            Layout::PartitionTable,
            Layout::Ext2,
            Layout::Foreign,
            Layout::Blank,
        ] {
            assert_eq!(
                format_allowed(layout, false, true),
                Ok(()),
                "the opt-in marker must lift {layout:?}"
            );
        }
        // ...а без него каждая разметка отвергается по имени.
        assert_eq!(
            format_allowed(Layout::PartitionTable, false, false),
            Err(Refusal::PartitionTable)
        );
        assert_eq!(
            format_allowed(Layout::Ext2, false, false),
            Err(Refusal::Ext2Like)
        );
        assert_eq!(
            format_allowed(Layout::Foreign, false, false),
            Err(Refusal::ForeignData)
        );
    }
}
