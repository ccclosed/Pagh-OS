//! Чистый ABI ввода указателя терминальной сессии (`/dev/vtmouse1`).
//!
//! Kernel-shell окна живёт в **своём** экземпляре консоли, но указатель у него
//! один на машину (`drivers::ps2_mouse` — PS/2 IRQ12). Композитор уже получает
//! настоящую мышь через `/dev/input/event0` и знает координаты внутри окна,
//! поэтому окно обязано вернуть их сессии — иначе scrollback колесом, выделение
//! и копирование в окне остаются мёртвыми (это и было честной границей этапа 1).
//!
//! `/dev/vtmouse1` — write-only поток записей по 16 байт little-endian:
//!
//! ```c
//! struct vtmouse_event {   /* 16 байт */
//!     int32_t  x;       /* пиксель поверхности сессии (не окна) */
//!     int32_t  y;       /* пиксель поверхности сессии */
//!     uint32_t buttons; /* bit0 ЛКМ, bit1 ПКМ, bit2 СКМ; абсолютное состояние */
//!     int32_t  wheel;   /* щелчки с прошлой записи; + = от пользователя (старее) */
//! };
//! ```
//!
//! Координаты — в пикселях **поверхности сессии**, а не окна: поверхность имеет
//! размер экрана (контракт `vtfb1` §5), клиент показывает её 1:1-вьюпортом
//! (с прокруткой) или необязательным целым зумом, и обратное преобразование
//! (`window_to_surface` + `View` в клиенте) — единственное место, где живут
//! масштаб и прокрутка.  Ядру не нужно знать геометрию окна.
//!
//! Запись может быть разорвана между `write(2)`: неполная запись остаётся в
//! буфере сессии и достраивается следующей (как у `vtinput`). Модуль `core`-only
//! и без состояния: host-свойство P68 исполняет ровно тот исходник, который
//! компилирует ядро.

/// Длина одной записи в потоке `/dev/vtmouse*`.
pub const EVENT_LEN: usize = 16;

/// Бит левой кнопки в `buttons`.
pub const BTN_LEFT: u32 = 1;
/// Бит правой кнопки.
pub const BTN_RIGHT: u32 = 2;
/// Бит средней кнопки.
pub const BTN_MIDDLE: u32 = 4;
/// Все определённые биты кнопок; остальные игнорируются (а не отвергаются:
/// новый композитор с дополнительной кнопкой не должен ломать ввод).
pub const BTN_MASK: u32 = BTN_LEFT | BTN_RIGHT | BTN_MIDDLE;

/// Одна запись ввода указателя.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VtMouseEvent {
    /// X в пикселях поверхности сессии.
    pub x: i32,
    /// Y в пикселях поверхности сессии.
    pub y: i32,
    /// Абсолютное состояние кнопок ([`BTN_LEFT`] и т. д.).
    pub buttons: u32,
    /// Вертикальные щелчки с прошлой записи: `+` — от пользователя (к более
    /// старым строкам), `-` — к пользователю.
    pub wheel: i32,
}

/// Декодировать одну запись; `None`, если байт меньше [`EVENT_LEN`].
pub fn decode_event(bytes: &[u8]) -> Option<VtMouseEvent> {
    if bytes.len() < EVENT_LEN {
        return None;
    }
    let i32_at = |at: usize| -> i32 {
        i32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    };
    Some(VtMouseEvent {
        x: i32_at(0),
        y: i32_at(4),
        buttons: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) & BTN_MASK,
        wheel: i32_at(12),
    })
}

/// Закодировать запись в 16 байт little-endian (для тестов и клиента).
pub fn encode_event(event: &VtMouseEvent) -> [u8; EVENT_LEN] {
    let mut out = [0u8; EVENT_LEN];
    out[0..4].copy_from_slice(&event.x.to_le_bytes());
    out[4..8].copy_from_slice(&event.y.to_le_bytes());
    out[8..12].copy_from_slice(&(event.buttons & BTN_MASK).to_le_bytes());
    out[12..16].copy_from_slice(&event.wheel.to_le_bytes());
    out
}

/// Прижать координату к `0..limit` (в пикселях).
///
/// Клиент присылает координаты поверхности, но окно может быть перетащено за
/// край экрана, а zoom — округлён: без клампа один щелчок дал бы `x` вне
/// поверхности и выделение «за кадром». `limit == 0` (поверхность ещё не
/// поднята) даёт `0`.
pub fn clamp_coord(value: i32, limit: usize) -> i32 {
    if limit == 0 {
        return 0;
    }
    let max = limit.saturating_sub(1).min(i32::MAX as usize) as i32;
    value.clamp(0, max)
}

/// Сборщик потока записей, переживающий разрыв `write(2)`.
///
/// Клиент вправе писать записи невыровненно (граница кадра, короткий счёт
/// `write`): неполный хвост остаётся здесь до следующей порции. Тот же
/// автомат, что у `vt_keys::KeyParser`, и по той же причине — состояние
/// принадлежит сессии, а не одному системному вызову.
#[derive(Clone, Copy, Debug)]
pub struct EventStream {
    pending: [u8; EVENT_LEN],
    len: u8,
}

impl Default for EventStream {
    fn default() -> Self {
        Self::new()
    }
}

impl EventStream {
    pub const fn new() -> EventStream {
        EventStream {
            pending: [0; EVENT_LEN],
            len: 0,
        }
    }

    /// Сбросить недособранную запись (выход/переоткрытие окна).
    pub fn reset(&mut self) {
        self.len = 0;
    }

    /// Длина недособранного хвоста в байтах.
    pub fn pending_len(&self) -> usize {
        self.len as usize
    }

    /// Скормить порцию байтов, вызывая `sink` для каждой целой записи.
    pub fn push(&mut self, bytes: &[u8], sink: &mut dyn FnMut(VtMouseEvent)) {
        let mut consumed = 0usize;
        while consumed < bytes.len() {
            let take = core::cmp::min(EVENT_LEN - self.len as usize, bytes.len() - consumed);
            let start = self.len as usize;
            self.pending[start..start + take].copy_from_slice(&bytes[consumed..consumed + take]);
            self.len += take as u8;
            consumed += take;
            if self.len as usize == EVENT_LEN {
                if let Some(event) = decode_event(&self.pending) {
                    sink(event);
                }
                self.len = 0;
            }
        }
    }
}

/// Есть ли в записи нажатая левая кнопка.
pub fn left_pressed(event: &VtMouseEvent) -> bool {
    event.buttons & BTN_LEFT != 0
}

/// Есть ли в записи нажатая правая кнопка.
pub fn right_pressed(event: &VtMouseEvent) -> bool {
    event.buttons & BTN_RIGHT != 0
}

/// Есть ли в записи нажатая средняя кнопка.
pub fn middle_pressed(event: &VtMouseEvent) -> bool {
    event.buttons & BTN_MIDDLE != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_roundtrip() {
        let event = VtMouseEvent {
            x: 1234,
            y: 567,
            buttons: BTN_LEFT | BTN_RIGHT,
            wheel: -3,
        };
        let bytes = encode_event(&event);
        assert_eq!(bytes.len(), EVENT_LEN);
        assert_eq!(decode_event(&bytes), Some(event));
        // Неизвестный бит кнопки отбрасывается, остальные поля целы.
        let mut noisy = bytes;
        noisy[8] |= 0x80;
        assert_eq!(decode_event(&noisy), Some(event));
    }

    #[test]
    fn short_record_is_none() {
        for len in 0..EVENT_LEN {
            assert!(decode_event(&[0u8; 16][..len]).is_none(), "len {len}");
        }
    }

    #[test]
    fn clamp_keeps_inside_surface() {
        assert_eq!(clamp_coord(-5, 1280), 0);
        assert_eq!(clamp_coord(0, 1280), 0);
        assert_eq!(clamp_coord(1279, 1280), 1279);
        assert_eq!(clamp_coord(5000, 1280), 1279);
        assert_eq!(clamp_coord(10, 0), 0);
    }

    #[test]
    fn button_helpers() {
        let event = VtMouseEvent {
            buttons: BTN_MIDDLE,
            ..Default::default()
        };
        assert!(!left_pressed(&event));
        assert!(!right_pressed(&event));
        assert!(middle_pressed(&event));
    }
}
