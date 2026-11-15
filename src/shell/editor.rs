//! `LineEditor`: модель буфер + курсор с чистыми операциями
//! вставки/удаления/перемещения.
//!
//! Курсор хранится в **символьных** единицах (не байтах), чтобы редактор
//! оставался корректным вокруг многобайтового UTF-8 (R11.6). Каждая операция
//! сохраняет два инварианта:
//!   * `cursor <= buf.chars().count()`
//!   * `buf.len() <= MAX_CMD_LEN` (длина в байтах ограничена, R11.1)
//! и никогда не разрезает многобайтовый символ UTF-8 — любая индексация
//! отображает символьный индекс в байтовый через `char_indices`.

use alloc::string::String;

/// Максимальная длина командной строки в байтах. Совпадает с константой
/// shell в `shell/mod.rs`, чтобы редактор и старый цикл чтения сходились на капе.
const MAX_CMD_LEN: usize = 256;

/// Редактируемая строка текста с курсором между символами.
#[allow(dead_code)]
pub struct LineEditor {
    buf: String,
    /// Позиция курсора в символьных единицах, в `0..=buf.chars().count()`.
    cursor: usize,
}

#[allow(dead_code)]
impl LineEditor {
    /// Создать пустой редактор с курсором в начале.
    pub fn new() -> Self {
        LineEditor {
            buf: String::new(),
            cursor: 0,
        }
    }

    /// Создать редактор с `line`, курсор в конце.
    pub fn from_line(line: &str) -> Self {
        let mut buf = String::new();
        let cursor = Self::push_bounded(&mut buf, line);
        LineEditor { buf, cursor }
    }

    /// Отобразить символьный индекс в соответствующий байтовый в `buf`.
    /// `char_idx == число символов` отображается в `buf.len()` (на один за концом).
    fn byte_index(&self, char_idx: usize) -> usize {
        match self.buf.char_indices().nth(char_idx) {
            Some((byte_idx, _)) => byte_idx,
            None => self.buf.len(),
        }
    }

    /// Дописать столько `line`, сколько влезает в `MAX_CMD_LEN` байт в `buf`,
    /// никогда не разрезая символ, и вернуть число добавленных символов.
    /// Используется конструкторами/`set_line`, чтобы заданные строки уважали байтовый кап.
    fn push_bounded(buf: &mut String, line: &str) -> usize {
        let mut count = 0;
        for ch in line.chars() {
            if buf.len() + ch.len_utf8() > MAX_CMD_LEN {
                break;
            }
            buf.push(ch);
            count += 1;
        }
        count
    }

    /// Вставить `ch` в курсор, сдвинув суффикс вправо, и продвинуть курсор на
    /// один (R1.3). Вставка отбрасывается, если она вывела бы буфер за
    /// `MAX_CMD_LEN` байт (R11.1).
    pub fn insert(&mut self, ch: char) {
        if self.buf.len() + ch.len_utf8() > MAX_CMD_LEN {
            return;
        }
        let at = self.byte_index(self.cursor);
        self.buf.insert(at, ch);
        self.cursor += 1;
    }

    /// Backspace: удалить символ перед курсором и сдвинуться влево
    /// (R1.4). Возвращает `true`, если буфер изменился.
    pub fn delete_back(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let at = self.byte_index(self.cursor - 1);
        self.buf.remove(at);
        self.cursor -= 1;
        true
    }

    /// Delete: удалить символ в курсоре, оставив курсор на месте
    /// (R1.5). Возвращает `true`, если буфер изменился.
    pub fn delete_fwd(&mut self) -> bool {
        if self.cursor >= self.char_count() {
            return false;
        }
        let at = self.byte_index(self.cursor);
        self.buf.remove(at);
        true
    }

    /// Заменить диапазон символов `[from, to)` ничем, поставив курсор в `from`.
    ///
    /// Индексы в символьных единицах, как и везде в типе. Вне диапазона или
    /// инвертированный диапазон клампится, а не отвергается, так что выделение,
    /// выходящее за любой конец строки, всё равно удаляет существующую часть.
    /// Возвращает, было ли что удалено.
    pub fn replace_range(&mut self, from: usize, to: usize) -> bool {
        let count = self.char_count();
        let from = from.min(count);
        let to = to.min(count);
        if from >= to {
            self.cursor = from;
            return false;
        }
        let start_byte = self.byte_index(from);
        let end_byte = self.byte_index(to);
        self.buf.replace_range(start_byte..end_byte, "");
        self.cursor = from;
        true
    }

    /// Сдвинуть курсор на символ влево, клампя в начале (R1.1).
    /// Возвращает `true`, если курсор сдвинулся.
    pub fn move_left(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        true
    }

    /// Сдвинуть курсор на символ вправо, клампя в конце (R1.1).
    /// Возвращает `true`, если курсор сдвинулся.
    pub fn move_right(&mut self) -> bool {
        if self.cursor >= self.char_count() {
            return false;
        }
        self.cursor += 1;
        true
    }

    /// Переместить курсор в начало строки (R1.2).
    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Переместить курсор в конец строки (R1.2).
    pub fn move_end(&mut self) {
        self.cursor = self.char_count();
    }

    /// Заменить буфер на `line` и поставить курсор в конец. Используется для
    /// recall истории и дополнения. Новая строка ограничена `MAX_CMD_LEN` байт и
    /// никогда не разрезает символ.
    pub fn set_line(&mut self, line: &str) {
        self.buf.clear();
        self.cursor = Self::push_bounded(&mut self.buf, line);
    }

    /// Текущее содержимое буфера.
    pub fn buffer(&self) -> &str {
        &self.buf
    }

    /// Позиция курсора в символьных единицах.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Пуст ли буфер.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Число символов в буфере (верхняя граница курсора).
    fn char_count(&self) -> usize {
        self.buf.chars().count()
    }
}
