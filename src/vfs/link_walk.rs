//! Чистый host-тестируемый резолвер путей с симлинками.
//!
//! Ядро резолвит путь покомпонентно: `..` поднимает стек каталогов, конечная
//! ссылка следуется только если syscall этого просит, промежуточные — всегда.
//! Пересечённая ссылка вставляет цель **перед** оставшимися компонентами: для
//! абсолютной цели обход начинается от корня VFS, для относительной сохраняет
//! текущий каталог. Больше [`SYMLOOP_MAX`] переходов за один резолв — `ELOOP`
//! (Linux `SYMLOOP_MAX`), а разросшийся путь ограничен, чтобы управляемый
//! атакующим образ не растил обход без предела.
//!
//! Walker ничего не знает о credentials: перед каждым lookup он спрашивает
//! [`LinkTree::may_search`], можно ли искать в каталоге. Адаптер ядра отвечает
//! эффективными credentials процесса — так POSIX-право поиска (`x`) проверяется
//! на **каждом** компоненте, включая компоненты подставленной цели ссылки, а не
//! только на компонентах из аргумента syscall.
//!
//! Модуль владеет всем алгоритмом и использует только `core` + `alloc`: ядро
//! подаёт адаптер [`LinkTree`] над `Arc<dyn VfsNode>`, а `host-tests` гоняют тот
//! же исходник на in-memory дереве с независимым оракулом (семантика, а не только
//! компиляция, проверяется на хосте — R11.6).
//!
//! [`guest_path_keeps_root`] зеркалит гостевую политику рядом с syscall-слоем
//! (`arch::x86_64::linux::io`); две копии сверяет host-свойство
//! `link_walk_agrees_with_io_guest_root`.
#![allow(dead_code)]

use alloc::string::String;
use alloc::vec::Vec;

/// Максимум симлинков, пересекаемых при резолве одного пути.
///
/// Linux `SYMLOOP_MAX`; 41-й переход — `ELOOP`.
pub const SYMLOOP_MAX: u32 = 40;

/// Предел длины разросшегося пути за один резолв (дальше `ENAMETOOLONG`).
///
/// Каждая цель ссылки приходит с диска и может управляться атакующим, поэтому
/// список ожидания не должен расти без предела: иначе 40 целей по `PATH_MAX`
/// материализовались бы раньше, чем бюджет остановит обход.
pub const MAX_EXPANDED_BYTES: usize = 4096;

/// Компоненты гостевых путей, резолвящиеся от корня VFS, а не перенаправляемые
/// под `/mnt`, — собственные деревья ядра (`/dev`, `/tmp`, синтетический
/// `/proc`) плюс сам явный вид `/mnt`.
pub const ROOT_KEEPING_COMPONENTS: [&str; 5] = ["mnt", "dev", "proc", "sys", "tmp"];

/// Резолвится ли абсолютный гостевой путь от корня VFS (а не под `/mnt`)?
/// Зеркалит `arch::x86_64::linux::io::guest_path_keeps_root`.
pub fn guest_path_keeps_root(abs: &str) -> bool {
    let first = abs.trim_start_matches('/').split('/').next().unwrap_or("");
    ROOT_KEEPING_COMPONENTS.contains(&first)
}

/// Почему путь не разрешился. Syscall-слой отображает это в `ENOENT`,
/// `ENOTDIR`, `ELOOP` и `ENAMETOOLONG`.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum WalkError {
    /// Какой-то компонент не существует (включая висячую ссылку, по которой
    /// нужно было пройти).
    NotFound,
    /// Промежуточный компонент — не каталог.
    NotDir,
    /// Пересечено больше [`SYMLOOP_MAX`] ссылок.
    TooManyLinks,
    /// Разросшийся путь превысил [`MAX_EXPANDED_BYTES`].
    TooLong,
    /// Вызывающему нельзя искать в одном из каталогов пути (POSIX требует бит
    /// `x` на каждом промежуточном каталоге).
    AccessDenied,
}

/// Цель симлинка в том виде, в каком её должен интерпретировать walker.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct LinkTarget {
    /// Цель в пространстве walker. Абсолютная цель уже отображена в
    /// пространство ядра (см. [`map_guest_target`]); относительная резолвится от
    /// каталога, где лежит ссылка.
    pub path: String,
    /// True для абсолютной цели: обход начинается с корня дерева.
    pub absolute: bool,
}

/// Отображает цель ссылки (*гостевой* путь, хранимый на диске дословно) в
/// пространство walker.
///
/// Абсолютные цели отображаются так же, как пути syscall: `/x` становится
/// `/mnt/x`, если `x` не дерево ядра; относительные остаются нетронутыми (они
/// резолвятся от каталога ссылки). Голый `/` — корень гостя, то есть корень
/// ext2 (`/mnt`), а не синтетический корень VFS; так же его отображает
/// `resolve_path` в слое сисколлов, поэтому ссылка на `/` и `statvfs("/")`
/// указывают на один и тот же каталог. `..` здесь намеренно **не**
/// схлопывается: стек каталогов walker обязан его увидеть, поэтому `/a/link/..`
/// при `link → /b/c` ведёт себя как в Linux, а не как лексический префикс из
/// `format!`.
pub fn map_guest_target(target: &str) -> LinkTarget {
    if !target.starts_with('/') {
        return LinkTarget {
            path: String::from(target),
            absolute: false,
        };
    }
    if target == "/" {
        return LinkTarget {
            path: String::from("/mnt"),
            absolute: true,
        };
    }
    let path = if guest_path_keeps_root(target) {
        String::from(target)
    } else {
        let mut s = String::with_capacity(target.len() + 4);
        s.push_str("/mnt");
        s.push_str(target);
        s
    };
    LinkTarget {
        path,
        absolute: true,
    }
}

/// Режет путь на компоненты, выбрасывая пустые и `.` и сохраняя `..`
/// (что значит `..` для стека каталогов, решает вызывающий).
pub fn components(path: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            other => out.push(other),
        }
    }
    out
}

/// ФС глазами walker: непрозрачные хендлы узлов плюс три вопроса, нужные
/// резолву.
pub trait LinkTree {
    /// Узел, с которого начинается резолв (корневой каталог дерева).
    fn root(&self) -> usize;
    /// Ищет `name` в каталоге `id`; `None`, если его нет.
    fn lookup(&mut self, id: usize, name: &str) -> Option<usize>;
    /// Является ли `id` каталогом?
    fn is_dir(&self, id: usize) -> bool;
    /// [`LinkTarget`], когда `id` — симлинк, иначе `None`. Ссылка с нечитаемой
    /// целью отдаёт пустой `path`, и обход проваливается с
    /// [`WalkError::NotFound`], а не трактует её как обычный файл.
    fn link_target(&self, id: usize) -> Option<LinkTarget>;

    /// Можно ли вызывающему искать (находить имя) в каталоге `id`?
    ///
    /// Walker спрашивает это перед **каждым** lookup. По умолчанию `true`:
    /// чистый алгоритм не моделирует права, и host-свойства гоняют его на
    /// вседозволенном дереве; адаптер ядра переопределяет метод эффективными
    /// credentials процесса, поэтому обход требует бита `x` на каждом входимом
    /// каталоге. `false` проваливает обход с [`WalkError::AccessDenied`].
    fn may_search(&self, _id: usize) -> bool {
        true
    }
}

/// Резолвит `path` в `tree` и возвращает id названного узла.
///
/// `path` уже в пространстве walker (ядро передаёт guest-mapped абсолютный
/// путь). `follow_final` выбирает семантику `stat`/`open` (true) или
/// `lstat`/`readlink`/`unlink` (false) для **последнего** компонента;
/// промежуточные ссылки следуются всегда.
pub fn resolve<T: LinkTree>(
    tree: &mut T,
    path: &str,
    follow_final: bool,
) -> Result<usize, WalkError> {
    let mut pending: Vec<String> = components(path).iter().map(|c| String::from(*c)).collect();
    pending.reverse(); // LIFO: следующий компонент — в конце.
    let mut expanded = path.len();
    let mut links: u32 = 0;
    let mut stack: Vec<usize> = alloc::vec![tree.root()];

    loop {
        let Some(comp) = pending.pop() else {
            // Путь израсходован: результат — текущий каталог ("/"
            // резолвится в корень).
            return Ok(*stack.last().expect("the stack always holds the root"));
        };
        if comp == ".." {
            // Никогда выше корня: "/.." — это "/".
            if stack.len() > 1 {
                stack.pop();
            }
            continue;
        }
        let dir = *stack.last().expect("the stack always holds the root");
        // Право поиска проверяется на каталоге, в котором ищется компонент, то
        // есть на каждом пройденном компоненте — включая подставленные
        // переходом по ссылке.
        if !tree.may_search(dir) {
            return Err(WalkError::AccessDenied);
        }
        let child = tree.lookup(dir, &comp).ok_or(WalkError::NotFound)?;
        let is_last = pending.is_empty();

        if let Some(target) = tree.link_target(child) {
            if follow_final || !is_last {
                links += 1;
                if links > SYMLOOP_MAX {
                    return Err(WalkError::TooManyLinks);
                }
                if target.path.is_empty() {
                    // Ссылка с нечитаемой целью: `ENOENT`, а не молчаливый
                    // откат к узлу ссылки.
                    return Err(WalkError::NotFound);
                }
                expanded += target.path.len();
                if expanded > MAX_EXPANDED_BYTES {
                    return Err(WalkError::TooLong);
                }
                if target.absolute {
                    // Абсолютная цель начинает обход с корня: стек каталогов
                    // забывает, где была ссылка. Компоненты *pending* после
                    // ссылки сохраняются — `/a/link/..` обязан применить `..` к
                    // каталогу цели.
                    stack.truncate(1);
                }
                // LIFO: компоненты цели расходуются раньше того, что шло после
                // ссылки.
                for c in components(&target.path).iter().rev() {
                    pending.push(String::from(*c));
                }
                continue;
            }
        }

        if !is_last && !tree.is_dir(child) {
            return Err(WalkError::NotDir);
        }
        stack.push(child);
    }
}
