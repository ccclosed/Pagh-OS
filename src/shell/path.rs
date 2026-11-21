//! Состояние CWD и чистая нормализация/разрешение путей (свёртка `.`/`..`,
//! кламп в корне).
//!
//! `normalize` и `resolve` — чистые функции над `&str`: без консольного и VFS
//! I/O, поэтому детерминированы и легко тестируемы. Глобальный CWD shell живёт
//! под [`Spinlock`] и всегда хранится нормализованным абсолютным путём.

use alloc::string::String;
use alloc::vec::Vec;

use crate::sync::spinlock::Spinlock;

/// CWD каждой консольной сессии: `[0]` — boot/shell, `[i >= 1]` — окно.
///
/// Раньше CWD был один на ядро. С появлением второй сессии это стало бы общим
/// состоянием двух окон: `cd` в одном менял бы промпт и относительные пути
/// другого. Массив индексируется номером сессии текущей задачи
/// ([`crate::drivers::session::current`]), поэтому команды (`cd`, `pwd`, `ls`,
/// Tab-дополнение) остаются без изменений — они зовут те же `cwd`/`set_cwd`.
///
/// Хранится нормализованным абсолютным путём. Инициализируется лениво: пустая
/// строка (const-конструируемый дефолт) трактуется как корень `/`, так что
/// наблюдаемый начальный CWD — `/` (R4.1) без runtime-инициализации.
static CWDS: [Spinlock<String>; crate::drivers::session_route::MAX_SESSIONS] =
    [const { Spinlock::new(String::new()) }; crate::drivers::session_route::MAX_SESSIONS];

/// CWD конкретной сессии (`0` — boot).
fn slot(session: usize) -> &'static Spinlock<String> {
    &CWDS[if session < CWDS.len() { session } else { 0 }]
}

/// Свернуть абсолютный путь в каноническую форму.
///
/// Применяет компоненты `.` (текущий) и `..` (родитель), схлопывает серии `/` и
/// клампит `..` в корне (лишние `..` не могут выйти за `/`). Результат всегда:
/// - начинается с `/`,
/// - не содержит `.`, `..` или пустых (`//`) компонент,
/// - не имеет завершающего `/`, кроме корня, который ровно `"/"`.
///
/// Пустой вход трактуется как `/`. Функция идемпотентна:
/// `normalize(normalize(p)) == normalize(p)`.
#[allow(dead_code)]
pub fn normalize(abs: &str) -> String {
    // Стек выживших компонент (без ведущего '/', без '.'/'..', непустых).
    let mut stack: Vec<&str> = Vec::new();

    for comp in abs.split('/') {
        match comp {
            // Пустые компоненты возникают из ведущего '/', завершающего '/' или
            // серий '//'; "." — текущий каталог. Оба отбрасываются.
            "" | "." => {}
            // Родитель: снять одну компоненту или остаться в корне, когда стек
            // уже пуст (кламп — `..` никогда не выходит за `/`).
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }

    if stack.is_empty() {
        return String::from("/");
    }

    let mut out = String::new();
    for comp in stack {
        out.push('/');
        out.push_str(comp);
    }
    out
}

/// Разрешить возможно относительный пользовательский путь относительно `base`,
/// затем нормализовать.
///
/// Если `input` начинается с `/`, он абсолютный и `base` игнорируется. Иначе
/// результат — `base`, соединённый с `input` (через `/`). `base` предполагается
/// уже абсолютным путём (по умолчанию `/`). Возвращаемое значение удовлетворяет
/// тому же каноническому контракту, что и [`normalize`].
#[allow(dead_code)]
pub fn resolve(base: &str, input: &str) -> String {
    if input.starts_with('/') {
        normalize(input)
    } else {
        // Соединить base + "/" + input; normalize свернёт любые дубли
        // разделителей (например, когда base — "/" или уже кончается на '/').
        let mut joined = String::from(base);
        joined.push('/');
        joined.push_str(input);
        normalize(&joined)
    }
}

/// Снимок глобального CWD shell.
///
/// Возвращает owned-клон, чтобы вызывающие не держали лок во время
/// использования. Значение всегда нормализованный абсолютный путь;
/// неинициализированное хранилище читается как `/`.
#[allow(dead_code)]
pub fn cwd() -> String {
    cwd_for(crate::drivers::session::current())
}

/// CWD указанной сессии (`0` — boot); см. [`cwd`].
#[allow(dead_code)]
pub fn cwd_for(session: usize) -> String {
    let guard = slot(session).lock();
    if guard.is_empty() {
        String::from("/")
    } else {
        guard.clone()
    }
}

/// Сохранить новый текущий рабочий каталог, сначала нормализовав его, чтобы
/// глобальный инвариант (канонический абсолютный путь) сохранялся.
#[allow(dead_code)]
pub fn set_cwd(abs: &str) {
    set_cwd_for(crate::drivers::session::current(), abs);
}

/// Сохранить CWD указанной сессии (`0` — boot).
#[allow(dead_code)]
pub fn set_cwd_for(session: usize, abs: &str) {
    let normalized = normalize(abs);
    let mut guard = slot(session).lock();
    *guard = normalized;
}
