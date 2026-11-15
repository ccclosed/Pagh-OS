//! xHCI-контроллер USB — опрос без прерываний, ориентирован на HID.
//!
//! Драйвер работает с контроллером напрямую и подаёт отчёты в тот же путь
//! ввода, что и PS/2: машина с USB-клавиатурой не зависит от того, транслирует
//! ли прошивка USB HID в PS/2-сканкоды через SMM.
//!
//! Устройство опрашивается из одного потока ядра (`usb`, запускается в
//! `boot.rs`): маршрутизации прерываний и общего с таймером обработчика нет,
//! отказ выражается в остановке потока опроса, а не в порче кольца из IRQ.
//! Форма та же, что у `nvme.rs` — единственного другого драйвера с DMA-протоколом.
//!
//! xHCI — это набор колец в RAM, которые контроллер обходит по физическим
//! адресам: командное кольцо, кольцо событий на каждый interrupter, контекст
//! устройства на слот и transfer-кольца на эндпоинт. Всё это должно быть
//! физически непрерывным и обнулённым (`DmaFrame`/`Ring`); любое действие —
//! «записать TRB, позвонить в doorbell, дождаться события».
//!
//! Область: перечисление доводит устройство до сконфигурированных HID-
//! интерфейсов с interrupt-IN эндпоинтами — это всё, что нужно boot-клавиатурам
//! и мышам; [`crate::drivers::usb_hid`] превращает отчёты в поток сканкодов,
//! который уже потребляет shell. Каждый boot-интерфейс клавиатуры/мыши
//! привязывается (consumer/system-коллекции композитной клавиатуры пропускаются:
//! раскладки для них нет), все они конфигурируются одной командой Configure
//! Endpoint и опрашиваются вместе. Только корневые порты — устройство за хабом
//! недоступно (hub-драйвера нет). Не реализовано: изохронные передачи,
//! suspend/resume, teardown hot-unplug, USB 3 link power management, разбор
//! HID report descriptor (report-протокол — best effort, см. `usb_hid`),
//! output-отчёты для LED клавиатуры.

use alloc::vec;
use alloc::vec::Vec;

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::drivers::pci::PciDevice;
use crate::drivers::usb_desc::{self, HidIface, RndisIface};
use crate::drivers::usb_hid_report::{self, RecoveryAction, ReportMode};
use crate::drivers::usb_rndis::{self, RndisTransport};
use crate::drivers::usb_rndis_proto;
use crate::drivers::xhci_hub::{
    self, port_state_to_neutral, FailReason, Phase, PortObservation, PortPolicy, Step,
};
use crate::memory::pmm;
use crate::sync::spinlock::Spinlock;

const PAGE_SIZE: usize = 4096;
const TRB_SIZE: usize = 16;

// ─── DMA-кадры ───────────────────────────────────────────────────────────────

/// Один обнулённый DMA-кадр, выровненный на страницу; адресуется физически (для
/// контроллера) и через HHDM (для CPU).
struct DmaFrame {
    phys: u64,
    virt: usize,
}

impl Drop for DmaFrame {
    fn drop(&mut self) {
        crate::memory::pmm::free_frame(self.phys);
    }
}

impl DmaFrame {
    fn new_zeroed(what: &str) -> Option<Self> {
        let phys = pmm::alloc_frame()?;
        let virt = crate::memory::vmm::phys_to_virt(phys) as usize;
        // SAFETY: PMM только что выдал этот эксклюзивный кадр, отображённый в HHDM,
        // поэтому обнуление всей страницы корректно.
        unsafe { core::ptr::write_bytes(virt as *mut u8, 0, PAGE_SIZE) };
        crate::debug!("xhci: dma frame {} phys={:#x} virt={:#x}", what, phys, virt);
        Some(Self { phys, virt })
    }

    fn phys(&self) -> u64 {
        self.phys
    }

    fn virt(&self) -> usize {
        self.virt
    }
}

// ─── Часы ────────────────────────────────────────────────────────────────────

/// Тиков TSC на микросекунду, измеряется один раз при загрузке. 0 — часы непригодны.
///
/// Все ожидания драйвера выражены в микросекундах: USB2-порт на реальном PCH
/// тратит на reset десятки миллисекунд, а бюджет спин-циклов под QEMU может
/// истечь посреди операции. Boot-меню опрашивает драйвер с выключенными
/// прерываниями (поэтому `scheduler::ticks` не растёт), но `rdtsc` работает в
/// любом контексте; калибровка по PIT при загрузке превращает каждое ожидание в
/// реальный дедлайн.
static TSC_PER_US: AtomicU64 = AtomicU64::new(0);

fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: `rdtsc` только читает счётчик меток времени, побочных эффектов памяти
    // нет; компилятору это сообщают (`nomem`), стек не используется.
    unsafe {
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack));
    }
    ((hi as u64) << 32) | lo as u64
}

/// Измерить TSC по 10-мс одноразовому таймеру PIT канала 2.
///
/// Вызывается из `init_usb` до того, как boot-меню начнёт использовать канал 2
/// для своего отсчёта, поэтому пользователи не пересекаются.
fn calibrate_clock() -> bool {
    use x86_64::instructions::port::Port;
    const PIT_HZ: u64 = 1_193_182;
    // 10-мс одноразовый запуск канала 2.
    let count = (PIT_HZ / 100) as u16 - 1;
    // SAFETY: порт 0x61 управляет каналом 2 PIT и отражает его выход; 0x43/0x42 —
    // командный порт PIT и порт данных канала 2. Значение gate восстанавливается на
    // каждом пути возврата, динамик не включается (бит 1 остаётся 0).
    unsafe {
        let mut gate: Port<u8> = Port::new(0x61);
        let mut cmd: Port<u8> = Port::new(0x43);
        let mut ch2: Port<u8> = Port::new(0x42);
        let saved = gate.read();
        gate.write((saved & !0x02) | 0x01);
        cmd.write(0b1011_0000);
        ch2.write((count & 0xFF) as u8);
        ch2.write((count >> 8) as u8);
        let start = rdtsc();
        let mut spins = 0u32;
        while gate.read() & 0x20 == 0 {
            spins += 1;
            if spins > 50_000_000 {
                gate.write(saved);
                crate::warn!("xhci: no clock (PIT channel 2 never timed out)");
                return false;
            }
            core::hint::spin_loop();
        }
        let end = rdtsc();
        gate.write(saved);
        // Окно 10 мс, поэтому тиков в секунду — дельта, умноженная на 100.
        let hz = end.wrapping_sub(start).saturating_mul(100);
        if !(1_000_000..=100_000_000_000).contains(&hz) {
            crate::warn!("xhci: no clock (implausible TSC rate {})", hz);
            return false;
        }
        TSC_PER_US.store((hz / 1_000_000).max(1), Ordering::Relaxed);
        crate::debug!("xhci: clock: TSC ~{} MHz", hz / 1_000_000);
    }
    true
}

fn clock_ready() -> bool {
    TSC_PER_US.load(Ordering::Relaxed) != 0
}

/// Микросекунды от произвольной эпохи; смысл имеют только разности.
fn now_us() -> u64 {
    let per_us = TSC_PER_US.load(Ordering::Relaxed);
    if per_us == 0 {
        return 0;
    }
    rdtsc() / per_us
}

/// Занять шину не менее `us` микросекунд там, где спецификация требует задержки
/// (USB2 reset recovery, power settle порта, таймаут handoff прошивки).
fn settle_us(us: u64) {
    if clock_ready() {
        let deadline = now_us() + us;
        while now_us() < deadline {
            core::hint::spin_loop();
        }
    } else {
        // Без калиброванных часов: запасной вариант — бюджет спин-циклов.
        spin_settle((us.saturating_mul(3)).min(u32::MAX as u64) as u32);
    }
}

// ─── TRB ─────────────────────────────────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Trb {
    dw0: u32,
    dw1: u32,
    dw2: u32,
    dw3: u32,
}

const _: () = assert!(core::mem::size_of::<Trb>() == TRB_SIZE);

impl Trb {
    fn new(dw0: u32, dw1: u32, dw2: u32, dw3: u32) -> Self {
        Self { dw0, dw1, dw2, dw3 }
    }

    fn trb_type(&self) -> u8 {
        ((self.dw3 >> 10) & 0x3F) as u8
    }

    fn completion_code(&self) -> u8 {
        ((self.dw2 >> 24) & 0xFF) as u8
    }

    /// Остаточная длина передачи из transfer event.
    fn residual(&self) -> u32 {
        self.dw2 & 0x00FF_FFFF
    }

    fn slot_id(&self) -> u8 {
        ((self.dw3 >> 24) & 0xFF) as u8
    }

    fn endpoint_id(&self) -> u8 {
        ((self.dw3 >> 16) & 0x1F) as u8
    }
}

// Типы TRB (xHCI 1.2, таблица 6-91).
const TRB_NORMAL: u8 = 1;
const TRB_SETUP: u8 = 2;
const TRB_DATA: u8 = 3;
const TRB_STATUS: u8 = 4;
const TRB_LINK: u8 = 6;
const TRB_ENABLE_SLOT: u8 = 9;
const TRB_DISABLE_SLOT: u8 = 10;
const TRB_ADDRESS_DEVICE: u8 = 11;
const TRB_CONFIGURE_ENDPOINT: u8 = 12;
const TRB_EVALUATE_CONTEXT: u8 = 13;
/// Reset Endpoint (xHCI 1.2, таблица 6-91): возвращает остановленный эндпоинт в
/// работу, чтобы после babble/stall переармировать кольцо, а не отключать ввод до
/// перезагрузки.
const TRB_RESET_ENDPOINT: u8 = 14;
const TRB_TRANSFER_EVENT: u8 = 32;
const TRB_COMMAND_COMPLETION: u8 = 33;
const TRB_PORT_STATUS_CHANGE: u8 = 34;

// Коды завершения (таблица 6-93).
const CC_SUCCESS: u8 = 1;
const CC_SHORT_PACKET: u8 = 13;

const TRB_CYCLE: u32 = 1 << 0;
/// Link TRB Toggle Cycle: контроллер переключает свой consumer cycle bit, когда
/// следует по Link TRB в начало кольца.
const TRB_TOGGLE: u32 = 1 << 1;
const TRB_IOC: u32 = 1 << 5;
const TRB_IDT: u32 = 1 << 6;
const TRB_DIR_IN: u32 = 1 << 16;
/// Поле Transfer Type в Setup TRB: 0 — без данных, 2 — IN, 3 — OUT.
const TRT_NO_DATA: u32 = 0 << 16;
const TRT_IN: u32 = 2 << 16;
const TRT_OUT: u32 = 3 << 16;

/// Ёмкость кольца в TRB (в страницу помещается 256, поэтому 64 оставляет место для link).
const RING_TRBS: usize = 64;
const EVENT_TRBS: usize = 256;

/// Кольцо TRB, по которому ходит контроллер.
struct Ring {
    frame: DmaFrame,
    enqueue: usize,
    dequeue: usize,
    cycle: u32,
    /// Только interrupt-IN: Normal TRB считается незавершённым, пока не пришло
    /// transfer event. Переармирование без ожидания поставило бы второе чтение в тот
    /// же буфер, и отчёты гоняли бы друг друга.
    outstanding: bool,
}

impl Ring {
    fn new(what: &str) -> Option<Self> {
        Some(Self {
            frame: DmaFrame::new_zeroed(what)?,
            enqueue: 0,
            dequeue: 0,
            cycle: TRB_CYCLE,
            outstanding: false,
        })
    }

    fn ptr(&self, index: usize) -> *mut Trb {
        (self.frame.virt() + index * TRB_SIZE) as *mut Trb
    }

    fn write(&self, index: usize, trb: Trb) {
        // SAFETY: `index < RING_TRBS`, а в страницу помещается 256 TRB, поэтому запись
        // остаётся внутри нашего DMA-кадра; volatile, потому что контроллер читает эти
        // байты без участия CPU.
        unsafe { write_volatile(self.ptr(index), trb) }
    }

    fn read(&self, index: usize) -> Trb {
        // SAFETY: как в `write`.
        unsafe { read_volatile(self.ptr(index)) }
    }

    fn base_phys(&self) -> u64 {
        self.frame.phys()
    }

    /// Добавить один TRB с cycle bit производителя и сдвинуть enqueue, установив в
    /// конце кольца замыкающий Link TRB.
    fn push(&mut self, dw0: u32, dw1: u32, dw2: u32, dw3: u32) {
        let index = self.enqueue;
        self.write(index, Trb::new(dw0, dw1, dw2, dw3 | self.cycle));
        self.enqueue += 1;
        if self.enqueue >= RING_TRBS - 1 {
            // ЗАМЫКАНИЕ КОЛЬЦА. Link TRB принадлежит текущему циклу производителя — так
            // контроллер распознаёт его готовым. `TRB_TOGGLE` велит контроллеру переключить
            // consumer cycle bit при переходе по ссылке, и только после этого производитель
            // переключает свой бит для TRB в начале кольца.
            //
            // Без `TRB_TOGGLE` (или с cycle bit следующего цикла) контроллер пропускает
            // ссылку, и после RING_TRBS-1 передач эндпоинт замолкает. `TRB_IOC` не является
            // полем Link TRB; для замыкания нужен toggle-бит.
            let next_cycle = if self.cycle == TRB_CYCLE {
                0
            } else {
                TRB_CYCLE
            };
            self.write(
                RING_TRBS - 1,
                Trb::new(
                    self.base_phys() as u32,
                    (self.base_phys() >> 32) as u32,
                    0,
                    ((TRB_LINK as u32) << 10) | TRB_TOGGLE | self.cycle,
                ),
            );
            self.enqueue = 0;
            self.cycle = next_cycle;
        }
    }

    /// Сторона кольца событий: опубликовал ли контроллер событие, которое CPU ещё не
    /// забрал? Свежий TRB от устаревшего содержимого кольца отличает cycle bit.
    fn event_pending(&self) -> bool {
        let trb = self.read(self.dequeue);
        (trb.dw3 & TRB_CYCLE) == self.cycle && trb.trb_type() != 0
    }

    fn next_event(&mut self) -> Trb {
        let trb = self.read(self.dequeue);
        self.dequeue += 1;
        if self.dequeue >= EVENT_TRBS {
            self.dequeue = 0;
            self.cycle = if self.cycle == TRB_CYCLE {
                0
            } else {
                TRB_CYCLE
            };
        }
        trb
    }
}

// ─── Регистры ────────────────────────────────────────────────────────────────

const CAP_CAPLENGTH: usize = 0x00;
const CAP_HCSPARAMS1: usize = 0x04;
const CAP_HCSPARAMS2: usize = 0x08;
const CAP_HCCPARAMS1: usize = 0x10;
const CAP_DBOFF: usize = 0x14;
const CAP_RTSOFF: usize = 0x18;

const OP_USBCMD: usize = 0x00;
const OP_USBSTS: usize = 0x04;
const OP_CRCR: usize = 0x18;
const OP_DCBAAP: usize = 0x30;
const OP_CONFIG: usize = 0x38;
const OP_PORTS: usize = 0x400;

/// Байт между соседними наборами регистров порта.
///
/// У порта четыре 32-битных регистра (PORTSC, PORTPMSC, PORTLI, PORTHLPMC),
/// поэтому шаг — 16. При шаге 4 порты 2..=8 читали и писали соседние регистры, и
/// USB2-устройство не обнаруживалось.
const OP_PORT_STRIDE: usize = 0x10;

/// Смещение PORTSC для номера порта с единицы.
///
/// Арифметика закреплена const-assert'ом ниже: такая константа незаметна, пока не
/// появится железо.
const fn portsc(port: u8) -> usize {
    OP_PORTS + (port as usize - 1) * OP_PORT_STRIDE
}

// Порт 1 — 0x400, каждый порт занимает 16 байт: порт 5 (первый USB2 на
// контроллере 4+4) — 0x440.
const _: () = assert!(portsc(1) == 0x400);
const _: () = assert!(portsc(5) == 0x440);

const USBCMD_RS: u32 = 1 << 0;
const USBCMD_HCRST: u32 = 1 << 1;
const USBSTS_HCH: u32 = 1 << 0;
const USBSTS_HSE: u32 = 1 << 1;
const USBSTS_HCE: u32 = 1 << 11;
const USBSTS_CNR: u32 = 1 << 12;

/// Значения Port Link State, с которыми работает драйвер (xHCI 1.2, таблица 5-27).
const PLS_U0: u32 = 0;

const PORTSC_CCS: u32 = 1 << 0;
const PORTSC_PED: u32 = 1 << 1;
const PORTSC_PR: u32 = 1 << 4;
const PORTSC_PLS_MASK: u32 = 0xF << 5;
const PORTSC_PLS_SHIFT: u32 = 5;
const PORTSC_PP: u32 = 1 << 9;
/// Port Link State записывается только вместе с этим битом; одна запись поля
/// игнорируется контроллером.
const PORTSC_LWS: u32 = 1 << 16;
const PORTSC_SPEED_MASK: u32 = 0xF << 10;
const PORTSC_SPEED_SHIFT: u32 = 10;
const PORTSC_CSC: u32 = 1 << 17;
const PORTSC_WRC: u32 = 1 << 19;
const PORTSC_PRC: u32 = 1 << 21;
const PORTSC_WPR: u32 = 1 << 31;
// Каждая запись PORTSC идёт через `xhci_hub::port_state_to_neutral()`
// (`xhci_hub_state_to_neutral` из `xhci-hub.c`): запись прочитанного значения
// возвращает write-1-to-clear бит `PED`, а PED=1 отключает порт. PR (бит 4),
// WPR (31), LWS (16) и change-биты (17-23) тоже пишутся нулём, если действие не
// является целью записи.

const RTS_IR0: usize = 0x20;
const IR_ERSTSZ: usize = 0x08;
const IR_ERSTBA: usize = 0x10;
const IR_ERDP: usize = 0x18;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ErstEntry {
    base: u64,
    size: u32,
    _reserved: u32,
}

// Позиции полей контекста (xHCI 1.2 §6.2.2).
const SLOT_CTX_ENTRIES_SHIFT: u32 = 27;
const SLOT_CTX_SPEED_SHIFT: u32 = 20;
const SLOT_CTX_PORT_SHIFT: u32 = 16;
const EP_CTX_CERR_SHIFT: u32 = 1;
const EP_CTX_EP_TYPE_SHIFT: u32 = 3;
const EP_CTX_MAX_PACKET_SHIFT: u32 = 16;
const EP_CTX_INTERVAL_SHIFT: u32 = 16;
const EP_TYPE_CONTROL: u32 = 4;
const EP_TYPE_INTERRUPT_IN: u32 = 7;
const EP_TYPE_BULK_OUT: u32 = 2;
const EP_TYPE_BULK_IN: u32 = 6;

/// Speed id из PORTSC (таблица 5-27).
const SPEED_LOW: u8 = 2;
const SPEED_FULL: u8 = 1;
const SPEED_HIGH: u8 = 3;
const SPEED_SUPER: u8 = 4;

// ─── Слоты ───────────────────────────────────────────────────────────────────

/// Один HID interrupt-IN эндпоинт: кольцо, буфер отчёта, DCI и режим, который
/// оставил SET_PROTOCOL.
struct HidEndpoint {
    /// Номер интерфейса — для диагностических строк.
    iface: u8,
    /// `bInterfaceSubClass` (1 = boot) и `bInterfaceProtocol` (1 — клавиатура,
    /// 2 — мышь): это нужно разборщику отчётов.
    subclass: u8,
    protocol: u8,
    /// Boot или report protocol — как решил SET_PROTOCOL.
    mode: ReportMode,
    /// Индекс эндпоинта в device context (2 * номер эндпоинта + направление).
    dci: u8,
    ring: Ring,
    buf: DmaFrame,
    /// Размер буфера передачи: не меньше `wMaxPacketSize` эндпоинта, иначе отчёт
    /// длиннее 4 байт вызовет babble.
    len: usize,
    /// Попытки Reset Endpoint после конфигурации (не больше
    /// `usb_hid_report::MAX_RECOVERIES`).
    recoveries: u8,
    /// Отчёты короче минимального размера раскладки — для счётчика диагностики.
    bad_reports: u64,
    /// Признак, что отчёт неверного размера уже залогирован: одна строка, а не на
    /// каждый опрос, чтобы сломанное устройство не заливало serial.
    bad_report_logged: bool,
    /// Выставляется, когда бюджет восстановлений исчерпан; эндпоинт больше не опрашивается.
    disabled: bool,
}

/// Номер эндпоинта в device context: 2 * номер + направление (IN — 1).
fn dci_of(ep_addr: u8) -> u8 {
    (ep_addr & 0x0F) * 2 + u8::from(ep_addr & 0x80 != 0)
}

/// Один RNDIS-канал на устройстве: bulk-пара, её кольца и буферы передачи.
///
/// Уведомления communication-интерфейса не опрашиваются: ответы RNDIS драйвер
/// забирает GET_ENCAPSULATED_RESPONSE на EP0, а события link state не влияют на
/// готовность канала (после успешного SET filter он уже data-initialized).
struct RndisEndpoint {
    iface: RndisIface,
    bulk_in_dci: u8,
    bulk_out_dci: u8,
    /// Кольцо и буфер приёма; один Normal TRB в полёте за раз.
    in_ring: Ring,
    in_buf: DmaFrame,
    in_len: usize,
    in_outstanding: bool,
    /// Кольцо и буфер передачи; сюда пишется 44-байтный конверт и кадр.
    out_ring: Ring,
    out_buf: DmaFrame,
    out_len: usize,
    out_outstanding: bool,
    in_recoveries: u8,
    out_recoveries: u8,
    /// Эндпоинт отключён после исчерпания Reset Endpoint; опрос прекращается, чтобы
    /// мёртвое устройство не заливало serial и не жгло кольцо.
    in_disabled: bool,
    out_disabled: bool,
    /// Ошибка уже залогирована: одна строка на эндпоинт, а не на каждый poll.
    in_error_logged: bool,
    out_error_logged: bool,
    /// Xid для keepalive; обычные управляющие запросы идут при подъёме.
    keepalive_xid: u32,
}

/// Одно адресованное устройство.
struct Slot {
    id: u8,
    port: u8,
    speed: u8,
    ep0: Ring,
    input_ctx: DmaFrame,
    device_ctx: DmaFrame,
    /// Буфер данных для control-передач (по одной за раз, опрос однопоточный).
    ctrl_buf: DmaFrame,
    /// Все привязанные HID interrupt-IN эндпоинты устройства. У композитного
    /// устройства их несколько (клавиатура + consumer/system-коллекции).
    hid: Vec<HidEndpoint>,
    /// RNDIS-канал, если конфигурация его выставила.
    rndis: Option<RndisEndpoint>,
}

/// Что драйвер узнал о порте. Boot-меню печатает это: на машине с мёртвой
/// клавиатурой и без serial здесь весь отчёт о проблеме.
///
/// Решения по таймингам живут в [`PortPolicy`] (host-тесты, см.
/// `src/drivers/xhci_hub.rs`); эта структура — копия для вывода плюс снимок
/// link-состояния порта, который политика сама напечатать не может.
#[derive(Clone, Copy)]
struct PortDiag {
    /// Последняя достигнутая стадия порта (`""` до первой попытки).
    stage: &'static str,
    /// Код завершения (или значение PORTSC), которым она закончилась.
    code: u32,
    /// Скорость, полученная при последнем успешном adopt/reset.
    speed: u8,
    /// Reset-попыток всего (`try=`; политика повторяет с backoff бесконечно).
    attempts: u8,
    /// PORTSC непосредственно перед последним reset и сразу после него. Эти два
    /// значения отделяют «порт не включился» от «устройство не ответило после
    /// включения».
    pre: u32,
    post: u32,
    /// Наблюдались ли PRC / PED за последнее ожидание reset. `seen_prc` = false
    /// означает, что контроллер даже не подтвердил запрос сброса.
    seen_prc: bool,
    seen_ped: bool,
    /// PED виден вместе с PLS=U0 и валидной скоростью. Один `ped_sc` этого не
    /// говорит: при reset-е PLS=Polling (или скорости нет), поэтому бит отличает
    /// U0-сэмпл от кратковременного PED.
    seen_ped_u0: bool,
    /// Поколение USB порта: 2, 3 или 0, если контроллер его не описал (Supported
    /// Protocol capabilities задают диапазоны USB2 и USB3; клавиатура — USB2, даже
    /// если порт выглядит как USB3).
    proto: u8,
    /// PORTSC на последнем PED+U0; если порт так и не дошёл до U0 — на последнем
    /// кратковременном PED. По построению на порт: у каждого `PortDiag` своя
    /// `PortPolicy`, общих значений и индексов нет.
    ped_sc: u32,
    /// Длительность последней попытки в миллисекундах. Ноль, если часов нет.
    ms: u16,
    /// Состояние-машина, проверенная host-тестами: фаза, дедлайны, последовательность
    /// debounce/reset по `hub.c`, backoff и снимки PRC/CSC/PED на попытку.
    policy: PortPolicy,
}

struct Controller {
    op: usize,
    db: usize,
    rt: usize,
    ctx_size: usize,
    /// Найденные на PCI xHCI-контроллеры (этот — выбранный).
    controllers: usize,
    ports: u8,
    cmdr: Ring,
    event: Ring,
    erst: DmaFrame,
    /// Массив базовых адресов device context (DCBAA). Контроллер записывает в этот
    /// кадр указатель device context при Enable Slot и читает его в каждой следующей
    /// команде слота, поэтому кадр принадлежит контроллеру, а не стеку функции:
    /// локальная страница вернулась бы в PMM сразу после `start_controller`. На
    /// реальном железе первая же аллокация, переиспользовавшая страницу, даёт
    /// контроллеру мусорный device context — Host Controller Error и halt посреди
    /// загрузки, замораживающий все порты.
    dcbaa: DmaFrame,
    /// Базовый адрес массива device context (DCBAAP); хранится для release-пути.
    dcbaa_phys: u64,
    /// Массив scratchpad-буферов (на него указывает DCBAA[0]) и его страницы. Оба
    /// должны жить дольше контроллера по той же причине, что и `dcbaa`.
    #[allow(dead_code)] // kept alive: DMA state the controller reads
    scratchpad_array: Option<DmaFrame>,
    #[allow(dead_code)] // kept alive: DMA state the controller reads
    scratchpad_pages: Vec<DmaFrame>,
    slots: Vec<Slot>,
    /// Отчётов, переданных в `usb_hid`, на текущий момент.
    reports: u64,
    /// Устройств перечислено на текущий момент.
    devices: u64,
    /// Порты, поднятые и сброшенные за загрузку. Порт поднимается ровно один раз:
    /// дальше о нём сообщает только состояние подключения.
    brought_up: Vec<u8>,
    /// Состояние перечисления каждого порта для экранной диагностики.
    diag: Vec<PortDiag>,
    /// Последняя ошибка, для однострочного статуса.
    last_fail: Option<(u8, &'static str, u32)>,
    /// Маска подключённых портов до сброса контроллера. Порты, которые были тогда и
    /// отсутствуют сейчас, потерял сброс.
    probe_mask: u32,
    /// Курсор циклического обхода: за один poll инициализируется не больше одного
    /// порта, чтобы медленный reset не останавливал отсчёт boot-меню.
    next_port: u8,
    /// Монотонные микросекунды для политики порта, когда калибровка TSC не удалась
    /// (1 мс на poll). Дедлайны политики в микросекундах; без реальных часов счётчик
    /// опросов — лучшая доступная база времени, и он намного лучше, чем считать все
    /// дедлайны уже истёкшими.
    fallback_us: u64,
    /// USB-поколение порта (2 или 3) из Supported Protocol capabilities — для
    /// экранной диагностики. Дублируется в `PortDiag.proto` (это печатают экраны) и
    /// хранится здесь для shell-команды `usb`.
    #[allow(dead_code)]
    protos: Vec<u8>,
    /// Маски портов ([usb2, usb3]) из Supported Protocol capabilities. Именно маски,
    /// а не (first, count): контроллер может сообщить два диапазона с перекрытием, и
    /// маска показывает это честно, не пряча за красивым диапазоном.
    proto_masks: [u32; 2],
    /// Сырые записи extended capabilities — для дампа на экран.
    xcaps: Vec<XCap>,
    /// CAPLENGTH, прочитанный при инициализации (дамп показывает, там ли
    /// operational base, где думает драйвер).
    caplength: u32,
    hciversion: u32,
    hcsparams1: u32,
    hccparams1: u32,
    /// Результат BIOS/USB-legacy handoff: `handoff`, `bios-owned`, `absent` или
    /// `bad-xecp`.
    legacy: &'static str,
    /// Безвредный регистр записан и прочитан при инициализации: это отделяет «наши
    /// записи игнорируются» от «контроллер игнорирует именно эту запись».
    writes_ok: bool,
    /// PCI command register при инициализации (бит 1 — memory space, бит 2 — bus
    /// mastering).
    pci_cmd: u32,
    /// Сколько раз контроллер найден остановленным (USBSTS.HCH) и значения, это
    /// доказавшие. Остановленный контроллер игнорирует все записи портов, что
    /// выглядит как «порты не включаются»; этот счётчик — разница.
    halts: u32,
    halt_cmd: u32,
    halt_sts: u32,
    /// Transfer events, пришедшие, пока команда (Reset Endpoint, Configure Endpoint,
    /// …) ждала своего завершения. `command()` не может их выбросить: эндпоинт
    /// навсегда остался бы «outstanding» и никогда не переармировался. Цикл опроса
    /// осушает очередь перед кольцом событий.
    deferred_events: Vec<Trb>,
    /// Успешных восстановлений Reset Endpoint за загрузку — для диагностики.
    hid_recoveries: u64,
    /// Отчёты короче минимального размера раскладки — для диагностики.
    hid_bad_reports: u64,
}

/// Одна запись extended capability, как её увидел обход: смещение в BAR, id,
/// смещение следующей capability и два dword'а, в которых Supported Protocol
/// хранит диапазон портов. Печатается на экране, когда диапазоны выглядят
/// неверно.
#[derive(Clone, Copy)]
pub struct XCap {
    pub offset: u32,
    pub id: u8,
    pub next: u8,
    /// Major revision из DW0, биты 31:24 (2 — USB2 protocol, 3 — USB3).
    pub major: u8,
    /// Minor revision из DW0, биты 23:16.
    pub minor: u8,
    pub dw1: u32,
    pub dw2: u32,
}

static CONTROLLER: Spinlock<Option<Controller>> = Spinlock::new(None);

/// Сколько xHCI-контроллеров увидело PCI-перечисление; хранится, даже если ни
/// один не запустился: экранной диагностике нужно отличать «нет контроллера» от
/// «контроллер есть, но устройств нет».
static CONTROLLERS_SEEN: AtomicUsize = AtomicUsize::new(0);

/// Один PCI xHCI-контроллер, как его увидел scan, для экранной диагностики. На
/// плате с двумя контроллерами оператор видит, на каком из них клавиатура.
#[derive(Clone, Copy)]
pub struct ControllerInfo {
    pub vendor: u16,
    pub device: u16,
    pub ports: u8,
    pub attached: u32,
    pub chosen: bool,
}

static CONTROLLER_SCAN: Spinlock<Vec<ControllerInfo>> = Spinlock::new(Vec::new());

/// Что нашёл scan контроллеров, в порядке PCI.
pub fn scan() -> Vec<ControllerInfo> {
    CONTROLLER_SCAN.lock().clone()
}

/// Сырой дамп регистров для экранной диагностики: (`caplength`, `usbcmd`,
/// `usbsts`, `config`, `write-ok`, первые четыре dword'а набора регистров
/// выбранного порта).
pub struct RawDump {
    pub caplength: u32,
    pub hciversion: u32,
    pub hcsparams1: u32,
    pub hccparams1: u32,
    pub usbcmd: u32,
    pub usbsts: u32,
    pub config: u32,
    pub writes_ok: bool,
    pub legacy: &'static str,
    pub port: u8,
    pub port_regs: [u32; 4],
    /// PCI command register (бит 2 — bus mastering; без него DMA-кольца контроллера
    /// обрываются и он останавливается).
    pub pci_cmd: u32,
    /// Halts с момента загрузки и значения регистров при первом.
    pub halts: u32,
    pub halt_cmd: u32,
    pub halt_sts: u32,
}

/// Маски протоколов портов `[usb2, usb3]` (бит N = порт N+1) для диагностики.
pub fn proto_masks() -> [u32; 2] {
    CONTROLLER
        .lock()
        .as_ref()
        .map(|c| c.proto_masks)
        .unwrap_or([0, 0])
}

/// Сырой обход extended capabilities для диагностики.
pub fn xcaps() -> Vec<XCap> {
    CONTROLLER
        .lock()
        .as_ref()
        .map(|c| c.xcaps.clone())
        .unwrap_or_default()
}

pub fn raw_dump() -> Option<RawDump> {
    let guard = CONTROLLER.lock();
    let c = guard.as_ref()?;
    let port = (1..=c.ports).find(|p| rd32(c.op, portsc(*p)) & PORTSC_CCS != 0)?;
    Some(RawDump {
        caplength: c.caplength,
        hciversion: c.hciversion,
        hcsparams1: c.hcsparams1,
        hccparams1: c.hccparams1,
        usbcmd: rd32(c.op, OP_USBCMD),
        usbsts: rd32(c.op, OP_USBSTS),
        config: rd32(c.op, OP_CONFIG),
        writes_ok: c.writes_ok,
        legacy: c.legacy,
        pci_cmd: c.pci_cmd,
        halts: c.halts,
        halt_cmd: c.halt_cmd,
        halt_sts: c.halt_sts,
        port,
        port_regs: [
            rd32(c.op, portsc(port)),
            rd32(c.op, portsc(port) + 4),
            rd32(c.op, portsc(port) + 8),
            rd32(c.op, portsc(port) + 12),
        ],
    })
}

/// Что USB-драйвер может сказать о себе — для boot-меню, баннера shell и команды
/// `usb`. На машине без serial эта структура и есть отчёт.
pub struct Summary {
    /// xHCI-контроллеров найдено на PCI.
    pub controllers: usize,
    /// Портов у контроллера, на котором работает драйвер (0, если ни один не запустился).
    pub ports: u8,
    /// Бит N = порт N+1 сообщает подключённое устройство прямо сейчас.
    pub ccs_mask: u32,
    /// Устройств, дошедших до сконфигурированного HID-интерфейса.
    pub devices: u64,
    /// HID-отчётов, переданных транслятору ввода.
    pub reports: u64,
    /// Маска подключённых портов до сброса контроллера (что оставила прошивка)
    /// против `ccs_mask` (что видит драйвер сейчас).
    pub probe_mask: u32,
    /// Последняя ошибка перечисления: порт, стадия, код. `None`, если всё, что
    /// пробовалось, сработало.
    pub last_fail: Option<(u8, &'static str, u32)>,
    /// HID-эндпоинты, восстановленные после ошибки передачи (babble/stall/…), а не
    /// отключённые до перезагрузки.
    pub hid_recoveries: u64,
    /// Отчёты короче минимальной раскладки (считаются, логируются один раз на
    /// эндпоинт).
    pub hid_bad_reports: u64,
}

impl Summary {
    /// Истина, когда хоть один порт сообщает подключённое устройство — первое, что
    /// проверяют, если клавиатура не печатает.
    pub fn any_attached(&self) -> bool {
        self.ccs_mask != 0
    }
}

/// Есть ли запущенный xHCI-контроллер. Нужно сетевому слою, чтобы отличить
/// «канала нет и не будет» от «канал появится, когда поток `usb` перечислит
/// телефон»: второй случай требует построить стек заранее и ждать.
pub fn present() -> bool {
    CONTROLLER.lock().is_some()
}

/// Снимок состояния драйвера, достаточно дешёвый для цикла перерисовки.
pub fn summary() -> Summary {
    let guard = CONTROLLER.lock();
    match guard.as_ref() {
        Some(c) => {
            let mut mask = 0u32;
            for port in 1..=c.ports {
                if rd32(c.op, portsc(port)) & PORTSC_CCS != 0 {
                    mask |= 1 << (port - 1);
                }
            }
            Summary {
                controllers: c.controllers,
                ports: c.ports,
                ccs_mask: mask,
                devices: c.devices,
                reports: c.reports,
                probe_mask: c.probe_mask,
                last_fail: c.last_fail,
                hid_recoveries: c.hid_recoveries,
                hid_bad_reports: c.hid_bad_reports,
            }
        }
        None => Summary {
            controllers: CONTROLLERS_SEEN.load(Ordering::Relaxed),
            ports: 0,
            ccs_mask: 0,
            devices: 0,
            reports: 0,
            probe_mask: 0,
            last_fail: None,
            hid_recoveries: 0,
            hid_bad_reports: 0,
        },
    }
}

/// Монотонные микросекунды для политики порта.
///
/// Дедлайны политики — реальное время (USB2 debounce/training/reset — десятки и
/// сотни миллисекунд по спецификации). `rdtsc` калибруется при загрузке; если
/// калибровка не удалась, `fallback_us` растёт на 1 мс за poll — поток опроса
/// спит один тик, а цикл boot-меню не медленнее, так что запасная база времени
/// консервативна.
fn fsm_now(ctrl: &mut Controller) -> u64 {
    if clock_ready() {
        now_us()
    } else {
        ctrl.fallback_us = ctrl.fallback_us.saturating_add(1_000);
        ctrl.fallback_us
    }
}

/// Скопировать снимок link-состояния попытки из политики в строку для вывода.
///
/// Все значения берутся из `diag[port-1].policy`, то есть по построению на порт.
/// Одинаковый `ped_sc` на всех USB2-портах — не общая переменная, а один и тот
/// же переходный момент reset (CCS+PED+PP+speed=high+PRC), поэтому рядом
/// печатается `seen_ped_u0`, показывающий, был ли снимок U0-сэмплом или только
/// PED во время reset/Polling.
fn sync_diag(ctrl: &mut Controller, port: u8) {
    let Some(d) = ctrl.diag.get_mut(port as usize - 1) else {
        return;
    };
    let p = d.policy;
    d.pre = p.pre();
    d.post = p.post();
    d.seen_prc = p.seen_prc();
    d.seen_ped = p.seen_ped();
    d.seen_ped_u0 = p.seen_ped_u0();
    d.ped_sc = p.ped_sc();
    d.attempts = p.resets().min(u8::MAX as u16) as u8;
}

/// Записать длительность последней попытки в миллисекундах (0 без часов).
fn record_attempt_ms(ctrl: &mut Controller, port: u8) {
    if !clock_ready() {
        return;
    }
    let started = ctrl
        .diag
        .get(port as usize - 1)
        .map(|d| d.policy.attempt_started_us())
        .unwrap_or(0);
    if started == 0 {
        return;
    }
    let ms = now_us().saturating_sub(started) / 1000;
    if let Some(d) = ctrl.diag.get_mut(port as usize - 1) {
        d.ms = ms.min(u16::MAX as u64) as u16;
    }
}

/// Строка стадии, пока политика ждёт (стадии отказа выставляет [`port_failure`],
/// перечисление — `ctrl.fail`/`ctrl.stage`).
fn waiting_stage(phase: Phase) -> &'static str {
    match phase {
        Phase::Debounce => "debounce",
        Phase::ResetWait => "reset",
        Phase::Stabilize => "stabilize",
        Phase::PowerSettle => "power-settle",
        Phase::ResumeWait => "resume",
        Phase::Empty => "no-device",
        Phase::Backoff | Phase::Idle | Phase::Enumerating => "",
    }
}

/// Грубая занятая задержка только там, где неправильно упорядоченный reset стоит
/// целой загрузки.
///
/// PIT здесь намеренно не трогается: boot-меню само программирует канал 2 и
/// вызывает драйвер во время отсчёта, второй пользователь канала сломал бы
/// обратный отсчёт.
fn spin_settle(iterations: u32) {
    let mut i = 0u32;
    while i < iterations {
        core::hint::spin_loop();
        i += 1;
    }
}

impl Controller {
    /// Записать, что произошло на порте, для экранной диагностики.
    fn stage(&mut self, port: u8, stage: &'static str, code: u32) {
        if port == 0 {
            return;
        }
        if let Some(d) = self.diag.get_mut(port as usize - 1) {
            d.stage = stage;
            d.code = code;
        }
    }

    /// Записать стадию ОТКАЗА (и запомнить её как последнюю ошибку — её показывает
    /// однострочный статус, когда HID-устройство не появилось).
    fn fail(&mut self, port: u8, stage: &'static str, code: u32) {
        self.stage(port, stage, code);
        self.last_fail = Some((port, stage, code));
    }
}

// ─── MMIO ────────────────────────────────────────────────────────────────────

fn rd32(base: usize, off: usize) -> u32 {
    // SAFETY: `base + off` лежит внутри окна BAR0, отображённого `map_bar0`
    // (64 КиБ покрывают capability + operational + runtime + doorbells), а чтение
    // volatile, поэтому регистр перечитывается каждый раз.
    unsafe { read_volatile((base + off) as *const u32) }
}

fn wr32(base: usize, off: usize, v: u32) {
    // SAFETY: как в `rd32`.
    unsafe { write_volatile((base + off) as *mut u32, v) }
}

fn rd64(base: usize, off: usize) -> u64 {
    (rd32(base, off) as u64) | ((rd32(base, off + 4) as u64) << 32)
}

fn wr64(base: usize, off: usize, v: u64) {
    wr32(base, off, v as u32);
    wr32(base, off + 4, (v >> 32) as u32);
}

// ─── Инициализация ───────────────────────────────────────────────────────────

/// Найти, выбрать и запустить xHCI-контроллер.
///
/// В машине может быть больше одного контроллера — чипсетный и отдельный на
/// передние порты, — и клавиатура висит на том, куда её воткнули. Драйвер
/// проверяет каждый контроллер, считает порты с подключённым устройством и
/// запускает тот, где их больше; при равенстве — первый в стабильном порядке PCI.
///
/// `false` — «контроллера нет или ни один не запустился»: USB-ввод недоступен,
/// остаются PS/2 и возможная трансляция прошивки.
pub fn init(devices: &[PciDevice]) -> bool {
    let candidates: Vec<&PciDevice> = devices
        .iter()
        .filter(|d| d.class == 0x0C && d.subclass == 0x03)
        .collect();
    CONTROLLERS_SEEN.store(candidates.len(), Ordering::Relaxed);
    if candidates.is_empty() {
        crate::warn!("xhci: no xHCI controller on the PCI bus");
        return false;
    }
    // Сначала часы: все ожидания ниже выражены в микросекундах, а окно PIT канала 2
    // свободно только до того, как boot-меню заберёт канал для своего отсчёта.
    if !clock_ready() {
        calibrate_clock();
    }
    crate::debug!(
        "xhci: {} xHCI controller(s) on the PCI bus",
        candidates.len()
    );
    let mut best: Option<(usize, u32)> = None; // (index, attached ports)
    let mut scan_infos: Vec<ControllerInfo> = Vec::new();
    for (index, dev) in candidates.iter().enumerate() {
        match probe_controller(dev) {
            Some((ports, mask)) => {
                let attached = mask.count_ones();
                crate::debug!(
                    "xhci: controller {}/{} {:04x}:{:04x} ports={} attached_mask={:#010x}",
                    index + 1,
                    candidates.len(),
                    dev.vendor_id,
                    dev.device_id,
                    ports,
                    mask
                );
                scan_infos.push(ControllerInfo {
                    vendor: dev.vendor_id,
                    device: dev.device_id,
                    ports,
                    attached: mask,
                    chosen: false,
                });
                if best.map_or(true, |(_, n)| attached > n) {
                    best = Some((index, attached));
                }
            }
            None => crate::warn!(
                "xhci: controller {}/{} is unusable",
                index + 1,
                candidates.len()
            ),
        }
    }
    let Some((index, _)) = best else {
        crate::warn!("xhci: no xHCI controller could be mapped");
        return false;
    };
    if candidates.len() > 1 {
        crate::debug!("xhci: using controller {}/{}", index + 1, candidates.len());
    }
    // Запомнить выбранный контроллер для экранного списка scan.
    let chosen_pair = (candidates[index].vendor_id, candidates[index].device_id);
    for info in scan_infos.iter_mut() {
        info.chosen = info.vendor == chosen_pair.0 && info.device == chosen_pair.1;
    }
    *CONTROLLER_SCAN.lock() = scan_infos;
    start_controller(candidates[index], candidates.len())
}

/// Отобразить BAR0 контроллера или сказать, почему нельзя.
fn map_bar0(dev: &PciDevice) -> Option<(usize, u64)> {
    let lo = crate::drivers::pci::config_read_u32(dev.address, 0x10);
    let hi = crate::drivers::pci::config_read_u32(dev.address, 0x14);
    let bar = ((hi as u64) << 32) | (lo as u64 & 0xFFFF_FFF0);
    if bar == 0 || lo & 0x6 != 0x4 {
        crate::warn!("xhci: BAR0 is not a 64-bit memory BAR (lo={:#x})", lo);
        return None;
    }
    match crate::memory::vmm::map_mmio(bar, 64 * 1024) {
        Ok(virt) => Some((virt as usize, bar)),
        Err(_) => {
            crate::warn!("xhci: cannot map BAR0 {:#x}", bar);
            None
        }
    }
}

/// Прочитать маску подключённых устройств, не тревожа контроллер.
///
/// Выполняется до BIOS handoff и аппаратного сброса, поэтому показывает, что
/// оставила прошивка; порты в RxDetect просто читаются как неподключённые и
/// будут перепроверены после сброса циклом подъёма.
fn probe_controller(dev: &PciDevice) -> Option<(u8, u32)> {
    let (base, _bar) = map_bar0(dev)?;
    let caplength = (rd32(base, CAP_CAPLENGTH) & 0xFF) as usize;
    let op = base + caplength;
    let hcsparams1 = rd32(base, CAP_HCSPARAMS1);
    let ports = ((hcsparams1 >> 24) & 0xFF) as u8;
    let hccparams1 = rd32(base, CAP_HCCPARAMS1);
    bios_handoff(base, hccparams1);
    let mut mask = 0u32;
    for port in 1..=ports {
        if rd32(op, portsc(port)) & PORTSC_CCS != 0 {
            mask |= 1 << (port - 1);
        }
    }
    Some((ports, mask))
}

/// Забрать контроллер у USB legacy support прошивки.
///
/// xHCI имеет extended capability «USB Legacy Support». Пока BIOS владеет
/// семафором, SMI-обработчики могут перехватывать доступ к портам, а прошивка
/// может оставить контроллер в состоянии, где OS вообще не видит состояния
/// портов. Стандартный handoff: запросить владение (OS-owned), ждать сброса
/// BIOS-owned и, если прошивка не отвечает, снять BIOS-owned самому после
/// ограниченного ожидания.
fn bios_handoff(base: usize, hccparams1: u32) -> &'static str {
    const CAP_ID_USB_LEGACY: u32 = 1;
    const LEGSUP_BIOS_OWNED: u32 = 1 << 16;
    const LEGSUP_OS_OWNED: u32 = 1 << 24;
    let xecp = ((hccparams1 >> 16) & 0xFFFF) as usize;
    // По спецификации extended capabilities начинаются с xECP*4; прошивка с xECP=0
    // может всё же выставить их там, куда их кладёт QEMU (0x20), поэтому этот участок
    // сканируется как fallback. Ошибка здесь означает пропущенный handoff, а
    // контроллер во власти BIOS может держать USB-порты под SMM — один из способов,
    // которым порт включается и снова уходит в Polling.
    let start = if xecp != 0 { xecp * 4 } else { 0x20 };
    let mut off = start;
    let mut guard = 0;
    // xECP — смещение в dword'ах, и реальные контроллеры используют дальний край BAR
    // (этот Intel PCH сообщает 0x2000 dword'ов = 32 КиБ). BAR отображён на 64 КиБ,
    // поэтому обход может туда дойти.
    while off + 4 <= 0x10000 && guard < 64 {
        guard += 1;
        let cap = rd32(base, off);
        let id = cap & 0xFF;
        if id == CAP_ID_USB_LEGACY {
            let mut value = cap;
            if value & LEGSUP_BIOS_OWNED != 0 {
                wr32(base, off, value | LEGSUP_OS_OWNED);
                // xHCI 1.2 §7.1.1: OS даёт прошивке до одной секунды, затем забирает владение
                // принудительно. Ограниченное ожидание гарантирует, что «принудительный» путь не
                // сработает, пока прошивка ещё отпускает порты.
                let clock = clock_ready();
                let start_us = if clock { now_us() } else { 0 };
                let mut spins = 0u32;
                loop {
                    value = rd32(base, off);
                    if value & LEGSUP_BIOS_OWNED == 0 {
                        crate::debug!("xhci: BIOS handed the controller over");
                        return "handoff";
                    }
                    spins += 1;
                    let expired = if clock {
                        now_us().wrapping_sub(start_us) > 1_000_000
                    } else {
                        spins > 2_000_000
                    };
                    if expired {
                        wr32(base, off, (value & !LEGSUP_BIOS_OWNED) | LEGSUP_OS_OWNED);
                        crate::warn!("xhci: firmware did not release the controller; forced");
                        return "forced";
                    }
                    core::hint::spin_loop();
                }
            }
            wr32(base, off, value | LEGSUP_OS_OWNED);
            return "os-owned";
        }
        let next = ((cap >> 8) & 0xFF) as usize;
        if next == 0 || next >= 0x100 {
            break;
        }
        off += next * 4;
    }
    "absent"
}

/// Поднять контроллер целиком: сброс, кольца, DCBAA, запуск.
fn start_controller(dev: &PciDevice, controllers: usize) -> bool {
    // Сначала memory space И bus mastering: командное/событийное кольца, DCBAA и
    // scratchpad — это DMA, и устройство с выключенным bus mastering не может их
    // читать.
    crate::drivers::pci::enable_bus_master(dev.address);
    let pci_cmd = crate::drivers::pci::config_read_u32(dev.address, 0x04) & 0xFFFF;
    crate::debug!(
        "xhci: PCI command={:#06x} (memory={} bus-master={})",
        pci_cmd,
        if pci_cmd & 0x2 != 0 { "on" } else { "OFF" },
        if pci_cmd & 0x4 != 0 { "on" } else { "OFF" }
    );
    // Реальная база времени для ожиданий порта: USB2 reset/recovery/settle — десятки
    // миллисекунд по спецификации, а спин-счётчик не миллисекунда.
    if !clock_ready() {
        calibrate_clock();
    }
    let Some((base, bar)) = map_bar0(dev) else {
        return false;
    };
    // Подключённые порты до сброса: диагностика показывает, какие соединения сброс
    // потерял.
    let mut ccs_mask_probe = 0u32;

    let caplength = (rd32(base, CAP_CAPLENGTH) & 0xFF) as usize;
    let op = base + caplength;
    let hcsparams1 = rd32(base, CAP_HCSPARAMS1);
    let hcsparams2 = rd32(base, CAP_HCSPARAMS2);
    let hccparams1 = rd32(base, CAP_HCCPARAMS1);
    let hciversion = rd32(base, CAP_CAPLENGTH) >> 16;
    let legacy = bios_handoff(base, hccparams1);
    let dboff = (rd32(base, CAP_DBOFF) & !0x3) as usize;
    let rtsoff = (rd32(base, CAP_RTSOFF) & !0x1F) as usize;
    let max_slots = (hcsparams1 & 0xFF) as usize;
    let ports = ((hcsparams1 >> 24) & 0xFF) as u8;
    let ctx_size = if hccparams1 & (1 << 2) != 0 { 64 } else { 32 };
    let scratchpads = (((hcsparams2 >> 21) & 0x1F) << 5) | ((hcsparams2 >> 27) & 0x1F);
    crate::debug!(
        "xhci: {:04x}:{:04x} bar={:#x} slots={} ports={} ctx={}B scratchpad={}",
        dev.vendor_id,
        dev.device_id,
        bar,
        max_slots,
        ports,
        ctx_size,
        scratchpads
    );
    if max_slots == 0 || ports == 0 {
        crate::warn!("xhci: controller reports no slots/ports");
        return false;
    }

    // Состояние портов ДО любых действий: прошивка могла уже поднять устройства, и
    // сравнение с состоянием после сброса показывает, не теряет ли их именно сброс.
    for port in 1..=ports {
        let sc = rd32(op, portsc(port));
        if sc & PORTSC_CCS != 0 {
            ccs_mask_probe |= 1 << (port as u32 - 1);
        }
        if sc != 0 {
            crate::debug!("xhci: pre-reset port {} PORTSC={:#010x}", port, sc);
        }
    }

    // Сброс: прошивка могла оставить контроллер со своим расписанием, а мы
    // собираемся заменить все известные ему кольца.
    wr32(op, OP_USBCMD, rd32(op, OP_USBCMD) & !USBCMD_RS);
    spin_until(|| rd32(op, OP_USBSTS) & USBSTS_HCH != 0, 100_000);
    wr32(op, OP_USBCMD, USBCMD_HCRST);
    spin_until(|| rd32(op, OP_USBCMD) & USBCMD_HCRST == 0, 1_000_000);
    spin_until(|| rd32(op, OP_USBSTS) & USBSTS_CNR == 0, 1_000_000);

    let Some(dcbaa) = DmaFrame::new_zeroed("dcbaa") else {
        return false;
    };
    let dcbaa_phys_value = dcbaa.phys();
    let mut scratchpad_array: Option<DmaFrame> = None;
    let mut scratchpad_pages: Vec<DmaFrame> = Vec::new();
    if scratchpads > 0 {
        let Some(array) = DmaFrame::new_zeroed("scratchpad array") else {
            return false;
        };
        for i in 0..scratchpads {
            let Some(page) = DmaFrame::new_zeroed("scratchpad page") else {
                return false;
            };
            // SAFETY: `array` — наша страница и `i < scratchpads`, поэтому 8-байтный слот
            // лежит внутри неё.
            unsafe { write_volatile((array.virt() as *mut u64).add(i as usize), page.phys()) };
            scratchpad_pages.push(page);
        }
        // SAFETY: DCBAA[0] — первые 8 байт нашего обнулённого кадра.
        unsafe { write_volatile(dcbaa.virt() as *mut u64, array.phys()) };
        // Страница массива — тоже DMA-состояние: DCBAA[0] указывает на неё, и
        // контроллер читает её, когда нужен scratchpad. Она живёт в `Controller` вместе с
        // кадром по той же причине времени жизни.
        scratchpad_array = Some(array);
    }

    let (Some(cmdr), Some(event), Some(erst)) = (
        Ring::new("command ring"),
        Ring::new("event ring"),
        DmaFrame::new_zeroed("erst"),
    ) else {
        return false;
    };
    // SAFETY: `erst` — наша обнулённая страница, а запись — её первые 16 байт.
    unsafe {
        write_volatile(
            erst.virt() as *mut ErstEntry,
            ErstEntry {
                base: event.base_phys(),
                size: EVENT_TRBS as u32,
                _reserved: 0,
            },
        );
    }

    let rt = base + rtsoff;
    wr32(rt + RTS_IR0, IR_ERSTSZ, 1);
    wr64(rt + RTS_IR0, IR_ERSTBA, erst.phys());
    wr64(rt + RTS_IR0, IR_ERDP, event.base_phys());

    wr32(op, OP_CONFIG, max_slots.min(u8::MAX as usize) as u32);
    wr64(op, OP_DCBAAP, dcbaa.phys());
    wr64(op, OP_CRCR, cmdr.base_phys() | TRB_CYCLE as u64);
    wr32(op, OP_USBCMD, USBCMD_RS);
    if !spin_until(|| rd32(op, OP_USBSTS) & USBSTS_HCH == 0, 100_000) {
        crate::warn!(
            "xhci: controller did not start (USBSTS={:#x})",
            rd32(op, OP_USBSTS)
        );
        return false;
    }
    // Write-read-back безвредного регистра: если он не проходит, все записи портов в
    // этом драйвере отбрасываются, и reset-логика уже не поможет.
    let before = rd32(op, OP_CONFIG);
    wr32(op, OP_CONFIG, before);
    let writes_ok = rd32(op, OP_CONFIG) == before;
    if !writes_ok {
        crate::warn!(
            "xhci: register write-readback failed (config {:#x})",
            before
        );
    }
    crate::debug!("xhci: running (ports={} cr={:#x})", ports, cmdr.base_phys());
    // Дать PHY устояться перед первым reset порта: USB2 хочет ~100 мс после сброса
    // контроллера (xHCI 1.2 §4.19.1). QEMU работает мгновенно, железо — нет.
    settle_us(100_000);
    // Состояние портов при подъёме, на уровне debug: какие порты подключены,
    // питаются и на какой скорости — первое, что смотрят, если перечисление не идёт.
    for port in 1..=ports {
        let sc = rd32(op, portsc(port));
        crate::debug!(
            "xhci: port {} PORTSC={:#010x} ccs={} ped={} pp={} speed={} pls={}",
            port,
            sc,
            sc & PORTSC_CCS != 0,
            sc & PORTSC_PED != 0,
            sc & PORTSC_PP != 0,
            (sc & PORTSC_SPEED_MASK) >> PORTSC_SPEED_SHIFT,
            (sc & PORTSC_PLS_MASK) >> 5
        );
    }

    // Какие порты USB2, а какие USB3 — из Supported Protocol capabilities. Это
    // ДИАГНОСТИКА, а не политика: путь reset узнаёт протокол из скорости самого
    // порта. Сырые записи хранятся для экрана, чтобы сверять их со спецификацией, а
    // не с нашим парсером: неверно прочитанные поля дают невозможные диапазоны вроде
    // `usb2=17-41 usb3=17-26`.
    let mut protos: Vec<u8> = vec![0u8; ports as usize];
    let mut proto_masks: [u32; 2] = [0, 0];
    let mut xcaps: Vec<XCap> = Vec::new();
    let xecp = ((hccparams1 >> 16) & 0xFFFF) as usize;
    {
        let mut off = if xecp != 0 { xecp * 4 } else { 0x20 };
        let mut guard = 0;
        while off + 16 <= 0x10000 && guard < 64 {
            guard += 1;
            let dw0 = rd32(base, off);
            let id = (dw0 & 0xFF) as u8;
            let next = ((dw0 >> 8) & 0xFF) as u8;
            let dw1 = rd32(base, off + 4);
            let dw2 = rd32(base, off + 8);
            if xcaps.len() < 12 && (id == 1 || id == 2 || id == 3) {
                xcaps.push(XCap {
                    offset: off as u32,
                    id,
                    next,
                    // DW0 — заголовок capability для любой extended capability, не только
                    // protocol.
                    major: ((dw0 >> 24) & 0xFF) as u8,
                    minor: ((dw0 >> 16) & 0xFF) as u8,
                    dw1,
                    dw2,
                });
            }
            if id == 2 {
                // Supported Protocol capability, xHCI 1.2 §7.2 / таблица 7-4: DW0 — заголовок
                // (major revision 31:24, minor 23:16), DW1 — строка имени, DW2 — слово PORT INFO:
                // биты 31:28 PSIC (число speed id), 15:8 число портов, 7:0 смещение порта.
                // Смещение 1-based (константы XHCI_EXT_PORT_OFF / XHCI_EXT_PORT_COUNT; «port count
                // includes the current port offset»). Неверное чтение этих полей и даёт
                // невозможные диапазоны и proto=usb0 для реальных USB2-портов. QEMU пишет ту же
                // раскладку полей (`hcd-xhci.c` 0x28/0x38), поэтому особый случай эмулятора не
                // нужен.
                let major = ((dw0 >> 24) & 0xFF) as u8;
                let minor = ((dw0 >> 16) & 0xFF) as u8;
                let first = (dw2 & 0xFF) as usize;
                let count = ((dw2 >> 8) & 0xFF) as usize;
                let psic = (dw2 >> 28) & 0xF;
                crate::debug!(
                    "xhci: xcap {:#06x} id={} next={:#04x} major={} minor={} offset={} count={} psic={} dw1={:#010x} dw2={:#010x}",
                    off,
                    id,
                    next,
                    major,
                    minor,
                    first,
                    count,
                    psic,
                    dw1,
                    dw2
                );
                let slot = match major {
                    2 => Some(0usize),
                    3 => Some(1usize),
                    _ => None,
                };
                if let Some(slot) = slot {
                    if count != 0 {
                        for port in first..first.saturating_add(count) {
                            if port >= 1 && port <= ports as usize {
                                proto_masks[slot] |= 1 << (port - 1);
                            }
                        }
                    }
                }
            }
            if next == 0 {
                break;
            }
            off += next as usize * 4;
        }
    }
    // Порт принадлежит максимум одному протоколу. Если диапазоны перекрываются,
    // побеждает USB3: у этого порта другой reset, и ошибиться в сторону USB2 дороже.
    let covered = proto_masks[0] | proto_masks[1];
    proto_masks[0] &= !proto_masks[1];
    for port in 1..=ports as usize {
        let bit = 1u32 << (port - 1);
        protos[port - 1] = if proto_masks[1] & bit != 0 {
            3
        } else if proto_masks[0] & bit != 0 {
            2
        } else {
            0
        };
    }
    let all_ports = if ports >= 32 {
        u32::MAX
    } else {
        (1u32 << ports) - 1
    };
    if covered != all_ports {
        crate::warn!(
            "xhci: protocol caps cover {:#010x} of {:#010x} ports (usb2={:#010x} usb3={:#010x})",
            covered,
            all_ports,
            proto_masks[0],
            proto_masks[1]
        );
    }

    let diag: Vec<PortDiag> = (0..ports as usize)
        .map(|index| {
            let proto = protos.get(index).copied().unwrap_or(0);
            PortDiag {
                stage: "",
                code: 0,
                speed: 0,
                attempts: 0,
                pre: 0,
                post: 0,
                seen_prc: false,
                seen_ped: false,
                seen_ped_u0: false,
                ped_sc: 0,
                ms: 0,
                proto,
                // USB3-порты используют WPR (warm reset), USB2 — PR. Неизвестный протокол (0)
                // считается USB2: PR — безопасный запрос.
                policy: PortPolicy::new(proto == 3),
            }
        })
        .collect();
    *CONTROLLER.lock() = Some(Controller {
        op,
        db: base + dboff,
        rt,
        ctx_size,
        controllers,
        ports,
        cmdr,
        event,
        erst,
        dcbaa,
        dcbaa_phys: dcbaa_phys_value,
        scratchpad_array,
        scratchpad_pages,
        slots: Vec::new(),
        reports: 0,
        devices: 0,
        brought_up: Vec::new(),
        probe_mask: ccs_mask_probe,
        next_port: 1,
        fallback_us: 0,
        protos,
        proto_masks,
        xcaps,
        caplength: caplength as u32,
        hciversion,
        hcsparams1,
        hccparams1,
        legacy,
        writes_ok,
        pci_cmd,
        halts: 0,
        halt_cmd: 0,
        halt_sts: 0,
        diag,
        last_fail: None,
        deferred_events: Vec::new(),
        hid_recoveries: 0,
        hid_bad_reports: 0,
    });
    true
}

/// Крутиться, пока `cond` не станет истинным или не кончится бюджет. Возвращает
/// результат проверки.
fn spin_until(cond: impl Fn() -> bool, budget: u32) -> bool {
    let mut i = 0;
    while i < budget {
        if cond() {
            return true;
        }
        core::hint::spin_loop();
        i += 1;
    }
    cond()
}

// ─── Команды и события ───────────────────────────────────────────────────────

fn ring_doorbell(ctrl: &Controller, slot: u8, target: u8) {
    wr32(ctrl.db, slot as usize * 4, target as u32);
}

/// Забрать одно событие и сдвинуть ERDP. `None`, если за бюджет ничего не
/// появилось.
fn next_event(ctrl: &mut Controller, budget: u32) -> Option<Trb> {
    if !spin_until(
        || {
            // Замыкание не может заимствовать `ctrl` изменяемо, поэтому проверка идёт сырым
            // чтением кольца событий через общую ссылку.
            ctrl.event.event_pending()
        },
        budget,
    ) {
        return None;
    }
    let ev = ctrl.event.next_event();
    let erdp = ctrl.event.base_phys() + (ctrl.event.dequeue * TRB_SIZE) as u64;
    wr64(ctrl.rt + RTS_IR0, IR_ERDP, erdp);
    Some(ev)
}

/// Отложить transfer event, пришедшее во время ожидания команды, чтобы цикл
/// опроса всё равно его доставил. Потеря оставила бы эндпоинт «outstanding»
/// навсегда: он бы не переармировался, и ввод молча умер бы.
fn defer_event(ctrl: &mut Controller, ev: Trb) {
    // Ограниченно: очередь осушается каждый poll, а команда ждёт одно завершение.
    // Полная очередь означает серьёзную проблему, но потерять отчёт лучше, чем растить
    // кучу ядра без границы.
    if ctrl.deferred_events.len() < 64 {
        ctrl.deferred_events.push(ev);
    }
}

/// Следующее событие для цикла опроса: сначала отложенные transfer events (они
/// старее того, что ещё в кольце), затем само кольцо.
fn next_poll_event(ctrl: &mut Controller, budget: u32) -> Option<Trb> {
    if let Some(ev) = ctrl.deferred_events.pop() {
        return Some(ev);
    }
    next_event(ctrl, budget)
}

/// Отправить командный TRB и вернуть событие завершения.
fn command(ctrl: &mut Controller, dw0: u32, dw1: u32, dw2: u32, dw3: u32) -> Option<Trb> {
    ctrl.cmdr.push(dw0, dw1, dw2, dw3);
    ring_doorbell(ctrl, 0, 0);
    for _ in 0..32 {
        let ev = next_event(ctrl, 500_000)?;
        match ev.trb_type() {
            TRB_COMMAND_COMPLETION => return Some(ev),
            TRB_PORT_STATUS_CHANGE => {
                let port = (ev.dw0 >> 24) & 0xFF;
                crate::debug!("xhci: port {} status change", port);
            }
            // Не наше: завершение HID interrupt-IN другого эндпоинта.
            TRB_TRANSFER_EVENT => defer_event(ctrl, ev),
            _ => {}
        }
    }
    None
}

/// Адрес аргумента input-context TRB: биты 4+ из dw0/dw1.
fn ctx_addr(phys: u64) -> (u32, u32) {
    ((phys & 0xFFFF_FFF0) as u32, (phys >> 32) as u32)
}

// ─── Перечисление ────────────────────────────────────────────────────────────

// ─── Подъём порта: исполнитель чистой политики ──────────────────────────────
//
// Решения принимаются в `xhci_hub.rs` и проверены host-тестами на симулированных
// портах. Этот участок лишь выполняет один побочный эффект, который просит
// политика, и синхронизирует копию для экрана. Ожидания USB2 debounce/reset здесь
// не блокируют: это дедлайны `PortPolicy`, проверяемые раз за poll, поэтому
// отсчёт boot-меню продолжается, а зависший порт стоит одного чтения регистра.

/// Декодировать сырое слово PORTSC в тип наблюдения политики.
fn observe_portsc(raw: u32) -> PortObservation {
    PortObservation::new(
        raw & PORTSC_CCS != 0,
        raw & PORTSC_PED != 0,
        raw & PORTSC_PP != 0,
        ((raw & PORTSC_SPEED_MASK) >> PORTSC_SPEED_SHIFT) as u8,
        ((raw & PORTSC_PLS_MASK) >> PORTSC_PLS_SHIFT) as u8,
        raw & PORTSC_PRC != 0,
        raw & PORTSC_CSC != 0,
        raw & PORTSC_PR != 0,
        raw,
    )
}

/// Выполнить один шаг политики порта для `port`.
///
/// Вызывается циклически (один порт за [`poll`]), поэтому каждая state machine
/// получает один ограниченный шаг за проход. Это и есть кооперативное ожидание,
/// нужное boot-меню: порт в debounce или в ожидании PRC трогается один раз, его
/// дедлайн сравнивается, и poll возвращается.
fn poll_port(ctrl: &mut Controller, port: u8) {
    if ctrl.slots.iter().any(|s| s.port == port) {
        return;
    }
    let off = portsc(port);
    let raw = rd32(ctrl.op, off);
    let obs = observe_portsc(raw);
    let now = fsm_now(ctrl);
    let Some(step) = ctrl
        .diag
        .get_mut(port as usize - 1)
        .map(|d| d.policy.step(obs, now))
    else {
        return;
    };
    match step {
        Step::Idle => {
            let phase = ctrl.diag[port as usize - 1].policy.phase();
            let stage = waiting_stage(phase);
            if !stage.is_empty() {
                ctrl.stage(port, stage, raw);
            }
            sync_diag(ctrl, port);
        }
        Step::PowerOn => {
            // Запись идёт через neutral-значение (PED/PR/WPR/LWS должны быть 0), PP читается
            // обратно; контроллер, потерявший запись, не должен оставить порт обесточенным и
            // молчащим.
            if !power_on_port(ctrl, port) {
                policy_fail(ctrl, port, obs, now, FailReason::NoPower);
                port_failure(ctrl, port, FailReason::NoPower, raw);
                return;
            }
            ctrl.stage(port, "power-settle", raw);
            sync_diag(ctrl, port);
        }
        Step::Reset { warm } => start_port_reset(ctrl, port, warm, raw),
        Step::Resume => {
            ctrl.stage(port, "resume", raw);
            match resume_port(ctrl, port) {
                Some(_speed) => {
                    // Resume подождал свои 20 мс; следующий шаг политики увидит U0 и вернёт
                    // Enumerate. Reset здесь не пишется.
                    sync_diag(ctrl, port);
                }
                None => {
                    policy_fail(ctrl, port, obs, now, FailReason::ResumeLost);
                    port_failure(ctrl, port, FailReason::ResumeLost, raw);
                }
            }
        }
        Step::Enumerate { speed } => enumerate_port(ctrl, port, speed, now),
        Step::Failed(reason) => port_failure(ctrl, port, reason, raw),
    }
}

/// Поднять PP с readback (и ограниченным повтором) — никогда не оставлять порт
/// выключенным.
///
/// `xhci_set_port_power()` из `xhci-hub.c` пишет `xhci_hub_state_to_neutral()` | PP
/// и читает регистр обратно один раз. У этого драйвера нет hub workqueue для
/// повтора потерянной записи, поэтому он повторяет несколько раз и только затем
/// сообщает `no-power`; политика отступает и возвращается к `PowerOn`, так что
/// порт не остаётся без питания.
fn power_on_port(ctrl: &mut Controller, port: u8) -> bool {
    let off = portsc(port);
    for _ in 0..3 {
        let cur = rd32(ctrl.op, off);
        wr32(ctrl.op, off, port_state_to_neutral(cur) | PORTSC_PP);
        let sc = rd32(ctrl.op, off);
        if sc & PORTSC_PP != 0 {
            return true;
        }
        settle_us(10_000);
    }
    crate::warn!(
        "xhci: port {}: PP did not come up after 3 writes (PORTSC={:#010x})",
        port,
        rd32(ctrl.op, off)
    );
    false
}

/// Сбросить устаревшие change-биты и записать PR (USB2) или WPR (USB3).
///
/// Политика уже перешла в `ResetWait`; `pre` — сырой PORTSC, из которого принято
/// решение.
fn start_port_reset(ctrl: &mut Controller, port: u8, warm: bool, raw: u32) {
    let off = portsc(port);
    // `xhci_hub_control`, case USB_PORT_FEAT_RESET (`xhci-hub.c`):
    //     portsc = xhci_hub_state_to_neutral(portsc);
    //     portsc |= PORT_RESET;
    //     writel(portsc);
    // Change-биты здесь НЕ очищаются: neutral-значение пишет 0 во все W1C-биты, а
    // change-биты подтверждаются только после того, как порт включился и удержался
    // (см. `enumerate_port`).
    let sc = rd32(ctrl.op, off);
    let bit = if warm { PORTSC_WPR } else { PORTSC_PR };
    wr32(ctrl.op, off, port_state_to_neutral(sc) | bit);
    ctrl.stage(port, "reset", raw);
    sync_diag(ctrl, port);
}

/// Политика сообщила устойчивый PED+U0: подтвердить change-биты и передать порт
/// перечислению.
///
/// Повторное чтение после reset — последний шанс увидеть потерю: если PED исчез
/// между проверкой устойчивости и этим чтением, попытка записывается как
/// `lost-after-reset`, и драйвер отступает (сразу не сбрасывает — следующий шаг
/// политики сначала наблюдает).
fn enumerate_port(ctrl: &mut Controller, port: u8, speed: u8, now: u64) {
    let off = portsc(port);
    let had_reset = ctrl.diag[port as usize - 1].policy.attempt_had_reset();
    let pre = rd32(ctrl.op, off);
    // `hub_port_reset` очищает C_PORT_RESET после завершения reset и включения
    // порта (usb_clear_port_feature(C_RESET)). Порядок тот же: порт уже удержал
    // PED+U0 в течение STABLE_US, а запись нейтральна, кроме W1C change-битов —
    // PED=1 она записать не может и порт не отключит.
    wr32(
        ctrl.op,
        off,
        port_state_to_neutral(pre) | PORTSC_PRC | PORTSC_WRC | PORTSC_CSC,
    );
    let cur = rd32(ctrl.op, off);
    let still = observe_portsc(cur);
    if !still.enabled() {
        policy_fail(ctrl, port, still, now, FailReason::LostAfterReset);
        port_failure(ctrl, port, FailReason::LostAfterReset, cur);
        return;
    }
    {
        let d = &mut ctrl.diag[port as usize - 1];
        d.speed = speed;
        d.post = cur;
        d.stage = if had_reset { "reset-ok" } else { "adopted" };
        d.code = cur;
    }
    record_attempt_ms(ctrl, port);
    sync_diag(ctrl, port);
    if enumerate_with_speed(ctrl, port, speed) {
        ctrl.devices += 1;
        ctrl.brought_up.push(port);
        if let Some(d) = ctrl.diag.get_mut(port as usize - 1) {
            d.policy.enumerated();
        }
    } else {
        // Конкретная стадия перечисления (`device-descriptor`, `no-hid-interface`, …)
        // уже записана в `enumerate_slot`; сохраняем её и обновляем только retry-учёт,
        // чтобы следующая попытка началась с reset после backoff.
        // Перечисление может блокироваться на таймаутах дескрипторов, поэтому backoff
        // стартует от момента реального отказа, а не от предшествовавшего reset.
        let failed_at = fsm_now(ctrl);
        let _ = ctrl
            .diag
            .get_mut(port as usize - 1)
            .map(|d| d.policy.enumeration_failed(still, failed_at));
        record_attempt_ms(ctrl, port);
        let failures = ctrl.diag[port as usize - 1].policy.failures();
        if failures <= 3 || failures % 8 == 0 {
            crate::warn!(
                "xhci: port {}: enumeration failed (attempt {}); retrying with a reset in {} ms",
                port,
                failures,
                xhci_hub::backoff_us(failures) / 1_000
            );
        }
        sync_diag(ctrl, port);
    }
}

/// Записать отказ политики/драйвера и залогировать его кратко: стадия, число
/// попыток и пауза до повтора.
fn port_failure(ctrl: &mut Controller, port: u8, reason: FailReason, raw: u32) {
    let stage = reason.stage();
    let was_last_fail = ctrl.last_fail.map_or(true, |(p, _, _)| p != port);
    ctrl.fail(port, stage, raw);
    record_attempt_ms(ctrl, port);
    sync_diag(ctrl, port);
    let failures = ctrl.diag[port as usize - 1].policy.failures();
    let retry_ms = xhci_hub::backoff_us(failures) / 1_000;
    // Первые попытки и каждая 8-я: ограниченно, чтобы навсегда мёртвый порт не
    // заливал serial. Полный снимок порта печатает команда `usb`.
    if failures <= 3 || failures % 8 == 0 || was_last_fail {
        crate::warn!(
            "xhci: port {}: {} (attempt {}, retry in {} ms)",
            port,
            stage,
            failures,
            retry_ms
        );
    }
}

/// Применить отказ политики, решение о котором принято вне `step` (проверки
/// PP/resume/после reset).
fn policy_fail(
    ctrl: &mut Controller,
    port: u8,
    obs: PortObservation,
    now: u64,
    reason: FailReason,
) {
    let _ = ctrl
        .diag
        .get_mut(port as usize - 1)
        .map(|d| d.policy.fail(obs, now, reason));
}

/// Port Link State из слова PORTSC.
fn port_pls(sc: u32) -> u32 {
    (sc & PORTSC_PLS_MASK) >> PORTSC_PLS_SHIFT
}

/// Speed id порта из слова PORTSC.
fn port_speed(sc: u32) -> u8 {
    ((sc & PORTSC_SPEED_MASK) >> PORTSC_SPEED_SHIFT) as u8
}

/// Программно разбудить порт, чей линк в suspended с включённым устройством.
///
/// Для USB2-порта запись PLS=U0 с LWS=1 запускает resume-сигнализацию (20 мс,
/// USB2 §7.1.7.7); для USB3 — resume линка. Затем оба сообщают U0.
fn resume_port(ctrl: &mut Controller, port: u8) -> Option<u8> {
    let off = portsc(port);
    let cur = rd32(ctrl.op, off);
    // `xhci_set_link_state()`: neutral-значение, PLS=U0 и LWS=1.
    let neutral = port_state_to_neutral(cur) & !PORTSC_PLS_MASK;
    wr32(
        ctrl.op,
        off,
        neutral | PORTSC_LWS | (PLS_U0 << PORTSC_PLS_SHIFT),
    );
    let clock = clock_ready();
    let start = if clock { now_us() } else { 0 };
    let mut spins = 0u32;
    loop {
        let sc = rd32(ctrl.op, off);
        if port_pls(sc) == PLS_U0 && sc & PORTSC_PED != 0 {
            let speed = port_speed(sc);
            if speed != 0 {
                // Resume-сигнализации нужно 20 мс до трафика.
                settle_us(20_000);
                return Some(adopt(ctrl, port, speed, "resumed", sc));
            }
        }
        if clock {
            if now_us().wrapping_sub(start) >= 40_000 {
                return None;
            }
        } else {
            spins += 1;
            if spins >= 400_000 {
                return None;
            }
        }
        core::hint::spin_loop();
    }
}

/// Принять уже включённый порт: подтвердить change-биты (neutral-запись, PED=1 не
/// возвращается) и запомнить скорость.
fn adopt(ctrl: &mut Controller, port: u8, speed: u8, stage: &'static str, sc: u32) -> u8 {
    let off = portsc(port);
    let cur = rd32(ctrl.op, off);
    wr32(
        ctrl.op,
        off,
        port_state_to_neutral(cur) | PORTSC_PRC | PORTSC_WRC | PORTSC_CSC,
    );
    let cur = rd32(ctrl.op, off);
    if let Some(d) = ctrl.diag.get_mut(port as usize - 1) {
        d.speed = speed;
        d.post = cur;
    }
    ctrl.stage(port, stage, sc);
    speed
}

/// Максимальный размер пакета EP0 до чтения дескриптора устройства, по скорости.
fn ep0_max_packet(speed: u8) -> u32 {
    match speed {
        SPEED_HIGH => 64,
        SPEED_SUPER => 512,
        _ => 8,
    }
}

/// Выполнить control-IN передачу на EP0 устройства и вернуть прочитанные байты.
fn control_in(
    ctrl: &mut Controller,
    slot_index: usize,
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    len: usize,
) -> Option<Vec<u8>> {
    if len > PAGE_SIZE {
        return None;
    }
    let slot_id = {
        let slot = ctrl.slots.get_mut(slot_index)?;
        // Setup-стадия: 8-байтный пакет запроса идёт в самом TRB (IDT).
        let dw0 = request_type as u32 | ((request as u32) << 8) | ((value as u32) << 16);
        let dw1 = index as u32 | ((len as u32) << 16);
        let dw3 = ((TRB_SETUP as u32) << 10) | TRB_IDT | TRT_IN;
        slot.ep0.push(dw0, dw1, 8, dw3);
        // Data-стадия: один IN TRB на весь буфер.
        let buf_phys = slot.ctrl_buf.phys();
        slot.ep0.push(
            buf_phys as u32,
            (buf_phys >> 32) as u32,
            len as u32,
            ((TRB_DATA as u32) << 10) | TRB_DIR_IN,
        );
        // Status-стадия: OUT, с IOC, чтобы пришло завершение.
        slot.ep0
            .push(0, 0, 0, ((TRB_STATUS as u32) << 10) | TRB_IOC);
        slot.id
    };
    ring_doorbell(ctrl, slot_id, 1);
    // Могут прийти два события: data-стадия и status-стадия. Ждём то, что несёт
    // завершение status TRB (эндпоинт 1, IOC установлен).
    let mut result = None;
    for _ in 0..8 {
        let ev = next_event(ctrl, 1_000_000)?;
        if ev.trb_type() != TRB_TRANSFER_EVENT {
            continue;
        }
        if ev.endpoint_id() != 1 {
            // Transfer event HID-эндпоинта завершился, пока ждём EP0: сохраняем его для
            // цикла опроса, а не проглатываем отчёт.
            defer_event(ctrl, ev);
            continue;
        }
        let cc = ev.completion_code();
        if cc != CC_SUCCESS && cc != CC_SHORT_PACKET {
            crate::warn!("xhci: control-IN failed: cc={}", cc);
            return None;
        }
        let got = len.saturating_sub(ev.residual() as usize);
        let slot = ctrl.slots.get(slot_index)?;
        // SAFETY: `ctrl_buf` — наша страница и `got <= len <= PAGE_SIZE`.
        let bytes =
            unsafe { core::slice::from_raw_parts(slot.ctrl_buf.virt() as *const u8, got) }.to_vec();
        result = Some(bytes);
        break;
    }
    result
}

/// Выполнить control-передачу БЕЗ data-стадии (стандартный OUT-запрос).
///
/// SET_CONFIGURATION обязателен: без него устройство остаётся
/// неконфигурированным, interrupt-эндпоинт отвечает STALL, и путь отчётов вечно
/// читает `cc=6` (stall) — клавиатура перечисляется, но не печатает.
fn control_no_data(
    ctrl: &mut Controller,
    slot_index: usize,
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
) -> bool {
    let slot_id = {
        let Some(slot) = ctrl.slots.get_mut(slot_index) else {
            return false;
        };
        let dw0 = request_type as u32 | ((request as u32) << 8) | ((value as u32) << 16);
        let dw1 = index as u32;
        let dw3 = ((TRB_SETUP as u32) << 10) | TRB_IDT | TRT_NO_DATA;
        // Поле status Setup TRB — длина SETUP PACKET, всегда 8, даже для запроса без
        // data-стадии (стадия задаётся TRT=0). 0 здесь заставлял контроллер отклонить всю
        // передачу до того, как её увидит устройство.
        slot.ep0.push(dw0, dw1, 8, dw3);
        // Status-стадия: для передачи без data-стадии status — IN.
        slot.ep0
            .push(0, 0, 0, ((TRB_STATUS as u32) << 10) | TRB_DIR_IN | TRB_IOC);
        slot.id
    };
    ring_doorbell(ctrl, slot_id, 1);
    for _ in 0..8 {
        let Some(ev) = next_event(ctrl, 1_000_000) else {
            return false;
        };
        if ev.trb_type() != TRB_TRANSFER_EVENT {
            continue;
        }
        if ev.endpoint_id() != 1 {
            // Transfer event HID-эндпоинта пришёл, пока ПЕРЕЧИСЛЕНИЕ отправляло
            // control-запрос; сохраняем его для цикла опроса.
            defer_event(ctrl, ev);
            continue;
        }
        let cc = ev.completion_code();
        if cc == CC_SUCCESS || cc == CC_SHORT_PACKET {
            return true;
        }
        crate::warn!("xhci: control request {:#04x} failed: cc={}", request, cc);
        return false;
    }
    false
}

/// Выполнить control-передачу с data-стадией OUT.
///
/// SEND_ENCAPSULATED_COMMAND несёт сообщение RNDIS в data-стадии: setup TRB
/// объявляет длину и `TRT_OUT`, data TRB идёт OUT без IOC (завершение несёт
/// status-стадия), status TRB — IN с IOC. Порядок важен: без `TRT_OUT`
/// контроллер ждал бы данные в другую сторону, а устройство не увидело бы
/// команду вовсе.
fn control_out(
    ctrl: &mut Controller,
    slot_index: usize,
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    data: &[u8],
) -> bool {
    if data.len() > PAGE_SIZE {
        return false;
    }
    let slot_id = {
        let Some(slot) = ctrl.slots.get_mut(slot_index) else {
            return false;
        };
        // SAFETY: `ctrl_buf` — наша страница; `data.len() <= PAGE_SIZE`, копия
        // не выходит за её границы.
        unsafe {
            core::ptr::copy_nonoverlapping(
                data.as_ptr(),
                slot.ctrl_buf.virt() as *mut u8,
                data.len(),
            );
        }
        let dw0 = request_type as u32 | ((request as u32) << 8) | ((value as u32) << 16);
        let dw1 = index as u32 | ((data.len() as u32) << 16);
        slot.ep0
            .push(dw0, dw1, 8, ((TRB_SETUP as u32) << 10) | TRB_IDT | TRT_OUT);
        let buf_phys = slot.ctrl_buf.phys();
        // Data-стадия OUT: бит направления не ставится, IOC — на status-стадии.
        slot.ep0.push(
            buf_phys as u32,
            (buf_phys >> 32) as u32,
            data.len() as u32,
            ((TRB_DATA as u32) << 10),
        );
        slot.ep0
            .push(0, 0, 0, ((TRB_STATUS as u32) << 10) | TRB_DIR_IN | TRB_IOC);
        slot.id
    };
    ring_doorbell(ctrl, slot_id, 1);
    for _ in 0..8 {
        let Some(ev) = next_event(ctrl, 1_000_000) else {
            return false;
        };
        if ev.trb_type() != TRB_TRANSFER_EVENT {
            continue;
        }
        if ev.endpoint_id() != 1 {
            defer_event(ctrl, ev);
            continue;
        }
        let cc = ev.completion_code();
        if cc == CC_SUCCESS || cc == CC_SHORT_PACKET {
            return true;
        }
        crate::warn!(
            "xhci: control-OUT request {:#04x} failed: cc={}",
            request,
            cc
        );
        return false;
    }
    false
}

/// EP0 как транспорт RNDIS: одна class-команда и один ответ.
struct Ep0Transport<'a> {
    ctrl: &'a mut Controller,
    slot_index: usize,
    iface: u8,
}

impl RndisTransport for Ep0Transport<'_> {
    fn command(&mut self, request: &[u8], response: &mut [u8]) -> Option<usize> {
        // SEND_ENCAPSULATED_COMMAND: class, interface, OUT (0x21/0x00).
        if !control_out(
            self.ctrl,
            self.slot_index,
            0x21,
            0x00,
            0,
            self.iface as u16,
            request,
        ) {
            return None;
        }
        // GET_ENCAPSULATED_RESPONSE: class, interface, IN (0xA1/0x01). Устройство
        // может ответить не сразу; несколько GET подряд покрывают эту задержку,
        // не пересылая команду повторно.
        for _ in 0..4 {
            let got = control_in(
                self.ctrl,
                self.slot_index,
                0xA1,
                0x01,
                0,
                self.iface as u16,
                response.len(),
            )?;
            if got.len() >= 8 {
                let n = got.len().min(response.len());
                response[..n].copy_from_slice(&got[..n]);
                return Some(n);
            }
        }
        None
    }
}

/// Прочитать дескриптор: `kind` — тип дескриптора, `index` — его индекс.
fn get_descriptor(
    ctrl: &mut Controller,
    slot_index: usize,
    kind: u8,
    index: u8,
    len: usize,
) -> Option<Vec<u8>> {
    control_in(
        ctrl,
        slot_index,
        0x80,
        6,
        ((kind as u16) << 8) | index as u16,
        0,
        len,
    )
}

/// Включить слот и вернуть его индекс в `ctrl.slots`.
fn enable_slot(ctrl: &mut Controller) -> Option<usize> {
    let ev = command(ctrl, 0, 0, 0, (TRB_ENABLE_SLOT as u32) << 10)?;
    if ev.completion_code() != CC_SUCCESS {
        crate::warn!("xhci: enable slot failed: cc={}", ev.completion_code());
        return None;
    }
    // Стадию порта записывает вызывающий, который знает порт; сама команда слота
    // сообщает лишь, был ли у контроллера свободный слот.
    let id = ev.slot_id();
    if id == 0 {
        return None;
    }
    // Запись device context для этого слота должна указывать на device context до
    // выдачи ADDRESS_DEVICE.
    let ep0 = Ring::new("ep0 ring")?;
    let input_ctx = DmaFrame::new_zeroed("input context")?;
    let device_ctx = DmaFrame::new_zeroed("device context")?;
    let ctrl_buf = DmaFrame::new_zeroed("control buffer")?;
    // SAFETY: DCBAA — наш кадр, а `id` — слот, который контроллер только что выдал,
    // поэтому индекс лежит внутри массива.
    unsafe {
        let entry =
            (crate::memory::vmm::phys_to_virt(ctrl.dcbaa_phys) as *mut u64).add(id as usize);
        write_volatile(entry, device_ctx.phys());
    }
    ctrl.slots.push(Slot {
        id,
        port: 0,
        speed: 0,
        ep0,
        input_ctx,
        device_ctx,
        ctrl_buf,
        hid: Vec::new(),
        rndis: None,
    });
    Some(ctrl.slots.len() - 1)
}

/// Собрать input context для ADDRESS_DEVICE и выдать команду.
fn address_device(ctrl: &mut Controller, slot_index: usize, port: u8, speed: u8) -> bool {
    let ctx_size = ctrl.ctx_size;
    let Some(slot) = ctrl.slots.get(slot_index) else {
        return false;
    };
    let input = slot.input_ctx.virt();
    let input_phys = slot.input_ctx.phys();
    let ep0_phys = slot.ep0.base_phys();
    let device_ctx_phys = slot.device_ctx.phys();
    let slot_id = slot.id;
    let max_packet = ep0_max_packet(speed);

    // SAFETY: `input` — наша обнулённая страница; Input Control Context (32 B),
    // slot context и EP0 context лежат в ней по смещениям, которые спецификация
    // задаёт для контроллера с таким размером контекста.
    unsafe {
        let p = input as *mut u32;
        // ICC: добавить slot + EP0.
        write_volatile(p, 0); // drop flags
        write_volatile(p.add(1), 0b11); // add flags: A0 (slot) + A1 (EP0)
                                        // Slot context: скорость + порт + одна запись контекста.
        let slot_ctx = p.add(8) as *mut u32; // 32-byte ICC
        write_volatile(
            slot_ctx,
            (1 << SLOT_CTX_ENTRIES_SHIFT) | ((speed as u32) << SLOT_CTX_SPEED_SHIFT),
        );
        write_volatile(slot_ctx.add(1), (port as u32) << SLOT_CTX_PORT_SHIFT);
        // EP0 context. DWORD 0 несёт только interval (0 для control-эндпоинта); CErr,
        // тип эндпоинта и максимальный размер пакета живут в DWORD 1. Раскладка важна:
        // CErr и max packet в DWORD 0 попадают в interval и оставляют EP info 2 пустым.
        // Control-передачи могут выживать на дефолтах EP0 устройства, поэтому ошибка
        // незаметна, пока не нужно сконфигурировать interrupt-эндпоинт.
        let ep_ctx = (input + ctx_size * 2) as *mut u32;
        write_volatile(ep_ctx, 0);
        write_volatile(
            ep_ctx.add(1),
            (3 << EP_CTX_CERR_SHIFT)
                | ((EP_TYPE_CONTROL) << EP_CTX_EP_TYPE_SHIFT)
                | ((max_packet) << EP_CTX_MAX_PACKET_SHIFT),
        );
        write_volatile(ep_ctx.add(2), (ep0_phys | 1) as u32);
        write_volatile(ep_ctx.add(3), (ep0_phys >> 32) as u32);
    }
    let _ = device_ctx_phys;

    // Параметр TRB — ГОСТЕВОЙ ФИЗИЧЕСКИЙ адрес; `input` — виртуальный адрес HHDM.
    // Виртуальный адрес заставлял контроллер читать чужую память и отклонять команду
    // с TRB error (cc=5).
    let (a0, a1) = ctx_addr(input_phys);
    let ev = command(
        ctrl,
        a0,
        a1,
        0,
        ((TRB_ADDRESS_DEVICE as u32) << 10) | ((slot_id as u32) << 24),
    );
    match ev {
        Some(ev) if ev.completion_code() == CC_SUCCESS => {
            ctrl.stage(port, "addressed", 0);
            true
        }
        Some(ev) => {
            ctrl.fail(port, "address", ev.completion_code() as u32);
            crate::warn!("xhci: address device failed: cc={}", ev.completion_code());
            false
        }
        None => {
            ctrl.fail(port, "address-noevent", 0);
            false
        }
    }
}

/// Обновить максимальный размер пакета EP0 после чтения первого дескриптора.
///
/// Full/low-speed устройство может начать с 8-байтного EP0 и сообщить настоящий
/// `bMaxPacketSize0` только в дескрипторе устройства; чтение 18-байтного
/// дескриптора через 8-байтный EP0 на реальном железе падает. Evaluate Context —
/// штатный способ изменить размер: input context снова несёт slot и EP0
/// контексты.
fn evaluate_ep0_max_packet(ctrl: &mut Controller, slot_index: usize, max_packet: u32) -> bool {
    let ctx_size = ctrl.ctx_size;
    let (input, input_phys, ep0_phys, slot_id, port, speed) = {
        let Some(slot) = ctrl.slots.get(slot_index) else {
            return false;
        };
        (
            slot.input_ctx.virt(),
            slot.input_ctx.phys(),
            slot.ep0.base_phys(),
            slot.id,
            slot.port,
            slot.speed,
        )
    };
    // SAFETY: наша обнулённая страница; смещения заданы спецификацией для такого размера контекста.
    unsafe {
        let p = input as *mut u32;
        core::ptr::write_bytes(input as *mut u8, 0, PAGE_SIZE);
        write_volatile(p, 0); // nothing to drop
        write_volatile(p.add(1), 0b11); // add slot + EP0
        let slot_ctx = p.add(8) as *mut u32;
        write_volatile(
            slot_ctx,
            (1 << SLOT_CTX_ENTRIES_SHIFT) | ((speed as u32) << SLOT_CTX_SPEED_SHIFT),
        );
        write_volatile(slot_ctx.add(1), (port as u32) << SLOT_CTX_PORT_SHIFT);
        let ep_ctx = (input + ctx_size * 2) as *mut u32;
        write_volatile(ep_ctx, 0);
        write_volatile(
            ep_ctx.add(1),
            (3 << EP_CTX_CERR_SHIFT)
                | (EP_TYPE_CONTROL << EP_CTX_EP_TYPE_SHIFT)
                | (max_packet << EP_CTX_MAX_PACKET_SHIFT),
        );
        write_volatile(ep_ctx.add(2), (ep0_phys | 1) as u32);
        write_volatile(ep_ctx.add(3), (ep0_phys >> 32) as u32);
    }
    let (a0, a1) = ctx_addr(input_phys);
    let ev = command(
        ctrl,
        a0,
        a1,
        0,
        ((TRB_EVALUATE_CONTEXT as u32) << 10) | ((slot_id as u32) << 24),
    );
    matches!(ev, Some(ev) if ev.completion_code() == CC_SUCCESS)
}

/// Сконфигурировать все эндпоинты устройства ОДНОЙ командой Configure Endpoint.
///
/// Поле «Context Entries» slot context должно быть старшим DCI, который несёт
/// device context; добавление эндпоинтов по одному переписывало бы slot context и
/// слало N команд на N интерфейсов. Совместная конфигурация HID и RNDIS ещё и
/// согласует состояние эндпоинтов xHCI с единственным SET_CONFIGURATION: у
/// телефона communication- и data-интерфейсы принадлежат одной конфигурации, и
/// раздельные Configure Endpoint оставили бы часть эндпоинтов без контекста.
///
/// `wMaxPacketSize` эндпоинта — нижняя граница буфера передачи: interrupt-передача
/// может законно доставить полный пакет, а запрос короче — это babble (код
/// завершения 3). Bulk-буферы берутся с запасом на целый Ethernet-кадр.
fn configure_endpoints(
    ctrl: &mut Controller,
    slot_index: usize,
    ifaces: &[HidIface],
    modes: &[ReportMode],
    rndis: Option<&RndisIface>,
) -> bool {
    if ifaces.len() != modes.len() || (ifaces.is_empty() && rndis.is_none()) {
        return false;
    }
    let ctx_size = ctrl.ctx_size;
    let (input, input_phys, slot_id, speed) = {
        let slot = &ctrl.slots[slot_index];
        (
            slot.input_ctx.virt(),
            slot.input_ctx.phys(),
            slot.id,
            slot.speed,
        )
    };
    // Сначала кольца и буферы: их физические адреса идут в контексты эндпоинтов. При
    // отказе Vec/Option дропаются и возвращают кадры в PMM, так что неудачный Configure
    // Endpoint ничего не оставляет.
    let mut eps: Vec<HidEndpoint> = Vec::with_capacity(ifaces.len());
    for (i, iface) in ifaces.iter().enumerate() {
        let Some(ring) = Ring::new("hid ring") else {
            return false;
        };
        let Some(buf) = DmaFrame::new_zeroed("hid buffer") else {
            return false;
        };
        let len = usb_hid_report::transfer_len(iface.max_packet);
        if (iface.max_packet & 0x07FF) as usize > usb_hid_report::MAX_REPORT {
            crate::warn!(
                "xhci: iface {}: wMaxPacketSize {} exceeds the {}B transfer buffer; clamped (a longer report would babble)",
                iface.interface,
                iface.max_packet,
                len
            );
        }
        eps.push(HidEndpoint {
            iface: iface.interface,
            subclass: iface.subclass,
            protocol: iface.protocol,
            mode: modes[i],
            dci: dci_of(iface.ep_addr),
            ring,
            buf,
            len,
            recoveries: 0,
            bad_reports: 0,
            bad_report_logged: false,
            disabled: false,
        });
    }
    let rndis_ep = match rndis {
        Some(iface) => {
            let in_mps = (iface.bulk_in_max_packet & 0x07FF) as u32;
            let out_mps = (iface.bulk_out_max_packet & 0x07FF) as u32;
            if in_mps == 0 || out_mps == 0 {
                crate::warn!(
                    "xhci: RNDIS bulk endpoint has zero wMaxPacketSize (in={} out={})",
                    iface.bulk_in_max_packet,
                    iface.bulk_out_max_packet
                );
                return false;
            }
            let Some(in_ring) = Ring::new("rndis in ring") else {
                return false;
            };
            let Some(in_buf) = DmaFrame::new_zeroed("rndis in buffer") else {
                return false;
            };
            let Some(out_ring) = Ring::new("rndis out ring") else {
                return false;
            };
            let Some(out_buf) = DmaFrame::new_zeroed("rndis out buffer") else {
                return false;
            };
            let in_dci = dci_of(iface.bulk_in_addr);
            let out_dci = dci_of(iface.bulk_out_addr);
            // Один DCI не может принадлежать двум эндпоинтам: композитный
            // дескриптор, переиспользующий номер, отвергается до Configure Endpoint.
            if in_dci == out_dci || eps.iter().any(|e| e.dci == in_dci || e.dci == out_dci) {
                crate::warn!(
                    "xhci: RNDIS endpoints {:#04x}/{:#04x} collide with another interface's DCI",
                    iface.bulk_in_addr,
                    iface.bulk_out_addr
                );
                return false;
            }
            Some(RndisEndpoint {
                iface: *iface,
                bulk_in_dci: in_dci,
                bulk_out_dci: out_dci,
                in_ring,
                in_buf,
                in_len: usb_rndis_proto::MAX_TRANSFER,
                in_outstanding: false,
                out_ring,
                out_buf,
                out_len: PAGE_SIZE,
                out_outstanding: false,
                in_recoveries: 0,
                out_recoveries: 0,
                in_disabled: false,
                out_disabled: false,
                in_error_logged: false,
                out_error_logged: false,
                keepalive_xid: 1,
            })
        }
        None => None,
    };
    // Старший присутствующий DCI задаёт поле Context Entries slot context.
    let max_dci = eps
        .iter()
        .map(|ep| ep.dci)
        .chain(rndis_ep.as_ref().map(|r| r.bulk_in_dci))
        .chain(rndis_ep.as_ref().map(|r| r.bulk_out_dci))
        .max()
        .unwrap_or(1);
    // SAFETY: `input` — наша обнулённая страница; контексты пишутся по смещениям,
    // которые диктует размер контекста контроллера, и каждый DCI <= 31 (номер
    // эндпоинта устройства — 4 бита), поэтому записи остаются в странице.
    unsafe {
        let p = input as *mut u32;
        core::ptr::write_bytes(input as *mut u8, 0, PAGE_SIZE);
        // Input Control Context: A0 (slot context) плюс Add-флаг на каждый DCI.
        let mut add = 1u32;
        for ep in &eps {
            add |= 1u32 << ep.dci;
        }
        if let Some(r) = rndis_ep.as_ref() {
            add |= 1u32 << r.bulk_in_dci;
            add |= 1u32 << r.bulk_out_dci;
        }
        write_volatile(p.add(1), add);
        let slot_ctx = p.add(8) as *mut u32;
        write_volatile(slot_ctx, (max_dci as u32) << SLOT_CTX_ENTRIES_SHIFT);
        for (ep, iface) in eps.iter().zip(ifaces.iter()) {
            let ep_ctx = (input + ctx_size * (1 + ep.dci as usize)) as *mut u32;
            // xHCI хранит polling interval как log2(микрокадров) для high/super-speed
            // (поэтому USB bInterval=1 — поле 0) и как число кадров для full/low speed. Сырой
            // bInterval давал 2^bInterval.
            let interval_field: u32 = if speed == SPEED_HIGH || speed == SPEED_SUPER {
                (iface.interval.saturating_sub(1) & 0x0F) as u32
            } else {
                (iface.interval & 0x0F) as u32
            };
            write_volatile(ep_ctx, interval_field << EP_CTX_INTERVAL_SHIFT);
            write_volatile(
                ep_ctx.add(1),
                (3 << EP_CTX_CERR_SHIFT)
                    | (EP_TYPE_INTERRUPT_IN << EP_CTX_EP_TYPE_SHIFT)
                    | ((iface.max_packet as u32) << EP_CTX_MAX_PACKET_SHIFT),
            );
            let ring_phys = ep.ring.base_phys();
            write_volatile(ep_ctx.add(2), (ring_phys | 1) as u32);
            write_volatile(ep_ctx.add(3), (ring_phys >> 32) as u32);
        }
        if let Some(r) = rndis_ep.as_ref() {
            // Bulk-эндпоинты: interval 0, CErr 3, тип 6 (IN) / 2 (OUT). wMaxPacketSize
            // передаётся маской: биты множителя high-speed у bulk не используются.
            for (dci, ep_type, mps, ring_phys) in [
                (
                    r.bulk_in_dci,
                    EP_TYPE_BULK_IN,
                    (r.iface.bulk_in_max_packet & 0x07FF) as u32,
                    r.in_ring.base_phys(),
                ),
                (
                    r.bulk_out_dci,
                    EP_TYPE_BULK_OUT,
                    (r.iface.bulk_out_max_packet & 0x07FF) as u32,
                    r.out_ring.base_phys(),
                ),
            ] {
                let ep_ctx = (input + ctx_size * (1 + dci as usize)) as *mut u32;
                write_volatile(ep_ctx, 0);
                write_volatile(
                    ep_ctx.add(1),
                    (3 << EP_CTX_CERR_SHIFT)
                        | (ep_type << EP_CTX_EP_TYPE_SHIFT)
                        | (mps << EP_CTX_MAX_PACKET_SHIFT),
                );
                write_volatile(ep_ctx.add(2), (ring_phys | 1) as u32);
                write_volatile(ep_ctx.add(3), (ring_phys >> 32) as u32);
            }
        }
    }
    // Физический адрес: см. тот же комментарий в `address_device`.
    let (a0, a1) = ctx_addr(input_phys);
    let ev = command(
        ctrl,
        a0,
        a1,
        0,
        ((TRB_CONFIGURE_ENDPOINT as u32) << 10) | ((slot_id as u32) << 24),
    );
    match ev {
        Some(ev) if ev.completion_code() == CC_SUCCESS => {}
        other => {
            crate::warn!(
                "xhci: configure endpoint ({} HID, {} RNDIS) failed (cc={:?})",
                eps.len(),
                rndis_ep.is_some() as u8,
                other.map(|e| e.completion_code())
            );
            return false;
        }
    }
    ctrl.slots[slot_index].hid = eps;
    ctrl.slots[slot_index].rndis = rndis_ep;
    true
}

/// Перечислить устройство на `port`; true, если HID-интерфейс сконфигурирован.
fn enumerate_with_speed(ctrl: &mut Controller, port: u8, speed: u8) -> bool {
    let Some(slot_index) = enable_slot(ctrl) else {
        ctrl.fail(port, "enable-slot", 0);
        return false;
    };
    ctrl.slots[slot_index].port = port;
    ctrl.slots[slot_index].speed = speed;
    let ok = enumerate_slot(ctrl, slot_index, port, speed);
    if !ok {
        // Неудачное перечисление НЕ должно оставлять слот: цикл опроса пропускает порт,
        // у которого слот существует, и повтор никогда бы не выполнился, а у контроллера
        // всего 64 слота.
        release_slot(ctrl, slot_index);
    }
    ok
}

/// Отключить выданный контроллером слот и убрать его учёт.
fn release_slot(ctrl: &mut Controller, slot_index: usize) {
    let Some(slot) = ctrl.slots.get(slot_index) else {
        return;
    };
    let slot_id = slot.id;
    if slot.rndis.is_some() {
        // Сетевой поток не должен видеть канал, чьи bulk-эндпоинты уже не существуют.
        usb_rndis::reset();
    }
    let _ = command(
        ctrl,
        0,
        0,
        0,
        ((TRB_DISABLE_SLOT as u32) << 10) | ((slot_id as u32) << 24),
    );
    // Очистить запись DCBAA, чтобы будущий slot id не сослался на старый контекст.
    // SAFETY: DCBAA — наш кадр, а `slot_id` выдал контроллер.
    unsafe {
        let entry =
            (crate::memory::vmm::phys_to_virt(ctrl.dcbaa_phys) as *mut u64).add(slot_id as usize);
        write_volatile(entry, 0);
    }
    ctrl.slots.remove(slot_index);
}

/// Всё после Enable Slot: адресация, дескрипторы, конфигурация, HID-эндпоинт.
fn enumerate_slot(ctrl: &mut Controller, slot_index: usize, port: u8, speed: u8) -> bool {
    let speed_name = match speed {
        SPEED_LOW => "low",
        SPEED_FULL => "full",
        SPEED_HIGH => "high",
        SPEED_SUPER => "super",
        _ => "?",
    };
    if !address_device(ctrl, slot_index, port, speed) {
        return false;
    }
    // Дескриптор устройства: сначала 8 байт (у full-speed устройства на этом этапе
    // может быть только 8-байтный EP0), затем все 18.
    let Some(head) = get_descriptor(ctrl, slot_index, 1, 0, 8) else {
        ctrl.fail(port, "device-descriptor", 0);
        crate::warn!("xhci: port {}: no device descriptor", port);
        return false;
    };
    if head.len() < 8 {
        ctrl.fail(port, "device-descriptor-short", head.len() as u32);
        return false;
    }
    let vendor = u16::from_le_bytes([head[0], head[1]]);
    let product = u16::from_le_bytes([head[2], head[3]]);
    // Первые 8 байт несут bMaxPacketSize0 в смещении 7. Для full/low speed
    // контроллеру сказано 8; если устройство говорит иначе, обновить EP0 до запроса
    // 18-байтного дескриптора, иначе длинное чтение на реальном железе падает.
    let device_mps = head[7] as u32;
    let assumed_mps = ep0_max_packet(speed);
    if device_mps != assumed_mps && matches!(device_mps, 8 | 16 | 32 | 64) {
        if evaluate_ep0_max_packet(ctrl, slot_index, device_mps) {
            crate::debug!(
                "xhci: port {}: EP0 max packet {} -> {}",
                port,
                assumed_mps,
                device_mps
            );
        } else {
            crate::warn!(
                "xhci: port {}: EP0 max packet update to {} failed",
                port,
                device_mps
            );
        }
    }
    let Some(dev) = get_descriptor(ctrl, slot_index, 1, 0, 18) else {
        ctrl.fail(port, "device-descriptor-full", 0);
        return false;
    };
    if dev.len() < 18 {
        ctrl.fail(port, "device-descriptor-full-short", dev.len() as u32);
        return false;
    }
    let max_packet0 = dev[7];
    crate::debug!(
        "xhci: port {} {:04x}:{:04x} speed={} ep0={}B",
        port,
        vendor,
        product,
        speed_name,
        max_packet0
    );
    match dev[4] {
        9 => {
            ctrl.fail(port, "hub-unsupported", 9);
            crate::warn!(
            "xhci: port {} holds a USB hub; hubs are not supported (keyboard behind it is invisible)",
            port
        )
        }
        0 => {}
        class => crate::debug!("xhci: port {} device class {:#04x}", port, class),
    }
    // Конфигурация: перебрать индексы, пока не найдётся RNDIS или HID. Устройство
    // может выставить RNDIS не первой конфигурацией; QEMU — первой, телефон —
    // обычно единственной. Ограничение четырьмя не даёт лживому bNumConfigurations
    // крутить цикл на мс.
    let n_configs = dev[17].max(1).min(4);
    let mut parsed: Option<(Vec<HidIface>, Option<RndisIface>)> = None;
    for ci in 0..n_configs {
        let Some(head) = get_descriptor(ctrl, slot_index, 2, ci, 9) else {
            continue;
        };
        if head.len() < 9 {
            continue;
        }
        let total = u16::from_le_bytes([head[2], head[3]]) as usize;
        if total < 9 || total > PAGE_SIZE {
            continue;
        }
        let Some(blob) = get_descriptor(ctrl, slot_index, 2, ci, total) else {
            continue;
        };
        let hid = usb_desc::parse_hid_interfaces(&blob);
        let rndis = usb_desc::parse_rndis_interfaces(&blob);
        if !hid.is_empty() || rndis.is_some() {
            parsed = Some((hid, rndis));
            break;
        }
    }
    let Some((ifaces, rndis)) = parsed else {
        ctrl.fail(port, "no-supported-interface", dev[4] as u32);
        crate::debug!(
            "xhci: port {}: device {:04x}:{:04x} has no HID or RNDIS interface",
            port,
            vendor,
            product
        );
        return false;
    };
    // Привязывать только boot-интерфейсы клавиатуры/мыши. HID-интерфейс с protocol 0
    // (consumer/system/vendor-коллекции, даже у клавиатуры с boot-интерфейсом) не
    // имеет раскладки, которую драйвер разбирает; привязка сожгла бы кольцо и буфер
    // ради отчётов, которые `usb_hid` обязан игнорировать. Пропущенные интерфейсы
    // логируются на debug.
    let mut chosen: Vec<HidIface> = Vec::new();
    for iface in ifaces {
        if !usb_hid_report::bindable(iface.subclass, iface.protocol) {
            crate::debug!(
                "xhci: port {}: iface {} (subclass={} protocol={}) is not a boot keyboard/mouse; not bound",
                port,
                iface.interface,
                iface.subclass,
                iface.protocol
            );
            continue;
        }
        // Один номер эндпоинта устройства нельзя конфигурировать дважды: два
        // HID-интерфейса с общим эндпоинтом не могут владеть одним DCI.
        if chosen
            .iter()
            .any(|c| c.ep_addr & 0x0F == iface.ep_addr & 0x0F)
        {
            crate::warn!(
                "xhci: port {}: iface {} reuses endpoint {:#04x}; keeping the first interface",
                port,
                iface.interface,
                iface.ep_addr
            );
            continue;
        }
        chosen.push(iface);
    }
    let config_value = rndis
        .as_ref()
        .map(|r| r.config_value)
        .or_else(|| chosen.first().map(|i| i.config_value));
    let Some(config_value) = config_value else {
        ctrl.fail(port, "no-boot-hid", dev[4] as u32);
        crate::debug!(
            "xhci: port {}: device {:04x}:{:04x} has HID interfaces but none is a boot keyboard/mouse",
            port,
            vendor,
            product
        );
        return false;
    };
    // SET_CONFIGURATION активирует устройство (без него все эндпоинты отвечают STALL), и
    // именно по bConfigurationValue устройство решает, какой из наборов интерфейсов
    // (RNDIS против CDC ECM у QEMU) сейчас активно.
    if !control_no_data(ctrl, slot_index, 0x00, 9, config_value as u16, 0) {
        ctrl.fail(port, "set-configuration", config_value as u32);
        crate::warn!("xhci: port {}: SET_CONFIGURATION failed", port);
        return false;
    }
    // RNDIS поднимается до Configure Endpoint: INIT/QUERY/SET идут через EP0 и не
    // требуют bulk-контекстов, зато после неудачи незачем конфигурировать пару,
    // которой никто не будет пользоваться.
    let mut rndis_link: Option<(RndisIface, usb_rndis::LinkInfo)> = None;
    if let Some(iface) = rndis {
        let mut transport = Ep0Transport {
            ctrl,
            slot_index,
            iface: iface.comm_interface,
        };
        match usb_rndis::bring_up(&mut transport) {
            Some(info) => rndis_link = Some((iface, info)),
            None => {
                crate::warn!(
                    "usb-rndis: port {}: init failed (iface={} data={}); link is down",
                    port,
                    iface.comm_interface,
                    iface.data_interface
                );
                // Если HID на этом устройстве нет, слот бесполезен: без RNDIS здесь
                // нечего привязывать, и повтор с reset — правильный путь.
                if chosen.is_empty() {
                    ctrl.fail(port, "rndis-init", 0);
                    return false;
                }
            }
        }
    }
    // SET_PROTOCOL(0) = BOOT (HID 1.11 §7.2.6): boot фиксирует раскладку 8 байт для
    // клавиатуры и 3 для мыши. Устройство может отказать (STALL): остаёмся в report
    // protocol, записываем это, чтобы парсер не выдавал его за boot, и сохраняем
    // эндпоинт — best-effort раскладки `usb_hid` лучше мёртвой клавиатуры.
    // SET_IDLE(0) = отчёт только при изменении.
    let mut modes: Vec<ReportMode> = Vec::with_capacity(chosen.len());
    for iface in &chosen {
        let mode = if control_no_data(ctrl, slot_index, 0x21, 0x0B, 0, iface.interface as u16) {
            ReportMode::Boot
        } else {
            crate::warn!(
                "xhci: port {}: iface {} rejected SET_PROTOCOL(boot); staying in report protocol (best-effort layout)",
                port,
                iface.interface
            );
            ReportMode::Report
        };
        if !control_no_data(ctrl, slot_index, 0x21, 0x0A, 0, iface.interface as u16) {
            crate::warn!(
                "xhci: port {}: iface {}: SET_IDLE failed",
                port,
                iface.interface
            );
        }
        modes.push(mode);
    }
    let rndis_iface = rndis_link.as_ref().map(|(iface, _)| iface);
    if !configure_endpoints(ctrl, slot_index, &chosen, &modes, rndis_iface) {
        // Configure Endpoint не состоялся: состояние RNDIS уже выставлено, и без
        // сброса сетевой поток увидел бы «готовый» канал без единого эндпоинта.
        usb_rndis::reset();
        ctrl.fail(port, "configure-endpoint", 0);
        return false;
    }
    let stage = match (chosen.is_empty(), rndis_link.is_some()) {
        (true, true) => "rndis-ready",
        (false, true) => "hid+rndis-ready",
        _ => "hid-ready",
    };
    ctrl.stage(port, stage, chosen.len() as u32);
    for (iface, ep) in chosen.iter().zip(ctrl.slots[slot_index].hid.iter()) {
        // Boot class: 0 — нет, 1 — клавиатура, 2 — мышь. `mode` — чего достиг
        // SET_PROTOCOL; `report` — размер буфера передачи из wMaxPacketSize (>= целого
        // пакета, поэтому babble исключён).
        crate::info!(
            "xhci: HID device on port {}: iface={} subclass={} protocol={} ep={:#04x} report={}B mode={} maxpkt={}",
            port,
            iface.interface,
            iface.subclass,
            iface.protocol,
            iface.ep_addr,
            ep.len,
            ep.mode.as_str(),
            iface.max_packet
        );
    }
    if let Some((iface, info)) = rndis_link {
        // Строка, которую E2E-сценарий и человек ищут в serial: интерфейсы, MAC и
        // состояние канала. medium=0 — 802.3; max_transfer — то, что объявило
        // устройство, и по нему видно, влезает ли кадр в один конверт.
        crate::info!(
            "usb-rndis: iface={} data={} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} link=up medium={} max_transfer={}",
            iface.comm_interface,
            iface.data_interface,
            info.mac[0],
            info.mac[1],
            info.mac[2],
            info.mac[3],
            info.mac[4],
            info.mac[5],
            info.medium,
            info.max_transfer
        );
    }
    true
}

/// Один проход опроса: перечислить новые подключения, затем собрать HID-отчёты и
/// передать их в [`crate::drivers::usb_hid`].
pub fn poll() {
    let mut guard = CONTROLLER.lock();
    let Some(ctrl) = guard.as_mut() else {
        return;
    };
    // Остановленный контроллер — худший отказ драйвера: HCH=1 означает, что он
    // перестал обходить кольца, и регистры портов хранят значения на момент смерти.
    // Если этого не замечать, экран показывает замороженное состояние как живое и
    // повторяет записи, которых контроллер не видит. Обнаружить, запомнить
    // доказывающие регистры и перезапустить.
    if rd32(ctrl.op, OP_USBSTS) & USBSTS_HCH != 0 {
        ctrl.halts = ctrl.halts.saturating_add(1);
        let cmd = rd32(ctrl.op, OP_USBCMD);
        let sts = rd32(ctrl.op, OP_USBSTS);
        if ctrl.halts == 1 {
            ctrl.halt_cmd = cmd;
            ctrl.halt_sts = sts;
            crate::warn!(
                "xhci: controller halted (usbcmd={:#010x} usbsts={:#010x} hse={} hce={}): restarting",
                cmd,
                sts,
                (sts & USBSTS_HSE != 0) as u8,
                (sts & USBSTS_HCE != 0) as u8
            );
        }
        if !restart_controller(ctrl) {
            crate::warn!(
                "xhci: controller {} did not come back (usbcmd={:#010x} usbsts={:#010x})",
                ctrl.halts,
                rd32(ctrl.op, OP_USBCMD),
                rd32(ctrl.op, OP_USBSTS)
            );
            return;
        }
        crate::debug!("xhci: controller restarted (halt #{})", ctrl.halts);
    }
    // 1. За poll продвигать политику ОДНОГО порта, циклически. Политика не
    //    блокируется (debounce/reset/backoff — дедлайн, проверяемый на следующих
    //    poll), поэтому медленный или мёртвый порт стоит одного чтения PORTSC за
    //    проход, а отсчёт boot-меню продолжается. Один порт за poll ещё и не даёт
    //    перечислению нового устройства задержать дедлайны остальных больше чем на
    //    проход.
    let port = ctrl.next_port;
    ctrl.next_port = if port >= ctrl.ports { 1 } else { port + 1 };
    poll_port(ctrl, port);

    // 2. Вооружить каждый простаивающий HID interrupt-IN эндпоинт. Завершения
    //    собираются после, а не по эндпоинту, чтобы завершение одного устройства не
    //    проглатывалось при обслуживании другого.
    for si in 0..ctrl.slots.len() {
        let slot_id = ctrl.slots[si].id;
        for hi in 0..ctrl.slots[si].hid.len() {
            let dci = {
                let hid = &mut ctrl.slots[si].hid[hi];
                if hid.disabled || hid.ring.outstanding {
                    None
                } else {
                    let buf_phys = hid.buf.phys();
                    hid.ring.push(
                        buf_phys as u32,
                        (buf_phys >> 32) as u32,
                        hid.len as u32,
                        ((TRB_NORMAL as u32) << 10) | TRB_IOC,
                    );
                    hid.ring.outstanding = true;
                    Some(hid.dci)
                }
            };
            if let Some(dci) = dci {
                ring_doorbell(ctrl, slot_id, dci);
            }
        }
    }
    // 3. RNDIS: вооружить bulk-IN и отправить не больше одного кадра из TX-очереди.
    //    Кольца и очереди разные, поэтому отправка кадра не может задержать приём.
    for si in 0..ctrl.slots.len() {
        let slot_id = ctrl.slots[si].id;
        let in_dci = {
            let Some(r) = ctrl.slots[si].rndis.as_mut() else {
                continue;
            };
            if r.in_disabled || r.in_outstanding {
                None
            } else {
                let buf_phys = r.in_buf.phys();
                r.in_ring.push(
                    buf_phys as u32,
                    (buf_phys >> 32) as u32,
                    r.in_len as u32,
                    ((TRB_NORMAL as u32) << 10) | TRB_IOC,
                );
                r.in_outstanding = true;
                Some(r.bulk_in_dci)
            }
        };
        if let Some(dci) = in_dci {
            ring_doorbell(ctrl, slot_id, dci);
        }
        let out_ready = ctrl.slots[si]
            .rndis
            .as_ref()
            .map_or(false, |r| !r.out_disabled && !r.out_outstanding);
        if !out_ready {
            continue;
        }
        let Some(frame) = usb_rndis::take_tx() else {
            continue;
        };
        let out_dci = {
            let Some(r) = ctrl.slots[si].rndis.as_mut() else {
                continue;
            };
            // SAFETY: `out_buf` — наша DMA-страница, `out_len` — её размер; срез живёт
            // до конца этой записи, а контроллер читает буфер только после doorbell.
            let buf =
                unsafe { core::slice::from_raw_parts_mut(r.out_buf.virt() as *mut u8, r.out_len) };
            match usb_rndis_proto::encode_data_into(&frame, buf) {
                Some(n) => {
                    let buf_phys = r.out_buf.phys();
                    r.out_ring.push(
                        buf_phys as u32,
                        (buf_phys >> 32) as u32,
                        n as u32,
                        ((TRB_NORMAL as u32) << 10) | TRB_IOC,
                    );
                    r.out_outstanding = true;
                    Some(r.bulk_out_dci)
                }
                None => {
                    usb_rndis::note_tx_error();
                    None
                }
            }
        };
        if let Some(dci) = out_dci {
            ring_doorbell(ctrl, slot_id, dci);
        }
    }
    // Keepalive — обычный control-запрос, bulk-трафику он не мешает: у EP0 своё
    // кольцо. Опрос идёт по счётчику poll'ов, а не по тикам, чтобы работать и в
    // boot-меню, где тик планировщика может не двигаться.
    if usb_rndis::keepalive_due() {
        if let Some(si) = ctrl.slots.iter().position(|s| s.rndis.is_some()) {
            let (iface, xid) = {
                let r = ctrl.slots[si].rndis.as_ref().unwrap();
                (r.iface.comm_interface, r.keepalive_xid)
            };
            let ok = {
                let mut transport = Ep0Transport {
                    ctrl,
                    slot_index: si,
                    iface,
                };
                usb_rndis::keepalive(&mut transport, xid)
            };
            if let Some(r) = ctrl.slots[si].rndis.as_mut() {
                r.keepalive_xid = xid.wrapping_add(1).max(1);
            }
            usb_rndis::note_keepalive(ok);
        }
    }
    // 4. Собрать события завершения. Каждое transfer event несёт slot и эндпоинт;
    //    маршрутизировать по ним.
    let mut events = 0usize;
    while events < 64 {
        let Some(ev) = next_poll_event(ctrl, 0) else {
            break;
        };
        if ev.trb_type() != TRB_TRANSFER_EVENT {
            continue;
        }
        events += 1;
        let slot_id = ev.slot_id();
        let dci = ev.endpoint_id();
        let Some(si) = ctrl.slots.iter().position(|s| s.id == slot_id) else {
            continue;
        };
        // RNDIS bulk-эндпоинты имеют собственные DCI; HID-поиск ниже их не
        // перехватывает, но явная ветка делает маршрут читаемым.
        if ctrl.slots[si]
            .rndis
            .as_ref()
            .map_or(false, |r| r.bulk_in_dci == dci)
        {
            handle_rndis_in(ctrl, si, ev);
            continue;
        }
        if ctrl.slots[si]
            .rndis
            .as_ref()
            .map_or(false, |r| r.bulk_out_dci == dci)
        {
            handle_rndis_out(ctrl, si, ev);
            continue;
        }
        let Some(hi) = ctrl.slots[si]
            .hid
            .iter()
            .position(|h| h.dci == dci && !h.disabled)
        else {
            continue;
        };
        // Снять снимок нужного завершению; держать заимствования слота короткими,
        // потому что восстановление шлёт команду через `ctrl`.
        let (len, mode, subclass, protocol, buf_virt) = {
            let hid = &ctrl.slots[si].hid[hi];
            (
                hid.len,
                hid.mode,
                hid.subclass,
                hid.protocol,
                hid.buf.virt(),
            )
        };
        ctrl.slots[si].hid[hi].ring.outstanding = false;
        let cc = ev.completion_code();
        if cc == CC_SUCCESS || cc == CC_SHORT_PACKET {
            let got = len.saturating_sub(ev.residual() as usize);
            if got == 0 {
                continue;
            }
            let mut report = [0u8; usb_hid_report::MAX_REPORT];
            let n = got.min(report.len());
            // SAFETY: буфер — наша DMA-страница, `n <= len <= MAX_REPORT`, и контроллер
            // записал в неё не больше `len` байт до публикации этого transfer event.
            unsafe {
                core::ptr::copy_nonoverlapping(buf_virt as *const u8, report.as_mut_ptr(), n);
            }
            let report = &report[..n];
            let required = usb_hid_report::minimum_report_len(subclass, protocol, mode);
            if n < required {
                ctrl.hid_bad_reports += 1;
                let hid = &mut ctrl.slots[si].hid[hi];
                hid.bad_reports += 1;
                if !hid.bad_report_logged {
                    hid.bad_report_logged = true;
                    crate::warn!(
                        "xhci: HID iface {} (DCI {}) got a {}B report, layout needs {}B (mode={}); report dropped (logging once)",
                        hid.iface,
                        dci,
                        n,
                        required,
                        mode.as_str()
                    );
                }
            } else {
                crate::drivers::usb_hid::feed_iface(report, subclass, protocol, mode);
                ctrl.reports += 1;
            }
            continue;
        }
        // Ошибка передачи (babble, data-buffer error, stall, USB transaction error, …):
        // восстановить, а не отключать эндпоинт с первой ошибкой. Отключение — последнее
        // средство, после `MAX_RECOVERIES` неудачных reset.
        let (previous, iface) = {
            let hid = &ctrl.slots[si].hid[hi];
            (hid.recoveries, hid.iface)
        };
        match usb_hid_report::recovery_action(previous) {
            RecoveryAction::ResetEndpoint => {
                ctrl.slots[si].hid[hi].recoveries += 1;
                let attempt = ctrl.slots[si].hid[hi].recoveries;
                let ok = reset_endpoint(ctrl, slot_id, dci);
                if ok {
                    ctrl.hid_recoveries += 1;
                }
                crate::warn!(
                    "xhci: HID iface {} (DCI {}) transfer error cc={}: Reset Endpoint {} ({}/{})",
                    iface,
                    dci,
                    cc,
                    if ok { "ok, re-arming" } else { "failed" },
                    attempt,
                    usb_hid_report::MAX_RECOVERIES
                );
                // Ошибка пришла от этого TRB, поэтому незавершённых TRB в кольце нет: следующий
                // poll положит новый Normal TRB.
                ctrl.slots[si].hid[hi].ring.outstanding = false;
            }
            RecoveryAction::Disable => {
                let hid = &mut ctrl.slots[si].hid[hi];
                hid.disabled = true;
                hid.ring.outstanding = false;
                crate::warn!(
                    "xhci: HID iface {} (DCI {}) still failing cc={} after {} Reset Endpoint attempt(s); input from it is off until reboot",
                    hid.iface,
                    dci,
                    cc,
                    usb_hid_report::MAX_RECOVERIES
                );
            }
        }
    }
}

/// Завершение bulk-IN RNDIS: разобрать батч конвертов и отдать кадры сетевому
/// стеку. Ошибка передачи восстанавливается Reset Endpoint, как у HID: без этого
/// одна babble-передача выключила бы сеть до перезагрузки.
fn handle_rndis_in(ctrl: &mut Controller, si: usize, ev: Trb) {
    let (len, buf_virt, dci) = {
        let Some(r) = ctrl.slots[si].rndis.as_ref() else {
            return;
        };
        (r.in_len, r.in_buf.virt(), r.bulk_in_dci)
    };
    if let Some(r) = ctrl.slots[si].rndis.as_mut() {
        r.in_outstanding = false;
    }
    let cc = ev.completion_code();
    if cc == CC_SUCCESS || cc == CC_SHORT_PACKET {
        let got = len.saturating_sub(ev.residual() as usize).min(len);
        if got == 0 {
            return;
        }
        // SAFETY: контроллер записал в `in_buf` не больше `len` байт и опубликовал
        // transfer event; срез не переживает этот обработчик.
        let data = unsafe { core::slice::from_raw_parts(buf_virt as *const u8, got) };
        let count = usb_rndis_proto::for_each_data_packet(data, 32, |frame| {
            usb_rndis::push_rx_frame(frame);
        });
        if count == 0 {
            // Конверт целиком битый: это не «нет данных», а ошибка протокола.
            usb_rndis::note_rx_error();
        } else if let Some(r) = ctrl.slots[si].rndis.as_mut() {
            r.in_recoveries = 0;
            r.in_error_logged = false;
        }
        return;
    }
    usb_rndis::note_rx_error();
    let (previous, iface) = {
        let Some(r) = ctrl.slots[si].rndis.as_ref() else {
            return;
        };
        (r.in_recoveries, r.iface.comm_interface)
    };
    if previous < usb_hid_report::MAX_RECOVERIES {
        if let Some(r) = ctrl.slots[si].rndis.as_mut() {
            r.in_recoveries = previous + 1;
        }
        let ok = reset_endpoint(ctrl, ctrl.slots[si].id, dci);
        if let Some(r) = ctrl.slots[si].rndis.as_mut() {
            if !r.in_error_logged {
                r.in_error_logged = true;
                crate::warn!(
                    "usb-rndis: iface {} bulk-IN transfer error cc={}: Reset Endpoint {} ({}/{})",
                    iface,
                    cc,
                    if ok { "ok, re-arming" } else { "failed" },
                    previous + 1,
                    usb_hid_report::MAX_RECOVERIES
                );
            }
        }
    } else if let Some(r) = ctrl.slots[si].rndis.as_mut() {
        r.in_disabled = true;
        crate::warn!(
            "usb-rndis: iface {} bulk-IN still failing cc={} after {} Reset Endpoint attempt(s); RX is off until the device is re-enumerated",
            iface,
            cc,
            usb_hid_report::MAX_RECOVERIES
        );
    }
}

/// Завершение bulk-OUT RNDIS. Кадр уже в TX-очереди не хранится: потеря
/// восстанавливается ретрансмиссией TCP, а не повтором на уровне USB.
fn handle_rndis_out(ctrl: &mut Controller, si: usize, ev: Trb) {
    let (dci, iface) = {
        let Some(r) = ctrl.slots[si].rndis.as_ref() else {
            return;
        };
        (r.bulk_out_dci, r.iface.comm_interface)
    };
    if let Some(r) = ctrl.slots[si].rndis.as_mut() {
        r.out_outstanding = false;
    }
    let cc = ev.completion_code();
    if cc == CC_SUCCESS || cc == CC_SHORT_PACKET {
        usb_rndis::note_tx_done();
        if let Some(r) = ctrl.slots[si].rndis.as_mut() {
            r.out_recoveries = 0;
            r.out_error_logged = false;
        }
        return;
    }
    usb_rndis::note_tx_error();
    let previous = ctrl.slots[si]
        .rndis
        .as_ref()
        .map(|r| r.out_recoveries)
        .unwrap_or(0);
    if previous < usb_hid_report::MAX_RECOVERIES {
        if let Some(r) = ctrl.slots[si].rndis.as_mut() {
            r.out_recoveries = previous + 1;
        }
        let ok = reset_endpoint(ctrl, ctrl.slots[si].id, dci);
        if let Some(r) = ctrl.slots[si].rndis.as_mut() {
            if !r.out_error_logged {
                r.out_error_logged = true;
                crate::warn!(
                    "usb-rndis: iface {} bulk-OUT transfer error cc={}: Reset Endpoint {} ({}/{})",
                    iface,
                    cc,
                    if ok { "ok, re-arming" } else { "failed" },
                    previous + 1,
                    usb_hid_report::MAX_RECOVERIES
                );
            }
        }
    } else if let Some(r) = ctrl.slots[si].rndis.as_mut() {
        r.out_disabled = true;
        crate::warn!(
            "usb-rndis: iface {} bulk-OUT still failing cc={} after {} Reset Endpoint attempt(s); TX is off until the device is re-enumerated",
            iface,
            cc,
            usb_hid_report::MAX_RECOVERIES
        );
    }
}

/// Восстановить остановленный эндпоинт командой xHCI Reset Endpoint (TRB
/// type 14: slot id в битах 24-31, DCI в 16-20).
///
/// Просто переармировать нельзя: ошибка передачи переводит эндпоинт в Halted, и
/// повторная постановка TRB даёт ту же ошибку. Reset Endpoint возвращает эндпоинт
/// в Stopped/Running, чтобы Normal TRB следующего poll выполнился.
fn reset_endpoint(ctrl: &mut Controller, slot_id: u8, dci: u8) -> bool {
    let ev = command(
        ctrl,
        0,
        0,
        0,
        ((TRB_RESET_ENDPOINT as u32) << 10) | ((dci as u32) << 16) | ((slot_id as u32) << 24),
    );
    matches!(ev, Some(ev) if ev.completion_code() == CC_SUCCESS)
}

/// Вернуть остановленный контроллер: сбросить, снова указать на наши кольца,
/// запустить.
///
/// Всё нужное принадлежит `Controller` (кольца, DCBAA, массив scratchpad),
/// поэтому эти кадры — поля: после сброса контроллер перечитывает их, и они
/// должны оставаться нашими.
fn restart_controller(ctrl: &mut Controller) -> bool {
    // Сначала стоп, затем сброс. Halt может быть HCE; запись в порт или команда его
    // не снимут, а сброс контроллера — снимет.
    wr32(ctrl.op, OP_USBCMD, 0);
    let _ = spin_until(|| rd32(ctrl.op, OP_USBSTS) & USBSTS_HCH != 0, 200_000);
    wr32(ctrl.op, OP_USBCMD, USBCMD_HCRST);
    if !spin_until(|| rd32(ctrl.op, OP_USBCMD) & USBCMD_HCRST == 0, 2_000_000) {
        return false;
    }
    if !spin_until(|| rd32(ctrl.op, OP_USBSTS) & USBSTS_CNR == 0, 2_000_000) {
        return false;
    }
    let max_slots = (ctrl.hcsparams1 & 0xFF).min(u8::MAX as u32);
    wr32(ctrl.op, OP_CONFIG, max_slots);
    wr64(ctrl.op, OP_DCBAAP, ctrl.dcbaa.phys());
    wr64(ctrl.op, OP_CRCR, ctrl.cmdr.base_phys() | TRB_CYCLE as u64);
    wr32(ctrl.rt + RTS_IR0, IR_ERSTSZ, 1);
    wr64(ctrl.rt + RTS_IR0, IR_ERSTBA, ctrl.erst.phys());
    wr64(ctrl.rt + RTS_IR0, IR_ERDP, ctrl.event.base_phys());
    wr32(ctrl.op, OP_USBCMD, USBCMD_RS);
    if !spin_until(|| rd32(ctrl.op, OP_USBSTS) & USBSTS_HCH == 0, 2_000_000) {
        return false;
    }
    // Сброс контроллера забывает все слоты и порты; кольца начинаются с первого TRB
    // снова с установленным cycle bit.
    ctrl.slots.clear();
    // Сброс контроллера забыл все эндпоинты: RNDIS-канал больше не существует.
    usb_rndis::reset();
    ctrl.deferred_events.clear();
    ctrl.brought_up.clear();
    ctrl.devices = 0;
    ctrl.cmdr.enqueue = 0;
    ctrl.cmdr.cycle = TRB_CYCLE;
    ctrl.cmdr.outstanding = false;
    ctrl.event.dequeue = 0;
    ctrl.event.cycle = TRB_CYCLE;
    for d in ctrl.diag.iter_mut() {
        d.stage = "";
        d.code = 0;
        d.speed = 0;
        d.attempts = 0;
        d.pre = 0;
        d.post = 0;
        d.seen_prc = false;
        d.seen_ped = false;
        d.seen_ped_u0 = false;
        d.ped_sc = 0;
        d.ms = 0;
        d.policy = PortPolicy::new(d.proto == 3);
    }
    true
}

/// Цикл опроса, работающий в потоке ядра `usb`.
///
/// Один проход: перечислить новые подключения, затем собрать отчёты
/// сконфигурированных HID-эндпоинтов. Сон в 1 мс между проходами ничего заметного
/// не стоит (передачи переармируются, а не перезапускаются) и не мешает shell и
/// сетевому насосу.
pub fn usb_thread() {
    loop {
        poll();
        crate::task::scheduler::sleep_ticks(1);
    }
}

/// Одна строка на порт, где есть (или было) устройство, для boot-меню.
///
/// `stage` — где остановилось перечисление (`hid-ready` означает, что устройство
/// печатает); два снимка PORTSC отделяют «порт не включился» от «устройство не
/// ответило после включения»; `ped_sc` — снимок PORTSC на последнем PED+U0 (или
/// на последнем кратковременном PED при `ped_u0=0`); `proto` — USB2 (где живёт
/// клавиатура) или USB3.
pub struct PortRow {
    pub port: u8,
    pub portsc: u32,
    pub stage: &'static str,
    pub code: u32,
    pub speed: u8,
    pub pre: u32,
    pub post: u32,
    pub attempts: u8,
    pub seen_prc: bool,
    pub seen_ped: bool,
    /// `ped_sc` — настоящий снимок включения PED+U0 (true) или последний
    /// кратковременный PED во время reset/training (false).
    pub seen_ped_u0: bool,
    pub ped_sc: u32,
    pub proto: u8,
    /// Длительность последней попытки в миллисекундах (0 без часов).
    pub ms: u16,
}

pub fn diagnostics() -> Vec<PortRow> {
    let guard = CONTROLLER.lock();
    let Some(c) = guard.as_ref() else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for port in 1..=c.ports {
        let sc = rd32(c.op, portsc(port));
        let Some(d) = c.diag.get(port as usize - 1) else {
            continue;
        };
        // Только интересные порты: подключено устройство, была попытка (устройство
        // исчезло) или стадия — реальный отказ.
        if sc & PORTSC_CCS == 0 && d.stage.is_empty() {
            continue;
        }
        if sc & PORTSC_CCS == 0 && d.stage == "no-device" {
            continue;
        }
        rows.push(PortRow {
            port,
            portsc: sc,
            stage: d.stage,
            code: d.code,
            speed: d.speed,
            pre: d.pre,
            post: d.post,
            attempts: d.attempts,
            seen_prc: d.seen_prc,
            seen_ped: d.seen_ped,
            seen_ped_u0: d.seen_ped_u0,
            ped_sc: d.ped_sc,
            proto: d.proto,
            ms: d.ms,
        });
    }
    rows
}

/// Сырой PORTSC каждого порта для shell-команды `usb`.
///
/// Главная диагностика на железе: если клавиатура подключена, а порт читается как
/// `ccs=false`, проблема ниже драйвера (кабель, порт или устройство не сообщает
/// скорость); если порт читается `ccs=true`, но строки `HID device on port N` не
/// было, сбой в шагах перечисления, и лог называет остановившийся шаг.
pub fn port_status() -> Vec<(u8, u32)> {
    let guard = CONTROLLER.lock();
    let Some(ctrl) = guard.as_ref() else {
        return Vec::new();
    };
    (1..=ctrl.ports)
        .map(|port| {
            let sc = rd32(ctrl.op, portsc(port));
            (port, sc)
        })
        .collect()
}

/// Декодировать PORTSC для вывода: `(ccs, ped, pp, speed, pls)`.
pub fn decode_portsc(sc: u32) -> (bool, bool, bool, u8, u8) {
    (
        sc & PORTSC_CCS != 0,
        sc & PORTSC_PED != 0,
        sc & PORTSC_PP != 0,
        ((sc & PORTSC_SPEED_MASK) >> PORTSC_SPEED_SHIFT) as u8,
        ((sc & PORTSC_PLS_MASK) >> 5) as u8,
    )
}

/// `(present, ports, reports)` для диагностики shell.
pub fn status() -> (bool, u8, u64) {
    match CONTROLLER.lock().as_ref() {
        Some(c) => (true, c.ports, c.reports),
        None => (false, 0, 0),
    }
}
