//! Пиксельная поверхность offscreen-сессии терминального окна.
//!
//! Сессия `1..` рисует во **свою** копию экрана тех же размеров, что boot
//! (`framebuffer::dimensions()`), а композитор блитит её в окно. Здесь живёт
//! сама память: кадры PMM, запиненные pin-ссылкой (`pmm::cow_ref`) ровно как
//! поверхность `/dev/vtconsole0` ([`super::vtsurface`]).
//!
//! # Почему кадры, а не один буфер
//!
//! Клиент отображает поверхность через `mmap(PROT_READ, MAP_SHARED)` постранично:
//! syscall-слой мапит каждый кадр отдельным листом (как у `vtconsole0`), поэтому
//! кадры не обязаны быть физически смежными. Ядро пишет в них через HHDM
//! (`vmm::phys_to_virt`), то есть без отдельного окна kernel-VA: `flush`
//! раскладывает строку прямоугольника по страницам через чистую
//! [`super::fb_surface::chunk_at`].
//!
//! # Жизнь кадров
//!
//! Поверхность аллоцируется при первом спавне сессии и живёт до перезагрузки:
//! `munmap`/exit клиента отпускают **свою** ссылку отображения, но не кадры
//! поверхности. Переоткрытие окна (`open` после `exit`) переиспользует ту же
//! память — клиент обязан смотреть на `generation`, чтобы не принять новый
//! кадр за продолжение старого (это и есть смысл поля в ABI).
//!
//! # Поколение кадра
//!
//! `generation` инкрементируется на **каждый** flush сессии (контракт `vtfb1`),
//! а не на каждую операцию рисования: клиент по нему понимает, что поверхность
//! изменилась, и не блитит её зря.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use super::fb_surface::{self, Geometry};
use super::session_route::MAX_SESSIONS;
use crate::memory::{pmm, vmm};
use crate::sync::spinlock::Spinlock;

/// Число offscreen-сессий (все, кроме boot-сессии 0).
const SESSIONS: usize = MAX_SESSIONS - 1;

struct Surface {
    geometry: Geometry,
    /// Кадры в логическом порядке; `mmap` отдаёт их по одному.
    pages: Vec<u64>,
    /// Счётчик flush'ей: контракт `VTFBIOGET_INFO`.
    generation: AtomicU64,
}

/// Поверхности сессий; индекс — `session - 1` (boot-сессия сюда не входит).
static SURFACES: [Spinlock<Option<Surface>>; SESSIONS] = [const { Spinlock::new(None) }; SESSIONS];

/// Слот поверхности для номера сессии; `None` для boot-сессии.
fn slot(session: usize) -> Option<usize> {
    if session == 0 || session >= MAX_SESSIONS {
        None
    } else {
        Some(session - 1)
    }
}

/// Аллоцировать поверхность сессии (идемпотентно).
///
/// Возвращает геометрию поверхности. Повторный вызов для уже
/// аллоцированной сессии — no-op, который возвращает существующую геометрию:
/// переоткрытие окна переиспользует кадры (см. модульную документацию).
///
/// `None` — PMM не смог выдать кадры или геометрия невозможна; вызывающий
/// (`framebuffer::init_session`) тогда честно не поднимает консоль сессии, и
/// `open(/dev/vtfb1)` вернёт `ENODEV`, а не пустое окно.
pub fn alloc(session: usize, width: usize, height: usize, bpp: usize) -> Option<Geometry> {
    let index = slot(session)?;
    let geometry = fb_surface::geometry(width, height, bpp)?;
    let mut guard = SURFACES[index].lock();
    if let Some(ref surface) = *guard {
        return Some(surface.geometry);
    }
    let mut pages: Vec<u64> = Vec::with_capacity(geometry.pages);
    while pages.len() < geometry.pages {
        let Some(frame) = pmm::alloc_frame() else {
            // Откат: вернуть всё, что успели взять, чтобы неудачный спавн не
            // вычерпал пул (in-QEMU selftest сравнивает free frames до/после).
            for &frame in &pages {
                if pmm::cow_unref(frame) {
                    pmm::free_frame(frame);
                }
            }
            return None;
        };
        // SAFETY: кадр только что выделен и доступен через HHDM целиком.
        unsafe {
            core::ptr::write_bytes(vmm::phys_to_virt(frame) as *mut u8, 0, fb_surface::PAGE);
        }
        // Pin: поверхность живёт до перезагрузки, отображения клиента держат
        // свои ссылки отдельно.
        pmm::cow_ref(frame);
        pages.push(frame);
    }
    *guard = Some(Surface {
        geometry,
        pages,
        generation: AtomicU64::new(0),
    });
    Some(geometry)
}

/// Освободить поверхность сессии (in-QEMU selftest / перезагрузка сессии).
///
/// Возвращает кадры в PMM только когда ушла последняя pin-ссылка; если клиент
/// всё ещё держит отображение, кадры остаются, а поверхность становится
/// «пустой» — следующий `alloc` возьмёт новые.
pub fn free(session: usize) {
    let Some(index) = slot(session) else {
        return;
    };
    let taken = SURFACES[index].lock().take();
    if let Some(surface) = taken {
        for frame in surface.pages {
            if pmm::cow_unref(frame) {
                pmm::free_frame(frame);
            }
        }
    }
}

/// Информация о поверхности для `VTFBIOGET_INFO`/`mmap`-клиента.
///
/// `None` — поверхность не аллоцирована (сессия не запущена, окно не
/// открывалось), и `open`/`ioctl` обязаны ответить `ENODEV`.
pub fn info(session: usize) -> Option<super::vtfb_abi::VtfbInfo> {
    let geometry = geometry(session)?;
    Some(super::vtfb_abi::VtfbInfo {
        width: geometry.width as u32,
        height: geometry.height as u32,
        stride: geometry.stride as u32,
        bpp: 32,
        generation: generation(session),
    })
}

/// `read(2)`-снимок поверхности: 24-байтный заголовок, затем пиксели.
///
/// Заголовок собирается на момент чтения (в нём `generation`), пиксели — из
/// кадров. План раскладки — чистая [`super::vtfb_abi::read_plan`], поэтому
/// границы заголовка/пикселей не дублируются здесь руками.
pub fn read_snapshot(session: usize, offset: usize, out: &mut [u8]) -> usize {
    let Some(info) = info(session) else {
        return 0;
    };
    let (header_bytes, pixel_offset, pixel_bytes) =
        super::vtfb_abi::read_plan(&info, offset as u64, out.len() as u64);
    if header_bytes + pixel_bytes == 0 {
        return 0;
    }
    let mut done = 0usize;
    if header_bytes > 0 {
        let header = super::vtfb_abi::encode_info(&info);
        let from = offset.min(super::vtfb_abi::VTFB_INFO_SIZE);
        let take = header_bytes as usize;
        out[..take].copy_from_slice(&header[from..from + take]);
        done += take;
    }
    if pixel_bytes > 0 {
        let take = pixel_bytes as usize;
        let got = read_at(session, pixel_offset as usize, &mut out[done..done + take]);
        done += got;
    }
    done
}

/// Геометрия поверхности сессии, если она аллоцирована.
pub fn geometry(session: usize) -> Option<Geometry> {
    let index = slot(session)?;
    SURFACES[index].lock().as_ref().map(|s| s.geometry)
}

/// Текущее поколение поверхности (0, если поверхности нет).
pub fn generation(session: usize) -> u64 {
    let Some(index) = slot(session) else {
        return 0;
    };
    SURFACES[index]
        .lock()
        .as_ref()
        .map(|s| s.generation.load(Ordering::Relaxed))
        .unwrap_or(0)
}

/// Физический кадр страницы поверхности (для `mmap`/`read`).
pub fn page(session: usize, page: u64) -> Option<u64> {
    let index = slot(session)?;
    SURFACES[index]
        .lock()
        .as_ref()
        .and_then(|s| s.pages.get(page as usize).copied())
}

/// Скопировать прямоугольник shadow-буфера писателя в поверхность.
///
/// `back`/`pitch`/`bpp` — раскладка писателя (`FramebufferWriter::back`): шаг
/// может быть шире строки поверхности, поэтому копирование идёт построчно.
/// Каждый flush инкрементирует `generation` — это единственное место, где она
/// меняется.
///
/// Прямоугольник предварительно клипуется по геометрии поверхности; пустое
/// пересечение — no-op, который не двигает `generation` (клиент не должен
/// видеть «кадр», которого нет).
pub fn write_rect(
    session: usize,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    back: &[u8],
    pitch: usize,
    bpp: usize,
) {
    let Some(index) = slot(session) else {
        return;
    };
    if w == 0 || h == 0 {
        return;
    }
    let mut guard = SURFACES[index].lock();
    let Some(surface) = guard.as_mut() else {
        return;
    };
    let geometry = surface.geometry;
    if x >= geometry.width || y >= geometry.height {
        return;
    }
    let x1 = (x + w).min(geometry.width);
    let y1 = (y + h).min(geometry.height);
    let row_bytes = (x1 - x) * geometry.bpp;
    if row_bytes == 0 {
        return;
    }
    for row in y..y1 {
        let dst_base = row * geometry.stride + x * geometry.bpp;
        let src_base = row * pitch + x * bpp;
        let mut written = 0usize;
        while written < row_bytes {
            let (page_index, in_page, chunk) =
                fb_surface::chunk_at(dst_base + written, row_bytes - written);
            if chunk == 0 {
                break;
            }
            let Some(&frame) = surface.pages.get(page_index) else {
                break;
            };
            // SAFETY: `page_index` внутри поверхности, `in_page + chunk <= PAGE`
            // по построению `chunk_at`, а `src_base + written + chunk` внутри
            // shadow-буфера писателя (row_bytes уже клипнут по его ширине).
            unsafe {
                core::ptr::copy_nonoverlapping(
                    back.as_ptr().add(src_base + written),
                    (vmm::phys_to_virt(frame) as *mut u8).add(in_page),
                    chunk,
                );
            }
            written += chunk;
        }
    }
    surface.generation.fetch_add(1, Ordering::Relaxed);
}

/// Линейное чтение поверхности с `offset` (fallback `read(2)`).
///
/// Возвращает число фактически скопированных байт — не больше, чем осталось до
/// конца поверхности.
pub fn read_at(session: usize, offset: usize, out: &mut [u8]) -> usize {
    let Some(index) = slot(session) else {
        return 0;
    };
    let guard = SURFACES[index].lock();
    let Some(surface) = guard.as_ref() else {
        return 0;
    };
    let total = surface.geometry.total_bytes;
    if offset >= total {
        return 0;
    }
    let len = out.len().min(total - offset);
    let mut done = 0usize;
    while done < len {
        let (page_index, in_page, chunk) = fb_surface::chunk_at(offset + done, len - done);
        if chunk == 0 {
            break;
        }
        let Some(&frame) = surface.pages.get(page_index) else {
            break;
        };
        // SAFETY: как в `write_rect` — страница поверхности существует, кусок
        // внутри неё, приёмник — срез `out` длиной `len`.
        unsafe {
            core::ptr::copy_nonoverlapping(
                (vmm::phys_to_virt(frame) as *const u8).add(in_page),
                out[done..done + chunk].as_mut_ptr(),
                chunk,
            );
        }
        done += chunk;
    }
    done
}

/// Залить поверхность нулями (новый кадр не показывает пиксели старой сессии).
pub fn clear(session: usize) {
    let Some(index) = slot(session) else {
        return;
    };
    let guard = SURFACES[index].lock();
    let Some(surface) = guard.as_ref() else {
        return;
    };
    for &frame in &surface.pages {
        // SAFETY: страница поверхности — кадр PMM, доступный через HHDM.
        unsafe {
            core::ptr::write_bytes(vmm::phys_to_virt(frame) as *mut u8, 0, fb_surface::PAGE);
        }
    }
}
