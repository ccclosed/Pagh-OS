//! Записанный выбор монтирования: маркер, который делает boot-меню диска вопросом
//! «один раз на машину».
//!
//! ЗАЧЕМ ОТДЕЛЬНЫЙ МАРКЕР. Меню (`crate::boot_menu`) показывает список
//! разделов и спрашивает, что монтировать/форматировать. Вопрос имеет смысл
//! ровно до первого ответа: после него безопасный ответ уже записан, и повторный
//! 20-секундный опрос на каждой загрузке — ритуал «как будто ОС ставят впервые».
//! Но и обратное неверно: наличие pagh-тома на диске НЕ означает, что оператор
//! когда-либо отвечал, — образ, собранный `tools/mkimage.py`, приезжает с уже
//! готовым томом. Поэтому пропуск меню решает не «том есть», а «выбор записан».
//!
//! ПОЧЕМУ СЫРОЙ СЕКТОР, А НЕ ФАЙЛ В `/mnt/etc`. Решение принимается ДО
//! монтирования: монтировать том, чтобы прочитать маркер, значило бы писать в
//! него (драйвер переписывает групповые дескрипторы) до того, как оператор
//! выбрал, — ровно то, чего boot-путь избегает. Сектор 1 устройства —
//! единственное место, которое читается без ФС и при этом принадлежит тому:
//! ext2 резервирует первый килобайт под boot block, эта ФС (размер блока 4 КиБ)
//! его никогда не пишет, а `s_first_data_block = 0` начинает её собственный
//! блок 0 с суперблока на смещении 1024. Прецедент того же приёма —
//! [`crate::fs::format_policy::OPT_IN_MARKER`] в секторе 0.
//!
//! ЦЕЛОСТНОСТЬ. Маркер пишется один раз и не переписывается. Любая порча —
//! обрезанная запись, мусор, чужой заголовок — ОБЯЗАНА читаться как «маркера
//! нет»: тогда меню спросит ещё раз, а не пропустит вопрос молча. Поэтому
//! `decode` строгий: магия, версия, известное действие, набор символов имени,
//! контрольная сумма и нулевой хвост сектора — всё сразу.
//!
//! ОГРАНИЧЕНИЕ, названное явно. Маркер живёт на носителе. Образ диска,
//! приехавший модулем Limine (`ram0`), каждый раз распаковывается в память
//! заново, поэтому выбор, записанный в него, до следующей загрузки не доживает:
//! загрузка с такого образа спрашивает один раз за загрузку. Это цена
//! честности — «запомнить» её нечем, пока оператор не ответит выбором
//! настоящего диска.

/// Размер адресуемой единицы маркера: один сектор устройства (не блок ФС).
pub const SECTOR_BYTES: usize = 512;

/// Сектор устройства, в котором живёт маркер: второй килобайт ext2 boot block.
///
/// Ноль нельзя: там [`crate::fs::format_policy::OPT_IN_MARKER`] и protective-MBR.
/// Также нельзя ничего ≥ 2: с 1024 байта начинается суперблок ext2.
pub const MARKER_SECTOR: u64 = 1;

/// Магия записи. Совпадение обязательно с нулевого байта сектора.
const MAGIC: &[u8; 16] = b"PAGH-MNT-CHOICE\0";

/// Версия формата. Меняется только вместе с раскладкой полей ниже.
const VERSION: u8 = 1;

/// Смещение байта версии.
const VERSION_OFF: usize = 16;
/// Смещение байта действия.
const ACTION_OFF: usize = 17;
/// Смещение `u16` длины имени (LE).
const NAME_LEN_OFF: usize = 18;
/// Смещение имени устройства.
const NAME_OFF: usize = 20;

/// Максимальная длина имени устройства в маркере. Имена драйверов короче
/// (`virtio-blk0p1`, `nvme0p1`, `ram0`), так что запас есть, а длина фиксирована
/// ради строгой проверки хвоста.
pub const NAME_MAX: usize = 32;

/// Смещение контрольной суммы.
const CHECKSUM_OFF: usize = NAME_OFF + NAME_MAX;
/// Длина значащей части записи; остаток сектора обязан быть нулевым.
const RECORD_LEN: usize = CHECKSUM_OFF + 4;

/// Действие, которым оператор ответил на вопрос меню.
///
/// Ровно два: меню не предлагает ничего третьего, а `Enter` на монтируемом томе
/// — это [`Action::Mount`], тогда как форматирование требует `F` и набранного
/// имени устройства.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Смонтировать выбранный том в `/mnt`.
    Mount = 1,
    /// Отформатировать выбранное устройство (ext2 + WAL) и смонтировать его.
    Format = 2,
}

impl Action {
    /// Байт, которым действие лежит в маркере.
    pub fn as_byte(self) -> u8 {
        self as u8
    }

    /// Разбор байта из маркера: неизвестное значение — порча, а не догадка.
    pub fn from_byte(byte: u8) -> Option<Action> {
        match byte {
            1 => Some(Action::Mount),
            2 => Some(Action::Format),
            _ => None,
        }
    }

    /// Имя для логов и для человекочитаемого зеркала в `/mnt/etc`.
    pub fn name(self) -> &'static str {
        match self {
            Action::Mount => "mount",
            Action::Format => "format",
        }
    }
}

/// Записанный ответ: действие плюс устройство, которого оно касается.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Choice {
    action: Action,
    name: [u8; NAME_MAX],
    name_len: usize,
}

impl Choice {
    /// Запись для устройства `device`, если имя вообще помещается в маркер.
    ///
    /// Имя проверяется тем же набором символов, что и при чтении: маркер,
    /// который `decode` отвергнет, записывать бессмысленно — он выглядел бы как
    /// «выбора нет» и меню спрашивало бы снова.
    pub fn new(action: Action, device: &str) -> Option<Choice> {
        let bytes = device.as_bytes();
        if bytes.is_empty() || bytes.len() > NAME_MAX || !bytes.iter().all(|&b| valid_name_byte(b))
        {
            return None;
        }
        let mut name = [0u8; NAME_MAX];
        name[..bytes.len()].copy_from_slice(bytes);
        Some(Choice {
            action,
            name,
            name_len: bytes.len(),
        })
    }

    /// Действие, которым ответил оператор.
    pub fn action(&self) -> Action {
        self.action
    }

    /// Имя устройства, к которому относится выбор.
    pub fn device(&self) -> &str {
        // Байты проверены `valid_name_byte` (ASCII) при записи и при чтении.
        core::str::from_utf8(&self.name[..self.name_len]).unwrap_or("")
    }

    /// Собрать сектор маркера. Буфер целиком переписывается (хвост — нули).
    ///
    /// `false` — буфер короче сектора или запись не помещается; в этом случае
    /// содержимое не тронуто, и вызывающий обязан оставить устройство без
    /// маркера, а не писать половину.
    pub fn encode(&self, sector: &mut [u8]) -> bool {
        if sector.len() < SECTOR_BYTES || self.name_len == 0 || self.name_len > NAME_MAX {
            return false;
        }
        for byte in sector[..SECTOR_BYTES].iter_mut() {
            *byte = 0;
        }
        sector[..MAGIC.len()].copy_from_slice(MAGIC);
        sector[VERSION_OFF] = VERSION;
        sector[ACTION_OFF] = self.action.as_byte();
        sector[NAME_LEN_OFF..NAME_LEN_OFF + 2]
            .copy_from_slice(&(self.name_len as u16).to_le_bytes());
        sector[NAME_OFF..NAME_OFF + NAME_MAX].copy_from_slice(&self.name);
        let checksum = checksum(&sector[..CHECKSUM_OFF]);
        sector[CHECKSUM_OFF..RECORD_LEN].copy_from_slice(&checksum.to_le_bytes());
        true
    }

    /// Разобрать сектор маркера. `None` — маркера нет (или он повреждён).
    ///
    /// Строгость намеренная: сомнительный сектор обязан означать «выбора нет».
    /// Пропустить меню по мусору значило бы молча решить за оператора, что
    /// монтировать, — единственный исход, который этот модуль предотвращает.
    pub fn decode(sector: &[u8]) -> Option<Choice> {
        if sector.len() < SECTOR_BYTES || &sector[..MAGIC.len()] != MAGIC {
            return None;
        }
        if sector[VERSION_OFF] != VERSION {
            return None;
        }
        let action = Action::from_byte(sector[ACTION_OFF])?;
        let name_len =
            u16::from_le_bytes([sector[NAME_LEN_OFF], sector[NAME_LEN_OFF + 1]]) as usize;
        if name_len == 0 || name_len > NAME_MAX {
            return None;
        }
        let stored = u32::from_le_bytes([
            sector[CHECKSUM_OFF],
            sector[CHECKSUM_OFF + 1],
            sector[CHECKSUM_OFF + 2],
            sector[CHECKSUM_OFF + 3],
        ]);
        if stored != checksum(&sector[..CHECKSUM_OFF]) {
            return None;
        }
        let mut name = [0u8; NAME_MAX];
        name.copy_from_slice(&sector[NAME_OFF..NAME_OFF + NAME_MAX]);
        if !name[..name_len].iter().all(|&b| valid_name_byte(b)) {
            return None;
        }
        // Хвост сектора — только нули: маркер пишет сектор целиком, поэтому
        // что-то после записи означает либо чужой формат, либо недописанный
        // сектор. В обоих случаях это не наш маркер.
        if sector[RECORD_LEN..SECTOR_BYTES].iter().any(|&b| b != 0) {
            return None;
        }
        Some(Choice {
            action,
            name,
            name_len,
        })
    }
}

/// Нужно ли спрашивать оператора: меню показывается **ровно тогда**, когда
/// записанного выбора нет.
///
/// Одна функция на всё решение, чтобы «том уже есть» не могло незаметно
/// подменить «выбор уже сделан»: это разные вопросы, и путать их — баг, из-за
/// которого меню исчезало навсегда у того, кто его не видел ни разу.
pub fn menu_required(recorded: Option<&Choice>) -> bool {
    recorded.is_none()
}

/// Символы, допустимые в имени устройства внутри маркера.
fn valid_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

/// FNV-1a 32: ловит недописанный или перетёртый маркер, не претендуя на роль
/// криптографической защиты (её здесь и не требуется — важна только защита от
/// случайного совпадения, чтобы не пропустить меню молча).
fn checksum(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &byte in bytes {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_survives_and_reports_the_choice() {
        let mut sector = [0u8; SECTOR_BYTES];
        let choice = Choice::new(Action::Format, "virtio-blk0p1").expect("valid device name");
        assert!(choice.encode(&mut sector));
        let read = Choice::decode(&sector).expect("recorded marker decodes");
        assert_eq!(read.action(), Action::Format);
        assert_eq!(read.device(), "virtio-blk0p1");
        assert!(!menu_required(Some(&read)));
    }

    #[test]
    fn a_zeroed_sector_is_not_a_recorded_choice() {
        assert!(Choice::decode(&[0u8; SECTOR_BYTES]).is_none());
        assert!(menu_required(None));
    }

    #[test]
    fn a_torn_or_foreign_sector_is_not_a_recorded_choice() {
        let mut sector = [0u8; SECTOR_BYTES];
        let choice = Choice::new(Action::Mount, "nvme0p1").expect("valid device name");
        assert!(choice.encode(&mut sector));
        // Порча контрольной суммы: недописанное имя.
        sector[NAME_OFF] ^= 0xFF;
        assert!(Choice::decode(&sector).is_none());
        // Чужой заголовок таблицы разделов в этом секторе — тоже «нет маркера».
        sector[..8].copy_from_slice(b"EFI PART");
        assert!(Choice::decode(&sector).is_none());
    }

    #[test]
    fn invalid_device_names_are_refused_instead_of_written() {
        assert!(Choice::new(Action::Mount, "").is_none());
        assert!(Choice::new(Action::Mount, "name with spaces").is_none());
        assert!(Choice::new(Action::Mount, &"x".repeat(NAME_MAX + 1)).is_none());
        assert!(Choice::new(Action::Mount, &"x".repeat(NAME_MAX)).is_some());
    }
}
