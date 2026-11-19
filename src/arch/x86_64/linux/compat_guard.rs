//! Кто именно находится внутри [`with_current_compat`] — владелец и глубина.
//!
//! ## Зачем отдельный модуль
//!
//! `COMPAT_STATES` — нереентерабельный спинлок, поэтому обработчик #PF обязан
//! знать, не находится ли ТЕКУЩИЙ контекст уже внутри замыкания `with_current_compat`
//! (повторный `lock()` с замаскированными прерываниями повесил бы CPU). Раньше
//! учёт был голым счётчиком глубины на весь CPU, и этого мало: счётчик живёт
//! ДОЛЬШЕ контекста, который его поднял.
//!
//! Обработчики syscall'ов идут с включёнными прерываниями (`linux_dispatch`), а
//! тик таймера вытесняет задачу. Тик, попавший между `enter` и `lock()` (там IF
//! ещё включён) или между снятием guard'а и `exit` (лок уже свободен, IF уже
//! восстановлен — это и наблюдалось в живом QEMU), оставляет глубину >0, пока
//! CPU уже исполняет ДРУГУЮ задачу в ring 3. Её законный ленивый page fault видел
//! «мы внутри `with_current_compat`», отказывался обслуживаться и убивал
//! процесс; а kill-путь, сбросивший счётчик, ломал баланс — возобновлённый
//! владелец делал `fetch_sub` из нуля, и счётчик навсегда становился
//! `usize::MAX` (после чего отказывались ВСЕ отказы подряд).
//!
//! Правильный вопрос — не «глубина > 0», а «это МОЯ глубина». Поэтому учёт
//! хранит pid владельца рядом с глубиной:
//!
//!   * [`CompatEntry::is_inside`] истинно только для задачи, чей kernel-контекст
//!     сейчас внутри замыкания. Другая задача, вытеснившая владельца, получает
//!     `false` и может безопасно взять лок: владелец не держит его, пока
//!     вытеснен (спинлок маскирует IF, поэтому удержание исключает переключение
//!     на этом CPU);
//!   * [`CompatEntry::abandon`] сбрасывает учёт только для настоящего владельца,
//!     поэтому kill-путь посторонней задачи не обнуляет чужую глубину и не
//!     приводит к underflow;
//!   * [`CompatEntry::enter`] чужой pid'ом начинает счёт заново, а не
//!     наследует протухшую глубину убитой задачи.
//!
//! Модуль `core`-only (только атомики), поэтому host-свойства исполняют ровно
//! тот исходник, который компилирует ядро, и проверяют межзадачные
//! чередования, которые в QEMU ловятся вероятностно.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// `pid` в этом слоте означает «владельца нет». Idle-задача (pid 0) никогда не
/// бывает Compat_Process, поэтому 0 безопасен как «пусто».
const NO_OWNER: u64 = 0;

/// Учёт входа в `COMPAT_STATES`: pid владельца + глубина его вложенных замыканий.
///
/// Один экземпляр на CPU (ядро однопроцессорное), поэтому атомики нужны только
/// против вытеснения прерыванием, а не против параллельных CPU.
pub struct CompatEntry {
    /// Глубина вложенных `with_current_compat` владельца [`CompatEntry::owner`].
    depth: AtomicUsize,
    /// pid задачи, чей kernel-контекст сейчас внутри замыкания (0 — ничей).
    owner: AtomicU64,
}

impl CompatEntry {
    /// Пустой учёт: никто не внутри.
    pub const fn new() -> Self {
        Self {
            depth: AtomicUsize::new(0),
            owner: AtomicU64::new(NO_OWNER),
        }
    }

    /// Отметить вход задачи `pid` в замыкание `with_current_compat`.
    ///
    /// Вложенный вход той же задачи увеличивает глубину; вход ДРУГОЙ задачи
    /// начинает счёт заново. Второй случай достижим только после того, как
    /// прежний владелец был вытеснен между `enter` и `lock()` и больше не
    /// вернулся (убит): его глубина протухла, и наследовать её нельзя — иначе
    /// новая задача получила бы чужой «re-entry» на ровном месте.
    pub fn enter(&self, pid: u64) {
        if self.owner.load(Ordering::Relaxed) == pid {
            self.depth.fetch_add(1, Ordering::Relaxed);
        } else {
            self.owner.store(pid, Ordering::Relaxed);
            self.depth.store(1, Ordering::Relaxed);
        }
    }

    /// Отметить выход задачи `pid` из замыкания.
    ///
    /// Выход не-владельца игнорируется: его вход уже был перекрыт [`Self::enter`]
    /// другой задачи (или учёт сброшен [`Self::abandon`]), и вычитать чужую
    /// глубину — это ровно тот underflow, который навсегда отравлял guard.
    /// Глубина насыщается на нуле, поэтому испорченный учёт не может превратиться
    /// в `usize::MAX`.
    pub fn exit(&self, pid: u64) {
        if self.owner.load(Ordering::Relaxed) != pid {
            return;
        }
        loop {
            let cur = self.depth.load(Ordering::Relaxed);
            let next = cur.saturating_sub(1);
            if self
                .depth
                .compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                if next == 0 {
                    self.owner.store(NO_OWNER, Ordering::Relaxed);
                }
                return;
            }
        }
    }

    /// Находится ли kernel-контекст задачи `pid` внутри замыкания прямо сейчас.
    ///
    /// Именно этот вопрос задаёт обработчик page fault; «глубина > 0» без
    /// владельца отвечал бы за весь CPU и отказывал посторонним задачам.
    pub fn is_inside(&self, pid: u64) -> bool {
        self.depth.load(Ordering::Relaxed) > 0 && self.owner.load(Ordering::Relaxed) == pid
    }

    /// Аварийно сбросить учёт задачи `pid`; `false`, если она не владелец.
    ///
    /// Нужен kill-пути исключения: прерванное замыкание уже никогда не вернётся,
    /// поэтому его глубина должна исчезнуть вместе с ним. Сброс чужого учёта
    /// запрещён — владелец может быть просто вытеснен и продолжить работу.
    pub fn abandon(&self, pid: u64) -> bool {
        if !self.is_inside(pid) {
            return false;
        }
        self.depth.store(0, Ordering::Relaxed);
        self.owner.store(NO_OWNER, Ordering::Relaxed);
        true
    }

    /// Текущая глубина (диагностика и host-свойства). Не использовать как ответ
    /// на вопрос «внутри ли я» — для этого есть [`Self::is_inside`].
    pub fn depth(&self) -> usize {
        self.depth.load(Ordering::Relaxed)
    }

    /// Текущий владелец (диагностика и host-свойства); 0 — ничей.
    pub fn owner(&self) -> u64 {
        self.owner.load(Ordering::Relaxed)
    }
}

impl Default for CompatEntry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Чередование, которое ловилось вероятностно в QEMU: владелец вытеснен
    /// между `enter` и `lock()`, а посторонняя задача задаёт вопрос guard'а.
    #[test]
    fn other_task_is_not_reentry() {
        let e = CompatEntry::new();
        e.enter(7);
        assert!(e.is_inside(7));
        assert!(!e.is_inside(8), "посторонняя задача — не повторный вход");
        e.exit(7);
        assert!(!e.is_inside(7));
        assert_eq!(e.depth(), 0);
    }

    /// Kill-путь посторонней задачи не сбрасывает чужую глубину, поэтому
    /// возобновлённый владелец не вычитает из нуля.
    #[test]
    fn foreign_abandon_keeps_owner_balance() {
        let e = CompatEntry::new();
        e.enter(7);
        assert!(!e.abandon(8), "чужой учёт не сбрасывается");
        assert_eq!(e.depth(), 1);
        e.exit(7);
        assert_eq!(e.depth(), 0, "баланс владельца сохранён");
        assert_eq!(e.owner(), NO_OWNER);
    }

    /// Сброс владельцем — ровно то, что делает kill-путь исключения.
    #[test]
    fn owner_abandon_clears_entry() {
        let e = CompatEntry::new();
        e.enter(7);
        e.enter(7);
        assert_eq!(e.depth(), 2);
        assert!(e.abandon(7));
        assert_eq!(e.depth(), 0);
        assert!(!e.is_inside(7));
    }

    /// Протухшая глубина убитой задачи не переходит на новую задачу.
    #[test]
    fn stale_depth_does_not_leak_to_new_owner() {
        let e = CompatEntry::new();
        e.enter(7);
        // 7 убит, не выйдя из замыкания: глубина 1 осталась.
        e.enter(8);
        assert!(e.is_inside(8));
        assert_eq!(e.depth(), 1, "новая задача начинает счёт заново");
        e.exit(8);
        assert_eq!(e.depth(), 0);
        assert!(!e.is_inside(8));
    }

    /// Перекрытый выход не уводит глубину в underflow.
    #[test]
    fn overlapping_exit_saturates() {
        let e = CompatEntry::new();
        e.enter(7);
        e.enter(8); // перекрыл учёт 7 (7 вытеснен и убит)
        e.exit(7); // выход не-владельца игнорируется
        assert_eq!(e.depth(), 1);
        e.exit(8);
        e.exit(8); // лишний выход не должен обнулить/перевернуть счётчик
        assert_eq!(e.depth(), 0);
    }
}
