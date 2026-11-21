// shell/mod.rs — интерактивный shell командной строки
// 64-битное ядро ОС x86_64 на Rust (#![no_std])

// Точки размещения подмодулей user-friendly shell. Каждый — чистый или
// тонкий I/O-модуль; `mod.rs` — единственное место, которое трогает клавиатуру,
// framebuffer, serial и VFS.
mod commands;
// Чистые подмодули открыты как `pub(crate)`, чтобы in-kernel набор свойств в
// `src/test.rs` мог прямо их проверять (свойства P21–P29 и unit-тесты
// registry/render/path/cat). Они остаются приватными для крейта — наружу
// ничего не выходит. `commands` приватный (это тонкий VFS/console I/O).
pub(crate) mod caret;
pub(crate) mod cat;
pub(crate) mod complete;
pub(crate) mod disk;
pub(crate) mod editor;
// Чистый getopt-парсер (host-свойство `getopt_args`): таблица опций команды →
// разобранные флаги/операнды, `--`, группировка `-la`, честная ошибка на
// неизвестную опцию.
pub(crate) mod getopt;
pub(crate) mod history;
// Сессионная идентичность фазы 1 (`id`/`groups`/`su`): чистый разбор
// `/etc/passwd` + `/etc/group` (`identity_db`, host-тестируемый) и VFS-слой
// хранения сессии (`identity`).
pub(crate) mod identity;
pub(crate) mod identity_db;
pub(crate) mod jobs;
pub(crate) mod keys;
// Чистый поиск программы по PATH (`lxrun <program>` и shell-fallback для голого
// имени): список каталогов/кандидатов, host-свойство `lxpath_paths`. Проба VFS и
// запуск — в `commands.rs`.
pub(crate) mod lxpath;
pub(crate) mod nano;
pub(crate) mod nano_config;
pub(crate) mod paint;
pub(crate) mod path;
pub(crate) mod shadow;
// Pure `ppkg install` progress model/renderers (host-tested in
// `host-tests/src/properties/ppkg_progress_ui.rs`) + its effectful console driver.
pub(crate) mod ppkg_progress;
mod progress;
pub(crate) mod registry;
pub(crate) mod render;
pub(crate) mod selection;
pub(crate) mod suggest;
pub(crate) mod texttools;
pub(crate) mod theme;
pub(crate) mod tree;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Напечатать строку в обе консоли (serial + framebuffer) без перевода строки.
fn print_both(s: &str) {
    crate::kprint!("{}", s);
    crate::fb_print!("{}", s);
}

/// Разрушающе стереть `n` видимых символов слева в обеих консолях.
///
/// Framebuffer-консоль поддерживает только разрушающий backspace (`0x08`,
/// сдвигает колонку на одну назад и затирает клетку); неразрушающего
/// позиционирования курсора у неё нет. Классическая тройка `backspace, space,
/// backspace` стирает один отрисованный глиф и оставляет колонку на одну левее.
/// Serial-терминалы трактуют ту же последовательность разумно.
fn erase_visible(n: usize) {
    for _ in 0..n {
        crate::kprint!("\x08 \x08");
        crate::fb_print!("\x08 \x08");
    }
}

/// Число видимых колонок, которые занимает строка на консоли.
///
/// Ввод с клавиатуры — ASCII, поэтому один `char` соответствует одной колонке.
/// Это задаёт число стираний в [`redraw_line`].
fn visible_len(s: &str) -> usize {
    s.chars().count()
}

/// Удалить активное выделение из строки, сообщив вызывающему, было ли что удалять.
///
/// Индексы строки берутся из собственной записи клавиатурного выделения, когда
/// она есть (`Ctrl+A` строит выделение из буфера, поэтому индексы точны);
/// перетаскивание мышью мапится обратно из клеток. Если ни один источник не даёт
/// диапазон, выделение сбрасывается — подсветка, которую нельзя редактировать,
/// это ложь.
fn delete_selection(editor: &mut editor::LineEditor, selection: &mut selection::Selection) -> bool {
    let Some(range) = selection.range() else {
        return false;
    };
    let cols = console_cols();
    let indices = selection
        .known_line_range()
        .or_else(|| range.line_indices(prompt_cols(), cols, editor.buffer().chars().count()));
    let Some((from, to)) = indices else {
        selection.clear();
        return false;
    };
    // Сначала очистить: `unpaint` восстанавливает клетки под подсветкой, а
    // вызывающий `redraw_line` затем печатает укороченный буфер поверх них.
    selection.clear();
    editor.replace_range(from, to);
    true
}

/// Перерисовать текущую область ввода, чтобы консоль точно отражала буфер
/// редактора (R1.6).
///
/// `shown` — сколько видимых символов ввода отрисовано после промпта. Мы
/// разрушающе стираем всю эту область, затем печатаем полный текущий буфер и
/// обновляем `shown`.
///
/// Framebuffer-консоль здесь остаётся разрушающей — строка стирается и
/// печатается заново, — но каретка больше не привязана к этому: `caret.rs`
/// рисует её в клетке логического курсора, так что правка в середине строки
/// показывает каретку там, куда попадёт следующий символ (см. `caret_cell`).
fn redraw_line(editor: &editor::LineEditor, shown: &mut usize, caret: &mut caret::Caret) {
    // Каретка — оверлей поверх заменяемого текста: сотрите её первой, иначе
    // глифы, напечатанные поверх, «запекут» её полосу в своих клетках.
    caret.erase();
    erase_visible(*shown);
    let buf = editor.buffer();
    print_both(buf);
    *shown = visible_len(buf);
}

/// Перейти на свежую строку, перерисовать промпт и напечатать текущий буфер
/// заново. Используется после вывода кандидатов Tab-дополнения (R3.4). Сбрасывает
/// `shown` к ширине напечатанного буфера.
fn reprompt_with_buffer(editor: &editor::LineEditor, shown: &mut usize, caret: &mut caret::Caret) {
    caret.erase();
    crate::kprintln!();
    crate::fb_println!();
    render::prompt(&path::cwd());
    let buf = editor.buffer();
    print_both(buf);
    *shown = visible_len(buf);
}

/// Вывести кандидатов дополнения на новой строке, затем перерисовать промпт и буфер.
fn list_candidates(
    editor: &editor::LineEditor,
    shown: &mut usize,
    candidates: &[String],
    caret: &mut caret::Caret,
) {
    caret.erase();
    crate::kprintln!();
    crate::fb_println!();
    let joined = candidates.join("  ");
    crate::kprintln!("{}", joined);
    crate::fb_println!("{}", joined);
    reprompt_with_buffer(editor, shown, caret);
}

/// Выполнить строку команды через диспетчеризацию по реестру команд.
///
/// Строка токенизируется через `split_whitespace`: `parts[0]` — имя команды,
/// `parts[1..]` — аргументы обработчика. Пустая строка или строка из пробелов —
/// no-op.
///
/// Если имя не встроенное и не содержит `/`, это может быть установленная
/// Linux-программа: диспетчер делает тот же PATH-поиск, что `lxrun`
/// (`lxpath`/`commands::find_path_program`), и запускает найденный ELF через
/// `commands::run_lxrun`. Имя с `/` остаётся путём. Ненайденная команда называет
/// токен, каталоги, где искали, и — если есть близкое совпадение — подсказку
/// «did you mean» (R7.1/R7.2).
fn execute_command(cmd: &str) {
    // Минимальная цепочка `&&`: выполнять части последовательно, останавливаясь
    // на первом наблюдаемом сигнале сбоя (неизвестная команда). Используется
    // файлом autorun (`rm -r X && lxrun ...`).
    if cmd.contains("&&") {
        for part in cmd.split("&&") {
            if !part.trim().is_empty() {
                execute_command(part);
            }
        }
        return;
    }
    let parts: Vec<&str> = cmd.trim().split_whitespace().collect();
    if parts.is_empty() {
        return;
    }

    let name = parts[0];
    let args = &parts[1..];

    // Serial-след для E2E: win-shell печатает только в пиксели, поэтому без
    // этой пары строк стенд не может отличить «команда дошла до shell'а окна и
    // вернулась» от «экран просто перерисовался». Имя команды, без аргументов и
    // вывода; boot-сессия этих строк не печатает вообще.
    let session = crate::drivers::session::current();
    let window = session != crate::drivers::session_route::BOOT_SESSION;
    if window {
        crate::drivers::serial::diag_line(&format!(
            "[DIAG] window session={} command={}",
            session, name
        ));
    }
    dispatch_command(name, args, &parts);
    if window {
        crate::drivers::serial::diag_line(&format!(
            "[DIAG] window session={} done={}",
            session, name
        ));
    }
}

/// Выполнить одну уже токенизированную строку через реестр/PATH-поиск.
///
/// Отделена от [`execute_command`] ради serial-следа: `[DIAG] … done=` должен
/// печататься и на пути PATH-fallback'а, который раньше возвращался из середины
/// диспетчера.
fn dispatch_command(name: &str, args: &[&str], parts: &[&str]) {
    match registry::lookup(name) {
        Some(spec) => {
            let mut ctx = registry::ShellCtx::new();
            (spec.handler)(&mut ctx, args);
        }
        None => {
            // Не встроенная команда. Голое имя (без `/`) может быть
            // установленной Linux-программой: shell сам делает ТОТ ЖЕ поиск по
            // PATH, что `lxrun` (`execvp`-семантика), и запускает найденный ELF
            // общим путём — без обязательного префикса `lxrun`. Встроенные
            // команды сюда не доходят (реестр выше), поэтому fallback не может
            // затереть `nano`/`id`/`df`/`ps`/`rust`.
            if lxpath::is_bare_name(name) {
                let dirs = lxpath::search_dirs(None);
                if let Some(path) = commands::find_path_program(name, &dirs) {
                    commands::run_lxrun(&path, parts);
                    return;
                }
                // Не нашли: назвать токен, сказать ГДЕ искали и не запускать
                // ничего другого (никакого «ближайшего бинаря»).
                render::error_line(&format!(
                    "{}: command not found in PATH (searched: {})",
                    name,
                    lxpath::display_dirs(&dirs)
                ));
            } else {
                // Путь — не команда: PATH-поиск к нему не применяется.
                render::error_line(&format!("Unknown command: '{}'", name));
            }
            // Подсказка ближайшей встроенной команды — отдельно от диагностики
            // поиска, чтобы опечатка в `ls` не выглядела как «нет такого файла».
            let names: Vec<&str> = registry::command_names().collect();
            if let Some(suggestion) = suggest::nearest_command(name, &names, 2) {
                render::error_line(&format!("did you mean '{}'?", suggestion));
            }
        }
    }
}

/// Обработать нажатие Tab: дополнить токен под курсором (R3.3/3.4/3.5).
///
/// Выбор цели дополнения: если в строке нет пробелов (единственный/первый
/// токен), она дополняется по именам команд; иначе текст после последнего
/// пробела трактуется как путь и дополняется по записям VFS родительского
/// каталога. Чтения VFS происходят здесь и передаются чистым функциям `complete`
/// как данные — сама логика дополнения не делает I/O.
fn handle_tab(editor: &mut editor::LineEditor, shown: &mut usize, caret: &mut caret::Caret) {
    let line = String::from(editor.buffer());

    match line.rfind(|c: char| c == ' ' || c == '\t') {
        // Нет пробелов -> первый/единственный токен: дополнить имя команды.
        None => {
            apply_completion(
                editor,
                shown,
                caret,
                "",
                "",
                complete::complete_command(&line),
            );
        }
        // Пробел есть -> дополнить хвостовой токен как путь.
        Some(idx) => {
            let before = &line[..=idx]; // command + args + separating space
            let token = &line[idx + 1..]; // the path partial being completed

            // Разбить токен на каталог (до последнего '/' включительно) и
            // дополняемый сегмент.
            let dir_part = match token.rfind('/') {
                Some(i) => &token[..=i],
                None => "",
            };

            // Разрешить родительский каталог относительно CWD и прочитать его
            // записи из VFS (как данные для чистого completer'а).
            let cwd = path::cwd();
            let parent = if dir_part.is_empty() {
                cwd.clone()
            } else {
                path::resolve(&cwd, dir_part)
            };

            let entries_owned: Vec<String> = match crate::vfs::lookup_path(&parent) {
                Ok(node) => match node.readdir() {
                    Ok(children) => children.iter().map(|c| String::from(c.name())).collect(),
                    Err(_) => Vec::new(),
                },
                Err(_) => Vec::new(),
            };
            let entry_refs: Vec<&str> = entries_owned.iter().map(|s| s.as_str()).collect();

            let comp = complete::complete_path(&cwd, token, &entry_refs);
            apply_completion(editor, shown, caret, before, dir_part, comp);
        }
    }
}

/// Применить [`complete::Completion`] к редактору.
///
/// `prefix` — текст перед дополняемым токеном (пусто для командного
/// дополнения); `dir_part` — ведущая часть пути, которую надо сохранить перед
/// дополненным сегментом (пусто для командного, где дополнение уже несёт полный
/// токен).
fn apply_completion(
    editor: &mut editor::LineEditor,
    shown: &mut usize,
    caret: &mut caret::Caret,
    prefix: &str,
    dir_part: &str,
    comp: complete::Completion,
) {
    match comp {
        // Нет совпадений: оставить строку без изменений (R3.5).
        complete::Completion::None => {}
        // Единственное совпадение: заменить токен полным дополнением (R3.3).
        complete::Completion::Single(s) => {
            let new_line = format!("{}{}{}", prefix, dir_part, s);
            editor.set_line(&new_line);
            redraw_line(editor, shown, caret);
        }
        // Несколько совпадений: расширить до longest common prefix, затем
        // перечислить кандидатов и перерисовать промпт + буфер (R3.4).
        complete::Completion::Multiple { lcp, candidates } => {
            let new_line = format!("{}{}{}", prefix, dir_part, lcp);
            editor.set_line(&new_line);
            list_candidates(editor, shown, &candidates, caret);
        }
    }
}

/// Главный цикл shell.
///
/// Один раз печатает приветственный баннер (R10.1–R10.3), затем запускает один
/// `LineEditor` на промпт. Один `Decoder` и `History` живут весь сеанс, чтобы
/// состояние Shift/расширенных префиксов и recall переживали промпты.
/// Выполнить первую строку `/mnt/etc/autorun` (если есть) как команду shell
/// перед интерактивным циклом. Ограничение 512 байт; отсутствующий файл
/// молчит.
fn run_autorun() {
    const MAX: u64 = 512;
    let node = match crate::vfs::lookup_path("/mnt/etc/autorun") {
        Ok(n) => n,
        Err(_) => return,
    };
    let mut buf = alloc::vec![0u8; MAX as usize];
    match node.read(0, &mut buf) {
        Ok(n) if n > 0 => {
            for raw in core::str::from_utf8(&buf[..n]).unwrap_or("").lines() {
                let line: String = raw.trim().into();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                crate::kprintln!("[autorun] {}", line);
                execute_command(&line);
            }
        }
        _ => {}
    }
}

pub fn shell_main() -> ! {
    shell_main_session(crate::drivers::session_route::BOOT_SESSION)
}

/// Главный цикл shell указанной консольной сессии.
///
/// `session == 0` — boot/shell-консоль на Limine-scanout: поведение ровно прежнее
/// (serial-баннер, `tty::init_shell`, autorun). `session >= 1` — терминальное окно
/// desktop'а: та же самая машина REPL (decoder/history/editor/caret/selection/
/// Tab/scrollback), но экземпляр консоли, кольцо ввода и `cwd` — этой сессии
/// ([`crate::drivers::session`], [`crate::drivers::framebuffer::writer_for`],
/// [`path`]). Команды не меняются: они по-прежнему зовут `fb_print!`/`kprint!`,
/// а маршрут по задаче решает, чей это экран.
///
/// Чего у окна СОЗНАТЕЛЬНО нет (честные границы, см. `docs/terminal-session.md`):
/// serial-эха баннера (окно — не лог) и autorun (файл — boot-сценарий, второй
/// прогон запустил бы те же команды). Управляющий терминал у окна СВОЙ
/// (`tty::init_shell_for(session, pid)`): foreground-группа, `TOSTOP`, `winsize`
/// и таблица заданий — на сессию, поэтому `Ctrl+Z`/`jobs`/`bg`/`fg` в окне не
/// трогают boot-shell и наоборот.
pub(crate) fn shell_main_session(session: usize) -> ! {
    let boot = session == crate::drivers::session_route::BOOT_SESSION;
    // Консоль сессии — свой управляющий терминал, и shell — его владелец по
    // умолчанию: записать собственную группу процессов сейчас, чтобы `lxrun`
    // мог передать терминал ребёнку и вернуть обратно (контроль доступа
    // терминала сравнивает pgid процесса с этой группой). У окна состояние
    // (foreground, TOSTOP, winsize) отдельное от boot-консоли.
    crate::drivers::tty::init_shell_for(session, crate::task::scheduler::current_pid());

    // Приветственный баннер — имя ОС + подсказка `help`, в обеих консолях, до
    // первого промпта и без заметной задержки (R10.1, R10.2, R10.3).
    if boot {
        crate::kprintln!();
        crate::kprintln!("========================================");
        crate::kprintln!("   Welcome to pagh OS Shell!");
        crate::kprintln!("========================================");
        crate::kprintln!("Type 'help' for available commands");
        crate::kprintln!("input: {}", crate::drivers::input_summary());
        if !crate::drivers::any_keyboard() {
            crate::kprintln!("warning: no keyboard detected (USB device count above, PS/2 absent)");
        }
        crate::kprintln!();
    }

    // Баннер окна — только в его собственную поверхность: serial — общий лог
    // ядра, и второй экземпляр строки приветствия в нём был бы шумом, который
    // контракт прямо запрещает.
    crate::fb_println!();
    crate::fb_println!("========================================");
    crate::fb_println!("   Welcome to pagh OS Shell!");
    crate::fb_println!("========================================");
    crate::fb_println!("Type 'help' for available commands");
    if boot {
        crate::fb_println!("input: {}", crate::drivers::input_summary());
    } else {
        crate::fb_println!("terminal session {} (kernel shell)", session);
    }
    crate::fb_println!();

    // Опциональный одноразовый autorun: если `/mnt/etc/autorun` существует, его
    // первая строка исполняется как набранная команда (бедняцкий rc.local —
    // удобно для воспроизводимых тестовых команд вроде `lxrun ... git clone ...`).
    // Удалите файл, чтобы отключить.
    if boot {
        run_autorun();
    }

    // Состояние на весь сеанс: decoder хранит состояние Shift/0xE0 между
    // промптами; history накапливает вызванные строки.
    let mut decoder = keys::Decoder::new();
    let mut history = history::History::new();

    // Поставить курсор мыши в центр экрана и подготовить status bar. Оверлей
    // ниже перерисовывает его на каждом пробуждении.
    let (fw, fh) = crate::drivers::framebuffer::dimensions();
    if fw != 0 {
        crate::drivers::cursor::move_to(fw / 2, fh / 2);
    }

    // Протолкнуть сохранённую тему в драйверы до первого промпта. Без этого
    // палитра доходит до экрана только когда её сменит команда, и консоль
    // остаётся на дефолтах драйвера (чёрный фон, белый текст); обои тогда
    // закрашивают область *вне* текстовой сетки, потому что каждая клетка сетки
    // перерисовывает свой фон старым цветом.
    theme::apply();

    // Одна каретка на весь сеанс: она должна стирать себя в той клетке, где
    // была нарисована, а эту клетку не восстановить из буфера.
    // Каретка отслеживается и скрытой: `Caret` помнит стёртую клетку, поэтому
    // повторное включение рисует её там, где курсор на самом деле, а не в
    // устаревшей позиции.
    let mut caret = caret::Caret::new();
    // Одно выделение на сеанс (перетаскивание живёт дольше одного прохода цикла)
    // и буфер обмена, который оно питает. Буфер намеренно сеансный: заполняется
    // копированием текста с экрана и потребляется Ctrl+Y / Ctrl+X.
    let mut selection = selection::Selection::new();
    let mut clipboard = String::new();
    let mut prev_left = false;
    // Последний виденный счётчик колеса, чтобы poll мог превратить накопительный
    // счётчик PS/2 в дельту текущего прохода (shell просыпается по `mouse.seq`,
    // который пакет колеса увеличивает как любой другой).
    let mut last_wheel = crate::drivers::ps2_mouse::poll().wheel;
    let mut status_cache: Option<(alloc::string::String, alloc::string::String)> = None;

    loop {
        // Окно, чей последний fd закрыт (или команда `exit`), выходит здесь —
        // на границе цикла, когда задача не держит чужих ресурсов.
        crate::drivers::session::check_reap(session);

        // Смена темы инвалидирует всё нарисованное: *модель* хранит цвет на
        // клетку (`grid_fg`), и это цвет, актуальный в момент записи клетки.
        // Перерисовать пиксели недостаточно — клетки надо напечатать заново через
        // новую палитру, поэтому промпт перерисовывается на свежей строке, а текст
        // статуса забывается, чтобы полоса тоже перерисовалась.
        if theme::take_force_redraw() {
            crate::fb_println!();
            status_cache = None;
        }

        // Промпт с учётом CWD, например `pagh:/> ` (R5.1–R5.3).
        render::prompt(&path::cwd());
        // Отбросить пакеты колеса, сгенерированные, пока клавиатурой/экраном
        // владела команда (`paint`, `nano`, полноэкранная программа): они не для
        // консольного вида, и воспроизведение накопленной дельты здесь прокрутило
        // бы свежий промпт.
        last_wheel = crate::drivers::ps2_mouse::poll().wheel;

        // Свежий редактор на каждый промпт; `shown` отслеживает ширину отрисованного ввода.
        let mut editor = editor::LineEditor::new();
        let mut shown: usize = 0;
        caret.reset();

        // Читать клавиши до Enter.
        // Учёт пробуждений для пропуска idle ниже: sentinel'ы заставляют первый
        // проход выполниться (`mouse.seq` никогда не `u64::MAX`).
        let mut last_mouse_seq = u64::MAX;
        let mut last_secs = u64::MAX;
        let mut last_blink = caret::Caret::blink_on(crate::task::scheduler::ticks());
        loop {
            // Idle на halt, пока прерывание не доставит событие (R11.5).
            crate::arch::cpu::halt();

            // Если клавиатурой владел userspace-терминал (`/dev/vtinput`), а его
            // процесс уже завершился, вернуть прямой PS/2-путь: иначе физические
            // клавиши остались бы «съеденными» закрытым окном. Проверка дешёвая
            // (атомик) и троттлится внутри; пока владелец жив — no-op.
            //
            // Только boot-сессия: защёлка владельца — про физическую клавиатуру,
            // а окно ею не владеет (его ввод идёт через `/dev/vtinput1`).
            if boot {
                crate::drivers::vtinput::reap_dead_client();
            }
            // Окно просыпается на каждом тике и здесь же замечает закрытие:
            // иначе оно осталось бы жить после исчезновения клиента.
            crate::drivers::session::check_reap(session);

            // Тик таймера будит shell каждую миллисекунду, но работа есть,
            // только если изменилось что-то, что может менять экран: пришёл
            // сканкод, сдвинулся указатель (пакет кнопки увеличивает тот же
            // `seq`), сменилась секунда часов status strip или переключилась фаза
            // мигания каретки. Пропуск цикла оверлея в остальное время держит
            // idle-shell простаивающим: каждый проход кончается flush кадра, а он
            // копирует всю shadow surface в устройство.
            let mouse = crate::drivers::ps2_mouse::poll();
            let ticks = crate::task::scheduler::ticks();
            let secs = ticks / crate::arch::x86_64::apic::TICK_HZ;
            let blink = caret::Caret::blink_on(ticks);
            let key_pending = crate::drivers::ps2_kbd::has_scancode();
            if !key_pending
                && mouse.seq == last_mouse_seq
                && secs == last_secs
                && blink == last_blink
            {
                continue;
            }
            last_mouse_seq = mouse.seq;
            last_secs = secs;
            last_blink = blink;

            // Живой оверлей: убрать стрелку на этот проход, перерисовать status
            // bar, затем до конца обработать ВСЕ ожидающие сканкоды, пока она
            // скрыта (чтобы вывод глифов не затирал её сохранённый фон).
            // Восстановленный фон *откладывается* в damage этого кадра, а не
            // пушится сразу: пуш здесь убрал бы стрелку с устройства на весь
            // проход — ровно тот мерцающий эффект, когда двигаешь мышь над
            // простаивающим shell.
            crate::drivers::cursor::hide_deferred();

            // Колесо мыши: один щелчок прокручивает консоль на три текстовые
            // строки. Вид — перерисовка текстовой области из модели scrollback,
            // поэтому каретка и выделение сначала сбрасываются — они описывают
            // живую сетку и иначе нарисовали бы полосу или подсветку посреди
            // истории.
            let wheel_delta = mouse.wheel.wrapping_sub(last_wheel);
            last_wheel = mouse.wheel;
            if wheel_delta != 0 {
                // PS/2 IntelliMouse сообщает «от пользователя» отрицательной
                // дельтой; консольный вид считает положительное более старым
                // текстом. Сменить знак один раз здесь, чтобы соглашение жило в
                // одном месте, а не в каждом вызывающем.
                let lines = -wheel_delta.clamp(-16, 16) * 3;
                scroll_console(lines, "wheel", &mut caret, &mut selection);
            }

            // Выделение мышью: нажатие задаёт якорь, перетаскивание расширяет,
            // отпускание копирует текст в буфер. Координаты клеток — пиксели
            // указателя, делённые на реальный размер клетки framebuffer, а
            // `Selection` отсекает всё левее промпта, так что перетаскивание,
            // начатое на промпте, всё равно выделяет пригодный текст строки.
            let pressed = mouse.left && !prev_left;
            let released = !mouse.left && prev_left;
            prev_left = mouse.left;
            // Клик — это ввод: оставлять пользователя в прокрученной истории,
            // пока перетаскивание подсвечивало бы невидимые клетки, — неправильно.
            // Клик означает «назад к живому экрану», как в любом терминале.
            if pressed && console_scrolled() {
                reset_console_view(&mut caret, &mut selection);
            }
            if pressed || (mouse.left && selection.is_active()) {
                // Pixel → cell через реальную сетку: указатель может стоять на
                // полосе статуса (ниже текста) или за правым краем, и клетка
                // обязана остаться внутри модели. Без зажима диапазон
                // выделения выходил за сетку, `read_row` подменял такую строку
                // последней, и копирование дублировало её текст.
                let (grid_cols, grid_rows) = crate::drivers::framebuffer::console().text_grid();
                let (col, row) =
                    crate::drivers::console_layout::cell_at(mouse.x, mouse.y, grid_cols, grid_rows);
                let cell = caret::Cell { col, row };
                if pressed {
                    selection.clear();
                    selection.begin(cell);
                } else if selection.extend(cell) {
                    selection.paint(console_cols());
                }
            } else if released {
                if let Some(range) = selection.finish() {
                    // Копирование молчит намеренно: это жест мышью, а печать в
                    // консоль вытолкнула бы с экрана тот самый текст, который
                    // пользователь только что выделил.
                    let text = range.text(console_cols());
                    if !text.is_empty() {
                        clipboard = text;
                    }
                    // Подсветка остаётся после отпускания: она показывает, что
                    // взято, и именно с ней работают Ctrl+X / Ctrl+Y.
                    selection.paint(console_cols());
                }
            }
            // ВНИМАНИЕ: каретка здесь НЕ стирается. Её стирают функции,
            // перерисовывающие строку (`redraw_line`/`list_candidates`), где это
            // действительно важно. Стирание на каждом проходе цикла сделало бы её
            // почти невидимой: `halt()` возвращается на каждом тике таймера, так
            // что стирание шло бы за рисованием через микросекунды, и каретка
            // отсутствовала бы почти весь период мигания.
            //
            // Status strip перерисовывается только при реальном изменении
            // текста: это единственное на экране, что меняется само (часы,
            // указатель), а пересборка на каждый тик была главной ценой shell.
            let mut status_dirty = false;
            if theme::snapshot().status_bar {
                if shell_status_text(&mouse, &mut status_cache) {
                    if let Some((left, right)) = status_cache.as_ref() {
                        crate::drivers::framebuffer::draw_status_bar(left, right);
                        status_dirty = true;
                    }
                }
            } else if status_cache.is_some() {
                // Только что выключили: стереть полосу один раз и запомнить, что
                // кэш больше не описывает экран, чтобы повторное включение
                // перерисовало, а не считало устаревший текст на месте.
                crate::drivers::framebuffer::with(|fb| {
                    let (w, h) = fb.dimensions();
                    fb.fill_rect(
                        0,
                        h - crate::drivers::framebuffer::STATUS_BAR_HEIGHT,
                        w,
                        crate::drivers::framebuffer::STATUS_BAR_HEIGHT,
                        0,
                    );
                });
                crate::drivers::framebuffer::flush();
                status_cache = None;
            }

            let mut entered = false;
            while let Some(scancode) = try_read_scancode() {
                // Неподдерживаемые / mid-prefix / break-коды декодируются в None
                // и игнорируются без паники (R11.2).
                let event = match decoder.feed(scancode) {
                    Some(e) => e,
                    None => continue,
                };

                // PageUp/PageDown — клавиши прокрутки консоли; любая другая
                // клавиша — ввод, а ввод означает, что пользователь закончил
                // читать историю: вернуться к живому экрану до применения клавиши,
                // чтобы набранный текст и каретка попали на то, что реально на экране.
                let scroll_key = matches!(event, keys::KeyEvent::PageUp | keys::KeyEvent::PageDown);
                if !scroll_key && console_scrolled() {
                    reset_console_view(&mut caret, &mut selection);
                }
                match event {
                    // Видимое выделение — реальное состояние ввода, а не декор:
                    // набор или удаление заменяет его, как в любом текстовом поле.
                    // Комбинация `Ctrl+A` + Delete — то, за чем тянутся
                    // пользователи, чтобы очистить строку, а раньше она ничего не
                    // делала, потому что эти ветки игнорировали выделение и
                    // действовали по голому курсору.
                    keys::KeyEvent::Char(c) => {
                        delete_selection(&mut editor, &mut selection);
                        editor.insert(c);
                        redraw_line(&editor, &mut shown, &mut caret);
                    }
                    keys::KeyEvent::Backspace => {
                        if !delete_selection(&mut editor, &mut selection) {
                            editor.delete_back();
                        }
                        redraw_line(&editor, &mut shown, &mut caret);
                    }
                    keys::KeyEvent::Delete => {
                        if !delete_selection(&mut editor, &mut selection) {
                            editor.delete_fwd();
                        }
                        redraw_line(&editor, &mut shown, &mut caret);
                    }
                    // Перемещение курсора меняет только логический курсор;
                    // каретку в его клетку ставит оверлей ниже, так что позиция в
                    // середине строки теперь — то, что пользователь действительно видит.
                    //
                    // Перемещение также сбрасывает выделение, как в любом
                    // текстовом поле: подсветка, переживающая сдвиг каретки, всё
                    // равно «редактируется» следующим Delete/Backspace, поэтому
                    // `Ctrl+A`, Left, Delete раньше стирали всю строку — выделение
                    // было невидимым состоянием, которое стрелки не трогали.
                    keys::KeyEvent::Left => {
                        selection.clear();
                        editor.move_left();
                    }
                    keys::KeyEvent::Right => {
                        selection.clear();
                        editor.move_right();
                    }
                    keys::KeyEvent::Home => {
                        selection.clear();
                        editor.move_home();
                    }
                    keys::KeyEvent::End => {
                        selection.clear();
                        editor.move_end();
                    }
                    // Вверх: вызвать более старую запись, отложив живую строку (R2.2).
                    keys::KeyEvent::Up => {
                        if let Some(line) = history.recall_prev(editor.buffer()) {
                            editor.set_line(line);
                            redraw_line(&editor, &mut shown, &mut caret);
                        }
                    }
                    // Вниз: вызвать более новую запись или восстановить
                    // отложенную живую строку при выходе за самую новую (R2.3).
                    keys::KeyEvent::Down => {
                        match history.recall_next() {
                            Some(line) => editor.set_line(line),
                            None => editor.set_line(history.saved_line()),
                        }
                        redraw_line(&editor, &mut shown, &mut caret);
                    }
                    keys::KeyEvent::Tab => {
                        handle_tab(&mut editor, &mut shown, &mut caret);
                    }
                    // Enter: завершить строку. Непустые строки записываются
                    // (с дедупом) и диспетчеризуются; навигация всегда сбрасывается.
                    // Ctrl+X вырезает выделенный текст из строки, Ctrl+Y вставляет
                    // буфер обратно. Вместе они делают копирование наблюдаемым:
                    // скопированный текст — тот, что вернётся, и ничего больше.
                    keys::KeyEvent::Ctrl('a') => {
                        // Выделить всю строку ввода. Диапазон строится из ширины
                        // промпта, так что это заодно путь к выделению без мыши:
                        // набор, Ctrl+A, Ctrl+X, Ctrl+V упражняет буфер обмена без
                        // мыши в тестовом стенде.
                        let line_len = editor.buffer().chars().count();
                        if line_len > 0 {
                            if let Some(cells) = line_cells(&editor) {
                                let cols = console_cols();
                                selection.clear();
                                selection.set_range(
                                    selection::CellRange::new(cells.first, cells.last),
                                    Some((0, line_len)),
                                );
                                selection.paint(cols);
                            }
                        }
                    }
                    keys::KeyEvent::Ctrl('x') => {
                        if let Some(range) = selection.range() {
                            let cols = console_cols();
                            // Предпочесть точные индексы, которые уже знает
                            // клавиатурное выделение; иначе вывести их из клеток
                            // для перетаскивания мышью.
                            let idx = selection.known_line_range().or_else(|| {
                                range.line_indices(
                                    prompt_cols(),
                                    cols,
                                    editor.buffer().chars().count(),
                                )
                            });
                            if let Some((from, to)) = idx {
                                let text = range.text(cols);
                                if !text.is_empty() {
                                    clipboard = text;
                                }
                                selection.clear();
                                editor.replace_range(from, to);
                                redraw_line(&editor, &mut shown, &mut caret);
                            }
                        }
                    }
                    keys::KeyEvent::Ctrl('y') | keys::KeyEvent::Ctrl('v') => {
                        if !clipboard.is_empty() {
                            // Многострочный буфер вставляется со свёрткой переводов
                            // строк в пробелы: редактор строки держит одну строку, а
                            // молча выбросить остальное — потерять текст.
                            let text = clipboard.replace('\n', " ");
                            for ch in text.chars() {
                                editor.insert(ch);
                            }
                            redraw_line(&editor, &mut shown, &mut caret);
                        }
                    }
                    keys::KeyEvent::Ctrl(_) | keys::KeyEvent::Escape => {}
                    // Постраничная прокрутка терминала: один экран минус строка
                    // перекрытия — тот же шаг, что даёт щелчок колеса, умноженный на
                    // три, для мыши.
                    keys::KeyEvent::PageUp => {
                        let rows = crate::drivers::framebuffer::console().text_grid().1;
                        scroll_console(
                            rows.saturating_sub(1).max(1) as i32,
                            "pageup",
                            &mut caret,
                            &mut selection,
                        );
                    }
                    keys::KeyEvent::PageDown => {
                        let rows = crate::drivers::framebuffer::console().text_grid().1;
                        scroll_console(
                            -(rows.saturating_sub(1).max(1) as i32),
                            "pagedown",
                            &mut caret,
                            &mut selection,
                        );
                    }
                    keys::KeyEvent::Enter => {
                        crate::kprintln!();
                        crate::fb_println!();
                        if editor.is_empty() {
                            history.reset_nav();
                        } else {
                            history.push(editor.buffer());
                            // Ожидающий вопрос `disk` владеет этой строкой: его
                            // ответ ("2" или имя устройства) — не команда, и прогон
                            // через диспетчер напечатал бы "Unknown command: 2" и
                            // оставил мастер ждать.
                            if disk::pending() {
                                disk::feed_line(editor.buffer());
                            } else {
                                execute_command(editor.buffer());
                            }
                        }
                        entered = true;
                        break;
                    }
                }
            }

            if entered {
                // Пере-промпт на следующем проходе внешнего цикла; курсор
                // перерисует оверлей следующего пробуждения, чтобы свежий промпт не затёрся.
                caret.erase();
                break;
            }

            // Поставить каретку в клетку логического курсора, мигая по
            // millisecond-тику планировщика. Консоль здесь ровно в конце буфера
            // (`redraw_line` оставляет её там), так что клетка находится проходом
            // назад по символам после курсора.
            if console_scrolled() {
                // Вид перерисовал текстовую область из истории; живая каретка
                // принадлежит живой сетке и не должна рисоваться в виде.
                caret.reset();
            } else if let Some(cell) = caret_cell(&editor) {
                // `Caret::place` вызывается и при выключенной каретке — с
                // `on = false`, — чтобы стереть её там, где она сейчас, а не
                // оставить полосу на экране после переключения.
                let blink = theme::snapshot().caret
                    && caret::Caret::blink_on(crate::task::scheduler::ticks());
                caret.place(cell, blink);
            }
            // Сначала запушить нарисованное за проход, затем нарисовать стрелку
            // поверх готового кадра. Консоль в RAM, так что оба flush малы:
            // кадровый несёт только dirty-прямоугольник, а `move_to` пушит только
            // стрелку 12x19. Такой порядок значит, что устройство показывает
            // старый кадр со стрелкой, пока новый кадр его не заменит, а стрелка
            // сразу возвращается — без разрыва и никогда двух стрелок одновременно.
            let _ = status_dirty;
            // Нарисовать стрелку в тот же shadow-кадр и запушить оба сразу:
            // восстановленный фон старой стрелки, изменения прохода и новая
            // стрелка доходят до устройства одним flush, так что кадра без
            // указателя не бывает.
            crate::drivers::cursor::place_deferred(mouse.x, mouse.y);
            crate::drivers::framebuffer::flush();
        }
    }
}

/// Видимая ширина промпта (`pagh:{cwd}> `) в клетках. Промпт разделяет клетки
/// консоли и индексы строки, поэтому выделению нужно знать, где начинается
/// редактируемый текст.
fn prompt_cols() -> usize {
    let cwd = path::cwd();
    "pagh:".chars().count() + cwd.chars().count() + "> ".chars().count()
}

/// Ширина консоли в клетках или 0, если framebuffer не инициализировался.
fn console_cols() -> usize {
    crate::drivers::framebuffer::console().text_grid().0
}

/// Строки консоли, которые занимает текущая строка ввода, `(first_row, last_row)`,
/// с колонками первого и последнего символа.
///
/// Строка печатается после промпта, поэтому не начинается с grid row 0:
/// boot-вывод и прошлые команды прокрутили консоль, да и промпт может
/// переноситься. Всё, что трогает клетки строки — каретка, выделение `Ctrl+A` и
/// отображение обратно в индексы, — должно сходиться на этих строках, поэтому они
/// вычисляются в одном месте. Вывод «строка начинается с row 0» — баг, который
/// это предотвращает: выделение, построенное на row 0, скопирует случайный
/// boot-текст.
fn line_cells(editor: &editor::LineEditor) -> Option<caret::CellRangeCells> {
    let (cols, _rows) = crate::drivers::framebuffer::console().text_grid();
    let (ccol, crow) = crate::drivers::framebuffer::console().cursor_cell()?;
    if cols == 0 {
        return None;
    }
    // Консоль ровно за последним напечатанным символом строки.
    let end = crow * cols + ccol;
    let start = end.checked_sub(editor.buffer().chars().count())?;
    let first_row = start / cols;
    // `Ctrl+A` выделяет весь буфер, поэтому диапазон покрывает целые строки: от
    // первого символа строки до последней напечатанной клетки консоли.
    Some(caret::CellRangeCells {
        first: caret::Cell {
            col: start % cols,
            row: first_row,
        },
        last: caret::Cell {
            col: ccol.saturating_sub(1).max(start % cols),
            row: crow,
        },
    })
}

/// Клетка, в которой должна быть каретка для текущей строки.
///
/// Консоль ровно в конце напечатанного буфера (`redraw_line` оставляет её там),
/// поэтому клетка курсора плюс число символов после логического курсора
/// фиксируют каретку. Возвращает `None`, если framebuffer не инициализировался
/// (serial-only boot) — тогда каретка не рисуется, и консоль ведёт себя как
/// раньше.
fn caret_cell(editor: &editor::LineEditor) -> Option<caret::Cell> {
    let (cols, _rows) = crate::drivers::framebuffer::console().text_grid();
    let console_cell = crate::drivers::framebuffer::console().cursor_cell()?;
    caret::Caret::cell_for(console_cell, editor, cols)
}

/// Попробовать прочитать сканкод клавиатуры **текущей консольной сессии**.
///
/// Один helper на boot-shell, `nano`, `paint` и `read_secret` (ввод пароля):
/// `ps2_kbd::take_scancode` маршрутизирует по задаче — физическое PS/2-кольцо для
/// boot-сессии, кольцо `/dev/vtinput1` для окна. Поэтому редакторы не знают о
/// двух источниках и работают в обеих сессиях.
pub(super) fn try_read_scancode() -> Option<u8> {
    crate::drivers::ps2_kbd::take_scancode()
}

/// Показывает ли консоль историю вместо живого низа.
fn console_scrolled() -> bool {
    crate::drivers::framebuffer::scroll_state().0 > 0
}

/// Прокрутить вид консоли на `delta` строк (положительное = старее) и
/// объявить об этом в serial-логе.
///
/// Каретка и выделение описывают *живую* сетку; вид истории рисуется из модели
/// scrollback, поэтому оба сбрасываются перед перерисовкой. Serial-строка
/// `[DIAG]` — доказательство для E2E-стенда, что событие колеса/клавиши
/// действительно сдвинуло вид (и насколько назад), и намеренно только serial:
/// печать через консоль добавляла бы строку на каждый щелчок колеса.
fn scroll_console(
    delta: i32,
    source: &str,
    caret: &mut caret::Caret,
    selection: &mut selection::Selection,
) {
    // Сначала перерисовать: это затирает полосу каретки или подсветку выделения
    // поверх текста, и только потом их модели сбрасываются. Очистка до no-op
    // прокрутки забыла бы выделение, оставив его пиксели на экране.
    if !crate::drivers::framebuffer::scroll_view(delta) {
        return;
    }
    caret.reset();
    selection.clear();
    let (view, history) = crate::drivers::framebuffer::scroll_state();
    let top = crate::drivers::framebuffer::scroll_top_text().unwrap_or_default();
    crate::kprintln!(
        "[DIAG] console scroll {} view={} history={} top=\"{}\"",
        source,
        view,
        history,
        top
    );
}

/// Вернуть консоль к живому низу (клик, Escape или набор).
///
/// На прозрачной консоли (активные обои) вид истории рисовал непрозрачные
/// клетки, поэтому обои надо вернуть до рисования живой сетки поверх них; этот
/// перерис владеет модуль темы.
fn reset_console_view(caret: &mut caret::Caret, selection: &mut selection::Selection) {
    caret.reset();
    selection.clear();
    if !crate::drivers::framebuffer::scroll_reset() {
        return;
    }
    if theme::snapshot().wallpaper != theme::Wallpaper::None {
        theme::repaint();
    }
    crate::kprintln!("[DIAG] console scroll reset");
}

/// Текст status bar для текущего состояния или `None`, если он повторил бы
/// строку, уже висящую на экране.
///
/// Полоса упоминает позицию мыши, поэтому перерисовка на каждом проходе означала
/// пересборку глифов ~1000 раз в секунду (на каждый тик таймера) для строки,
/// меняющейся только когда указатель сдвинулся на клетку или прошла секунда.
/// Эта перерисовка и делала shell вялым: она на его собственном потоке, между
/// нажатиями. Кэширование текста превращает её в сравнение на проход.
fn shell_status_text(
    mouse: &crate::drivers::ps2_mouse::MouseState,
    cached: &mut Option<(alloc::string::String, alloc::string::String)>,
) -> bool {
    // Две половины живут в `theme`, потому что смена темы должна перерисовывать
    // саму полосу (её цвета меняются без изменения текста), а вторая копия этой
    // format-строки разошлась бы с той, что рисует переключатель.
    let next = (theme::status_left(), theme::status_right(mouse));
    if cached.as_ref() == Some(&next) {
        return false;
    }
    *cached = Some(next);
    true
}
