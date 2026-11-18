//! Обработчики команд (`help`, `clear`, `echo`, ...), построенные на реестре.
//!
//! У каждого обработчика сигнатура `fn(ctx: &mut ShellCtx, args: &[&str])`,
//! где `args` — токены после имени команды (для строки `write /mnt/f hi`
//! обработчик `write` получает `["/mnt/f", "hi"]`). Обработчики регистрируются
//! в [`super::registry::COMMANDS`] и вызываются диспетчером из `mod.rs` через
//! [`super::registry::lookup`].
//!
//! Пути по-прежнему абсолютные; разрешение относительно CWD идёт отдельно.
//! Двойные хелперы вывода ([`shell_println`] / [`shell_print_bytes`]) и
//! path/сетевые хелперы живут здесь, чтобы обработчики их делили.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::identity;
use super::registry::ShellCtx;
use crate::arch::x86_64::linux::cred::{Creds, GROUPS_CAP};

/// Напечатать строку в serial и framebuffer, соблюдая двойное соглашение shell
/// о выводе.
pub(super) fn shell_println(text: &str) {
    crate::kprintln!("{}", text);
    crate::fb_println!("{}", text);
}

/// Напечатать сырые байты как текст в serial и framebuffer. Валидный UTF-8
/// печатается как есть; невалидные байты заменяются, чтобы процедура оставалась
/// panic-free.
pub(super) fn shell_print_bytes(bytes: &[u8]) {
    let mut start = 0;
    while start < bytes.len() {
        match core::str::from_utf8(&bytes[start..]) {
            Ok(s) => {
                crate::kprint!("{}", s);
                crate::fb_print!("{}", s);
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                if valid > 0 {
                    // SAFETY: bytes[start..start+valid] is validated UTF-8.
                    let s = unsafe { core::str::from_utf8_unchecked(&bytes[start..start + valid]) };
                    crate::kprint!("{}", s);
                    crate::fb_print!("{}", s);
                }
                // На невалидный байт вывести replacement-символ и пропустить его.
                crate::kprint!("\u{FFFD}");
                crate::fb_print!("\u{FFFD}");
                start += valid + 1;
            }
        }
    }
}

/// Разрешить путь пользователя относительно текущего рабочего каталога shell,
/// вернув абсолютный нормализованный путь.
///
/// Абсолютный вход (начинается с `/`) игнорирует CWD, так что `/mnt/...` работает
/// как раньше; относительные пути и `.`/`..` сворачиваются относительно CWD
/// (R4.6, R9.5). Все обработчики, принимающие путь, пропускают аргумент через
/// это, чтобы относительные пути работали везде.
pub(super) fn resolve_arg(arg: &str) -> String {
    super::path::resolve(&super::path::cwd(), arg)
}

/// Разбить абсолютный путь на `(parent_path, leaf_name)`.
///
/// `"/mnt/foo"` -> `("/mnt", "foo")`, `"/mnt/a/b"` -> `("/mnt/a", "b")`,
/// `"/foo"` -> `("/", "foo")`. Возвращает `None` для путей без листа
/// (`"/"`, `""` или путь только с завершающим слэшем).
fn split_path(path: &str) -> Option<(&str, &str)> {
    let trimmed = path.trim_end_matches('/');
    let idx = trimmed.rfind('/')?;
    let leaf = &trimmed[idx + 1..];
    if leaf.is_empty() {
        return None;
    }
    let parent = if idx == 0 { "/" } else { &trimmed[..idx] };
    Some((parent, leaf))
}

/// `help` (R6.2/6.3/6.4): без аргумента перечислить все зарегистрированные
/// команды и их однострочные описания; с аргументом напечатать usage и описание
/// команды либо внятное «no such command». Генерируется целиком из реестра, чтобы
/// список не расходился с таблицей.
pub(super) fn cmd_help(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.is_empty() {
        shell_println("Available commands:");
        for spec in super::registry::COMMANDS {
            shell_println(&alloc::format!("  {:8} - {}", spec.name, spec.description));
        }
    } else {
        let name = args[0];
        match super::registry::lookup(name) {
            Some(spec) => {
                shell_println(&alloc::format!("usage: {}", spec.usage));
                shell_println(spec.description);
            }
            None => {
                shell_println(&alloc::format!("help: no such command '{}'", name));
            }
        }
    }
}

/// `clear` (R9.3): очистить и serial-scrollback, и framebuffer-вид.
pub(super) fn cmd_clear(_ctx: &mut ShellCtx, _args: &[&str]) {
    for _ in 0..50 {
        crate::kprintln!();
    }
    crate::drivers::framebuffer::clear_screen();
}

/// `echo` (R9.4): соединить аргументы одинарными пробелами и напечатать результат.
pub(super) fn cmd_echo(_ctx: &mut ShellCtx, args: &[&str]) {
    if !args.is_empty() {
        let text = args.join(" ");
        crate::kprintln!("{}", text);
        crate::fb_println!("{}", text);
    }
}

/// `uptime`: напечатать счётчик тиков планировщика и примерное значение секунд.
pub(super) fn cmd_uptime(_ctx: &mut ShellCtx, _args: &[&str]) {
    let ticks = crate::task::scheduler::ticks();
    crate::kprintln!(
        "Ticks: {} (~{} sec)",
        ticks,
        ticks / crate::arch::x86_64::apic::TICK_HZ
    );
    crate::fb_println!(
        "Ticks: {} (~{} sec)",
        ticks,
        ticks / crate::arch::x86_64::apic::TICK_HZ
    );
}

/// `date`: напечатать wall-clock CMOS RTC в UTC.
///
/// `uptime` сообщает монотонные тики с загрузки; у shell не было способа увидеть
/// реальное время, хотя тот же читатель RTC уже питает `clock_gettime(2)` и
/// проверку TLS-сертификатов. `date` печатает эти часы в ISO-форме плюс
/// Unix-секунды и день недели, чтобы строку лога или транскрипт можно было
/// пометить временем без userspace-программы.
///
/// Поля приходят из [`crate::arch::x86_64::linux::rtc::read_date`], то есть из
/// того же защищённого двойным чтением снимка, что использует ядро, так что они
/// относятся к одной согласованной секунде. Ядро считает, что RTC везде хранит
/// UTC (NTP ещё не реализован); машина с локальным временем в RTC покажет его
/// как `UTC` — команда не может угадать, и неверный часовой пояс лучше выдуманной
/// поправки.
pub(super) fn cmd_date(_ctx: &mut ShellCtx, _args: &[&str]) {
    let d = crate::arch::x86_64::linux::rtc::read_date();
    let secs = crate::arch::x86_64::linux::timeconv::civil_to_unix(
        d.year, d.month, d.day, d.hour, d.minute, d.second,
    );
    let unix = if secs < 0 { 0 } else { secs as u64 };
    // 1970-01-01 был четвергом, поэтому день эпохи 0 — индекс 4.
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let weekday = WEEKDAYS[((unix / 86_400 + 4) % 7) as usize];
    shell_println(&format!(
        "{} {:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC (unix {})",
        weekday, d.year, d.month, d.day, d.hour, d.minute, d.second, unix
    ));
}

/// `pwd` (R4.2): напечатать абсолютный текущий рабочий каталог.
pub(super) fn cmd_pwd(_ctx: &mut ShellCtx, _args: &[&str]) {
    shell_println(&super::path::cwd());
}

/// `cd [path]`: сменить текущий рабочий каталог shell.
///
/// Без аргумента сбросить CWD в домашний дефолт `/` (R4.4). С путём — разрешить
/// его относительно текущего CWD, проверить, что это существующий каталог, и
/// сохранить (R4.3). Отсутствующий путь или не-каталог печатает внятную ошибку и
/// оставляет CWD неизменным (R4.5).
pub(super) fn cmd_cd(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.is_empty() {
        // Нет аргумента: сбросить в домашний дефолт `/` (R4.4).
        super::path::set_cwd("/");
        return;
    }

    let target = super::path::resolve(&super::path::cwd(), args[0]);
    match crate::vfs::lookup_path(&target) {
        Ok(node) => {
            if node.is_directory() {
                super::path::set_cwd(&target);
            } else {
                super::render::error_line(&alloc::format!("cd: {}: not a directory", args[0]));
            }
        }
        Err(_) => {
            super::render::error_line(&alloc::format!("cd: {}: not found", args[0]));
        }
    }
}

/// `ls [path]`: перечислить записи каталога (каталоги с завершающим `/`) или
/// напечатать имя самого файла. По умолчанию `/`, если путь не задан.
pub(super) fn cmd_ls(_ctx: &mut ShellCtx, args: &[&str]) {
    // `-l`: длинная форма — режим, ссылки, владелец/группа, размер, имя.
    // Остальное — путь. Нет пути: CWD (R9.2); относительный путь разрешается
    // относительно CWD, чтобы работали `.`/`..` (R4.6).
    let mut long = false;
    let mut path_arg: Option<&str> = None;
    for a in args {
        if *a == "-l" {
            long = true;
        } else if a.starts_with('-') && a.len() > 1 && path_arg.is_none() {
            shell_println(&alloc::format!("ls: unknown option: {}", a));
            return;
        } else if path_arg.is_none() {
            path_arg = Some(a);
        } else {
            shell_println("ls: too many operands");
            return;
        }
    }
    let path = match path_arg {
        Some(p) => resolve_arg(p),
        None => super::path::cwd(),
    };
    match crate::vfs::lookup_path(&path) {
        Ok(node) => {
            if node.is_directory() {
                match node.readdir() {
                    Ok(children) => {
                        for child in children.iter() {
                            if long {
                                shell_println(&ls_long_line(&**child));
                            } else if child.is_directory() {
                                shell_println(&alloc::format!("{}/", child.name()));
                            } else {
                                shell_println(child.name());
                            }
                        }
                    }
                    Err(_) => {
                        shell_println(&alloc::format!("ls: {}: cannot read directory", path));
                    }
                }
            } else if long {
                // `ls -l file`: та же строка, что показал бы листинг каталога.
                shell_println(&ls_long_line(&*node));
            } else {
                // Не каталог: напечатать только имя, как `ls file`.
                shell_println(node.name());
            }
        }
        Err(_) => {
            shell_println(&alloc::format!("ls: {}: not found", path));
        }
    }
}

/// Слово режима в стиле `-rw-r--r--` для `ls -l`: буква типа, девять бит прав и
/// маркеры setuid/setgid/sticky в слоте execute (`s`/`S`/`t`/`T`), как в coreutils.
///
/// У ядра нет доступа к `mtime`/`ctime` на `VfsNode`, поэтому длинный листинг
/// намеренно ограничивается mode/links/owner/size, а не печатает время, которое
/// VFS не может дать (фальшивая колонка `Jan 1 1970` читалась бы как реальные
/// данные).
fn ls_mode_string(node: &dyn crate::vfs::VfsNode) -> alloc::string::String {
    let mode = node.mode();
    let type_char = if node.is_directory() {
        'd'
    } else if node.is_symlink() {
        'l'
    } else {
        '-'
    };
    let mut out = alloc::string::String::with_capacity(10);
    out.push(type_char);
    // (бит чтения, бит записи, бит execute, спецбит, спецбит-если-execute, спецбит-если-не)
    let triplets = [
        (0o400u32, 0o200, 0o100, 0o4000u32, 's', 'S'), // user + setuid
        (0o040, 0o020, 0o010, 0o2000u32, 's', 'S'),    // group + setgid
        (0o004, 0o002, 0o001, 0o1000u32, 't', 'T'),    // other + sticky
    ];
    for (r, w, x, special_bit, special_x, special_no_x) in triplets {
        out.push(if mode & r != 0 { 'r' } else { '-' });
        out.push(if mode & w != 0 { 'w' } else { '-' });
        let special = mode & special_bit != 0;
        out.push(if special && mode & x != 0 {
            special_x
        } else if special {
            special_no_x
        } else if mode & x != 0 {
            'x'
        } else {
            '-'
        });
    }
    out
}

/// Одна строка `ls -l`: режим, число ссылок, владелец, группа, размер, имя (плюс
/// `-> target` для симлинков).
fn ls_long_line(node: &dyn crate::vfs::VfsNode) -> alloc::string::String {
    let (uid, gid) = node.owner();
    let mut line = alloc::format!(
        "{} {:>3} {:>5} {:>5} {:>9} {}",
        ls_mode_string(node),
        node.nlink(),
        uid,
        gid,
        node.size(),
        node.name()
    );
    if node.is_symlink() {
        if let Some(target) = node.read_link() {
            line.push_str(" -> ");
            line.push_str(&target);
        }
    }
    line
}

/// `tree [-L N] [path]`: рекурсивный листинг каталога.
///
/// `ls` показывает один уровень; сайдбар редактора показывает дерево только при
/// редактировании. Эта команда печатает всё дерево ниже `path` (отсортировано,
/// два пробела отступа на уровень) — то, что нужно человеку, осматривающему
/// `/mnt` или свежераспакованный пакет.
///
/// Рендеринг намеренно ASCII: шрифт консоли покрывает ASCII 0x20..0x7E и не имеет
/// glyph'ов рисования рамок, так что `|--`/backtick-арт печатался бы пустотой.
///
/// Глубина ограничена `-L` (по умолчанию 4), и каждый каталог читается через
/// `readdir`, так что цикл симлинков не может рекурсировать бесконечно. Счётчики
/// в трейлере покрывают всё ниже напечатанного корня, не сам корень.
pub(super) fn cmd_tree(_ctx: &mut ShellCtx, args: &[&str]) {
    let mut limit = 4usize;
    let mut path_arg: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if a == "-L" {
            i += 1;
            let Some(n) = args.get(i).and_then(|s| s.parse::<usize>().ok()) else {
                shell_println("tree: usage: tree [-L DEPTH] [PATH]");
                return;
            };
            limit = n;
        } else if a.starts_with('-') && a.len() > 1 {
            shell_println(&alloc::format!("tree: unknown option: {}", a));
            return;
        } else if path_arg.is_none() {
            path_arg = Some(a);
        } else {
            shell_println("tree: too many operands");
            return;
        }
        i += 1;
    }
    let path = match path_arg {
        Some(p) => resolve_arg(p),
        None => super::path::cwd(),
    };
    match crate::vfs::lookup_path(&path) {
        Ok(node) => {
            if node.is_directory() {
                let (mut dirs, mut files) = (0usize, 0usize);
                shell_println(&path);
                tree_walk(&*node, "", 0, limit, &mut dirs, &mut files);
                shell_println(&alloc::format!("{} directories, {} files", dirs, files));
            } else {
                shell_println(node.name());
            }
        }
        Err(_) => shell_println(&alloc::format!("tree: {}: not found", path)),
    }
}

/// Depth-first половина [`cmd_tree`]: напечатать детей `node` по имени, затем
/// рекурсивно войти в каталоги. Ребёнок, чей `readdir` упал, печатается, но не
/// обходится (ради листинга диагностический шум не стоит того).
fn tree_walk(
    node: &dyn crate::vfs::VfsNode,
    prefix: &str,
    depth: usize,
    limit: usize,
    dirs: &mut usize,
    files: &mut usize,
) {
    if depth >= limit {
        return;
    }
    let Ok(children) = node.readdir() else {
        return;
    };
    let mut items: alloc::vec::Vec<&alloc::sync::Arc<dyn crate::vfs::VfsNode>> =
        children.iter().collect();
    items.sort_by(|a, b| a.name().cmp(b.name()));
    for child in items {
        let is_dir = child.is_directory();
        if is_dir {
            *dirs += 1;
            shell_println(&alloc::format!("{}{}/", prefix, child.name()));
        } else {
            *files += 1;
            shell_println(&alloc::format!("{}{}", prefix, child.name()));
        }
        if is_dir {
            let next = alloc::format!("{}  ", prefix);
            tree_walk(&**child, &next, depth + 1, limit, dirs, files);
        }
    }
}

/// `cat <path>`: напечатать содержимое файла, ограничив суммарно прочитанные
/// байты, чтобы бесконечное устройство не крутилось вечно, и завершить вывод на
/// свежей строке, чтобы следующий промпт не приклеился к последней строке файла.
pub(super) fn cmd_cat(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.is_empty() {
        shell_println("cat: missing operand");
        return;
    }
    let path = resolve_arg(args[0]);
    match crate::vfs::lookup_path(&path) {
        Ok(node) => {
            if node.is_directory() {
                shell_println(&alloc::format!("cat: {}: is a directory", path));
            } else {
                // Правила streaming/переводов строк живут в `shell::cat`, чтобы
                // in-kernel набор свойств проверял те же байты, что получает
                // пользователь; этот обработчик — только VFS-чтение + приёмник консоли.
                let out = super::cat::stream(
                    |offset, buf| node.read(offset, buf).map_err(|_| ()),
                    shell_print_bytes,
                );
                if out.read_failed {
                    shell_println(&alloc::format!("cat: {}: read error", path));
                }
            }
        }
        Err(_) => {
            shell_println(&alloc::format!("cat: {}: not found", path));
        }
    }
}

/// `mkdir <path>`: создать каталог под существующим родителем.
pub(super) fn cmd_mkdir(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.is_empty() {
        shell_println("mkdir: missing operand");
        return;
    }
    let path = resolve_arg(args[0]);
    match split_path(&path) {
        Some((parent, leaf)) => match crate::vfs::lookup_path(parent) {
            Ok(dir) => match dir.create_dir(leaf) {
                Ok(_) => {}
                Err(e) => shell_println(&alloc::format!("mkdir: {}: {:?}", path, e)),
            },
            Err(_) => shell_println(&alloc::format!("mkdir: {}: parent not found", path)),
        },
        None => shell_println(&alloc::format!("mkdir: {}: invalid path", path)),
    }
}

/// `touch <path>`: создать пустой файл (no-op, если он уже есть).
pub(super) fn cmd_touch(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.is_empty() {
        shell_println("touch: missing operand");
        return;
    }
    let path = resolve_arg(args[0]);
    match split_path(&path) {
        Some((parent, leaf)) => match crate::vfs::lookup_path(parent) {
            Ok(dir) => match dir.create_file(leaf) {
                Ok(_) => {}
                Err(crate::vfs::VfsError::AlreadyExists) => {}
                Err(e) => shell_println(&alloc::format!("touch: {}: {:?}", path, e)),
            },
            Err(_) => shell_println(&alloc::format!("touch: {}: parent not found", path)),
        },
        None => shell_println(&alloc::format!("touch: {}: invalid path", path)),
    }
}

/// `write <path> <text>`: записать текст в файл, создав его при необходимости.
pub(super) fn cmd_write(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.len() < 2 {
        shell_println("write: usage: write <path> <text>");
        return;
    }
    let path = resolve_arg(args[0]);
    let text = args[1..].join(" ");
    // Открыть файл, если он есть, иначе создать (в его родителе).
    let file = match crate::vfs::lookup_path(&path) {
        Ok(node) => Ok(node),
        Err(_) => match split_path(&path) {
            Some((parent, leaf)) => match crate::vfs::lookup_path(parent) {
                Ok(dir) => dir.create_file(leaf),
                Err(_) => Err(crate::vfs::VfsError::NotFound),
            },
            None => Err(crate::vfs::VfsError::InvalidArgument),
        },
    };
    match file {
        Ok(node) => {
            if node.is_directory() {
                shell_println(&alloc::format!("write: {}: is a directory", path));
            } else {
                // Заменять, а не перезаписывать на месте: более короткий текст не
                // должен оставлять хвост прошлого файла (см. `vfs::replace_contents`).
                match crate::vfs::replace_contents(&node, text.as_bytes()) {
                    Ok(n) => {
                        node.sync();
                        shell_println(&alloc::format!("write: wrote {} bytes to {}", n, path));
                    }
                    Err(e) => shell_println(&alloc::format!("write: {}: {:?}", path, e)),
                }
            }
        }
        Err(e) => shell_println(&alloc::format!("write: {}: {:?}", path, e)),
    }
}

/// `rm <path>`: удалить файл или пустой каталог.
pub(super) fn cmd_rm(_ctx: &mut ShellCtx, args: &[&str]) {
    let mut recursive = false;
    let mut targets: alloc::vec::Vec<&str> = alloc::vec::Vec::new();
    for a in args {
        match *a {
            "-r" | "-R" | "-rf" | "-fr" | "--recursive" => recursive = true,
            x if x.starts_with('-') && x.len() > 1 => { /* other flags (-f) are no-ops here */ }
            _ => targets.push(a),
        }
    }
    if targets.is_empty() {
        shell_println("rm: missing operand");
        return;
    }
    for t in targets {
        let path = resolve_arg(t);
        if let Err(msg) = rm_path(&path, recursive, 0) {
            shell_println(&alloc::format!("rm: {}: {}", path, msg));
        }
    }
}

/// Удалить один операнд. Каталоги требуют `-r`; удаление post-order (сначала
/// дети) с ограничением глубины, чтобы некорректное дерево не рекурсировало
/// бесконечно.
fn rm_path(path: &str, recursive: bool, depth: u8) -> Result<(), &'static str> {
    if depth > 24 {
        return Err("tree too deep");
    }
    let node = crate::vfs::lookup_path(path).map_err(|_| "not found")?;
    if node.is_directory() && !recursive {
        return Err("is a directory (use -r)");
    }
    if node.is_directory() {
        for child in node.readdir().map_err(|_| "readdir failed")? {
            let child_path = if path.ends_with('/') {
                alloc::format!("{}{}", path, child.name())
            } else {
                alloc::format!("{}/{}", path, child.name())
            };
            rm_path(&child_path, recursive, depth + 1)?;
        }
    }
    let (parent, leaf) = split_path(path).ok_or("invalid path")?;
    if leaf.is_empty() {
        return Err("cannot remove that");
    }
    let dir = crate::vfs::lookup_path(parent).map_err(|_| "parent not found")?;
    dir.remove(leaf).map_err(|e| match e {
        crate::vfs::VfsError::AlreadyExists => "directory not empty",
        crate::vfs::VfsError::NotSupported => "not supported there",
        _ => "remove failed",
    })
}

/// `sync`: сбросить смонтированную файловую систему.
pub(super) fn cmd_sync(_ctx: &mut ShellCtx, _args: &[&str]) {
    // Сбросить смонтированную ФС. Каждая мутация уже журналируется на коммите,
    // так что в v1 это no-op-подтверждение.
    match crate::vfs::lookup_path("/mnt") {
        Ok(node) => {
            node.sync();
            shell_println("sync: ok");
        }
        Err(_) => shell_println("sync: /mnt not mounted"),
    }
}

/// `fscrash`: показать replay журнала + персистентность на реальном диске.
pub(super) fn cmd_fscrash(_ctx: &mut ShellCtx, _args: &[&str]) {
    fscrash_demo();
}

/// `pci`: перечислить и показать PCI-устройства.
pub(super) fn cmd_pci(_ctx: &mut ShellCtx, _args: &[&str]) {
    let devices = crate::drivers::pci::enumerate();
    if devices.is_empty() {
        shell_println("pci: no devices found");
    } else {
        for dev in devices.iter() {
            let a = dev.address;
            let tag = if dev.is_virtio() { " [virtio]" } else { "" };
            shell_println(&alloc::format!(
                "{:02x}:{:02x}.{} {:04x}:{:04x} {:02x}:{:02x}{}",
                a.bus,
                a.device,
                a.function,
                dev.vendor_id,
                dev.device_id,
                dev.class,
                dev.subclass,
                tag
            ));
        }
    }
}

/// `id [user]`: идентичность сессии или запись `/etc/passwd`.
///
/// Без аргумента — сессионные креденшелы shell (то, с чем `lxrun` запускает
/// Linux-программы; `su` меняет именно их). С аргументом — пользователь из
/// `/etc/passwd` и его группы из `/etc/group`; это чисто справочный вывод и он
/// не требует, чтобы пользователь был текущим. Формат повторяет coreutils
/// (`uid=1000(oleg) gid=1000(oleg) groups=…`), чтобы привычные скрипты/глаза
/// не удивлялись.
pub(super) fn cmd_id(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.len() > 1 {
        shell_println("id: usage: id [user]");
        return;
    }
    if let Some(name) = args.first() {
        let Some(user) = identity::lookup_user(name) else {
            shell_println(&alloc::format!("id: '{}': no such user", name));
            return;
        };
        let groups = identity::display_groups_for_user(name).unwrap_or_else(|| {
            let mut v = alloc::vec::Vec::new();
            v.push(user.gid);
            v
        });
        shell_println(&alloc::format!(
            "uid={}({}) gid={}({}) groups={}",
            user.uid,
            user.name,
            user.gid,
            identity::group_label(user.gid),
            identity::format_gids(&groups)
        ));
        return;
    }
    let creds = identity::current();
    let groups = identity::session_groups();
    let mut line = alloc::format!(
        "uid={}({}) gid={}({})",
        creds.uid,
        identity::user_label(creds.uid),
        creds.gid,
        identity::group_label(creds.gid)
    );
    if creds.euid != creds.uid {
        line.push_str(&alloc::format!(
            " euid={}({})",
            creds.euid,
            identity::user_label(creds.euid)
        ));
    }
    if creds.egid != creds.gid {
        line.push_str(&alloc::format!(
            " egid={}({})",
            creds.egid,
            identity::group_label(creds.egid)
        ));
    }
    line.push_str(&alloc::format!(
        " groups={}",
        identity::format_gids(&groups)
    ));
    shell_println(&line);
}

/// `groups [user]`: имена групп сессии (или пользователя из `/etc/group`).
pub(super) fn cmd_groups(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.len() > 1 {
        shell_println("groups: usage: groups [user]");
        return;
    }
    let gids = match args.first() {
        Some(name) => match identity::display_groups_for_user(name) {
            Some(gids) => gids,
            None => {
                shell_println(&alloc::format!("groups: '{}': no such user", name));
                return;
            }
        },
        None => identity::session_groups(),
    };
    let groups = identity::load_groups();
    let mut line = alloc::string::String::new();
    for (i, gid) in gids.iter().enumerate() {
        if i > 0 {
            line.push(' ');
        }
        match crate::shell::identity_db::group_name(&groups, *gid) {
            Some(name) => line.push_str(name),
            None => line.push_str(&alloc::format!("{}", gid)),
        }
    }
    if line.is_empty() {
        line.push_str("(none)");
    }
    shell_println(&line);
}

/// `su [-p] [user]`: сменить сессионную идентичность shell.
///
/// Пароль требуется там, где он что-то доказывает: из **не-root** сессии
/// (`su` без аргумента — возврат к root, `su <user>` — переход под другим
/// именем). Root-сессия понижается без пароля, как `su` из root в Linux, а
/// `-p` заставляет спросить пароль и там — это единственный способ проверить
/// сохранённый хэш с консоли. Закрытая запись (`!`/`*`, как `oleg` на свежем
/// диске) не пускает никого: fail-closed, пока root-сессия не задаст пароль
/// через `passwd`.
///
/// Shell-встроенные команды по-прежнему исполняются kernel-кодом; меняется
/// идентичность процессов, которые запускает `lxrun`.
pub(super) fn cmd_su(_ctx: &mut ShellCtx, args: &[&str]) {
    let Some((target, require_password)) = parse_identity_args("su", args, true) else {
        return;
    };
    switch_session("su", target, require_password);
}

/// `login [-p] <user>`: установить сессионную идентичность пользователя.
///
/// В отличие от `su` (переключение) цель обязательна. Аутентификация — та же,
/// что у `su`: из не-root сессии спрашивается пароль цели, `-p` спрашивает его
/// и из root-сессии. `root` аутентифицируется своим паролем из `/etc/shadow`;
/// пока он закрыт (`*`), `login root` из не-root невозможен.
pub(super) fn cmd_login(_ctx: &mut ShellCtx, args: &[&str]) {
    let Some((target, require_password)) = parse_identity_args("login", args, false) else {
        return;
    };
    switch_session("login", target, require_password);
}

/// `whoami`: имя пользователя по **effective** uid из `/etc/passwd`.
///
/// Это ровно то, с чем `lxrun` запускает программы: сессионная идентичность
/// (`su`/`login`/`/etc/pagh-session`). Если записи нет — печатается число,
/// как `id`, а не выдуманное имя.
pub(super) fn cmd_whoami(_ctx: &mut ShellCtx, args: &[&str]) {
    if !args.is_empty() {
        shell_println("whoami: usage: whoami");
        return;
    }
    let creds = identity::current();
    shell_println(&identity::user_label(creds.euid));
}

/// Разбор аргументов `su`/`login`: `[-p|--password] <user>`.
///
/// `default_root` — `su` без цели переключает на root; `login` цель обязан
/// назвать. `None` — сообщение об ошибке уже напечатано (вызывающий
/// возвращается молча).
fn parse_identity_args<'a>(
    command: &str,
    args: &'a [&'a str],
    default_root: bool,
) -> Option<(&'a str, bool)> {
    let mut require_password = false;
    let mut target: Option<&'a str> = None;
    for arg in args {
        if *arg == "-p" || *arg == "--password" {
            require_password = true;
        } else if arg.starts_with('-') {
            shell_println(&format!("{}: unknown option '{}'", command, arg));
            print_identity_usage(command, default_root);
            return None;
        } else if target.is_none() {
            target = Some(arg);
        } else {
            print_identity_usage(command, default_root);
            return None;
        }
    }
    match (target, default_root) {
        (Some(target), _) => Some((target, require_password)),
        (None, true) => Some(("root", require_password)),
        (None, false) => {
            print_identity_usage(command, false);
            None
        }
    }
}

fn print_identity_usage(command: &str, default_root: bool) {
    if default_root {
        shell_println(&format!("{}: usage: {} [-p] [user]", command, command));
    } else {
        shell_println(&format!("{}: usage: {} [-p] <user>", command, command));
    }
}

/// Общая часть `su`/`login`: доказать право (если нужно), построить
/// идентичность цели и поставить её сессии.
///
/// Пароль спрашивается, когда сессия не root (иначе смена была бы эскалацией
/// без аутентификации) или когда `require_password` выставлен явно (`-p`).
/// Root-сессия понижается без пароля — как `su` из root в Linux.
///
/// Возвращает `None`, если переключение не состоялось (сообщение уже
/// напечатано). Отдельная функция, чтобы `su` и `login` не расходились в
/// правилах: обе идут через [`identity::login_creds`].
fn switch_session(command: &str, target: &str, require_password: bool) -> Option<Creds> {
    let current = identity::current();
    let to_root = target == "root" || target == "0";
    let authenticated = require_password || !current.is_root();

    if to_root {
        if authenticated && !authenticate(command, "root") {
            return None;
        }
        identity::set_session(Creds::ROOT);
        if authenticated {
            shell_println(&format!(
                "{}: authenticated as root: session identity is root (uid=0, gid=0, no groups)",
                command
            ));
        } else {
            shell_println(&format!(
                "{}: session identity is root (uid=0, gid=0, no groups)",
                command
            ));
        }
        return Some(Creds::ROOT);
    }

    if authenticated && !authenticate(command, target) {
        return None;
    }
    match identity::login_creds(target) {
        Ok(creds) => {
            identity::set_session(creds);
            let how = if authenticated {
                format!("authenticated as {}", target)
            } else {
                String::from("root session: no password required")
            };
            shell_println(&format!(
                "{}: {}: {} -> {} (uid={} gid={})",
                command,
                how,
                identity::user_label(current.euid),
                target,
                creds.uid,
                creds.gid
            ));
            shell_println(&format!(
                "{}: groups={}",
                command,
                identity::format_gids(&identity::session_groups())
            ));
            shell_println(&format!(
                "{}: 'lxrun' now starts programs with these credentials; shell built-ins still run as kernel/root (phase-1 limitation)",
                command
            ));
            Some(creds)
        }
        Err(identity::LoginError::UnknownUser) => {
            shell_println(&format!(
                "{}: user '{}' does not exist in /etc/passwd",
                command, target
            ));
            None
        }
        Err(identity::LoginError::RootTarget) => {
            shell_println(&format!(
                "{}: '{}' has uid 0; use '{} root' to reset to the root session",
                command, target, command
            ));
            None
        }
        Err(identity::LoginError::GroupDatabase) => {
            shell_println(&format!(
                "{}: '{}': group database (/etc/group) unavailable",
                command, target
            ));
            None
        }
        Err(identity::LoginError::TooManyGroups) => {
            shell_println(&format!(
                "{}: '{}': too many supplementary groups (this kernel supports at most {})",
                command, target, GROUPS_CAP
            ));
            None
        }
    }
}

/// Спросить пароль и проверить его против `/etc/shadow` (fail-closed).
///
/// Отсутствие записи, закрытый хэш (`!`/`*`), отсутствие клавиатуры и неверный
/// пароль одинаково возвращают `false`: «не смогли проверить» не должно
/// превращаться в «пустить». Сообщение печатается здесь, чтобы `su`/`login`
/// не дублировали формулировки.
fn authenticate(command: &str, user: &str) -> bool {
    if identity::lookup_user(user).is_none() {
        shell_println(&format!(
            "{}: user '{}' does not exist in /etc/passwd",
            command, user
        ));
        return false;
    }
    if !identity::has_password(user) {
        shell_println(&format!(
            "{}: '{}' has no password set (locked in /etc/shadow); a root session must run 'passwd {}'",
            command, user, user
        ));
        return false;
    }
    let Some(password) = read_secret(&format!("Password for {}: ", user)) else {
        shell_println(&format!("{}: authentication cancelled", command));
        return false;
    };
    if !identity::verify_user_password(user, &password) {
        shell_println(&format!("{}: incorrect password", command));
        return false;
    }
    true
}

/// `passwd [user]`: сменить пароль записи `/etc/shadow`.
///
/// root-сессия меняет пароль любой записи без старого пароля (как root в
/// Linux). Не-root сессия может менять **только свой** пароль и доказывает
/// владение: если хэш уже задан — вводится текущий пароль. Закрытая (`!`/`*`)
/// запись — это «первый пароль» учётки (доказывать нечего), но root при этом
/// остаётся закрытым: задать ему пароль может только root-сессия, поэтому
/// `su root` из не-root по-прежнему невозможен.
///
/// Пароль читается с клавиатуры без эха и не попадает ни в history, ни в
/// вывод. Пустой пароль отвергается: `passwd` не должен молча открыть учётку.
pub(super) fn cmd_passwd(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.len() > 1 {
        shell_println("passwd: usage: passwd [user]");
        return;
    }
    let current = identity::current();
    let target = match args.first() {
        Some(name) => String::from(*name),
        None => match identity::name_for_uid(current.euid) {
            Some(name) => name,
            None => {
                shell_println(&format!(
                    "passwd: no /etc/passwd entry for uid {}",
                    current.euid
                ));
                return;
            }
        },
    };
    if identity::lookup_user(&target).is_none() {
        shell_println(&format!(
            "passwd: user '{}' does not exist in /etc/passwd",
            target
        ));
        return;
    }
    if !current.is_root() {
        if identity::name_for_uid(current.euid).as_deref() != Some(target.as_str()) {
            shell_println(
                "passwd: permission denied (only a root session may change another user's password)",
            );
            return;
        }
        if identity::has_password(&target) {
            let Some(old) = read_secret("Current password: ") else {
                shell_println("passwd: cancelled");
                return;
            };
            if !identity::verify_user_password(&target, &old) {
                shell_println("passwd: incorrect current password");
                return;
            }
        }
    }
    let Some(new) = read_secret("New password: ") else {
        shell_println("passwd: cancelled");
        return;
    };
    if new.is_empty() {
        shell_println("passwd: empty password is not allowed");
        return;
    }
    let Some(again) = read_secret("Retype new password: ") else {
        shell_println("passwd: cancelled");
        return;
    };
    if new != again {
        shell_println("passwd: the passwords do not match");
        return;
    }
    if identity::set_user_password(&target, &new) {
        shell_println(&format!("passwd: password updated for '{}'", target));
    } else {
        shell_println("passwd: cannot update /etc/shadow");
    }
}

/// Прочитать строку с клавиатуры **без эха** (пароль).
///
/// `None` — нет клавиатуры (headless-прогон не должен висеть на промпте) или
/// Ctrl+C/Esc. Введённое не печатается и не попадает в history: эхо выключено
/// на уровне этого цикла, а строка живёт только в стеке. Backspace правит
/// накопленный текст. В idle цикл уходит в `halt` с включёнными прерываниями
/// (как основной цикл shell), поэтому не жжёт CPU и не спит вечно.
fn read_secret(prompt: &str) -> Option<String> {
    if !crate::drivers::any_keyboard() {
        shell_println("no keyboard detected; cannot read a password");
        return None;
    }
    crate::kprint!("{}", prompt);
    crate::fb_print!("{}", prompt);
    let mut decoder = super::keys::Decoder::new();
    let mut line = String::new();
    loop {
        let Some(scancode) = super::try_read_scancode() else {
            crate::arch::cpu::halt();
            continue;
        };
        match decoder.feed(scancode) {
            Some(super::keys::KeyEvent::Enter) => {
                crate::kprintln!();
                crate::fb_println!();
                return Some(line);
            }
            Some(super::keys::KeyEvent::Ctrl('c')) | Some(super::keys::KeyEvent::Escape) => {
                // Защёлку ^C, которую ставит декодер, гасим: она предназначена
                // для ожидания foreground-ребёнка `lxrun`, и оставлять её
                // взведённой значило бы мгновенно завершить следующую программу.
                super::keys::CTRL_C_LATCH.store(false, core::sync::atomic::Ordering::Relaxed);
                crate::kprintln!();
                crate::fb_println!();
                return None;
            }
            Some(super::keys::KeyEvent::Backspace) => {
                line.pop();
            }
            Some(super::keys::KeyEvent::Char(c)) => {
                // Верхняя граница — в байтах (`shadow::PASSWORD_MAX`), поэтому
                // проверяем длину будущей строки, а не число символов: иначе
                // многобайтный символ на пределе дал бы «пароль слишком
                // длинный» уже после двух одинаковых вводов.
                if line.len() + c.len_utf8() <= super::shadow::PASSWORD_MAX {
                    line.push(c);
                }
            }
            _ => {}
        }
    }
}

/// `exec`: запустить встроенный тестовый пользовательский процесс.
pub(super) fn cmd_exec(_ctx: &mut ShellCtx, _args: &[&str]) {
    // create_user_process программирует TSS RSP0 и ненадолго переключает CR3;
    // запускать с выключенными прерываниями, чтобы окно CR3 не вытеснилось
    // на этом прерываемом потоке shell.
    let result =
        crate::arch::cpu::without_interrupts(crate::task::process::spawn_test_user_process);
    match result {
        Ok(pid) => {
            shell_println(&alloc::format!("exec: started user process pid {}", pid));
        }
        Err(e) => {
            shell_println(&alloc::format!("exec: failed: {}", e));
        }
    }
}

/// `ifconfig`: показать конфигурацию сетевого интерфейса.
pub(super) fn cmd_ifconfig(_ctx: &mut ShellCtx, _args: &[&str]) {
    match crate::net::ip_config() {
        Some(cfg) => {
            let m = cfg.mac.0;
            shell_println(&alloc::format!("eth0  inet {}", cfg.addr));
            shell_println(&alloc::format!("      gateway {}", cfg.gateway));
            if let Some(v6) = crate::net::ip6_config() {
                shell_println(&alloc::format!("      inet6 {}", v6.addr));
            }
            if let Some(gw6) = crate::net::ip6_gateway() {
                shell_println(&alloc::format!("      gateway6 {} (link-local)", gw6));
            }
            shell_println(&alloc::format!(
                "      ether {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                m[0],
                m[1],
                m[2],
                m[3],
                m[4],
                m[5]
            ));
        }
        None => shell_println("ifconfig: no interface"),
    }
}

/// `ping <ip>`: послать один ICMP echo-запрос (v4 или v6) и напечатать RTT.
pub(super) fn cmd_ping(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.is_empty() {
        shell_println("usage: ping <ip|ipv6>");
        return;
    }
    let ip = match crate::net::wire::parse_ip_literal(args[0]) {
        Some(ip) => ip,
        None => {
            shell_println("ping: invalid IP address");
            return;
        }
    };
    shell_println(&alloc::format!("ping: sending echo request to {} ...", ip));
    match crate::net::ping(ip) {
        Some(rtt_ticks) => {
            let ms = rtt_ticks * 1000 / crate::arch::x86_64::apic::TICK_HZ as u64;
            shell_println(&alloc::format!("ping: reply from {} in {} ms", ip, ms));
        }
        None => shell_println("ping: no reply (timeout)"),
    }
}

/// `nc <ip> <port> [text]` (R15.2, R15.3): открыть TCP-соединение с endpoint и
/// отзеркалить данные.
///
/// Упрощение v1 (задокументировано): shell построчный, поэтому вместо полностью
/// интерактивной сессии команда соединяется, посылает одну строку (хвостовые
/// `text`-аргументы или тестовую строку по умолчанию), собирает отзеркаленные
/// байты в коротком ограниченном окне poll, печатает их и закрывается. Это
/// проверяет гостевой TCP-клиент против echo-сервера.
pub(super) fn cmd_nc(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.len() < 2 {
        shell_println("usage: nc <ip> <port> [text]");
        return;
    }

    let ip = match crate::net::wire::parse_ip_literal(args[0]) {
        Some(ip) => ip,
        None => {
            shell_println("nc: invalid IP address");
            return;
        }
    };
    let port: u16 = match args[1].parse() {
        Ok(p) => p,
        Err(_) => {
            shell_println("nc: invalid port");
            return;
        }
    };

    // Payload: остальные аргументы, соединённые, или тестовая строка по умолчанию.
    let payload: String = if args.len() > 2 {
        args[2..].join(" ")
    } else {
        String::from("pagh-nc-test")
    };

    let remote = crate::net::IpEndpoint::new(ip, port);

    shell_println(&alloc::format!(
        "nc: connecting to {}:{} ...",
        args[0],
        port
    ));

    match crate::net::nc_echo(remote, payload.as_bytes()) {
        crate::net::NcResult::Echoed(bytes) => {
            if bytes.is_empty() {
                shell_println("nc: connected, no data echoed back");
            } else {
                shell_println(&alloc::format!("nc: echoed {} bytes:", bytes.len()));
                shell_print_bytes(&bytes);
                crate::kprintln!();
                crate::fb_println!();
            }
        }
        crate::net::NcResult::Failed => {
            shell_println("nc: connection failed");
        }
    }
}

/// `selftest [passes]`: запустить набор self-test ядра (вывод в serial).
///
/// Каждая зарегистрированная процедура должна быть неразрушающей
/// (восстанавливает свободные счётчики PMM, heap, состояние прерываний, VFS, …).
/// `passes >= 2` превращает это утверждение в измеримое: ВТОРОЙ проход той же
/// загрузки должен сообщить то же число процедур, ноль упавших проверок и ту же
/// дельту `[selftest] PMM hygiene`, что и первый — процедура, удерживающая
/// кадры, VFS-узлы или heap, заставит проходы разойтись, и именно так стенд был
/// найден неидемпотентным (одна ELF-fuzz процедура когда-то съедала весь пул PMM).
pub(super) fn cmd_selftest(_ctx: &mut ShellCtx, args: &[&str]) {
    let passes = args
        .first()
        .and_then(|a| a.parse::<u32>().ok())
        .unwrap_or(1)
        .clamp(1, 10);
    shell_println("Running kernel self-test (output on serial)...");
    let mut results: alloc::vec::Vec<(usize, u32, u32, i64)> = alloc::vec::Vec::new();
    for pass in 1..=passes {
        if passes > 1 {
            shell_println(&alloc::format!(
                "--- self-test pass {}/{} ---",
                pass,
                passes
            ));
        }
        results.push(crate::test::run_all());
    }
    if passes > 1 {
        // Вердикт идемпотентности в ОДНОЙ строке: поздний проход не должен
        // сообщить больше падений, чем первый (и другое число процедур).
        // Процедура, удерживающая состояние — прежде всего кадры, — заставит
        // проходы разойтись здесь, а строки `[selftest] PMM hygiene` выше
        // показывают, в какую сторону двинулся пул. E2E ищет этот маркер.
        let (n0, f0, s0, d0) = results[0];
        let (nl, fl, sl, dl) = *results.last().expect("at least one pass");
        // Расхождение = любой проход сообщает больше падений или пропусков, чем
        // первый, другое число процедур или БОЛЬШУЮ дельту кадров, чем первый.
        // Постоянный остаток на проход (фиксированная warm-up аллокация, которая
        // стабилизируется) — не расхождение; растущий — ровно то удержание,
        // которое проверяется.
        let diverged = results
            .iter()
            .any(|(n, f, sk, d)| *n != n0 || *f > f0 || *sk > s0 || *d > d0);
        crate::kprintln!(
            "SELFTEST IDEMPOTENCY: passes={} first={} routines/{} failed/{} skipped/{} frames last={} routines/{} failed/{} skipped/{} frames diverged={}",
            passes,
            n0,
            f0,
            s0,
            d0,
            nl,
            fl,
            sl,
            dl,
            diverged
        );
    }
    shell_println("Self-test complete (see serial log).");
}

/// `fscrash` demo: показать replay журнала + персистентность на реальном диске
/// end-to-end.
///
/// Пишет `/mnt/crashtest.txt` известным содержимым через журналируемый путь
/// записи, затем перемонтирует ext2 с того же блочного устройства — это
/// запускает `journal.recover()` — и читает файл обратно, утверждая, что
/// закоммиченная запись выжила. Печатает PASS/FAIL.
///
/// Настоящий power-cut посреди записи нельзя внедрить на живом железе;
/// RAM-mock property-тесты P10/P11 покрывают атомарность краша. Эта команда
/// показывает путь mount + recover + persist end-to-end на реальном устройстве.
fn fscrash_demo() {
    use crate::fs::ext2::Ext2Fs;

    const CONTENT: &[u8] = b"pagh-fscrash-journal-replay-consistency-OK";

    let blk = match crate::drivers::get_block("virtio-blk0") {
        Some(b) => b,
        None => {
            shell_println("fscrash: no virtio-blk device");
            return;
        }
    };

    // 1. Смонтировать и записать тестовый файл через журналируемый путь.
    let root = match Ext2Fs::mount(blk.clone()) {
        Ok(r) => r,
        Err(e) => {
            shell_println(&alloc::format!("fscrash: mount failed: {:?}", e));
            return;
        }
    };
    let file = match root.create_file("crashtest.txt") {
        Ok(f) => f,
        Err(crate::vfs::VfsError::AlreadyExists) => match root.lookup("crashtest.txt") {
            Ok(f) => f,
            Err(e) => {
                shell_println(&alloc::format!("fscrash: open failed: {:?}", e));
                return;
            }
        },
        Err(e) => {
            shell_println(&alloc::format!("fscrash: create failed: {:?}", e));
            return;
        }
    };
    if let Err(e) = file.write(0, CONTENT) {
        shell_println(&alloc::format!("fscrash: write failed: {:?}", e));
        return;
    }
    shell_println("fscrash: wrote /mnt/crashtest.txt (committed via journal)");

    // 2. Принудительно перемонтировать заново; это запускает journal recover()
    //    до построения корня, имитируя путь монтирования после краша.
    shell_println("fscrash: remounting (triggers journal recover)...");
    let root2 = match Ext2Fs::mount(blk) {
        Ok(r) => r,
        Err(e) => {
            shell_println(&alloc::format!("fscrash: remount failed: {:?}", e));
            return;
        }
    };

    // 3. Прочитать файл обратно и проверить, что содержимое сохранилось.
    let node = match root2.lookup("crashtest.txt") {
        Ok(n) => n,
        Err(e) => {
            shell_println(&alloc::format!(
                "fscrash: FAIL — file missing after remount: {:?}",
                e
            ));
            return;
        }
    };
    let mut buf = [0u8; 64];
    match node.read(0, &mut buf) {
        Ok(n) if &buf[..n] == CONTENT => {
            shell_println(&alloc::format!(
                "fscrash: PASS — content survived remount/replay ({} bytes), filesystem consistent",
                n
            ));
        }
        Ok(n) => {
            shell_println(&alloc::format!(
                "fscrash: FAIL — content mismatch after remount ({} bytes)",
                n
            ));
        }
        Err(e) => shell_println(&alloc::format!("fscrash: FAIL — read-back error: {:?}", e)),
    }
}

/// Скопировать все байты из `src` в `dst` чанками, ограничив суммарный объём,
/// чтобы бесконечное устройство не крутилось вечно. Возвращает число скопированных байт.
fn copy_bytes(
    src: &alloc::sync::Arc<dyn crate::vfs::VfsNode>,
    dst: &alloc::sync::Arc<dyn crate::vfs::VfsNode>,
) -> Result<u64, crate::vfs::VfsError> {
    const MAX_BYTES: u64 = 16 * 1024 * 1024;
    // `cp` заменяет назначение: копирование более короткого файла поверх длинного
    // раньше оставляло старый хвост. Сначала truncate, как `O_TRUNC`.
    dst.truncate(0)?;
    let mut buf = [0u8; 512];
    let mut offset: u64 = 0;
    loop {
        if offset >= MAX_BYTES {
            break;
        }
        let n = src.read(offset, &mut buf)?;
        if n == 0 {
            break;
        }
        dst.write(offset, &buf[..n])?;
        offset += n as u64;
    }
    dst.sync();
    Ok(offset)
}

/// `cp <src> <dst>`: скопировать файл. Если `dst` — существующий каталог,
/// исходный файл копируется в него под своим именем.
pub(super) fn cmd_cp(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.len() < 2 {
        shell_println("cp: usage: cp <src> <dst>");
        return;
    }
    let src = resolve_arg(args[0]);
    let dst = resolve_arg(args[1]);

    let src_node = match crate::vfs::lookup_path(&src) {
        Ok(n) => n,
        Err(_) => {
            shell_println(&alloc::format!("cp: {}: not found", src));
            return;
        }
    };
    if src_node.is_directory() {
        shell_println(&alloc::format!("cp: {}: is a directory (not copied)", src));
        return;
    }

    // Разрешить назначение: в каталог (сохранить leaf-имя источника) или в явный
    // путь файла (создать в родителе при необходимости).
    let dst_node = match crate::vfs::lookup_path(&dst) {
        Ok(node) if node.is_directory() => {
            let leaf = match split_path(&src) {
                Some((_, l)) => l,
                None => {
                    shell_println("cp: invalid source path");
                    return;
                }
            };
            match node.create_file(leaf) {
                Ok(n) => n,
                Err(crate::vfs::VfsError::AlreadyExists) => match node.lookup(leaf) {
                    Ok(n) => n,
                    Err(e) => {
                        shell_println(&alloc::format!("cp: {}: {:?}", dst, e));
                        return;
                    }
                },
                Err(e) => {
                    shell_println(&alloc::format!("cp: {}: {:?}", dst, e));
                    return;
                }
            }
        }
        Ok(node) => node, // overwrite existing file
        Err(_) => match split_path(&dst) {
            Some((parent, leaf)) => match crate::vfs::lookup_path(parent) {
                Ok(dir) => match dir.create_file(leaf) {
                    Ok(n) => n,
                    Err(e) => {
                        shell_println(&alloc::format!("cp: {}: {:?}", dst, e));
                        return;
                    }
                },
                Err(_) => {
                    shell_println(&alloc::format!("cp: {}: parent not found", dst));
                    return;
                }
            },
            None => {
                shell_println(&alloc::format!("cp: {}: invalid path", dst));
                return;
            }
        },
    };

    match copy_bytes(&src_node, &dst_node) {
        Ok(n) => shell_println(&alloc::format!("cp: copied {} bytes", n)),
        Err(e) => shell_println(&alloc::format!("cp: copy failed: {:?}", e)),
    }
}

/// `mv <src> <dst>`: переместить/переименовать файл (копировать, затем удалить источник).
pub(super) fn cmd_mv(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.len() < 2 {
        shell_println("mv: usage: mv <src> <dst>");
        return;
    }
    let src = resolve_arg(args[0]);

    let src_node = match crate::vfs::lookup_path(&src) {
        Ok(n) => n,
        Err(_) => {
            shell_println(&alloc::format!("mv: {}: not found", src));
            return;
        }
    };
    if src_node.is_directory() {
        shell_println(&alloc::format!("mv: {}: is a directory (not moved)", src));
        return;
    }

    // Переиспользовать cp для копирования, затем unlink источника.
    cmd_cp(_ctx, args);

    match split_path(&src) {
        Some((parent, leaf)) => match crate::vfs::lookup_path(parent) {
            Ok(dir) => {
                if let Err(e) = dir.remove(leaf) {
                    shell_println(&alloc::format!("mv: remove {}: {:?}", src, e));
                }
            }
            Err(_) => shell_println(&alloc::format!("mv: {}: parent not found", src)),
        },
        None => shell_println(&alloc::format!("mv: {}: invalid path", src)),
    }
}

/// `stat <path>`: напечатать имя, тип и размер файла/каталога.
pub(super) fn cmd_stat(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.is_empty() {
        shell_println("stat: missing operand");
        return;
    }
    let path = resolve_arg(args[0]);
    match crate::vfs::lookup_path(&path) {
        Ok(node) => {
            let kind = if node.is_directory() {
                "directory"
            } else {
                "file"
            };
            shell_println(&alloc::format!("  path: {}", path));
            shell_println(&alloc::format!("  name: {}", node.name()));
            shell_println(&alloc::format!("  type: {}", kind));
            shell_println(&alloc::format!("  size: {} bytes", node.size()));
            if node.is_directory() {
                if let Ok(children) = node.readdir() {
                    shell_println(&alloc::format!("  entries: {}", children.len()));
                }
            }
        }
        Err(_) => shell_println(&alloc::format!("stat: {}: not found", path)),
    }
}

/// `chmod <octal-mode> <path>…`: сменить биты прав.
///
/// Это те же девять бит прав плюс setuid/setgid/sticky, которые принимает
/// `chmod(2)` (контракт `set_mode` VFS), поэтому восьмеричный аргумент
/// проверяется против `0o7777` и ничего больше: режим с выставленными старшими
/// битами — опечатка, а не просьба проставить file-type биты из userspace.
/// ФС без on-disk метаданных (ramfs, `/dev`, `/proc`) отвечают `NotSupported`;
/// команда сообщает это по пути, а не валит весь вызов, так что
/// `chmod 600 /mnt/etc/hosts /tmp/x` всё равно исправит ext2-файл.
pub(super) fn cmd_chmod(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.len() < 2 {
        shell_println("chmod: usage: chmod OCTAL-MODE PATH...");
        return;
    }
    let Ok(mode) = u32::from_str_radix(args[0], 8) else {
        shell_println(&alloc::format!("chmod: invalid mode: '{}'", args[0]));
        return;
    };
    if mode & !0o7777 != 0 {
        shell_println(&alloc::format!(
            "chmod: invalid mode: '{}' (only permission bits, e.g. 755)",
            args[0]
        ));
        return;
    }
    for raw in &args[1..] {
        let path = resolve_arg(raw);
        match crate::vfs::lookup_path(&path) {
            Ok(node) => match node.set_mode(mode) {
                Ok(()) => {}
                Err(crate::vfs::VfsError::NotSupported) => {
                    shell_println(&alloc::format!(
                        "chmod: {}: operation not supported (no on-disk mode)",
                        path
                    ));
                }
                Err(e) => shell_println(&alloc::format!("chmod: {}: {:?}", path, e)),
            },
            Err(_) => shell_println(&alloc::format!("chmod: {}: not found", path)),
        }
    }
}

/// `chown <uid>[:<gid>] <path>…`: сменить владельца и/или группу узла.
///
/// Спецификация в форме Linux: `1000:1000` ставит обоих, `1000` сохраняет
/// группу, `:1000` сохраняет владельца. Принимаются только числовые id — в ядре
/// нет базы пользователей для резолва имён, и ненастоящее имя должно падать, а не
/// молча становиться uid 0. Как и [`cmd_chmod`], пути без on-disk метаданных
/// отвечают `NotSupported` по пути, остальные пути всё равно пробуются.
pub(super) fn cmd_chown(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.len() < 2 {
        shell_println("chown: usage: chown UID[:GID] PATH...");
        return;
    }
    let spec = args[0];
    let (uid_part, gid_part) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (spec, None),
    };
    let uid = if uid_part.is_empty() {
        None
    } else {
        match uid_part.parse::<u32>() {
            Ok(v) => Some(v),
            Err(_) => {
                shell_println(&alloc::format!(
                    "chown: invalid user: '{}' (numeric uid expected)",
                    uid_part
                ));
                return;
            }
        }
    };
    let gid = match gid_part {
        None => None,
        Some("") => None,
        Some(g) => match g.parse::<u32>() {
            Ok(v) => Some(v),
            Err(_) => {
                shell_println(&alloc::format!(
                    "chown: invalid group: '{}' (numeric gid expected)",
                    g
                ));
                return;
            }
        },
    };
    if uid.is_none() && gid.is_none() {
        shell_println("chown: usage: chown UID[:GID] PATH...");
        return;
    }
    for raw in &args[1..] {
        let path = resolve_arg(raw);
        match crate::vfs::lookup_path(&path) {
            Ok(node) => {
                let (cur_uid, cur_gid) = node.owner();
                let new_uid = uid.unwrap_or(cur_uid);
                let new_gid = gid.unwrap_or(cur_gid);
                match node.set_owner(new_uid, new_gid) {
                    Ok(()) => {}
                    Err(crate::vfs::VfsError::NotSupported) => {
                        shell_println(&alloc::format!(
                            "chown: {}: operation not supported (no on-disk owner)",
                            path
                        ));
                    }
                    Err(e) => shell_println(&alloc::format!("chown: {}: {:?}", path, e)),
                }
            }
            Err(_) => shell_println(&alloc::format!("chown: {}: not found", path)),
        }
    }
}

/// `sleep <seconds>`: заблокировать shell на заданное целое число секунд,
/// засыпая между тиками таймера вместо busy-spin.
pub(super) fn cmd_sleep(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.is_empty() {
        shell_println("sleep: usage: sleep <seconds>");
        return;
    }
    match args[0].parse::<u64>() {
        Ok(secs) => crate::task::scheduler::sleep_ticks(secs * crate::arch::x86_64::apic::TICK_HZ),
        Err(_) => shell_println(&alloc::format!("sleep: invalid number '{}'", args[0])),
    }
}

/// `nano <path>`: запустить полноэкранный редактор nano+.
pub(super) fn cmd_nano(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.first() == Some(&"--settings") {
        super::nano_config::command(&args[1..]);
        return;
    }
    if args.is_empty() {
        shell_println("usage: nano <path> | nano --settings [show|reset|set <key> <value>]");
        return;
    }
    super::nano::run(args[0]);
}

/// `rust <path> [args...]`: запустить Linux ELF через слой совместимости.
///
/// Встроенный мини-Rust интерпретатор удалён: он разбирал и исполнял недоверенный
/// `.rs`/`.pbc` прямо в ring 0, то есть парсер атакующего работал кодом ядра.
/// Команда оставлена чистым алиасом `lxrun` для нативных Rust-userland бинарников
/// (статических musl ELF); исходники и `.pbc`-пакеты отвергаются.
pub(super) fn cmd_rust(ctx: &mut ShellCtx, args: &[&str]) {
    if args.is_empty() {
        shell_println("usage: rust <static-elf> [args...]");
        return;
    }
    if args[0].ends_with(".rs") || args[0].ends_with(".pbc") {
        super::render::error_line("rust: mini-Rust removed; use lxrun for Linux ELF binaries");
        return;
    }
    cmd_lxrun(ctx, args);
}

/// `cargo`: запустить установленный `/mnt/usr/bin/cargo` через Linux-слой.
pub(super) fn cmd_cargo(ctx: &mut ShellCtx, args: &[&str]) {
    if crate::vfs::lookup_path("/mnt/usr/bin/cargo").is_ok() {
        runtime_command(ctx, "/mnt/usr/bin/cargo", &[], args);
    } else {
        super::render::error_line("cargo: not installed (the embedded mini-Rust was removed)");
    }
}

/// `rustc`: запустить установленный `/mnt/usr/bin/rustc` через Linux-слой.
pub(super) fn cmd_rustc(ctx: &mut ShellCtx, args: &[&str]) {
    if crate::vfs::lookup_path("/mnt/usr/bin/rustc").is_ok() {
        runtime_command(ctx, "/mnt/usr/bin/rustc", &[], args);
    } else {
        super::render::error_line("rustc: not installed (the embedded mini-Rust was removed)");
    }
}

fn runtime_command(ctx: &mut ShellCtx, path: &'static str, prefix: &[&'static str], args: &[&str]) {
    let mut argv: alloc::vec::Vec<&str> =
        alloc::vec::Vec::with_capacity(1 + prefix.len() + args.len());
    argv.push(path);
    argv.extend_from_slice(prefix);
    argv.extend_from_slice(args);
    cmd_lxrun(ctx, &argv);
}

pub(super) fn cmd_lua(ctx: &mut ShellCtx, args: &[&str]) {
    runtime_command(ctx, "/mnt/usr/bin/lua", &[], args);
}

/// Найти установленный glibc CPython в /mnt/usr/bin (любое имя python3*),
/// предпочитая самый конкретный/версионированный вход, чтобы алиас работал с
/// тем, что поставил apt/provisioning (python3.13, 3.16, ...).
fn find_python() -> Option<alloc::string::String> {
    let dir = crate::vfs::lookup_path("/mnt/usr/bin").ok()?;
    let entries = dir.readdir().ok()?;
    let mut best: Option<alloc::string::String> = None;
    for e in entries {
        let n = e.name();
        if n.starts_with("python3") && !n.ends_with(".py") {
            let full = alloc::format!("/mnt/usr/bin/{}", n);
            if best.as_ref().map_or(true, |b| full.len() > b.len()) {
                best = Some(full);
            }
        }
    }
    best
}

pub(super) fn cmd_python(ctx: &mut ShellCtx, args: &[&str]) {
    let Some(py) = find_python() else {
        shell_println("python: no python3 in /mnt/usr/bin yet; run 'ppkg install python3' or wait for first-boot provisioning");
        return;
    };
    let mut argv: alloc::vec::Vec<&str> = alloc::vec::Vec::with_capacity(1 + args.len());
    argv.push(py.as_str());
    argv.extend_from_slice(args);
    cmd_lxrun(ctx, &argv);
}

/// `settings`: интерактивная панель настройки терминала.
///
/// Намеренно модальная — владеет клавиатурой, пока пользователь не выйдет,
/// потому что каждое нажатие там означает «поменяй и покажи», которое
/// построчный редактор shell иначе проглотил бы. `paint` придерживается того же подхода.
pub(super) fn cmd_settings(_ctx: &mut ShellCtx, _args: &[&str]) {
    crate::kprintln!("settings: theme/wallpaper/status/caret");
    crate::kprintln!("settings keys: arrows or hjkl, t=theme w=wallpaper r=reset q=quit");
    super::theme::run_panel();
}

/// `theme`: показать схемы или переключиться на схему по имени / циклически без аргумента.
pub(super) fn cmd_theme(_ctx: &mut ShellCtx, args: &[&str]) {
    use super::theme::{self, THEMES};
    match args.first().copied() {
        None | Some("list") => {
            let current = theme::snapshot().theme().name;
            for t in THEMES {
                let mark = if t.name == current { "*" } else { " " };
                shell_println(&format!("{} {}", mark, t.name));
            }
            shell_println("theme <name> | theme next | settings");
        }
        Some("next") => {
            let name = theme::with(|s| s.next_theme());
            theme::apply();
            theme::repaint();
            shell_println(&format!("theme: {}", name));
        }
        Some(name) => match THEMES.iter().position(|t| t.name == name) {
            Some(i) => {
                theme::with(|s| s.theme = i);
                theme::apply();
                theme::repaint();
                shell_println(&format!("theme: {}", name));
            }
            None => {
                let names: alloc::vec::Vec<&str> = THEMES.iter().map(|t| t.name).collect();
                super::render::error_line(&format!(
                    "theme: unknown '{}' (have: {})",
                    name,
                    names.join(", ")
                ));
            }
        },
    }
}

/// `wallpaper`: задать (или перечислить) фоновый слой за текстом.
pub(super) fn cmd_wallpaper(_ctx: &mut ShellCtx, args: &[&str]) {
    use super::theme::{self, Wallpaper};
    match args.first().copied() {
        None | Some("list") => {
            let current = theme::snapshot().wallpaper;
            for w in Wallpaper::ALL {
                let mark = if w == current { "*" } else { " " };
                shell_println(&format!("{} {}", mark, w.name()));
            }
            shell_println("wallpaper <name> | settings");
        }
        Some(name) => match Wallpaper::from_name(name) {
            Some(w) => {
                theme::with(|s| s.wallpaper = w);
                // `repaint` заново выдаёт сетку, а ручка opaque/transparent
                // консоли решает, штампует ли эта выдача её фон поверх картинки.
                // Без `apply` ручка всё ещё говорит "opaque", и каждые обои были
                // невидимы внутри текстовой сетки.
                theme::apply();
                theme::repaint();
                shell_println(&format!("wallpaper: {}", w.name()));
            }
            None => super::render::error_line(&format!("wallpaper: unknown '{}'", name)),
        },
    }
}

pub(super) fn cmd_pythonc(ctx: &mut ShellCtx, args: &[&str]) {
    if args.is_empty() {
        shell_println("usage: pythonc <file.py> [file2.py ...]");
        return;
    }
    let Some(py) = find_python() else {
        shell_println("pythonc: no python3 in /mnt/usr/bin yet; run 'ppkg install python3' first");
        return;
    };
    let mut argv: alloc::vec::Vec<&str> = alloc::vec::Vec::with_capacity(3 + args.len());
    argv.push(py.as_str());
    argv.push("-m");
    argv.push("py_compile");
    argv.extend_from_slice(args);
    cmd_lxrun(ctx, &argv);
}

/// `paint`: запустить framebuffer-приложение рисования (мышь + клавиатура).
pub(super) fn cmd_paint(_ctx: &mut ShellCtx, _args: &[&str]) {
    crate::kprintln!("paint: launching... (Esc or 'q' to quit)");
    crate::kprintln!(
        "paint keys: p=pencil e=eraser l=line r=rect f=fillrect c=circle d=disc b=bucket i=picker"
    );
    crate::kprintln!(
        "paint keys: 1-0=color [ ]=brush u=undo x=clear m=maximize s=save g=load q=quit"
    );
    crate::kprintln!(
        "paint window: title-bar buttons minimize/maximize/close; taskbar 'Paint' toggles minimize"
    );
    super::paint::run();
}

/// `pkg <host> <path> [port]`: скачать `.deb` по HTTP и установить его файлы на
/// смонтированную ext2 в `/mnt`.
///
/// Прогоняет полный конвейер Package_Fetcher -> Deb_Parser -> Tar_Reader ->
/// Package_Installer: открыть TCP-соединение с `host:port` (порт по умолчанию 80),
/// `GET` `.deb`, разобрать контейнер `ar`, распаковать `data.tar` (только gzip —
/// `xz`/`zstd` Debian-пакеты отвергаются с внятным сообщением) и записать каждый
/// безопасный regular file на ext2. Сеть должна быть поднята (см. `ifconfig`).
///
/// Пример:
///   pkg 10.0.2.2 /pool/main/h/hello/hello-static_2.10.gz.deb
pub(super) fn cmd_pkg(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.len() < 2 {
        shell_println("usage: pkg <host> <path> [port]");
        shell_println("  downloads a .deb over HTTP and installs it under /mnt");
        shell_println("  note: only gzip-compressed .deb data is supported (not xz/zstd)");
        shell_println("  tip: for name-based installs with dependency resolution, use `ppkg`");
        return;
    }
    let host = args[0];
    let path = args[1];
    let port: u16 = if args.len() >= 3 {
        match args[2].parse() {
            Ok(p) => p,
            Err(_) => {
                shell_println(&alloc::format!("pkg: invalid port '{}'", args[2]));
                return;
            }
        }
    } else {
        80
    };

    shell_println(&alloc::format!(
        "pkg: downloading http://{}:{}{} ...",
        host,
        port,
        path
    ));
    let bytes = match crate::net::http_fetch::fetch_deb(host, port, path) {
        Ok(b) => b.0,
        Err(e) => {
            shell_println(&alloc::format!("pkg: download failed: {:?}", e));
            return;
        }
    };
    shell_println(&alloc::format!("pkg: downloaded {} bytes", bytes.len()));

    // Разобрать `ar`-контейнер .deb и найти его члены.
    let members = match crate::pkg::deb::parse_ar(&bytes) {
        Ok(m) => m,
        Err(e) => {
            shell_println(&alloc::format!("pkg: not a valid .deb: {:?}", e));
            return;
        }
    };
    let deb = match crate::pkg::deb::locate_members(&members) {
        Ok(d) => d,
        Err(e) => {
            shell_println(&alloc::format!("pkg: malformed .deb: {:?}", e));
            return;
        }
    };

    // Распаковать член data.tar (gzip, xz или zstd).
    let comp = match crate::pkg::deb::compression_of(deb.data.name) {
        Ok(c) => c,
        Err(e) => {
            shell_println(&alloc::format!("pkg: {:?}", e));
            return;
        }
    };
    let tar = match crate::pkg::deb::decompress_data(&deb.data, comp) {
        Ok(t) => t,
        Err(e) => {
            shell_println(&alloc::format!(
                "pkg: cannot decompress {} ({:?}); supported: gzip/xz/zstd .deb",
                deb.data.name,
                e
            ));
            return;
        }
    };

    // Перечислить и установить regular files на ext2 под /mnt.
    let entries = match crate::pkg::tar::read_tar(&tar) {
        Ok(e) => e,
        Err(e) => {
            shell_println(&alloc::format!("pkg: bad data.tar: {:?}", e));
            return;
        }
    };
    match crate::pkg::install_fs::install_data_tar(&entries, "/mnt") {
        Ok(n) => {
            shell_println(&alloc::format!("pkg: installed {} files under /mnt", n));
            if let Ok(node) = crate::vfs::lookup_path("/mnt") {
                node.sync();
            }
            shell_println("pkg: done. Run an installed binary with: lxrun /mnt/<path>");
        }
        // `Display` называет отвергнутый путь и причину; installer уже выдал
        // структурированную диагностику.
        Err(e) => shell_println(&alloc::format!("pkg: install failed: {}", e)),
    }
}

/// `lxrun <path> [args...]`: запустить установленный статически слинкованный
/// Linux-бинарник как ring-3 `Compat_Process`.
///
/// Читает ELF по `path` из ext2, загружает через Linux ELF loader, строит
/// начальный стек System V с переданными аргументами и ставит в очередь
/// планировщика. Поддерживаются только статические `ET_EXEC` / static-PIE
/// `ET_DYN` (динамически слинкованные отвергаются). Как и `exec`, запускается с
/// выключенными прерываниями на время короткого переключения CR3, которое
/// делают загрузчик/маппер стека.
/// `warn` включает зеркалирование строк лога ядра `[WARN]` на
/// framebuffer-консоль. Serial всегда получает каждую строку; это управляет
/// только экранным спамом, который ломал полноэкранные TUI.
pub(super) fn cmd_warn(_ctx: &mut ShellCtx, args: &[&str]) {
    match args.first().copied() {
        Some("on") => {
            crate::log::set_fb_warn_mirror(true);
            shell_println("warn: [WARN] lines will be shown on screen");
        }
        Some("off") => {
            crate::log::set_fb_warn_mirror(false);
            shell_println("warn: [WARN] lines hidden from screen (serial still logs them)");
        }
        _ => {
            shell_println(&alloc::format!(
                "warn: on-screen [WARN] lines are {}",
                if crate::log::fb_warn_mirror() {
                    "ON"
                } else {
                    "OFF"
                }
            ));
            shell_println("usage: warn <on|off>");
        }
    }
}

/// Дождаться foreground-задания, превращая ^C/^Z в SIGINT/SIGTSTP.
///
/// Возвращается, когда программа выходит или останавливается. Остановленная
/// программа записывается в таблицу заданий shell; `fg`/`bg` возобновляют её
/// позже.
///
/// Терминал принадлежит заданию на всё время: `pgid` — foreground process group
/// терминала (`drivers::tty`), так что ^C/^Z доставляются ИМЕННО этой группе, а
/// не только pid, которого ждёт shell, и shell забирает терминал обратно на
/// каждом пути выхода (exit, stop). Это и делает остановку много-процессного
/// foreground-задания (процесс, сделавший fork) единой.
fn foreground_wait(pid: u64, pgid: u64, command: &str) {
    use crate::arch::x86_64::linux::signal::send_signal_to_pgid_all;
    use crate::arch::x86_64::linux::signal_frame::{SIGINT, SIGTSTP};
    use core::sync::atomic::Ordering;

    crate::shell::keys::CTRL_C_LATCH.store(false, Ordering::Relaxed);
    crate::shell::keys::CTRL_Z_LATCH.store(false, Ordering::Relaxed);
    crate::drivers::ps2_kbd::CTRL_C.store(false, Ordering::Relaxed);
    crate::drivers::ps2_kbd::CTRL_Z.store(false, Ordering::Relaxed);
    crate::drivers::ps2_kbd::FG_PID.store(pid, Ordering::Relaxed);
    // Задание владеет терминалом, пока выполняется (также ставится `lxrun`
    // внутри критической секции spawn; `fg` ставит его здесь перед возобновлением).
    crate::drivers::tty::set_foreground_pgid(pgid);
    let mut parked = false;
    while crate::task::compat::compat_exists(pid) && !parked {
        let stopped = crate::shell::keys::CTRL_Z_LATCH.swap(false, Ordering::Relaxed)
            | crate::drivers::ps2_kbd::CTRL_Z.swap(false, Ordering::Relaxed);
        if stopped {
            // Остановить foreground-группу ТЕРМИНАЛА. Записанный pid — лишь
            // собственный waiter shell: задание, сделавшее fork, имеет несколько
            // thread groups, и все они должны встать, а не только лидер.
            let target = crate::drivers::tty::foreground_pgid();
            match send_signal_to_pgid_all(target, SIGTSTP) {
                Ok(()) => {
                    shell_println("^Z");
                    crate::shell::jobs::record(pid, pgid, String::from(command), true);
                    parked = true;
                }
                Err(_) => shell_println("^Z: no live foreground job"),
            }
        }
        let pressed = crate::shell::keys::CTRL_C_LATCH.swap(false, Ordering::Relaxed)
            | crate::drivers::ps2_kbd::CTRL_C.swap(false, Ordering::Relaxed);
        if pressed {
            // ^C — тоже сигнал терминала: SIGINT идёт foreground-группе.
            shell_println("^C");
            if send_signal_to_pgid_all(crate::drivers::tty::foreground_pgid(), SIGINT).is_err() {
                let _ = crate::arch::x86_64::linux::signal::send_signal(pid, SIGINT);
            }
        }
        crate::task::scheduler::yield_current();
    }
    crate::drivers::ps2_kbd::FG_PID.store(0, Ordering::Relaxed);
    // Задание закончило с экраном: продолжить shell ниже его последней строки,
    // а не затирать её (VT знает, где кончился его курсор, консоль — нет).
    crate::drivers::vt::cursor_to_console();
    if !parked {
        // Задание вышло: забыть его. Припаркованное задание остаётся в таблице
        // для `jobs`/`fg`/`bg`.
        crate::shell::jobs::remove(pid);
    }
    // Вернуть терминал shell. Остановленное задание возобновляет `fg`,
    // который заново вооружает его группу перед SIGCONT.
    crate::drivers::tty::restore_shell_foreground();
}

/// `jobs`: перечислить запомненные задания.
pub(super) fn cmd_jobs(_ctx: &mut ShellCtx, _args: &[&str]) {
    let jobs = crate::shell::jobs::snapshot();
    if jobs.is_empty() {
        shell_println("jobs: none");
        return;
    }
    for (index, (pid, pgid, command, stopped)) in jobs.iter().enumerate() {
        shell_println(&format!(
            "[{}] {} pid={} pgid={} {}",
            index + 1,
            if *stopped { "Stopped" } else { "Running" },
            pid,
            pgid,
            command
        ));
    }
}

/// `bg`: возобновить самое свежее остановленное задание в фоне.
pub(super) fn cmd_bg(_ctx: &mut ShellCtx, _args: &[&str]) {
    let Some((pid, pgid, command)) = crate::shell::jobs::last() else {
        shell_println("bg: no jobs");
        return;
    };
    // Возобновляются все thread groups задания (групповая остановка — на thread
    // group), а терминал остаётся у shell — это и значит «фон»: следующее чтение
    // терминала заданием снова остановит его через SIGTTIN.
    let _ = crate::arch::x86_64::linux::signal::send_signal_to_pgid_all(
        pgid,
        crate::arch::x86_64::linux::signal_frame::SIGCONT,
    );
    crate::shell::jobs::set_stopped(pid, false);
    shell_println(&format!("bg: [{}] {}", pid, command));
}

/// `fg`: возобновить самое свежее остановленное задание и дождаться его.
pub(super) fn cmd_fg(_ctx: &mut ShellCtx, _args: &[&str]) {
    let Some((pid, pgid, command)) = crate::shell::jobs::last() else {
        shell_println("fg: no jobs");
        return;
    };
    // Отдать терминал заданию ДО его возобновления: задание, пишущее сразу после
    // SIGCONT, не должно быть остановлено как «фоновое» (оно снова foreground) —
    // порядок и делает `fg` надёжным.
    crate::drivers::tty::set_foreground_pgid(pgid);
    let _ = crate::arch::x86_64::linux::signal::send_signal_to_pgid_all(
        pgid,
        crate::arch::x86_64::linux::signal_frame::SIGCONT,
    );
    crate::shell::jobs::set_stopped(pid, false);
    shell_println(&format!("fg: [{}] {}", pid, command));
    foreground_wait(pid, pgid, &command);
}

pub(super) fn cmd_lxrun(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.is_empty() {
        shell_println("usage: lxrun <path> [args...]");
        return;
    }
    let path = resolve_arg(args[0]);
    // argv[0] — имя программы как набрано; остальные токены следуют.
    let argv: alloc::vec::Vec<&[u8]> = args.iter().map(|s| s.as_bytes()).collect();

    // Убедиться, что VT-эмулятор существует, чтобы ANSI-вывод nvim рендерился
    // правильно. Он создаётся один раз и затем живёт: консоль и VT делят один
    // framebuffer, а переинициализация на каждый запуск стирала экран и оставляла
    // курсор shell позади (промпт уезжал вниз на каждом запуске).
    crate::drivers::vt::ensure();
    let env: [&[u8]; 12] = [
        b"TERM=xterm",
        b"PYTHON_BASIC_REPL=1",
        b"PATH=/mnt/usr/bin",
        b"LD_LIBRARY_PATH=/mnt/usr/lib/x86_64-linux-gnu:/mnt/usr/lib:/mnt/usr/lib64:/mnt/lib64",
        b"PYTHONHOME=/mnt/usr",
        b"PYTHONPATH=/mnt/usr/lib/python3.13:/mnt/usr/lib/python3.16",
        b"RUSTUP_HOME=/mnt/usr/lib/rustlib",
        b"HOME=/mnt/home/user",
        // Git находит remote-хелперы (git-remote-https) через GIT_EXEC_PATH;
        // без этого вкомпилированный префикс /usr/lib/git-core всё равно
        // разрешался бы через маппинг /mnt, но явное значение держит поиск на
        // ext2-дереве, даже когда cwd другой.
        b"GIT_EXEC_PATH=/mnt/usr/lib/git-core",
        b"GIT_TEMPLATE_DIR=/mnt/usr/share/git-core/templates",
        b"GIT_TRACE=1",
        // Nvim не может создать ~/.local/state/nvim/log; заглушить спам
        // "failed to open $NVIM_LOG_FILE" в stderr, направив его в /dev/null.
        b"NVIM_LOG_FILE=/dev/null",
    ];
    // Спавнить И опубликовать терминальное состояние ребёнка внутри ОДНОЙ
    // секции `without_interrupts`. Ребёнок становится runnable сразу, как
    // возвращаются прерывания, так что всё, что shell делает после, гонится с ним:
    //   * поздняя пометка задания / установка foreground pgid оставила бы окно,
    //     в котором новый процесс «фоновый» относительно shell — его read там
    //     был бы отвергнут, а write остановлен;
    //   * поздняя печать баннера и выравнивание курсора VT позволили бы быстрой
    //     программе написать первую строку на позиции курсора ПРОШЛОГО задания,
    //     так что её вывод лёг бы на строку промпта shell.
    // Оба лечатся выполнением всей передачи до того, как ребёнок сможет
    // исполниться. Вывод консоли здесь безопасен при замаскированных
    // прерываниях: он берёт локи framebuffer/serial, ни один не берётся дважды и
    // ни один не блокирует.
    let result = crate::arch::cpu::without_interrupts(|| {
        let r = crate::task::process::run_linux_binary_as(
            crate::shell::identity::current(),
            &path,
            &argv,
            &env,
        );
        if let Ok(pid) = r {
            let child_pgid = crate::task::compat::pgid_of(pid).unwrap_or(pid);
            crate::task::compat::mark_tty_job(pid);
            crate::drivers::tty::set_foreground_pgid(child_pgid);
            // Вывод задания идёт через VT, shell — через текстовую консоль: оба
            // рисуют в один framebuffer, поэтому передать VT клетку, до которой
            // дошла консоль. Без этого первая строка программы легла бы поверх
            // строки "lxrun: started" выше.
            shell_println(&alloc::format!("lxrun: started Compat_Process pid {}", pid));
            crate::drivers::vt::cursor_from_console();
        }
        r
    });
    match result {
        Ok(pid) => {
            // Ожидание foreground: процесс теперь владеет клавиатурой (его stdin
            // читает поток PS/2 напрямую), поэтому немедленное возобновление
            // промпта заставило бы shell красть каждое нажатие.
            // Ctrl+C доставляет реальный SIGINT ребёнку (Phase-2 signals): защёлка
            // отражает реальное намерение пользователя у клавиатуры, даже когда
            // программа сама не декодирует модификаторы, а проверки EINTR в
            // блокирующих ожиданиях будят ребёнка, чтобы сигнал доставился на
            // следующем возврате из syscall. Программа, ставящая обработчик,
            // может продолжать работу (каждый следующий ^C пересылает заново); без
            // обработчика действие по умолчанию убивает её, завершая этот цикл.
            // Ctrl+Z останавливает foreground-группу терминала (см.
            // `foreground_wait`); shell забирает терминал обратно на выходе.
            let command = args.first().copied().unwrap_or("?").into();
            let child_pgid = crate::task::compat::pgid_of(pid).unwrap_or(pid);
            crate::shell::jobs::record(pid, child_pgid, command, false);
            foreground_wait(pid, child_pgid, args.first().copied().unwrap_or("?"));
        }
        Err(e) => {
            crate::drivers::ps2_kbd::FG_PID.store(0, core::sync::atomic::Ordering::Relaxed);
            let detail = match e {
                crate::task::process::RunError::ArgsTooLarge => "argument list too large",
                crate::task::process::RunError::NotFound => "file not found or unreadable",
                crate::task::process::RunError::LoadFailed(c) => c,
                crate::task::process::RunError::StackFailed => "initial stack construction failed",
                crate::task::process::RunError::AccessDenied => {
                    "search permission denied on the path"
                }
            };
            shell_println(&alloc::format!("lxrun: {}: {}", path, detail));
        }
    }
}

/// `ppkg <subcommand> ...`: пакетный менеджер по именам.
///
/// В отличие от `pkg` (сырые host/path/port), `ppkg` работает по *имени пакета*
/// против скачанного индекса репозитория, автоматически разрешая зависимости:
///
///   * `ppkg update`                  — скачать + разобрать индекс `Packages`.
///   * `ppkg install <name> [name2…]` — разрешить зависимости и поставить каждый пакет.
///   * `ppkg show <name>`             — напечатать метаданные пакета из индекса.
///   * `ppkg list [substr]`           — перечислить (подходящие) имена доступных пакетов.
///   * `ppkg setmirror <host> [base]` — направить apt на другое зеркало.
///
/// Транспорт: обычная dev-сборка pagh включает исходящий доступ к пакетам через
/// дефолтную фичу `network_packages`. HTTPS шифруется, но остаётся
/// неаутентифицированным, поэтому каждое обновление печатает предупреждение о
/// доверии. Fail-closed ядро можно собрать с `--no-default-features`. `setmirror`
/// принимает префикс схемы.
///
/// Индекс живёт только в RAM (64 MiB диска слишком мало для кэша), поэтому
/// `ppkg update` надо повторять каждую загрузку до `install`/`show`/`list`.
pub(super) fn cmd_apt(_ctx: &mut ShellCtx, args: &[&str]) {
    if args.is_empty() {
        apt_usage();
        return;
    }
    match args[0] {
        "update" => cmd_apt_update(),
        "install" => cmd_apt_install(&args[1..]),
        "show" => cmd_apt_show(&args[1..]),
        "list" => cmd_apt_list(&args[1..]),
        "setmirror" => cmd_apt_setmirror(&args[1..]),
        other => {
            shell_println(&alloc::format!("ppkg: unknown subcommand '{}'", other));
            apt_usage();
        }
    }
}

/// Напечатать сводку usage `ppkg`.
fn apt_usage() {
    shell_println("usage: ppkg <command> [args]");
    shell_println("  update                  download & parse the package index");
    shell_println("  install [--quiet] [--progress=ansi|cr|plain] <name> [name...]");
    shell_println("                          install package(s) by name + deps, with progress");
    shell_println("  show <name>             show a package's metadata");
    shell_println("  list [substr]           list available package names");
    shell_println("  setmirror [scheme://]<host> [base]  set the mirror");
    shell_println("                          e.g. setmirror https://deb.debian.org /debian");
    shell_println("  transport: network downloads enabled in the development build");
    shell_println(
        "             https: server certificate verified (chain/SAN/expiry/CertificateVerify)",
    );
    shell_println(
        "             metadata: InRelease/Release.gpg verified against the pinned Debian keys",
    );
    shell_println(
        "                       (subkey binding, expiry/revocation, 2025 clock gate); Packages and",
    );
    shell_println(
        "                       each .deb are checked against the signed SHA-256 before use",
    );
    shell_println("             an unsigned mirror is REFUSED; there is no 'continue anyway' flag");
}

/// `ppkg update`: обновить in-RAM индекс пакетов.
fn cmd_apt_update() {
    let cfg = crate::pkg::apt::config();
    if cfg.tls {
        shell_println(
            "ppkg: HTTPS transport: server certificate verified against the committed CA bundle",
        );
    } else {
        shell_println("ppkg: HTTP transport: the mirror is NOT authenticated (cleartext)");
    }
    shell_println(
        "ppkg: metadata will be verified against the pinned Debian archive keys (InRelease/Release.gpg)",
    );
    shell_println(&alloc::format!(
        "ppkg: updating from {}://{}:{}{} ({}/{}/{})",
        cfg.scheme(),
        cfg.host,
        cfg.port,
        cfg.base,
        cfg.suite,
        cfg.component,
        cfg.arch
    ));
    match crate::pkg::apt::update() {
        Ok(n) => {
            super::render::success_line(&alloc::format!("ppkg: done - {} packages available", n))
        }
        Err(e) => {
            super::render::error_line(&alloc::format!("ppkg: update failed: {}", e.message()))
        }
    }
}

/// Parse the `ppkg install` progress flags.
///
/// `--quiet`/`-q` forces [`ProgressMode::Plain`] (no live UI at all);
/// `--progress=ansi|cr|plain` picks a renderer explicitly; otherwise the mode is
/// auto-detected from console capability ([`super::progress::auto_mode`]).
/// Returns the names to install or `None` on a malformed flag.
fn parse_install_args<'a>(
    args: &'a [&'a str],
) -> Option<(super::ppkg_progress::ProgressMode, Vec<&'a str>)> {
    use super::ppkg_progress::ProgressMode;
    let mut mode: Option<ProgressMode> = None;
    let mut quiet = false;
    let mut names: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if arg == "--quiet" || arg == "-q" {
            quiet = true;
        } else if arg == "--progress" {
            i += 1;
            let value = args.get(i)?;
            mode = Some(ProgressMode::parse(value)?);
        } else if let Some(value) = arg.strip_prefix("--progress=") {
            mode = Some(ProgressMode::parse(value)?);
        } else if arg.starts_with("--") {
            return None;
        } else {
            names.push(arg);
        }
        i += 1;
    }
    let mode = if quiet {
        ProgressMode::Plain
    } else {
        mode.unwrap_or_else(super::progress::auto_mode)
    };
    Some((mode, names))
}

/// `ppkg install [--quiet] [--progress=ansi|cr|plain] <name> [name2 …]`: разрешить
/// и установить каждый названный пакет, рисуя ход установки (DNF-style).
fn cmd_apt_install(args: &[&str]) {
    let Some((mode, names)) = parse_install_args(args) else {
        shell_println(
            "usage: ppkg install [--quiet] [--progress=ansi|cr|plain] <name> [name2 ...]",
        );
        return;
    };
    if names.is_empty() {
        shell_println(
            "usage: ppkg install [--quiet] [--progress=ansi|cr|plain] <name> [name2 ...]",
        );
        return;
    }
    let mut any_installed = false;
    for name in names {
        shell_println(&alloc::format!("ppkg: resolving {} ...", name));
        let mut progress = super::progress::ProgressDriver::new(mode);
        let result = crate::pkg::apt::install_observed(name, &mut progress);
        progress.finish();
        match result {
            Ok(installed) => {
                if installed.is_empty() {
                    shell_println(&alloc::format!(
                        "ppkg: {} is already installed (nothing to do)",
                        name
                    ));
                } else {
                    any_installed = true;
                    super::render::success_line(&alloc::format!(
                        "ppkg: installed {} package(s): {}",
                        installed.len(),
                        installed.join(" ")
                    ));
                }
            }
            Err(e) => super::render::error_line(&alloc::format!("ppkg: {}: {}", name, e.message())),
        }
    }
    if any_installed {
        shell_println("ppkg: run an installed static binary with: lxrun /mnt/<path>");
    }
}

/// `ppkg show <name>`: напечатать метаданные пакета из индекса.
fn cmd_apt_show(args: &[&str]) {
    if args.is_empty() {
        shell_println("usage: ppkg show <name>");
        return;
    }
    let name = args[0];
    match crate::pkg::apt::show(name) {
        Some(s) => {
            shell_println(&alloc::format!("Package: {}", s.package));
            shell_println(&alloc::format!("Version: {}", s.version));
            shell_println(&alloc::format!("Architecture: {}", s.arch));
            shell_println(&alloc::format!("Filename: {}", s.filename));
            let depends = if s.depends.is_empty() {
                String::from("(none)")
            } else {
                s.depends.join(", ")
            };
            shell_println(&alloc::format!("Depends: {}", depends));
            shell_println(&alloc::format!("Size: {} bytes", s.size));
        }
        None => {
            if crate::pkg::apt::has_index() {
                shell_println(&alloc::format!("ppkg: no such package '{}'", name));
            } else {
                shell_println("ppkg: no package index - run `ppkg update` first");
            }
        }
    }
}

/// `ppkg list [substr]`: перечислить (подходящие) имена доступных пакетов с ограничением.
fn cmd_apt_list(args: &[&str]) {
    let filter = args.first().copied();
    if !crate::pkg::apt::has_index() {
        shell_println("ppkg: no package index - run `ppkg update` first");
        return;
    }
    let names = crate::pkg::apt::list(filter);
    if names.is_empty() {
        shell_println("ppkg: no matching packages");
        return;
    }
    // Ограничить вывод, чтобы полный индекс не залил консоль.
    const MAX_LINES: usize = 100;
    for name in names.iter().take(MAX_LINES) {
        shell_println(name);
    }
    if names.len() > MAX_LINES {
        shell_println(&alloc::format!(
            "... ({} more; narrow with `ppkg list <substr>`)",
            names.len() - MAX_LINES
        ));
    }
}

/// `ppkg setmirror <host> [basepath]`: сменить активное зеркало.
fn cmd_apt_setmirror(args: &[&str]) {
    if args.is_empty() {
        shell_println("usage: ppkg setmirror <host> [basepath]");
        return;
    }
    let host = args[0];
    let base = args.get(1).copied();
    crate::pkg::apt::set_mirror(host, base);
    let cfg = crate::pkg::apt::config();
    shell_println(&alloc::format!(
        "ppkg: mirror set to {}://{}:{}{}",
        cfg.scheme(),
        cfg.host,
        cfg.port,
        cfg.base
    ));
    if cfg.tls {
        shell_println(
            "ppkg: transport is HTTPS (TLS 1.3) with fail-closed certificate verification",
        );
        shell_println("ppkg: NOTE: only hosts chaining to the committed CA bundle are accepted (ISRG X1/X2, GTS R1/R4)");
    } else {
        shell_println(
            "ppkg: transport is HTTP (cleartext) -- the mirror and the index are NOT authenticated",
        );
    }
}

/// `df`: ёмкость файловых систем из реестра монтирований VFS.
///
/// Заголовок и раскладка колонок следуют `df -k`; `1K-blocks` — это
/// `blocks_total * block_size / 1024`. Синтетические ФС (`rootfs`, `devtmpfs`,
/// `proc`) не имеют учёта ёмкости, поэтому их числовые колонки — `-`, а не
/// выдуманный ноль. `ext2` сообщает реальные счётчики через `VfsNode::fs_stat`.
/// `usb`: живое состояние xHCI-контроллера и что на каждом порту.
///
/// На реальной машине это первое, на что смотрят, когда USB-клавиатура не
/// работает: порт с `ccs=false` при вставленном устройстве означает, что
/// контроллер его не увидел (кабель, порт или устройство), а `ccs=true` без
/// строки `HID device on port N` в логе означает, что перечисление (шаги в
/// `drivers::xhci`) не прошло.
pub(super) fn cmd_usb(_ctx: &mut ShellCtx, _args: &[&str]) {
    let summary = crate::drivers::xhci::summary();
    let (kb_reports, mouse_reports, unmapped) = crate::drivers::usb_hid::stats();
    if summary.controllers == 0 {
        shell_println("usb: no xHCI controller on the PCI bus");
        shell_println("usb: the kernel drives xHCI only; a PS/2 keyboard needs no driver");
        return;
    }
    if summary.ports == 0 {
        shell_println(&alloc::format!(
            "usb: {} controller(s) found, none could be started (see the boot log)",
            summary.controllers
        ));
        return;
    }
    shell_println(&alloc::format!(
        "usb: {} controller(s), active one {} ports, {} HID device(s), {} report(s)",
        summary.controllers,
        summary.ports,
        summary.devices,
        summary.reports
    ));
    shell_println(&alloc::format!(
        "usb: key reports {}, mouse reports {}, unmapped usages {}",
        kb_reports,
        mouse_reports,
        unmapped
    ));
    // Восстановление endpoint: ненулевой счётчик сбросов — доказательство, что
    // устройство прислало отчёт, не влезший в буфер, и драйвер восстановился,
    // вместо того чтобы отключить ввод. `bad reports` считает отчёты короче, чем
    // требует их layout (отброшены, логируются один раз на endpoint).
    shell_println(&alloc::format!(
        "usb: HID endpoint reset(s) {}, bad report(s) {}",
        summary.hid_recoveries,
        summary.hid_bad_reports
    ));
    shell_println("  PORT  PORTSC      CONNECT  ENABLED  POWER  SPEED  LINK");
    for (port, sc) in crate::drivers::xhci::port_status() {
        let (ccs, ped, pp, speed, pls) = crate::drivers::xhci::decode_portsc(sc);
        let speed_text = match speed {
            1 => "full",
            2 => "low",
            3 => "high",
            4 => "super",
            _ => "-",
        };
        shell_println(&alloc::format!(
            "  {:>4}  {:#010x}  {:>7}  {:>7}  {:>5}  {:>5}  {}",
            port,
            sc,
            ccs,
            ped,
            pp,
            speed_text,
            match pls {
                0 => "U0",
                3 => "U3",
                5 => "RxDetect",
                _ => "poll",
            }
        ));
    }
    shell_println("usb: a keyboard shows as `ccs=true` on its port, then `HID device on port N`");
}

/// `disk`: блочные устройства и осознанный путь к отформатированному `/mnt`.
///
/// Интерактивный мастер живёт в [`crate::shell::disk`]; это тонкий шим команды,
/// так что REPL и мастер остаются независимыми.
pub(super) fn cmd_disk(_ctx: &mut ShellCtx, args: &[&str]) {
    crate::shell::disk::cmd(args);
}

pub(super) fn cmd_df(_ctx: &mut ShellCtx, args: &[&str]) {
    let mut human = false;
    for a in args {
        if *a == "-h" {
            human = true;
        } else {
            shell_println(&alloc::format!("df: unknown option: {}", a));
            shell_println("df: usage: df [-h]");
            return;
        }
    }
    if human {
        shell_println("Filesystem       Size   Used  Avail Use% Mounted on");
    } else {
        shell_println("Filesystem     1K-blocks  Used Available Use% Mounted on");
    }
    let size_cell = |bytes: u64| -> alloc::string::String {
        if human {
            human_size(bytes.saturating_mul(1024))
        } else {
            alloc::format!("{}", bytes)
        }
    };
    for mount in crate::vfs::mount_snapshot() {
        let fs = mount.fs;
        let point = &mount.point;
        let stats = crate::vfs::lookup_path(point)
            .ok()
            .and_then(|node| node.fs_stat());
        match stats {
            Some(s) if s.block_size > 0 => {
                let block_kb = (s.block_size / 1024).max(1);
                let total = s.blocks_total.saturating_mul(block_kb);
                let free = s.blocks_free.saturating_mul(block_kb);
                let used = total.saturating_sub(free);
                let pct = if total == 0 {
                    0
                } else {
                    used.saturating_mul(100) / total
                };
                if human {
                    shell_println(&format!(
                        "{:<12} {:>11} {:>9} {:>11} {:>3}% {}",
                        fs,
                        size_cell(total),
                        size_cell(used),
                        size_cell(free),
                        pct,
                        point
                    ));
                } else {
                    shell_println(&format!(
                        "{:<12} {:>9} {:>5} {:>9} {:>3}% {}",
                        fs, total, used, free, pct, point
                    ));
                }
            }
            // Ёмкости для отчёта нет (контракт VFS: `fs_stat() == None`). Ramfs
            // всё равно знает, сколько байт он ДЕРЖИТ, и это стоит показать:
            // колонки total/available остаются `-` (он ограничен только физической
            // памятью), но `Used` перестаёт быть пожатием плеч. Чисто синтетические
            // деревья (/dev, /proc) не имеют собственных байт и остаются целиком
            // из прочерков, как и печатает Linux по той же причине.
            _ => {
                let used = crate::vfs::lookup_path(point)
                    .ok()
                    .map(|node| tree_bytes(&node, 0))
                    .unwrap_or(0);
                if matches!(fs, "ramfs" | "rootfs") && used > 0 {
                    if human {
                        shell_println(&format!(
                            "{:<12} {:>11} {:>9} {:>11} {:>3}% {}",
                            fs,
                            "-",
                            human_size(used),
                            "-",
                            "-",
                            point
                        ));
                    } else {
                        shell_println(&format!(
                            "{:<12} {:>9} {:>5} {:>9} {:>3}% {}",
                            fs,
                            "-",
                            used.div_ceil(1024),
                            "-",
                            "-",
                            point
                        ));
                    }
                } else if human {
                    shell_println(&format!(
                        "{:<12} {:>11} {:>9} {:>11} {:>3}% {}",
                        fs, "-", "-", "-", "-", point
                    ));
                } else {
                    shell_println(&format!(
                        "{:<12} {:>9} {:>5} {:>9} {:>3}% {}",
                        fs, "-", "-", "-", "-", point
                    ));
                }
            }
        }
    }
}

/// Байты, удерживаемые `node` и всем под ним, в ramfs-дереве.
///
/// Используется `df` для ФС без учёта ёмкости (см. контракт VFS `fs_stat`): они
/// не могут сказать, сколько места осталось, но могут сказать, сколько держат.
/// Ограничено глубиной и циклобезопасно по построению — монтирования ищутся по
/// пути, а дети каталога обходятся только через `readdir`, который в этих
/// деревьях не может вернуться к предку.
fn tree_bytes(node: &alloc::sync::Arc<dyn crate::vfs::VfsNode>, depth: usize) -> u64 {
    const MAX_DEPTH: usize = 8;
    if depth > MAX_DEPTH {
        return 0;
    }
    if !node.is_directory() {
        return node.size();
    }
    let Ok(children) = node.readdir() else {
        return 0;
    };
    children
        .iter()
        .map(|child| tree_bytes(child, depth + 1))
        .fold(0u64, |acc, n| acc.saturating_add(n))
}

/// Форматировать число байт как `-h`: двоичные единицы, один знак после точки
/// для KiB и выше, целое `B` ниже 1 KiB — соглашение coreutils, посчитанное
/// целочисленной арифметикой, так что вывод точен и без float-форматирования.
pub(crate) fn human_size(bytes: u64) -> alloc::string::String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return alloc::format!("{} B", bytes);
    }
    let mut scale: u64 = 1024;
    let mut unit = 0usize;
    while unit + 1 < UNITS.len() && bytes / scale >= 1024 {
        scale *= 1024;
        unit += 1;
    }
    let whole = bytes / scale;
    let remainder = bytes % scale;
    // Округлить до десятых, чтобы 1023.96 MiB печаталось как 1024.0 MiB, а не 1023.9.
    let tenths = (remainder * 10 + scale / 2) / scale;
    if tenths >= 10 {
        alloc::format!("{}.0 {}", whole + 1, UNITS[unit])
    } else {
        alloc::format!("{}.{} {}", whole, tenths, UNITS[unit])
    }
}

/// `free`: использование физической памяти и кучи ядра.
///
/// Раскладка — busybox `free` (одна строка заголовка, колонки KiB), но числа
/// свои у Pagh: `Mem` берётся прямо из счётчиков кадров PMM (`total_frames` /
/// `free_frames` × 4096), так что `total` — RAM, которую аллокатор получил при
/// загрузке, `free` — что ещё доступно любому позднему пользователю PMM, а
/// `used` — всё остальное (образ ядра, таблицы страниц, кадры кучи ядра и
/// страницы живых пользовательских программ). Swap-устройства нет, поэтому эта
/// строка — константный ноль, и block cache нет, поэтому `buffers` тоже ноль.
///
/// Строка `Heap` показывает фиксированный регион кучи ядра, а не класс памяти
/// Linux: `used` — что выдал глобальный аллокатор, `free` — остаток региона. Она
/// заслуживает своей строки, потому что регион мапится при загрузке и поэтому
/// всегда входит в `Mem used`, а занята на самом деле лишь его часть —
/// разросшаяся или текущая куча видна здесь задолго до исчерпания физической
/// памяти.
pub(super) fn cmd_free(_ctx: &mut ShellCtx, args: &[&str]) {
    let mut human = false;
    for a in args {
        if *a == "-h" {
            human = true;
        } else {
            shell_println(&alloc::format!("free: unknown option: {}", a));
            shell_println("free: usage: free [-h]");
            return;
        }
    }
    const PAGE_SIZE: usize = 4096;
    let total = crate::memory::pmm::total_frames().saturating_mul(PAGE_SIZE) as u64;
    let free = crate::memory::pmm::free_frames().saturating_mul(PAGE_SIZE) as u64;
    let used = total.saturating_sub(free);
    let (heap_size, heap_used, heap_free) = crate::memory::heap::stats();
    let (heap_size, heap_used, heap_free) = (heap_size as u64, heap_used as u64, heap_free as u64);

    // Заголовок форматируется теми же ширинами, что строки, чтобы колонки не
    // разъезжались, когда в строке появляется поле.
    shell_println(&format!(
        "{:<6} {:>13} {:>12} {:>12} {:>12} {:>12}",
        "", "total", "used", "free", "shared", "buffers"
    ));
    let cell = |v: u64| -> alloc::string::String {
        if human {
            human_size(v)
        } else {
            alloc::format!("{}", v / 1024)
        }
    };
    shell_println(&format!(
        "{:<6} {:>13} {:>12} {:>12} {:>12} {:>12}",
        "Mem:",
        cell(total),
        cell(used),
        cell(free),
        cell(0),
        cell(0)
    ));
    shell_println(&format!(
        "{:<6} {:>13} {:>12} {:>12}",
        "Swap:",
        cell(0),
        cell(0),
        cell(0)
    ));
    shell_println(&format!(
        "{:<6} {:>13} {:>12} {:>12}",
        "Heap:",
        cell(heap_size),
        cell(heap_used),
        cell(heap_free)
    ));
}

/// `ps`: живая таблица compat-процессов, отрисованная из `/proc/<pid>/*`.
///
/// Заголовок и колонки имитируют `ps` (`PID PPID STAT COMMAND`), но буква
/// состояния и имя команды берутся из тех же per-pid procfs-узлов, которые
/// прочитала бы Linux-программа, так что это ещё и smoke-тест того дерева.
/// Потоки перечисляются отдельно; `COMMAND` — basename argv[0] или `[comm]`,
/// когда cmdline пуст.
pub(super) fn cmd_ps(_ctx: &mut ShellCtx, _args: &[&str]) {
    shell_println("  PID  PPID STAT COMMAND");
    // Потоки ядра (сам shell, сетевой насос, gated selftest harness) — НЕ
    // Linux compat-процессы и не имеют записи в /proc, поэтому таблица состояла
    // из голого заголовка всякий раз, когда не было живого `lxrun`-процесса —
    // что читается как «на машине вообще нет процессов», ровно наоборот от того,
    // зачем нужен `ps`. Они перечисляются из таблицы имён планировщика, в
    // скобках, как поток ядра Linux.
    let kernel_tasks = crate::task::scheduler::task_names_snapshot();
    let mut pids = crate::task::compat::compat_pid_tgid_snapshot();
    pids.sort_by_key(|(pid, _tgid)| *pid);
    let compat_pids: alloc::collections::BTreeSet<u64> = pids.iter().map(|(p, _)| *p).collect();
    for (pid, _tgid) in pids.iter().copied() {
        let mut stat_buf = [0u8; 512];
        let stat_len = crate::vfs::lookup_path(&format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|node| node.read(0, &mut stat_buf).ok())
            .unwrap_or(0);
        let stat = core::str::from_utf8(&stat_buf[..stat_len]).unwrap_or("");
        let (comm, state, ppid) =
            if let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) {
                let comm = &stat[open + 1..close];
                let mut fields = stat[close + 1..].trim_start().split(' ');
                (
                    comm,
                    fields.next().unwrap_or("?"),
                    fields.next().unwrap_or("0"),
                )
            } else {
                ("?", "?", "0")
            };
        let mut cmdline = [0u8; 256];
        let cmd_len = crate::vfs::lookup_path(&format!("/proc/{pid}/cmdline"))
            .ok()
            .and_then(|node| node.read(0, &mut cmdline).ok())
            .unwrap_or(0);
        let argv0 = cmdline[..cmd_len].split(|b| *b == 0).next().unwrap_or(&[]);
        let command = if argv0.is_empty() {
            format!("[{comm}]")
        } else {
            let text = core::str::from_utf8(argv0).unwrap_or(comm);
            String::from(text.rsplit('/').next().unwrap_or(text))
        };
        shell_println(&format!("{:>5} {:>5} {:>4} {}", pid, ppid, state, command));
    }
    for (pid, name) in kernel_tasks {
        if compat_pids.contains(&pid) {
            continue; // a thread with compat state was already rendered from /proc
        }
        // `T` — остановленное состояние, точно как в /proc/<pid>/stat; всё
        // остальное, что планировщик может исполнять, — `R` (running или runnable).
        let state = if crate::task::scheduler::is_stopped(pid) {
            "T"
        } else {
            "R"
        };
        shell_println(&format!("{:>5} {:>5} {:>4} [{}]", pid, 0, state, name));
    }
}
