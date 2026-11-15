//! Чистый, host-тестируемый учёт файловых дескрипторов (R2.4, R2.6, R2.14).
//!
//! Правило «наименьший свободный дескриптор >= 3» и семантика EBADF на
//! отсутствующий fd — чистая арифметика индексов по вектору слотов. Она вынесена
//! сюда — только `core` + `alloc`, БЕЗ зависимости от `VfsNode`, `Errno`, VMM или
//! глобального изменяемого состояния, — чтобы ядерный [`super::fd::FdTable`]
//! (чей `OpenObject` встраивает `Arc<dyn VfsNode>`) не попадал в host-сборку, а
//! Property 7 всё равно проверялось на хосте с фиктивным типом `T` (R11.6).
//!
//! Модуль — host-тестируемый шов, подключённый в `host-tests/src/lib.rs`.
#![allow(dead_code)]

use alloc::vec::Vec;

/// Маркерная ошибка для fd, не указывающего ни на какой открытый слот. Ядерная
/// обёртка отображает её в `Errno::EBADF` (R2.6, R2.14); отсутствие зависимостей
/// здесь и позволяет модулю компилироваться в host-tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadFd;

/// Ошибка [`FdSlots::try_alloc`]: размещение объекта в наименьшем свободном
/// индексе `>= min` вырастило бы таблицу дескрипторов сверх предела. Слой
/// syscall отображает её в `EMFILE`; ничего не аллоцируется.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableFull;

/// Жёсткий предел числа слотов в таблице дескрипторов процесса (дефолтный
/// `RLIMIT_NOFILE` Linux). Слой syscall обеспечивает тот же предел для любого
/// переданного вызывающим номера (`dup2`/`dup3`/`fcntl(F_DUPFD)`), а
/// [`FdSlots::try_alloc`] — для самой таблицы, чтобы будущий вызывающий не
/// вырастил `Vec` до выбранного атакующим индекса и не уронил ядро на
/// необслуживаемой аллокации.
pub const FD_TABLE_LIMIT: usize = 1024;

/// Вернуть наименьший индекс `>= min`, чей слот свободен.
///
/// Слот свободен, когда он `Some(None)` (явно пустой) или лежит за концом
/// `slots` (`None` из `slice::get`). Занятые слоты (`Some(Some(_))`)
/// пропускаются. Возвращённый индекс может быть равен или больше `slots.len()`,
/// то есть вызывающему нужно вырастить вектор. Чисто и полно (R2.4).
pub fn lowest_free_index<T>(slots: &[Option<T>], min: usize) -> usize {
    let mut i = min;
    loop {
        match slots.get(i) {
            // Занят: продолжаем сканирование.
            Some(Some(_)) => i += 1,
            // Явно пуст или за концом: индекс свободен.
            Some(None) | None => return i,
        }
    }
}

/// Обобщённая таблица дескрипторов: вектор опциональных слотов с выделением по
/// наименьшему свободному индексу и сообщением об отсутствии в стиле EBADF.
///
/// Не несёт ядерных или VFS-зависимостей, поэтому host-тестируется для
/// Property 7 с любым фиктивным типом `T`. Ядро собирает её как
/// `FdSlots<OpenObject>` внутри [`super::fd::FdTable`].
#[derive(Clone, Debug, Default)]
pub struct FdSlots<T> {
    slots: Vec<Option<T>>,
}

impl<T> FdSlots<T> {
    /// Пустая таблица без слотов.
    pub fn new() -> Self {
        Self { slots: Vec::new() }
    }

    /// Таблица, заранее заполненная слотами (например, стандартные потоки в 0/1/2).
    pub fn from_slots(slots: Vec<Option<T>>) -> Self {
        Self { slots }
    }

    /// Выделить наименьший свободный индекс `>= min` в пределах
    /// [`FD_TABLE_LIMIT`], при необходимости вырастив вектор пустыми слотами,
    /// положить туда `obj` и вернуть индекс (R2.4).
    ///
    /// F-r4-life-3: раньше это была непогрешимая версия, которой пользовались
    /// `open`/`pipe`/`socket`/`accept`/`dup2`, поэтому `pipe()` после 1024 fd
    /// выдавал индекс 1024+ и таблица росла без предела. Теперь и этот вход
    /// ограничен тем же [`FD_TABLE_LIMIT`]; вызывающий отображает
    /// [`TableFull`] в `EMFILE`.
    pub fn alloc(&mut self, min: usize, obj: T) -> Result<u32, TableFull> {
        self.try_alloc(min, FD_TABLE_LIMIT, obj)
    }

    /// [`FdSlots::alloc`] с жёстким пределом индекса: если наименьший свободный
    /// индекс `>= min` окажется `>= limit`, вернуть [`TableFull`] **до** роста
    /// вектора и любой аллокации.
    ///
    /// Это вызов, который обязан использовать `fcntl(F_DUPFD)`: `min` — `arg`
    /// вызывающего, `limit` — [`FD_TABLE_LIMIT`]. Проверка идёт по индексу слота,
    /// а не только по `min`, поэтому полная таблица всё равно сообщает `EMFILE`,
    /// а не растёт безгранично.
    pub fn try_alloc(&mut self, min: usize, limit: usize, obj: T) -> Result<u32, TableFull> {
        let idx = lowest_free_index(&self.slots, min);
        if idx >= limit {
            return Err(TableFull);
        }
        Ok(self.store(idx, obj))
    }

    /// Положить `obj` по `idx`, при необходимости вырастив пустыми слотами. `idx`
    /// должен быть получен из [`lowest_free_index`].
    fn store(&mut self, idx: usize, obj: T) -> u32 {
        if idx < self.slots.len() {
            self.slots[idx] = Some(obj);
        } else {
            // Вырасти пустыми слотами до `idx`, затем положить `obj` по `idx`.
            while self.slots.len() < idx {
                self.slots.push(None);
            }
            self.slots.push(Some(obj));
        }
        idx as u32
    }

    /// Положить `obj` по явному индексу `idx`, вырастив вектор пустыми слотами
    /// при необходимости и заменив (освободив) уже лежащий там объект. Вернуть
    /// ранее занимавший слот объект, если он был. Используется `dup2`/`dup3`,
    /// которые называют явный целевой дескриптор.
    ///
    /// F-r4-life-3: явный индекс тоже ограничен [`FD_TABLE_LIMIT`] — иначе
    /// `dup2(old, 0x7fff_ffff)` вырастил бы `Vec` до выбранного вызывающим
    /// индекса. `dup2`/`dup3` отображают отказ в `EBADF` (Linux: `newfd` вне
    /// `RLIMIT_NOFILE`).
    pub fn try_set(&mut self, idx: u32, limit: usize, obj: T) -> Result<Option<T>, TableFull> {
        let idx = idx as usize;
        if idx >= limit {
            return Err(TableFull);
        }
        while self.slots.len() <= idx {
            self.slots.push(None);
        }
        Ok(self.slots[idx].replace(obj))
    }

    /// Позаимствовать объект по `fd` или `None` для слота вне диапазона/пустого
    /// (вызывающий отображает `None` -> EBADF, R2.14).
    pub fn get(&self, fd: u32) -> Option<&T> {
        self.slots.get(fd as usize).and_then(|slot| slot.as_ref())
    }

    /// Изменяемо позаимствовать объект по `fd` или `None` для слота вне
    /// диапазона/пустого (вызывающий отображает `None` -> EBADF, R2.14).
    pub fn get_mut(&mut self, fd: u32) -> Option<&mut T> {
        self.slots
            .get_mut(fd as usize)
            .and_then(|slot| slot.as_mut())
    }

    /// Освободить слот `fd`. Возвращает [`BadFd`], когда слот вне диапазона или
    /// уже пуст, оставляя таблицу неизменной; иначе очищает слот и возвращает
    /// `Ok` (R2.6, R2.14).
    pub fn close(&mut self, fd: u32) -> Result<(), BadFd> {
        match self.slots.get_mut(fd as usize) {
            Some(slot) if slot.is_some() => {
                *slot = None;
                Ok(())
            }
            _ => Err(BadFd),
        }
    }

    /// Число отслеживаемых слотов (включая пустые).
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Отслеживает ли таблица вообще хоть один слот.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}
