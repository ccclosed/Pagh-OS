//! Эффектный `ioctl(SIOCGIF*)` surface: `ifreq` (40 байт) ↔ живое состояние
//! канала.
//!
//! Linux-программы (net-tools/busybox `ifconfig`, `ss`, композитор Pagh-OS)
//! узнают адрес/маску/флаги/MAC интерфейса не из `/proc`, а через ioctl на
//! любом `AF_INET`-сокете. До этого слоя такие запросы падали в общий
//! `ENOTTY`-catch-all, и `ifconfig` не мог показать даже IP, хотя
//! `/proc/net/dev` его уже отдаёт.
//!
//! Раскладка `ifreq`/`sockaddr` и номера запросов — в чистом
//! [`super::net_if_abi`] (host-свойство `net_if_abi`); здесь только проверка
//! пользовательского указателя, копирование 40 байт и заполнение полей из
//! [`crate::net::iface_stats`]. Неизвестный запрос сохраняет прежнюю семантику
//! `ENOTTY` — этот слой не притворяется, что поддерживает весь `SIOC*`.
#![allow(dead_code)]

use super::check_user_ptr_writable;
use super::errno::Errno;
use super::net_if_abi as abi;

/// Обслужить `ifreq`-запрос на `AF_INET`-сокете; `ENOTTY` — не наш запрос.
///
/// Семантика ошибок как у Linux для одиночного интерфейса: не наш запрос —
/// `ENOTTY`; нет канала (стек без NIC/RNDIS) — `ENODEV`; имя в `ifr_name` не
/// совпало с единственным интерфейсом — `ENODEV`; адрес/маска ещё не назначены
/// (DHCP в полёте) — `EADDRNOTAVAIL`; непроходимый пользовательский буфер —
/// `EFAULT` (проверка до записи, поэтому TOCTOU не превращает её в ring-0 #PF).
///
/// `arg` — пользовательский указатель на 40-байтный `ifreq`; буфер обязан быть
/// и читаемым, и записываемым: Linux копирует `ifreq` из user-space, меняет
/// поле и копирует обратно.
pub fn if_ioctl(request: u64, arg: u64) -> Result<u64, Errno> {
    if !abi::is_ifreq(request) {
        return Err(Errno::ENOTTY);
    }
    // `SIOCGIFCONF` — единственный запрос, чей аргумент не `ifreq`, а
    // 16-байтный `struct ifconf`; он обслуживается отдельно, до чтения 40 байт.
    if request == abi::SIOCGIFCONF {
        return if_conf(arg);
    }
    check_user_ptr_writable(arg, abi::IFREQ_SIZE as u64)?;

    // SAFETY: `check_user_ptr_writable` только что обошёл все страницы буфера
    // и подтвердил, что диапазон целиком лежит в пользовательской половине,
    // отображён и записываем. Активный CR3 в syscall — CR3 вызывающего, поэтому
    // чтение 40 байт из `arg` не может уйти в чужое адресное пространство.
    let mut req = [0u8; abi::IFREQ_SIZE];
    unsafe {
        core::ptr::copy_nonoverlapping(arg as *const u8, req.as_mut_ptr(), abi::IFREQ_SIZE);
    }

    let stats = match crate::net::iface_stats() {
        Some(s) => s,
        None => return Err(Errno::ENODEV),
    };

    match request {
        // Единственный интерфейс имеет индекс 1, как `eth0` в Linux.
        abi::SIOCGIFNAME => {
            if abi::read_i32(&req, abi::I32_OFF) != 1 {
                return Err(Errno::ENODEV);
            }
            abi::write_ifname(&mut req, stats.name);
        }
        abi::SIOCGIFFLAGS => {
            check_name(&req, stats.name)?;
            // Интерфейс существует — он административно поднят (стек всегда
            // передаёт, когда канал выбран); RUNNING отражает физический линк.
            let mut flags = abi::IFF_UP | abi::IFF_BROADCAST | abi::IFF_MULTICAST;
            if stats.running {
                flags |= abi::IFF_RUNNING;
            }
            abi::write_i16(&mut req, abi::FLAGS_OFF, flags);
        }
        abi::SIOCGIFADDR => {
            check_name(&req, stats.name)?;
            let Some(cidr) = stats.cidr else {
                // Как Linux: интерфейс без адреса — не `ENODEV`, а «адреса нет».
                return Err(Errno::EADDRNOTAVAIL);
            };
            req[abi::SOCKADDR_OFF..abi::SOCKADDR_OFF + abi::SOCKADDR_IN_SIZE]
                .copy_from_slice(&abi::sockaddr_in(cidr.addr.0, 0));
        }
        abi::SIOCGIFNETMASK => {
            check_name(&req, stats.name)?;
            let Some(cidr) = stats.cidr else {
                return Err(Errno::EADDRNOTAVAIL);
            };
            req[abi::SOCKADDR_OFF..abi::SOCKADDR_OFF + abi::SOCKADDR_IN_SIZE]
                .copy_from_slice(&abi::sockaddr_in(cidr.mask_octets(), 0));
        }
        abi::SIOCGIFHWADDR => {
            check_name(&req, stats.name)?;
            req[abi::SOCKADDR_OFF..abi::SOCKADDR_OFF + abi::SOCKADDR_IN_SIZE]
                .copy_from_slice(&abi::sockaddr_hwaddr(stats.mac.0));
        }
        abi::SIOCGIFMTU => {
            check_name(&req, stats.name)?;
            abi::write_i32(&mut req, abi::I32_OFF, stats.mtu as i32);
        }
        abi::SIOCGIFINDEX => {
            check_name(&req, stats.name)?;
            abi::write_i32(&mut req, abi::I32_OFF, 1);
        }
        // `is_ifreq` уже отфильтровал; ветка нужна только для исчерпывающего
        // match и честно сохраняет старую семантику неизвестного запроса.
        _ => return Err(Errno::ENOTTY),
    }

    // SAFETY: тот же проверенный `arg`, что и на чтении, и тот же 40-байтный
    // диапазон; `req` — локальный массив, источник валиден на всё копирование.
    unsafe {
        core::ptr::copy_nonoverlapping(req.as_ptr(), arg as *mut u8, abi::IFREQ_SIZE);
    }
    Ok(0)
}

/// `SIOCGIFCONF`: перечислить интерфейсы в пользовательский массив `ifreq`.
///
/// Linux-контракт (`net/core/dev_ioctl.c:dev_ifconf` + `inet_gifconf`): аргумент
/// — `struct ifconf { int ifc_len; char *ifc_buf; }`; в буфер пишется столько
/// 40-байтных `ifreq`, сколько влезает в `ifc_len` (по одному на интерфейс с
/// IPv4-адресом), после чего `ifc_len` перезаписывается фактическим числом
/// байт. Не хватает места — 0 записей и `ifc_len = 0`, без ошибки (именно так
/// `ifconfig` узнаёт, что нужно спросить размер: он передаёт большой буфер и
/// разбирает результат). Возврат — 0.
///
/// Канала нет — `ENODEV` (как у остальных `SIOCGIF*`). Интерфейс без адреса
/// (DHCP ещё в полёте) — 0 записей, как Linux для `sin_addr`-less устройства.
fn if_conf(arg: u64) -> Result<u64, Errno> {
    check_user_ptr_writable(arg, abi::IFCONF_SIZE as u64)?;
    let mut conf = [0u8; abi::IFCONF_SIZE];
    // SAFETY: диапазон проверен `check_user_ptr_writable`; активный CR3 —
    // адресное пространство вызывающего, 16 байт целиком в user-половине.
    unsafe {
        core::ptr::copy_nonoverlapping(arg as *const u8, conf.as_mut_ptr(), abi::IFCONF_SIZE);
    }
    let stats = match crate::net::iface_stats() {
        Some(s) => s,
        None => return Err(Errno::ENODEV),
    };
    let capacity = abi::read_ifconf_len(&conf);
    let buf = abi::read_ifconf_buf(&conf);
    let mut written = 0i32;
    if let Some(cidr) = stats.cidr {
        if capacity >= abi::IFREQ_SIZE as i32 {
            check_user_ptr_writable(buf, abi::IFREQ_SIZE as u64)?;
            let mut req = [0u8; abi::IFREQ_SIZE];
            abi::write_ifname(&mut req, stats.name);
            req[abi::SOCKADDR_OFF..abi::SOCKADDR_OFF + abi::SOCKADDR_IN_SIZE]
                .copy_from_slice(&abi::sockaddr_in(cidr.addr.0, 0));
            // SAFETY: 40-байтный диапазон `buf` только что проверен как
            // пользовательский и записываемый; source — локальный массив.
            unsafe {
                core::ptr::copy_nonoverlapping(req.as_ptr(), buf as *mut u8, abi::IFREQ_SIZE);
            }
            written = abi::IFREQ_SIZE as i32;
        }
    }
    abi::write_ifconf_len(&mut conf, written);
    // SAFETY: тот же проверенный 16-байтный пользовательский диапазон, что и
    // на чтении; обновлённый `ifc_len` — единственное изменённое поле.
    unsafe {
        core::ptr::copy_nonoverlapping(conf.as_ptr(), arg as *mut u8, abi::IFCONF_SIZE);
    }
    Ok(0)
}

/// Требует, чтобы `ifr_name` называл единственный интерфейс.
///
/// Linux ищет устройство по имени и отвечает `ENODEV`, если такого нет;
/// пустое имя или `lo` не совпадут с `eth0` и тоже получат `ENODEV` — это
/// честно (loopback в этом ядре нет), а не «первый попавшийся интерфейс».
fn check_name(req: &[u8; abi::IFREQ_SIZE], name: &str) -> Result<(), Errno> {
    match abi::read_ifname(req) {
        Some(requested) if requested == name => Ok(()),
        _ => Err(Errno::ENODEV),
    }
}
