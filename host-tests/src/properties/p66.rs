// Feature: terminal kernel session (desktop-вариант B, второй shell), property P66.
//
// Две чистые половины этой фичи:
//
// * `session_route::route` — маршрут «задача → консольная сессия». Ошибка здесь
//   не падает: чужой вывод просто уезжает в другое окно (или в boot-консоль), и
//   это выглядит как «команда ничего не напечатала». Поэтому свойства
//   фиксируют приоритет (владелец сессии сильнее привязки), границу индекса и
//   идемпотентность.
// * `fb_surface` — арифметика offscreen-поверхности. Ошибка здесь — выход за
//   пределы кадра PMM (`PAGE` — жёсткая граница физической страницы), поэтому
//   свойства фиксируют геометрию (`stride == width*bpp`, `total == stride*height`,
//   `pages == ceil(total/PAGE)`) и то, что кусок никогда не пересекает страницу,
//   а полный обход покрывает ровно `total` байт.

use proptest::prelude::*;

use crate::fb_surface::{chunk_at, geometry, PAGE};
use crate::session_route::{route, BOOT_SESSION, MAX_SESSIONS};

fn owners() -> impl Strategy<Value = [u64; MAX_SESSIONS]> {
    prop::array::uniform(0u64..=8)
}

fn pids() -> impl Strategy<Value = u64> {
    prop_oneof![Just(0u64), Just(1u64), Just(2u64), 0u64..=8]
}

proptest! {
    /// Маршрут всегда возвращает существующую сессию.
    #[test]
    fn route_is_in_range(pid in pids(), own in owners(), bind in prop::option::of(0u64..12)) {
        let session = route(pid, &own, bind);
        prop_assert!(session < MAX_SESSIONS);
    }

    /// Собственная задача сессии остаётся в своей сессии, даже если кто-то
    /// записал для её pid привязку в чужое окно.
    #[test]
    fn owner_beats_bind(pid in 1u64..=8, own in owners(), bind in 0u64..12) {
        let mut owners = own;
        owners[1] = pid;
        prop_assert_eq!(route(pid, &owners, Some(bind)), 1);
    }

    /// Без владельца и без привязки задача принадлежит boot-сессии.
    #[test]
    fn unbound_is_boot(pid in pids(), own in owners()) {
        let mut owners = own;
        for slot in owners.iter_mut().skip(1) {
            if *slot == pid {
                *slot = 0;
            }
        }
        prop_assert_eq!(route(pid, &owners, None), BOOT_SESSION);
    }

    /// Привязка вне диапазона сессий — это отсутствие привязки, а не паника и не
    /// «следующий слот по модулю».
    #[test]
    fn out_of_range_bind_is_boot(bind in (MAX_SESSIONS as u64)..64) {
        prop_assert_eq!(route(3, &[0; MAX_SESSIONS], Some(bind)), BOOT_SESSION);
    }

    /// Идемпотентность: маршрут по результату не меняется (писатель не
    /// «переезжает» при повторном разрешении).
    #[test]
    fn route_is_idempotent(pid in pids(), own in owners(), bind in prop::option::of(0u64..MAX_SESSIONS as u64)) {
        let first = route(pid, &own, bind);
        prop_assert_eq!(route(pid, &own, bind), first);
    }

    /// Геометрия: шаг/размер/страницы согласованы.
    #[test]
    fn geometry_arithmetic(width in 1usize..4096, height in 1usize..2048, bpp in 3usize..=4) {
        let g = geometry(width, height, bpp).expect("valid geometry");
        prop_assert_eq!(g.stride, width * bpp);
        prop_assert_eq!(g.total_bytes, g.stride * height);
        prop_assert_eq!(g.pages, g.total_bytes.div_ceil(PAGE));
        prop_assert!(g.pages * PAGE >= g.total_bytes);
        prop_assert!((g.pages - 1) * PAGE < g.total_bytes);
    }

    /// Невозможная геометрия отвергается, а не «примерно считается».
    #[test]
    fn geometry_rejects_impossible(width in 0usize..4, height in 0usize..4, bpp in 0usize..=8) {
        if width == 0 || height == 0 || !(3..=4).contains(&bpp) {
            prop_assert!(geometry(width, height, bpp).is_none());
        }
    }

    /// Кусок никогда не пересекает границу страницы и не больше запроса.
    #[test]
    fn chunk_stays_inside_page(offset in 0usize..1_000_000, len in 0usize..8192) {
        let (_, in_page, chunk) = chunk_at(offset, len);
        prop_assert!(in_page < PAGE);
        prop_assert!(in_page + chunk <= PAGE);
        prop_assert!(chunk <= len);
        prop_assert_eq!(chunk == 0, len == 0);
    }

    /// Полный обход поверхности: куски складываются ровно в длину, номера
    /// страниц строго возрастают (нет зацикливания и нет дыр).
    #[test]
    fn chunk_walk_covers_request(width in 1usize..2048, height in 1usize..1024, bpp in 3usize..=4, skip in 0usize..PAGE, take in 1usize..(PAGE * 3)) {
        let g = geometry(width, height, bpp).expect("valid geometry");
        let offset = skip.min(g.total_bytes.saturating_sub(1));
        let len = take.min(g.total_bytes - offset);
        let mut cur = offset;
        let mut done = 0usize;
        let mut last_page = None;
        while done < len {
            let (page, _, chunk) = chunk_at(cur, len - done);
            prop_assert!(chunk > 0);
            if let Some(prev) = last_page {
                prop_assert!(page > prev);
            }
            last_page = Some(page);
            cur += chunk;
            done += chunk;
        }
        prop_assert_eq!(done, len);
        prop_assert_eq!(cur, offset + len);
    }
}