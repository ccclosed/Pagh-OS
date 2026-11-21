//! Символьные устройства desktop-варианта B: `/dev/fb0` и
//! `/dev/input/event0`.
//!
//! Узлы — тонкие адаптеры между VFS-трейтом и драйверами
//! ([`crate::drivers::framebuffer`], [`crate::drivers::evdev`]). Реальная
//! работа (ioctl/mmap/read/poll) живёт в syscall-слое: `VfsNode` не знает
//! пользовательских указателей и `errno`.

use alloc::sync::Arc;

use super::{VfsNode, VfsResult};
use crate::arch::x86_64::linux::cred::gid;
use crate::drivers::framebuffer::FbInfo;

/// `/dev/fb0` — Limine-фреймбуфер, который desktop забирает через open/mmap.
pub struct FramebufferDevice;

impl VfsNode for FramebufferDevice {
    fn name(&self) -> &str {
        "fb0"
    }
    fn is_directory(&self) -> bool {
        false
    }
    fn is_fbdev(&self) -> bool {
        true
    }
    fn fb_info(&self) -> Option<FbInfo> {
        crate::drivers::framebuffer::fb_info()
    }
    fn size(&self) -> u64 {
        self.fb_info().map(|i| i.smem_len).unwrap_or(0)
    }
    fn read(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        // Фреймбуфер читается через mmap: байтового read(2) у Linux fbdev нет
        // (он вернул бы EINVAL); syscall-слой отклоняет такое чтение.
        Err(super::VfsError::NotSupported)
    }
    fn write(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(super::VfsError::NotSupported)
    }
    /// `root:video 0660`: экран принадлежит композитору, а не любому
    /// пользователю. Членство в `video` — граница доверия (см.
    /// `docs/security/groups.md`), поэтому other-биты нулевые.
    fn mode(&self) -> u32 {
        0o660
    }
    fn owner(&self) -> (u32, u32) {
        (0, gid::VIDEO)
    }
}

/// `/dev/input/event0` — evdev-подобный поток событий.
pub struct EventDevice;

impl VfsNode for EventDevice {
    fn name(&self) -> &str {
        "event0"
    }
    fn is_directory(&self) -> bool {
        false
    }
    fn is_evdev(&self) -> bool {
        true
    }
    fn size(&self) -> u64 {
        // Размер не байтовый: читатель забирает по одному событию из очереди.
        0
    }
    fn read(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(super::VfsError::NotSupported)
    }
    fn write(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(super::VfsError::NotSupported)
    }
    /// `root:input 0660`: клавиатура/мышь — sensitive-устройство (кейлоггер).
    fn mode(&self) -> u32 {
        0o660
    }
    fn owner(&self) -> (u32, u32) {
        (0, gid::INPUT)
    }
}

/// Каталог-контейнер для устройств (например, `/dev/input`).
pub struct DeviceDirectory {
    name: &'static str,
    children: alloc::vec::Vec<Arc<dyn VfsNode>>,
}

impl DeviceDirectory {
    pub fn new(name: &'static str, children: alloc::vec::Vec<Arc<dyn VfsNode>>) -> Self {
        DeviceDirectory { name, children }
    }
}

impl VfsNode for DeviceDirectory {
    fn name(&self) -> &str {
        self.name
    }
    fn is_directory(&self) -> bool {
        true
    }
    fn readdir(&self) -> VfsResult<alloc::vec::Vec<Arc<dyn VfsNode>>> {
        Ok(self.children.clone())
    }
    fn lookup(&self, name: &str) -> VfsResult<Arc<dyn VfsNode>> {
        self.children
            .iter()
            .find(|c| c.name() == name)
            .cloned()
            .ok_or(super::VfsError::NotFound)
    }
}

/// `/dev/fb0` как `Arc<dyn VfsNode>`.
pub fn fb0() -> Arc<dyn VfsNode> {
    Arc::new(FramebufferDevice) as Arc<dyn VfsNode>
}

/// `/dev/input` с единственным `event0`.
pub fn input_dir() -> Arc<dyn VfsNode> {
    let event0 = Arc::new(EventDevice) as Arc<dyn VfsNode>;
    Arc::new(DeviceDirectory::new("input", alloc::vec![event0])) as Arc<dyn VfsNode>
}

/// `/dev/vtconsole0` — read-only shared VT surface (ABI v1).
///
/// Содержимое буфера живёт в [`crate::drivers::vtsurface`]; узел только
/// сообщает геометрию/кадры для `mmap` и отдаёт `read(2)`-снимок. Запись через
/// этот fd запрещена: экран публикует только ядро.
pub struct VtConsoleDevice;

impl VfsNode for VtConsoleDevice {
    fn name(&self) -> &str {
        "vtconsole0"
    }
    fn is_directory(&self) -> bool {
        false
    }
    fn is_vtconsole(&self) -> bool {
        true
    }
    fn vtconsole_info(&self) -> Option<crate::drivers::vtsurface::VtConsoleInfo> {
        crate::drivers::vtsurface::info()
    }
    fn vtconsole_page(&self, index: u64) -> Option<u64> {
        crate::drivers::vtsurface::page(index)
    }
    fn mode(&self) -> u32 {
        // Экран консоли может содержать секреты: читать shared surface может
        // только root или член группы `tty` (терминальная сессия/композитор).
        // other-биты нулевые, запись узлом и так запрещена.
        0o660
    }
    fn owner(&self) -> (u32, u32) {
        (0, gid::TTY)
    }
    fn size(&self) -> u64 {
        self.vtconsole_info()
            .map(|i| i.total_bytes as u64)
            .unwrap_or(0)
    }
    fn read(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let Ok(offset) = u32::try_from(offset) else {
            return Ok(0);
        };
        Ok(crate::drivers::vtsurface::read_at(offset, buf))
    }
    fn write(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(super::VfsError::NotSupported)
    }
}

/// `/dev/vtinput` — write-only приём VT-байтов от терминала.
pub struct VtInputDevice;

impl VfsNode for VtInputDevice {
    fn name(&self) -> &str {
        "vtinput"
    }
    fn is_directory(&self) -> bool {
        false
    }
    fn is_vtinput(&self) -> bool {
        true
    }
    /// `root:input 0660`: приём VT-байтов (ввод терминала) — sensitive, как и
    /// evdev; писать может root или член `input`.
    fn mode(&self) -> u32 {
        0o660
    }
    fn owner(&self) -> (u32, u32) {
        (0, gid::INPUT)
    }
    fn read(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(super::VfsError::NotSupported)
    }
    fn write(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        Ok(crate::drivers::vtinput::write(buf))
    }
}

/// Имя узла сессии: `vtfb1`, `vtfb2`, …
///
/// `VfsNode::name` отдаёт `&str`, а VFS-таблица сравнивает имена при `lookup`,
/// поэтому имена — статическая таблица по номеру слота, а не `format!` (у узла
/// нет состояния аллокации, и `readdir` не должен аллоцировать имена).
fn session_name(prefix: &str, session: usize) -> &str {
    // Статический набор имён 0..MAX_SESSIONS: строка нужна только для VFS, а
    // не для ABI, поэтому расширение MAX_SESSIONS добавляет строку здесь.
    let names: &[&'static str] = match prefix {
        "vtfb" => &["vtfb0", "vtfb1", "vtfb2", "vtfb3"],
        "vtinput" => &["vtinput0", "vtinput1", "vtinput2", "vtinput3"],
        "vtmouse" => &["vtmouse0", "vtmouse1", "vtmouse2", "vtmouse3"],
        _ => &[""],
    };
    names.get(session).copied().unwrap_or("")
}

/// `/dev/vtfb{N}` — read-only пиксельная поверхность терминальной сессии `N`.
///
/// Композитор терминального окна читает её через `ioctl(VTFBIOGET_INFO)` +
/// `mmap(PROT_READ, MAP_SHARED)` (или `read(2)`-fallback с 24-байтным
/// заголовком). Содержимое живёт в [`crate::drivers::offscreen`]; узел только
/// сообщает геометрию/поколение/кадры. Запись запрещена: пиксели публикует
/// kernel-shell сессии.
pub struct TerminalFramebufferDevice {
    session: usize,
}

impl TerminalFramebufferDevice {
    pub fn new(session: usize) -> TerminalFramebufferDevice {
        TerminalFramebufferDevice { session }
    }
}

impl VfsNode for TerminalFramebufferDevice {
    fn name(&self) -> &str {
        session_name("vtfb", self.session)
    }
    fn is_directory(&self) -> bool {
        false
    }
    fn vtfb_session(&self) -> Option<usize> {
        Some(self.session)
    }
    fn vtfb_info(&self) -> Option<crate::drivers::vtfb_abi::VtfbInfo> {
        // После выхода сессии — `ENODEV`: поверхность в PMM остаётся (кадры
        // нельзя забрать, пока их держит отображение клиента), но окна больше
        // нет, и ioctl/`mmap` по старому fd обязаны это сказать, а не отдать
        // геометрию мёртвой сессии. Повторный `open` поднимает её заново.
        if !crate::drivers::session::live(self.session) {
            return None;
        }
        crate::drivers::offscreen::info(self.session)
    }
    fn vtfb_page(&self, index: u64) -> Option<u64> {
        crate::drivers::offscreen::page(self.session, index)
    }
    fn size(&self) -> u64 {
        // Логический размер `read(2)`-файла: заголовок плюс пиксели.
        self.vtfb_info()
            .map(|i| crate::drivers::vtfb_abi::read_size(&i))
            .unwrap_or(0)
    }
    fn read(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let Ok(offset) = usize::try_from(offset) else {
            return Ok(0);
        };
        Ok(crate::drivers::offscreen::read_snapshot(
            self.session,
            offset,
            buf,
        ))
    }
    fn write(&self, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(super::VfsError::NotSupported)
    }
    /// `root:tty 0660`: пиксели окна могут содержать секреты (вывод команд),
    /// поэтому читает только root или член группы `tty` — как `vtconsole0`.
    fn mode(&self) -> u32 {
        0o660
    }
    fn owner(&self) -> (u32, u32) {
        (0, gid::TTY)
    }
}

/// `/dev/vtinput{N}` — write-only приём VT-байтов терминальной сессии `N`.
pub struct TerminalInputDevice {
    session: usize,
}

impl TerminalInputDevice {
    pub fn new(session: usize) -> TerminalInputDevice {
        TerminalInputDevice { session }
    }
}

impl VfsNode for TerminalInputDevice {
    fn name(&self) -> &str {
        session_name("vtinput", self.session)
    }
    fn is_directory(&self) -> bool {
        false
    }
    fn vtinput_session(&self) -> Option<usize> {
        Some(self.session)
    }
    /// `root:input 0660`: ввод окна — sensitive, как evdev и `vtinput`.
    fn mode(&self) -> u32 {
        0o660
    }
    fn owner(&self) -> (u32, u32) {
        (0, gid::INPUT)
    }
    fn read(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(super::VfsError::NotSupported)
    }
    fn write(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        Ok(crate::drivers::vtinput::write_session(self.session, buf))
    }
}

/// `/dev/vtmouse{N}` — write-only поток указателя терминальной сессии `N`.
///
/// Записи по 16 байт [`crate::drivers::vt_mouse_abi::VtMouseEvent`]; клиент
/// окна переводит координаты внутри окна в пиксели поверхности и присылает
/// движение/кнопки/колесо. Kernel-shell читает состояние через
/// `ps2_mouse::poll()` (маршрут по задаче). `read` — `EBADF`/`NotSupported`:
/// устройство одностороннее, как `vtinput`.
pub struct TerminalMouseDevice {
    session: usize,
}

impl TerminalMouseDevice {
    pub fn new(session: usize) -> TerminalMouseDevice {
        TerminalMouseDevice { session }
    }
}

impl VfsNode for TerminalMouseDevice {
    fn name(&self) -> &str {
        session_name("vtmouse", self.session)
    }
    fn is_directory(&self) -> bool {
        false
    }
    fn vtmouse_session(&self) -> Option<usize> {
        Some(self.session)
    }
    /// `root:input 0660`: указатель окна — sensitive (позиция/кнопки), как evdev.
    fn mode(&self) -> u32 {
        0o660
    }
    fn owner(&self) -> (u32, u32) {
        (0, gid::INPUT)
    }
    fn read(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(super::VfsError::NotSupported)
    }
    fn write(&self, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        Ok(crate::drivers::vt_mouse::write_session(self.session, buf))
    }
}

/// `/dev/vtconsole0` как `Arc<dyn VfsNode>`.
pub fn vtconsole0() -> Arc<dyn VfsNode> {
    Arc::new(VtConsoleDevice) as Arc<dyn VfsNode>
}

/// `/dev/vtinput` как `Arc<dyn VfsNode>`.
pub fn vtinput() -> Arc<dyn VfsNode> {
    Arc::new(VtInputDevice) as Arc<dyn VfsNode>
}

/// `/dev/vtfb{N}` как `Arc<dyn VfsNode>`.
pub fn vtfb(session: usize) -> Arc<dyn VfsNode> {
    Arc::new(TerminalFramebufferDevice::new(session)) as Arc<dyn VfsNode>
}

/// `/dev/vtinput{N}` как `Arc<dyn VfsNode>`.
pub fn vtinput_session(session: usize) -> Arc<dyn VfsNode> {
    Arc::new(TerminalInputDevice::new(session)) as Arc<dyn VfsNode>
}

/// `/dev/vtmouse{N}` как `Arc<dyn VfsNode>`.
pub fn vtmouse(session: usize) -> Arc<dyn VfsNode> {
    Arc::new(TerminalMouseDevice::new(session)) as Arc<dyn VfsNode>
}

/// `/dev/vtfb1` как `Arc<dyn VfsNode>` (слот сессии 1; сохранено для тестов).
pub fn vtfb1() -> Arc<dyn VfsNode> {
    vtfb(1)
}

/// `/dev/vtinput1` как `Arc<dyn VfsNode>`.
pub fn vtinput1() -> Arc<dyn VfsNode> {
    vtinput_session(1)
}

/// `/dev/vtmouse1` как `Arc<dyn VfsNode>`.
pub fn vtmouse1() -> Arc<dyn VfsNode> {
    vtmouse(1)
}
