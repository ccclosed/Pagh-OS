//! `History`: ограниченный ring-буфер прошлых команд с recall вверх/вниз,
//! сохранением/восстановлением текущей строки и дедупом подряд идущих.
//!
//! Чистая логика — без консольного и VFS I/O — поэтому проверяется
//! property-тестами изолированно (свойство 23). Интерактивный цикл ведёт её:
//! Up вызывает [`History::recall_prev`], Down — [`History::recall_next`], Enter —
//! [`History::push`] (для непустых строк) и затем [`History::reset_nav`].

use alloc::collections::VecDeque;
use alloc::string::String;

/// Максимальное число хранимых записей истории. Вместе с `MAX_CMD_LEN` (256)
/// редактора строки это ограничивает память истории `CAP * MAX_CMD_LEN`.
/// (R11.1).
const CAP: usize = 64;

/// Ограниченный ring-буфер прошлых командных строк с recall вверх/вниз и
/// сохранением/восстановлением текущей строки.
///
/// Раскладка хранения: `entries` держит строки, **старейшая в начале**,
/// **новейшая в конце** (`push_back`/`pop_front`).
///
/// Соглашение индекса навигации: `nav` = `None`, пока пользователь редактирует
/// живую (текущую) строку. При навигации `nav` = `Some(i)`, где `i` считается
/// **от новейшей записи**: `i == 0` — новейшая, `i == entries.len() - 1` —
/// старейшая. Отображение в индекс `VecDeque` — `entries.len() - 1 - i`.
#[allow(dead_code)]
pub struct History {
    /// Хранимые строки, старейшая в начале, новейшая в конце. Длина `<= CAP`.
    entries: VecDeque<String>,
    /// `None` = редактируется живая строка; `Some(i)` = навигация, `i` считается
    /// от новейшей записи (0 = новейшая).
    nav: Option<usize>,
    /// Текущая строка, отложенная при первом `recall_prev`, чтобы проход
    /// `recall_next` за новейшую запись мог восстановить ровно то, что
    /// пользователь набирал.
    saved: String,
}

#[allow(dead_code)]
impl History {
    /// Создать пустую историю без активной навигации.
    pub fn new() -> Self {
        History {
            entries: VecDeque::new(),
            nav: None,
            saved: String::new(),
        }
    }

    /// Записать командную строку (R2.1, R2.4, R2.5).
    ///
    /// - Пустые строки игнорируются.
    /// - Строка, равная новейшей записи, пропускается (дедуп подряд идущих).
    /// - Иначе строка дописывается как новейшая; если буфер превысил бы `CAP`,
    ///   старейшая запись (в начале) отбрасывается.
    ///
    /// Запись всегда завершает любую активную навигацию: `nav` сбрасывается в
    /// `None`, отложенная строка очищается.
    pub fn push(&mut self, line: &str) {
        if line.is_empty() {
            // Всё равно оставить навигацию в чистом состоянии на Enter.
            self.nav = None;
            self.saved.clear();
            return;
        }

        // Дедуп подряд идущих: пропустить, если совпадает с новейшей записью.
        if self.entries.back().map(|s| s.as_str()) != Some(line) {
            self.entries.push_back(String::from(line));
            if self.entries.len() > CAP {
                self.entries.pop_front();
            }
        }

        self.nav = None;
        self.saved.clear();
    }

    /// Перейти к СТАРЫМ записям (клавиша Up, R2.2).
    ///
    /// При первом вызове после редактирования (`nav == None`) текущая строка
    /// вызывающего откладывается в `saved`, чтобы её можно было восстановить
    /// позже, и возвращается новейшая запись. Каждый следующий вызов шагает на
    /// одну запись старее. Возвращает `None` (оставляя `nav` без изменений),
    /// когда уже на старейшей записи или записей нет.
    pub fn recall_prev(&mut self, current: &str) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }

        let next_idx = match self.nav {
            None => {
                // Первый recall: отложить живую строку и выбрать новейшую.
                self.saved.clear();
                self.saved.push_str(current);
                0
            }
            Some(i) => {
                // Уже на старейшей записи: старее нечего показать.
                if i + 1 >= self.entries.len() {
                    return None;
                }
                i + 1
            }
        };

        self.nav = Some(next_idx);
        // Отобразить newest-relative индекс в индекс VecDeque (новейшая в конце).
        let vec_idx = self.entries.len() - 1 - next_idx;
        self.entries.get(vec_idx).map(|s| s.as_str())
    }

    /// Перейти к НОВЫМ записям (клавиша Down, R2.3).
    ///
    /// Возвращает следующую более новую запись, пока она есть. При шаге за
    /// новейшую запись навигация кончается (`nav` = `None`) и возвращается
    /// `None`, сигналя вызывающему восстановить отложенную строку через
    /// [`History::saved_line`]. Возвращает `None` сразу, если навигация не активна.
    pub fn recall_next(&mut self) -> Option<&str> {
        match self.nav {
            None => None,
            Some(0) => {
                // За новейшей записью: назад к живой строке.
                self.nav = None;
                None
            }
            Some(i) => {
                let next_idx = i - 1;
                self.nav = Some(next_idx);
                let vec_idx = self.entries.len() - 1 - next_idx;
                self.entries.get(vec_idx).map(|s| s.as_str())
            }
        }
    }

    /// Сбросить состояние навигации (вызывается на Enter). Очищает активную
    /// навигацию и отложенную строку.
    pub fn reset_nav(&mut self) {
        self.nav = None;
        self.saved.clear();
    }

    /// Текущая строка, отложенная при первом `recall_prev`. Нужна вызывающему,
    /// чтобы восстановить живую строку, когда `recall_next` шагает за новейшую
    /// запись.
    pub fn saved_line(&self) -> &str {
        &self.saved
    }
}
