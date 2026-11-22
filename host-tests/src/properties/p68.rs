// Feature: terminal session pointer input (`/dev/vtmouse{N}`), property P68.
//
// Ошибка в этом ABI не падает: указатель «уезжает» на чужую клетку, колесо
// теряет щелчок или выделение начинается там, где пользователь не нажимал.
// Поэтому свойства фиксируют ровно то, на что опираются shell/`nano`/`paint`:
// раскладку записи, маску кнопок, кламп координат к поверхности и склейку
// потока, нарезанного по границам `write(2)`.

use proptest::prelude::*;

use crate::vt_mouse_abi::{
    clamp_coord, decode_event, encode_event, EventStream, VtMouseEvent, BTN_LEFT, BTN_MASK,
    BTN_MIDDLE, BTN_RIGHT, EVENT_LEN,
};

fn event() -> impl Strategy<Value = VtMouseEvent> {
    (
        -100_000i32..100_000,
        -100_000i32..100_000,
        0u32..16,
        -10_000i32..10_000,
    )
        .prop_map(|(x, y, buttons, wheel)| VtMouseEvent {
            x,
            y,
            buttons,
            wheel,
        })
}

proptest! {
    /// Кодирование и декодирование — взаимно обратны для любого значения;
    /// неизвестные биты кнопок отбрасываются, остальное не меняется.
    #[test]
    fn record_roundtrip(event in event()) {
        let bytes = encode_event(&event);
        prop_assert_eq!(bytes.len(), EVENT_LEN);
        let decoded = decode_event(&bytes).expect("16 bytes decode");
        prop_assert_eq!(decoded.x, event.x);
        prop_assert_eq!(decoded.y, event.y);
        prop_assert_eq!(decoded.buttons, event.buttons & BTN_MASK);
        prop_assert_eq!(decoded.wheel, event.wheel);
    }

    /// Короче 16 байт — не запись (а не «примерно запись»): иначе хвост
    /// предыдущего события стал бы координатой следующего.
    #[test]
    fn short_prefix_is_never_an_event(len in 0usize..EVENT_LEN) {
        let bytes = [0xABu8; EVENT_LEN];
        prop_assert!(decode_event(&bytes[..len]).is_none());
    }

    /// Кламп всегда оставляет координату внутри поверхности; нулевая
    /// поверхность (сессия ещё не поднята) даёт 0, а не мусор.
    #[test]
    fn clamp_is_total(value in any::<i32>(), limit in 0usize..20_000) {
        let clamped = clamp_coord(value, limit);
        if limit == 0 {
            prop_assert_eq!(clamped, 0);
        } else {
            prop_assert!(clamped >= 0);
            prop_assert!((clamped as usize) < limit);
        }
    }

    /// Поток, нарезанный в произвольных местах, даёт ровно тот же набор
    /// событий, что и запись целиком; недособранный хвост виден в `pending_len`
    /// и обнуляется ровно при целой записи.
    #[test]
    fn stream_survives_arbitrary_splits(
        events in prop::collection::vec(event(), 0..8),
        cuts in prop::collection::vec(0usize..EVENT_LEN * 2, 0..12),
    ) {
        let mut wire = Vec::new();
        for event in &events {
            wire.extend_from_slice(&encode_event(event));
        }
        let mut whole = Vec::new();
        let mut whole_stream = EventStream::new();
        whole_stream.push(&wire, &mut |event| whole.push(event));
        prop_assert_eq!(whole.len(), events.len());
        prop_assert_eq!(whole_stream.pending_len(), 0);

        let mut split = Vec::new();
        let mut stream = EventStream::new();
        let mut cursor = 0usize;
        let mut cut_index = 0usize;
        while cursor < wire.len() {
            let step = cuts
                .get(cut_index)
                .copied()
                .unwrap_or(EVENT_LEN)
                .clamp(1, wire.len() - cursor);
            stream.push(&wire[cursor..cursor + step], &mut |event| split.push(event));
            cursor += step;
            cut_index += 1;
            prop_assert!(stream.pending_len() < EVENT_LEN);
        }
        prop_assert_eq!(split, whole);
        prop_assert_eq!(stream.pending_len(), 0);
    }

    /// Кнопки независимы: любая комбинация трёх бит декодируется в те же три
    /// флага (shell различает ЛКМ/ПКМ/СКМ).
    #[test]
    fn buttons_are_three_independent_bits(mask in 0u32..8) {
        let event = VtMouseEvent {
            x: 0,
            y: 0,
            buttons: mask,
            wheel: 0,
        };
        let decoded = decode_event(&encode_event(&event)).expect("decode");
        prop_assert_eq!(decoded.buttons & BTN_LEFT != 0, mask & 1 != 0);
        prop_assert_eq!(decoded.buttons & BTN_RIGHT != 0, mask & 2 != 0);
        prop_assert_eq!(decoded.buttons & BTN_MIDDLE != 0, mask & 4 != 0);
    }

    /// Сброс потока забывает хвост: новое окно в том же слоте не должно
    /// достраивать запись из байтов закрытого.
    #[test]
    fn reset_forgets_pending(prefix in prop::collection::vec(any::<u8>(), 0..EVENT_LEN)) {
        let mut stream = EventStream::new();
        let mut seen = Vec::new();
        stream.push(&prefix, &mut |event| seen.push(event));
        prop_assert!(seen.is_empty());
        prop_assert!(stream.pending_len() < EVENT_LEN);
        stream.reset();
        prop_assert_eq!(stream.pending_len(), 0);
        let event = VtMouseEvent { x: 7, y: 9, buttons: BTN_LEFT, wheel: 1 };
        stream.push(&encode_event(&event), &mut |e| seen.push(e));
        prop_assert_eq!(seen, vec![event]);
    }
}
