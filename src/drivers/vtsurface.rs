//! Разделяемая VT-поверхность для `/dev/vtconsole0` (ABI v1).
//!
//! Kernel-VT/консоль остаются единственным владельцем сетки: они публикуют
//! снимок видимой матрицы в буфер из кадров PMM, а userspace-терминал читает его
//! через `mmap(..., PROT_READ, MAP_SHARED)` или `read(2)`. Раскладка байтов —
//! [`super::vt_abi`] (128-байтный заголовок + 16-байтные клетки, seqlock).
//!
//! ## Когерентность
//!
//! Все публикации — под одним `Spinlock<Option<Surface>>`: писатель увеличивает
//! `seq` (нечётный), пишет клетки/курсор, увеличивает `write_seq` и снова
//! `seq` (чётный). `read(2)` копирует под тем же локом, поэтому отдаёт целостный
//! снимок; mmap-читатель обязан пользоваться seqlock, как в ABI.
//!
//! ## Жизнь страниц
//!
//! Буфер аллоцируется один раз при boot (`init`), каждый кадр держит pin-ссылку
//! (`pmm::cow_ref`). Пользовательское отображение read-only добавляет свою
//! ссылку на лист (`VT_RO_BIT`, освобождается `munmap`/exit через `cow_unref`),
//! поэтому закрытие fd/отображения не может освободить поверхность под ядром.
//!
//! ## Стоимость
//!
//! Публикация включается только когда поверхность кем-то открыта
//! ([`activate`]); без клиента `publish_*` — один atomic load и возврат, так что
//! boot/shell без desktop не платят за фичу.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use super::framebuffer::{CHAR_HEIGHT, CHAR_WIDTH, STATUS_BAR_HEIGHT};
use super::vt_abi::{self, Cell, Header};
use crate::memory::{pmm, vmm};
use crate::sync::spinlock::Spinlock;

/// Геометрия поверхности для userspace (`PGVT_IOC_INFO`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VtConsoleInfo {
    pub cols: u16,
    pub rows: u16,
    pub total_bytes: u32,
}

struct Surface {
    cols: u16,
    rows: u16,
    total: u32,
    /// Кадры поверхности в логическом порядке; `mmap` отдаёт их по одному.
    pages: Vec<u64>,
    /// Чётный `seq` последней завершённой публикации.
    seq: u32,
    /// Счётчик завершённых публикаций (`write_seq`).
    write_seq: u32,
}

static SURFACE: Spinlock<Option<Surface>> = Spinlock::new(None);
/// Сколько открытых fd/активных читателей видят поверхность. 0 — публикация
/// выключена (один atomic load на клетку).
static ACTIVE: AtomicU32 = AtomicU32::new(0);

fn geometry() -> (u16, u16) {
    let (w, h) = crate::drivers::framebuffer::dimensions();
    let cols = if w == 0 {
        80
    } else {
        (w / CHAR_WIDTH).clamp(1, vt_abi::MAX_COLS as usize)
    };
    let rows = if h == 0 {
        25
    } else {
        (h.saturating_sub(STATUS_BAR_HEIGHT) / CHAR_HEIGHT).clamp(1, vt_abi::MAX_ROWS as usize)
    };
    (cols as u16, rows as u16)
}

impl Surface {
    fn new() -> Option<Self> {
        let (cols, rows) = geometry();
        let total = vt_abi::total_bytes(cols, rows)?;
        let page_count = (total as usize).div_ceil(4096);
        let mut pages: Vec<u64> = Vec::with_capacity(page_count);
        for _ in 0..page_count {
            let frame = match pmm::alloc_frame() {
                Some(f) => f,
                None => {
                    for &f in &pages {
                        if pmm::cow_unref(f) {
                            pmm::free_frame(f);
                        }
                    }
                    return None;
                }
            };
            // SAFETY: кадр только что выделен и доступен через HHDM.
            unsafe {
                core::ptr::write_bytes(vmm::phys_to_virt(frame) as *mut u8, 0, 4096);
            }
            // Pin кэша: поверхность живёт столько же, сколько ядро; читательские
            // отображения добавляют свои ссылки отдельно.
            pmm::cow_ref(frame);
            pages.push(frame);
        }
        let s = Surface {
            cols,
            rows,
            total,
            pages,
            seq: 0,
            write_seq: 0,
        };
        let header = Header::new(cols, rows)?;
        s.write_logical(0, &vt_abi::encode_header(&header));
        Some(s)
    }

    fn write_logical(&self, offset: u32, data: &[u8]) {
        let mut cur = offset as usize;
        let mut src = 0usize;
        while src < data.len() {
            let page = cur / 4096;
            let in_page = cur % 4096;
            let chunk = core::cmp::min(4096 - in_page, data.len() - src);
            let Some(&frame) = self.pages.get(page) else {
                return;
            };
            // SAFETY: страница поверхности — кадр PMM, доступный через HHDM;
            // `in_page + chunk <= 4096`.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    data[src..src + chunk].as_ptr(),
                    (vmm::phys_to_virt(frame) as *mut u8).add(in_page),
                    chunk,
                );
            }
            cur += chunk;
            src += chunk;
        }
    }

    fn read_logical(&self, offset: u32, out: &mut [u8]) -> usize {
        let total =
            core::cmp::min(out.len() as u64, self.total.saturating_sub(offset) as u64) as usize;
        let mut cur = offset as usize;
        let mut dst = 0usize;
        while dst < total {
            let page = cur / 4096;
            let in_page = cur % 4096;
            let chunk = core::cmp::min(4096 - in_page, total - dst);
            let Some(&frame) = self.pages.get(page) else {
                break;
            };
            // SAFETY: как в `write_logical`.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    (vmm::phys_to_virt(frame) as *const u8).add(in_page),
                    out[dst..dst + chunk].as_mut_ptr(),
                    chunk,
                );
            }
            cur += chunk;
            dst += chunk;
        }
        dst
    }

    fn put_u16(&self, offset: u32, v: u16) {
        self.write_logical(offset, &v.to_le_bytes());
    }

    fn begin(&mut self) {
        self.seq = vt_abi::seq_begin(self.seq);
        let seq = self.seq;
        if let Some(&page) = self.pages.first() {
            // SAFETY: заголовок лежит в первой странице поверхности.
            unsafe {
                core::ptr::write_unaligned(
                    (vmm::phys_to_virt(page) as *mut u8).add(8) as *mut u32,
                    seq,
                );
            }
        }
    }

    fn end(&mut self) {
        self.write_seq = self.write_seq.wrapping_add(1);
        let ws = self.write_seq;
        let seq = {
            self.seq = vt_abi::seq_end(self.seq);
            self.seq
        };
        if let Some(&page) = self.pages.first() {
            // SAFETY: заголовок — первая страница поверхности.
            unsafe {
                let hdr = vmm::phys_to_virt(page) as *mut u8;
                core::ptr::write_unaligned(hdr.add(60) as *mut u32, ws);
                core::ptr::write_unaligned(hdr.add(8) as *mut u32, seq);
            }
        }
    }

    fn set_cursor(&self, cx: u16, cy: u16, visible: bool, style: u8, top: u16, bot: u16) {
        let cx = cx.min(self.cols.saturating_sub(1));
        let cy = cy.min(self.rows.saturating_sub(1));
        let top = top.min(self.rows.saturating_sub(1));
        let bot = bot.min(self.rows.saturating_sub(1)).max(top);
        self.put_u16(20, cx);
        self.put_u16(22, cy);
        self.write_logical(24, &[visible as u8, style]);
        self.put_u16(26, top);
        self.put_u16(28, bot);
    }

    fn put_cell(&self, x: u16, y: u16, cell: &Cell) {
        if x >= self.cols || y >= self.rows {
            return;
        }
        let idx = (y as u32 * self.cols as u32 + x as u32) * vt_abi::CELL_SIZE as u32;
        self.write_logical(vt_abi::CELLS_OFFSET + idx, &vt_abi::encode_cell(cell));
    }
}

/// Аллоцировать поверхность один раз при boot. Без фреймбуфера используется
/// сетка 80×25 (ABI-фолбэк), чтобы устройство существовало и на serial-only.
pub fn init() {
    if SURFACE.lock().is_some() {
        return;
    }
    let Some(surface) = Surface::new() else {
        crate::warn!("[VT] shared surface allocation failed; /dev/vtconsole0 will be ENODEV");
        return;
    };
    let mut g = SURFACE.lock();
    if g.is_none() {
        *g = Some(surface);
    } else {
        // Проиграли гонку: вернуть кадры неудачливого аллокатора.
        for &f in &surface.pages {
            if pmm::cow_unref(f) {
                pmm::free_frame(f);
            }
        }
    }
}

fn ensure() -> bool {
    if SURFACE.lock().is_some() {
        return true;
    }
    init();
    SURFACE.lock().is_some()
}

/// Геометрия поверхности для `PGVT_IOC_INFO`/`mmap`; `None` — аллокация не
/// удалась (open вернёт ENODEV).
pub fn info() -> Option<VtConsoleInfo> {
    if !ensure() {
        return None;
    }
    SURFACE.lock().as_ref().map(|s| VtConsoleInfo {
        cols: s.cols,
        rows: s.rows,
        total_bytes: s.total,
    })
}

/// Физический кадр страницы поверхности (0-based логическая страница).
pub fn page(index: u64) -> Option<u64> {
    SURFACE
        .lock()
        .as_ref()
        .and_then(|s| s.pages.get(index as usize).copied())
}

/// Целостный снимок для `read(2)` fallback: копирует `out.len()` байт с
/// `offset`; возвращает фактическое число байт.
pub fn read_at(offset: u32, out: &mut [u8]) -> usize {
    if !ensure() {
        return 0;
    }
    SURFACE
        .lock()
        .as_ref()
        .map(|s| s.read_logical(offset, out))
        .unwrap_or(0)
}

/// Открыт ли сейчас читатель; пока 0, публикация клеток пропускается.
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed) != 0
}

/// Отметить открытие `/dev/vtconsole0`. На первом открытии публикует текущий
/// текст консоли, чтобы клиент сразу увидел shell, а не пустую сетку.
pub fn activate() -> bool {
    if !ensure() {
        return false;
    }
    if ACTIVE.load(Ordering::Relaxed) != 0 {
        ACTIVE.fetch_add(1, Ordering::Relaxed);
        return true;
    }
    // Снимок снимается до включения публикации: иначе наши же записи из
    // framebuffer-лока могли бы попасть в снимок дважды.
    let snapshot = crate::drivers::framebuffer::text_snapshot();
    ACTIVE.fetch_add(1, Ordering::Relaxed);
    if let Some(snap) = snapshot {
        publish_shell_full(
            &snap.cells,
            snap.cols as u16,
            snap.rows as u16,
            snap.cursor.0 as u16,
            snap.cursor.1 as u16,
            true,
        );
    }
    crate::info!("[VT] /dev/vtconsole0 attached: shared surface active");
    true
}

/// Закрытие читателя. Публикация выключается только когда закрыт последний fd;
/// уже взятые mmap-снимки остаются валидными до `munmap`.
pub fn deactivate() {
    // Saturating decrement без deprecated `fetch_update`; гонка двух close
    // может лишь оставить публикацию включённой, но не уводит счётчик ниже 0.
    if ACTIVE.load(Ordering::Relaxed) != 0 {
        ACTIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Сбросить счётчик для in-QEMU selftest (не ABI).
pub fn reset_for_tests() {
    ACTIVE.store(0, Ordering::Relaxed);
}

fn begin_publish(s: &mut Surface) {
    s.begin();
}

fn end_publish(s: &mut Surface) {
    s.end();
}

/// Опубликовать одну клетку shell-консоли (вызывается из framebuffer при
/// записи символа). Без активного читателя — no-op.
pub fn publish_shell_cell(col: usize, row: usize, ch: u8, fg: u32, bg: u32) {
    if !is_active() {
        return;
    }
    let mut g = SURFACE.lock();
    let Some(s) = g.as_mut() else { return };
    if col >= s.cols as usize || row >= s.rows as usize {
        return;
    }
    begin_publish(s);
    s.put_cell(col as u16, row as u16, &Cell::from_byte(ch, fg, bg));
    end_publish(s);
}

/// Опубликовать курсор shell-консоли.
pub fn publish_shell_cursor(col: usize, row: usize, visible: bool) {
    if !is_active() {
        return;
    }
    let mut g = SURFACE.lock();
    let Some(s) = g.as_mut() else { return };
    let rows = s.rows;
    begin_publish(s);
    s.set_cursor(
        col as u16,
        row as u16,
        visible,
        0,
        0,
        rows.saturating_sub(1),
    );
    end_publish(s);
}

/// Полный снимок shell-сетки (после scroll/clear/repaint).
pub fn publish_shell_full(
    cells: &[(u8, u32, u32)],
    cols: u16,
    rows: u16,
    cursor_x: u16,
    cursor_y: u16,
    visible: bool,
) {
    if !is_active() {
        return;
    }
    let mut g = SURFACE.lock();
    let Some(s) = g.as_mut() else { return };
    begin_publish(s);
    for row in 0..rows.min(s.rows) {
        for col in 0..cols.min(s.cols) {
            let idx = row as usize * cols as usize + col as usize;
            let Some(&(ch, fg, bg)) = cells.get(idx) else {
                continue;
            };
            s.put_cell(col, row, &Cell::from_byte(ch, fg, bg));
        }
    }
    let bottom = s.rows.saturating_sub(1);
    s.set_cursor(cursor_x, cursor_y, visible, 0, 0, bottom);
    end_publish(s);
}

/// Очистить всю поверхность (fallback `clear` консоли).
pub fn publish_shell_clear(fg: u32, bg: u32) {
    if !is_active() {
        return;
    }
    let mut g = SURFACE.lock();
    let Some(s) = g.as_mut() else { return };
    begin_publish(s);
    let blank = Cell::from_byte(b' ', fg, bg);
    for row in 0..s.rows {
        for col in 0..s.cols {
            s.put_cell(col, row, &blank);
        }
    }
    let bottom = s.rows.saturating_sub(1);
    s.set_cursor(0, 0, true, 0, 0, bottom);
    end_publish(s);
}

/// Опубликовать одну клетку kernel-VT (`vt.rs`).
pub fn publish_vt_cell(col: usize, row: usize, ch: u8, fg: u32, bg: u32) {
    publish_shell_cell(col, row, ch, fg, bg);
}

/// Опубликовать полный снимок kernel-VT после scroll/erase.
pub fn publish_vt_full(
    cells: &[(u8, u32, u32)],
    cols: u16,
    rows: u16,
    cursor_x: u16,
    cursor_y: u16,
    scroll_top: u16,
    scroll_bot: u16,
) {
    if !is_active() {
        return;
    }
    let mut g = SURFACE.lock();
    let Some(s) = g.as_mut() else { return };
    begin_publish(s);
    for row in 0..rows.min(s.rows) {
        for col in 0..cols.min(s.cols) {
            let idx = row as usize * cols as usize + col as usize;
            let Some(&(ch, fg, bg)) = cells.get(idx) else {
                continue;
            };
            s.put_cell(col, row, &Cell::from_byte(ch, fg, bg));
        }
    }
    s.set_cursor(cursor_x, cursor_y, true, 0, scroll_top, scroll_bot);
    end_publish(s);
}

/// Опубликовать курсор kernel-VT (позиция/scroll region меняются часто).
pub fn publish_vt_cursor(
    cursor_x: u16,
    cursor_y: u16,
    visible: bool,
    scroll_top: u16,
    scroll_bot: u16,
) {
    if !is_active() {
        return;
    }
    let mut g = SURFACE.lock();
    let Some(s) = g.as_mut() else { return };
    begin_publish(s);
    s.set_cursor(cursor_x, cursor_y, visible, 0, scroll_top, scroll_bot);
    end_publish(s);
}
