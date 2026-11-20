//! Отдельная интерактивная shell-сессия: процесс-потомок и два pipe.
//!
//! Окно терминала раньше зеркалило boot-консоль (`/dev/vtconsole0`): пользователь видел
//! загрузочный лог, а нажатия уходили в `/dev/vtinput` — то есть в stdin
//! foreground-процесса (`lxrun`/desktop), потому что kernel-shell занят ожиданием
//! `lxrun`. Эта сессия разрывает связь с консолью: окно владеет **собственным**
//! shell-процессом, у которого stdin и stdout — pipe'ы. Вывод потомка рендерится в
//! сетку окна, клавиатура окна пишется в его stdin; boot-VT при этом не читается и
//! не затронут.
//!
//! Почему потомок, а не kernel-shell: shell ядра — задача, привязанная к консоли
//! (`fb_println!`/`kprintln!` + PS/2-кольцо), и её нельзя направить в pipe без
//! переписывания всего консольного UI. Процесс через `posix_spawn`/`vfork` + `pipe2`
//! ядро уже поддерживает (этим путём desktop запускает приложения), поэтому
//! минимальный шаг — именно он. Полноценный PTY/termios/job-control — следующий этап;
//! здесь нет управляющего терминала, размеров окна и сигналов: только два потока
//! байтов.

use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread::{self, JoinHandle};

/// Сколько байт читается из pipe потомка за один `read`.
const READ_CHUNK: usize = 4096;

/// Запущенная shell-сессия: потомок, его stdin и поток чтения stdout/stderr.
///
/// Поток чтения нужен потому, что читать блокирующий pipe из UI-цикла нельзя:
/// окно обязано перерисовываться и опрашивать сокет композитора, пока shell
/// молчит. Поток складывает прочитанные куски в канал, UI забирает их
/// неблокирующе раз в кадр.
pub struct ShellSession {
    child: Child,
    stdin: Option<ChildStdin>,
    output: Receiver<Vec<u8>>,
    readers: Vec<JoinHandle<()>>,
    /// Все pipe'ы закрыты: читать больше нечего (потомок мог ещё не выйти).
    output_closed: bool,
}

impl ShellSession {
    /// Запустить `command` с pipe'ами stdin/stdout/stderr.
    ///
    /// stderr сливается в тот же канал, что stdout: у сессии одно окно и нет
    /// отдельной панели для ошибок, поэтому оба потока обязаны идти в сетку в
    /// порядке, в котором shell их написал.
    pub fn spawn(mut command: Command) -> io::Result<ShellSession> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "child stdout missing"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "child stderr missing"))?;

        let (tx, output) = mpsc::channel();
        let mut readers = Vec::with_capacity(2);
        readers.push(spawn_reader(stdout, tx.clone()));
        readers.push(spawn_reader(stderr, tx));
        Ok(ShellSession {
            child,
            stdin,
            output,
            readers,
            output_closed: false,
        })
    }

    /// Записать байты клавиатуры в stdin потомка.
    ///
    /// `BrokenPipe` — не ошибка сессии: потомок мог выйти между кадром и
    /// нажатием; окно уже покажет это по [`ShellSession::try_wait`].
    pub fn write_input(&mut self, bytes: &[u8]) -> io::Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Ok(());
        };
        match stdin.write_all(bytes).and_then(|()| stdin.flush()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                self.stdin = None;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Забрать все готовые байты вывода, не блокируясь.
    ///
    /// `None` — новых данных нет; пустой вектор означает «канал закрыт», и
    /// вызывающий больше не должен ждать вывода.
    pub fn try_read(&mut self) -> Option<Vec<u8>> {
        if self.output_closed {
            return None;
        }
        let mut collected = Vec::new();
        loop {
            match self.output.try_recv() {
                Ok(chunk) => collected.extend_from_slice(&chunk),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.output_closed = true;
                    break;
                }
            }
        }
        if collected.is_empty() && !self.output_closed {
            None
        } else {
            Some(collected)
        }
    }

    /// Закрыт ли вывод потомка (оба pipe'а дочитаны).
    pub fn output_closed(&self) -> bool {
        self.output_closed
    }

    /// Статус потомка, если он уже завершился.
    pub fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    /// Завершить потомка и дождаться потоков чтения.
    ///
    /// Закрытие stdin даёт нормальному shell'у EOF, но окно закрывается и по
    /// кнопке композитора — тогда потомку нужен `SIGKILL`, иначе он останется
    /// жить без окна. Чтение после смерти процесса возвращает EOF, поэтому
    /// join не может висеть на живом pipe.
    pub fn shutdown(&mut self) {
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
        self.output_closed = true;
    }
}

impl Drop for ShellSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Поток, перекладывающий байты одного pipe'а в общий канал.
fn spawn_reader(mut pipe: impl Read + Send + 'static, tx: mpsc::Sender<Vec<u8>>) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut buf = [0u8; READ_CHUNK];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    })
}

/// Команда запуска собственного shell-потомка (этот же бинарь, режим
/// `--shell-child`).
///
/// Путь берётся из `argv[0]`, а не из `/proc/self/exe`: desktop запускает
/// приложение абсолютным путём, и argv[0] уже проверен execve'ом; readlink
/// `/proc/self/exe` — лишняя зависимость от procfs. Оба варианта без `'/'`
/// отвергаются, чтобы не искать «pagh-terminal» в `$PATH` (в pagh его нет).
pub fn self_shell_command(argv0: Option<&str>) -> io::Result<Command> {
    let candidate = argv0
        .filter(|path| path.contains('/'))
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "cannot locate the terminal binary")
        })?;
    let mut command = Command::new(candidate);
    command.arg("--shell-child");
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Собрать вывод до появления `needle` (с дедлайном, чтобы тест не висел).
    fn collect_until(session: &mut ShellSession, needle: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        let mut text = Vec::new();
        while Instant::now() < deadline {
            if let Some(bytes) = session.try_read() {
                text.extend_from_slice(&bytes);
                if String::from_utf8_lossy(&text).contains(needle) {
                    return String::from_utf8_lossy(&text).into_owned();
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "shell session produced no {needle:?} within {timeout:?}: {:?}",
            String::from_utf8_lossy(&text)
        );
    }

    /// Основной контракт: пишем в stdin — читаем ответ из stdout того же
    /// процесса. Это ровно то, что делает окно с клавиатурой и экраном.
    #[test]
    fn pipe_round_trip() {
        let mut command = Command::new("sh");
        command.arg("-c").arg(
            "printf 'ready\\n'; while IFS= read -r line; do printf 'got:%s\\n' \"$line\"; done",
        );
        let mut session = ShellSession::spawn(command).expect("spawn sh");
        collect_until(&mut session, "ready", Duration::from_secs(5));

        session.write_input(b"help\n").expect("write input");
        let text = collect_until(&mut session, "got:help", Duration::from_secs(5));
        assert!(text.contains("got:help"), "unexpected output: {text:?}");

        session.write_input(b"ls -la\n").expect("write input");
        let text = collect_until(&mut session, "got:ls -la", Duration::from_secs(5));
        assert!(text.contains("got:ls -la"), "unexpected output: {text:?}");
    }

    /// Закрытие сессии убивает потомка, а не оставляет его без окна.
    #[test]
    fn shutdown_reaps_the_child() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("sleep 60");
        let mut session = ShellSession::spawn(command).expect("spawn sleep");
        let pid = session.child.id();
        let timeout = Instant::now() + Duration::from_secs(5);
        loop {
            match session.try_wait() {
                Ok(Some(_)) => panic!("sleep exited before shutdown"),
                Ok(None) if Instant::now() < timeout => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Ok(None) => break,
                Err(error) => panic!("try_wait failed: {error}"),
            }
        }
        session.shutdown();
        let status = session.child.wait().expect("wait");
        assert!(!status.success(), "killed child must not report success");
        // PID больше не существует: kill(pid, 0) обязан вернуть ESRCH.
        let still_there = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("kill -0 {pid} 2>/dev/null"))
            .status()
            .expect("kill -0");
        assert!(!still_there.success(), "child {pid} survived shutdown");
    }

    /// `self_shell_command` берёт путь из argv[0] и добавляет режим потомка.
    #[test]
    fn self_shell_command_uses_argv0() {
        let command = self_shell_command(Some("/mnt/pagh-terminal")).expect("command");
        assert_eq!(command.get_program(), "/mnt/pagh-terminal");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, vec![std::ffi::OsStr::new("--shell-child")]);
    }
}
