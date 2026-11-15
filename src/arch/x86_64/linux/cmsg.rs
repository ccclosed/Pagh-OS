//! Чистая разметка ancillary data (`cmsg`) Linux для AF_UNIX `SCM_RIGHTS`.
//!
//! Здесь нет ни пользовательской памяти, ни fd-таблиц: только байтовая
//! раскладка `struct msghdr`/`struct cmsghdr`/`struct iovec`, разбор control-
//! буфера отправки и план доставки дескрипторов на приёме. Поэтому модуль
//! `core`+`alloc`-only и подключается в `host-tests` через `#[path]`: тесты,
//! как и ядро, исполняют ровно этот исходник.
//!
//! Числа сверены с Linux x86_64 (`sizeof(struct msghdr) == 56`,
//! `sizeof(struct cmsghdr) == 16`, `CMSG_SPACE(sizeof(int)) == 24`) и с
//! наблюдаемым поведением `recvmsg(2)` (см. `docs/desktop-b.md` §6).
#![allow(dead_code)]

use alloc::vec::Vec;

/// `SOL_SOCKET` — уровень cmsg для `SCM_RIGHTS` (`include/uapi/asm-generic/socket.h`).
pub const SOL_SOCKET: i32 = 1;
/// `SCM_RIGHTS` — тип cmsg, несущий массив `int fd`.
pub const SCM_RIGHTS: i32 = 1;
/// Linux `SCM_MAX_FD`: максимум дескрипторов в одном `SCM_RIGHTS`
/// (`include/net/scm.h`). Сверх него `__scm_send` отвечает `EINVAL`.
pub const SCM_MAX_FD: usize = 253;

/// `sizeof(struct msghdr)` на x86_64: имя 8 + выравнивание 8 + iov 16/
/// iovlen 8 + control 8/controllen 8 + flags 4 + паддинг 4.
pub const MSGHDR_SIZE: usize = 56;
/// `sizeof(struct cmsghdr)`: `size_t cmsg_len` + `int cmsg_level` + `int cmsg_type`.
pub const CMSGHDR_SIZE: usize = 16;
/// `sizeof(struct iovec)`.
pub const IOV_SIZE: usize = 16;
/// Linux `UIO_MAXIOV`: предел `msg_iovlen`.
pub const IOV_MAX: usize = 1024;

/// Верхняя граница control-буфера, который ядро копирует себе на разбор.
///
/// Linux отвергает только `msg_controllen > INT_MAX` (`ENOBUFS`), но реальный
/// AF_UNIX-cmsg — это `SCM_RIGHTS` на ≤253 fd, то есть ≤`CMSG_SPACE(1012)` =
/// 1028 байт. 64 КиБ с запасом покрывают любое законное сообщение и при этом
/// не дают вызывающему заставить ядро скопировать произвольный control-буфер.
pub const MAX_CONTROL: usize = 64 * 1024;
/// `INT_MAX`: Linux отвергает `msg_controllen` больше него с `ENOBUFS`.
pub const INT_MAX: u64 = 0x7fff_ffff;

/// `MSG_CTRUNC`: на приёме часть ancillary data не поместилась в буфер.
pub const MSG_CTRUNC: u32 = 0x8;
/// `MSG_DONTWAIT`: разовая неблокирующая операция.
pub const MSG_DONTWAIT: u32 = 0x40;
/// `MSG_EOR`: на SOCK_STREAM семантика конца записи, принимается как no-op.
pub const MSG_EOR: u32 = 0x80;
/// `MSG_NOSIGNAL`: не посылать SIGPIPE (в этом ядре сигналов за EPIPE нет).
pub const MSG_NOSIGNAL: u32 = 0x4000;
/// `MSG_CMSG_CLOEXEC`: полученные fd помечаются close-on-exec.
pub const MSG_CMSG_CLOEXEC: u32 = 0x4000_0000;

// Смещения полей `struct msghdr` (x86_64, little-endian).
/// `msg_namelen` (socklen_t).
pub const MSGHDR_NAMELEN_OFF: u64 = 8;
/// `msg_iov`.
pub const MSGHDR_IOV_OFF: u64 = 16;
/// `msg_iovlen`.
pub const MSGHDR_IOVLEN_OFF: u64 = 24;
/// `msg_control`.
pub const MSGHDR_CONTROL_OFF: u64 = 32;
/// `msg_controllen`.
pub const MSGHDR_CONTROLLEN_OFF: u64 = 40;
/// `msg_flags`.
pub const MSGHDR_FLAGS_OFF: u64 = 48;

/// `CMSG_ALIGN(len)` — выравнивание длины cmsg на `sizeof(size_t)`.
pub fn cmsg_align(len: usize) -> usize {
    (len + 7) & !7
}

/// `CMSG_LEN(payload)` — длина cmsg вместе с заголовком (без паддинга).
pub fn cmsg_len(payload: usize) -> usize {
    CMSGHDR_SIZE + payload
}

/// `CMSG_SPACE(payload)` — сколько места cmsg занимает в буфере (с паддингом).
pub fn cmsg_space(payload: usize) -> usize {
    cmsg_align(cmsg_len(payload))
}

/// Ошибка разбора control-буфера `sendmsg` — сводится к `EINVAL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlError {
    /// `cmsg_len` меньше заголовка или выходит за control-буфер (`!CMSG_OK`).
    Malformed,
    /// `SOL_SOCKET` cmsg неизвестного типа (например `SCM_CREDENTIALS`).
    Unsupported,
    /// Больше [`SCM_MAX_FD`] дескрипторов.
    TooManyFds,
}

/// Разобрать control-буфер `sendmsg` AF_UNIX в список fd — аналог
/// `__scm_send`/`scm_fp_copy` для `SCM_RIGHTS`.
///
/// Семантика, как у Linux:
///  * control-буфер короче `sizeof(struct cmsghdr)` — законный пустой список;
///  * каждый cmsg обязан пройти `CMSG_OK` (`cmsg_len >= 16` и целиком внутри
///    буфера), иначе [`ControlError::Malformed`];
///  * cmsg уровня не `SOL_SOCKET` игнорируются целиком (так Linux пропускает
///    `IPPROTO_IP`-записи, которые к AF_UNIX отношения не имеют);
///  * несколько `SCM_RIGHTS` cmsg накапливаются в порядке появления (Linux так
///    же сливает их в один `scm_fp_list` и доставляет одним cmsg);
///  * хвостовой кусок меньше заголовка игнорируется, как `for_each_cmsghdr`.
pub fn decode_send_control(control: &[u8]) -> Result<Vec<i32>, ControlError> {
    let mut fds: Vec<i32> = Vec::new();
    if control.len() < CMSGHDR_SIZE {
        return Ok(fds);
    }
    let mut off = 0usize;
    while off + CMSGHDR_SIZE <= control.len() {
        let cmsg_len = read_u64(control, off) as usize;
        let level = read_u32(control, off + 8) as i32;
        let ty = read_u32(control, off + 12) as i32;
        // `CMSG_OK`: `cmsg_len >= sizeof(cmsghdr)` и `cmsg_len <= остаток`.
        if cmsg_len < CMSGHDR_SIZE || cmsg_len > control.len() - off {
            return Err(ControlError::Malformed);
        }
        if level == SOL_SOCKET {
            if ty != SCM_RIGHTS {
                return Err(ControlError::Unsupported);
            }
            // `scm_fp_copy`: `(cmsg_len - CMSG_LEN(0)) / sizeof(int)` fd; хвост
            // меньше int не считается fd (Linux его не замечает).
            let payload = cmsg_len - CMSGHDR_SIZE;
            let mut i = 0usize;
            while i + 4 <= payload {
                fds.push(read_u32(control, off + CMSGHDR_SIZE + i) as i32);
                if fds.len() > SCM_MAX_FD {
                    return Err(ControlError::TooManyFds);
                }
                i += 4;
            }
        }
        let step = cmsg_align(cmsg_len);
        if step < cmsg_len || off + step < off {
            return Err(ControlError::Malformed);
        }
        off += step;
    }
    Ok(fds)
}

/// План доставки `SCM_RIGHTS` на приёме.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RightsPlan {
    /// Сколько fd реально помещается и будет установлено получателю.
    pub deliver: usize,
    /// `cmsg_len` доставленного cmsg (`16 + 4*deliver`) или 0, если cmsg нет.
    pub cmsg_len: usize,
    /// `msg_controllen` для записи обратно в `msghdr`.
    pub used: usize,
    /// Часть fd не поместилась (`MSG_CTRUNC`); лишние закрываются.
    pub truncated: bool,
}

/// Рассчитать, сколько из `count` принятых fd помещается в control-буфер
/// ёмкости `capacity` (`msg_controllen` вызывающего).
///
/// Числа повторяют наблюдаемое Linux поведение `recvmsg(2)`:
///  * `capacity < CMSG_SPACE(sizeof(int))` — cmsg не пишется вообще
///    (`MSG_CTRUNC`), `msg_controllen` на выходе 0;
///  * частичное размещение — в заголовок попадает столько fd, сколько влезло,
///    `msg_controllen` на выходе равен ёмкости (Linux отмечает весь буфер
///    использованным), лишние fd закрываются;
///  * полное размещение — `msg_controllen` равен `CMSG_SPACE(cmsg_len)`, но не
///    больше ёмкости (буфер ровно по `cmsg_len` без паддинга тоже валиден).
pub fn plan_rights(count: usize, capacity: usize) -> RightsPlan {
    if count == 0 {
        return RightsPlan {
            deliver: 0,
            cmsg_len: 0,
            used: 0,
            truncated: false,
        };
    }
    // Меньше заголовка + одного fd: доставлять нечего.
    if capacity < cmsg_len(4) {
        return RightsPlan {
            deliver: 0,
            cmsg_len: 0,
            used: 0,
            truncated: true,
        };
    }
    let fit = (capacity - CMSGHDR_SIZE) / 4;
    let deliver = core::cmp::min(count, fit);
    let cmsg_len = cmsg_len(4 * deliver);
    let truncated = deliver < count;
    let used = if truncated {
        capacity
    } else {
        core::cmp::min(capacity, cmsg_space(4 * deliver))
    };
    RightsPlan {
        deliver,
        cmsg_len,
        used,
        truncated,
    }
}

/// Записать `SCM_RIGHTS` cmsg с `fds` в `out` (только байты `cmsg_len`,
/// паддинг не трогается). Возвращает `cmsg_len` или `None`, если буфер мал.
pub fn encode_rights(out: &mut [u8], fds: &[i32]) -> Option<usize> {
    let len = cmsg_len(4 * fds.len());
    if out.len() < len || fds.len() > SCM_MAX_FD {
        return None;
    }
    write_u64(out, 0, len as u64);
    write_u32(out, 8, SOL_SOCKET as u32);
    write_u32(out, 12, SCM_RIGHTS as u32);
    for (i, fd) in fds.iter().enumerate() {
        write_u32(out, CMSGHDR_SIZE + 4 * i, *fd as u32);
    }
    Some(len)
}

fn read_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn read_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_ne_bytes([
        buf[off],
        buf[off + 1],
        buf[off + 2],
        buf[off + 3],
        buf[off + 4],
        buf[off + 5],
        buf[off + 6],
        buf[off + 7],
    ])
}

fn write_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_ne_bytes());
}

fn write_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_ne_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// Размеры/смещения обязаны совпадать с Linux x86_64: это тот же ABI, в
    /// который пишет glibc `CMSG_FIRSTHDR`/`CMSG_NXTHDR`.
    #[test]
    fn layout_matches_linux_uapi() {
        assert_eq!(MSGHDR_SIZE, 56);
        assert_eq!(CMSGHDR_SIZE, 16);
        assert_eq!(IOV_SIZE, 16);
        assert_eq!(cmsg_len(4), 20); // CMSG_LEN(sizeof(int))
        assert_eq!(cmsg_space(4), 24); // CMSG_SPACE(sizeof(int))
        assert_eq!(cmsg_len(12), 28);
        assert_eq!(cmsg_space(12), 32); // CMSG_SPACE(3*sizeof(int))
        assert_eq!(cmsg_space(0), 16);
        assert_eq!(cmsg_align(17), 24);
        assert_eq!((MSGHDR_NAMELEN_OFF, MSGHDR_FLAGS_OFF), (8, 48));
    }

    fn rights_buf(fds: &[i32]) -> Vec<u8> {
        let mut buf = vec![0u8; cmsg_space(4 * fds.len())];
        assert_eq!(encode_rights(&mut buf, fds), Some(cmsg_len(4 * fds.len())));
        buf
    }

    #[test]
    fn decode_single_and_multiple_rights() {
        assert_eq!(decode_send_control(&[]), Ok(Vec::new()));
        assert_eq!(decode_send_control(&[0u8; 8]), Ok(Vec::new()));
        assert_eq!(decode_send_control(&rights_buf(&[3])), Ok(vec![3]));

        // Два SCM_RIGHTS cmsg в одном sendmsg сливаются, как в Linux.
        let mut two = rights_buf(&[7]);
        two.extend_from_slice(&rights_buf(&[8, 9]));
        assert_eq!(decode_send_control(&two), Ok(vec![7, 8, 9]));
    }

    #[test]
    fn decode_ignores_foreign_level_and_rejects_unknown_sol_socket() {
        let mut buf = vec![0u8; cmsg_space(4)];
        // IPPROTO_TCP (6) / тип 1: Linux такой cmsg молча пропускает.
        write_u64(&mut buf, 0, cmsg_len(4) as u64);
        write_u32(&mut buf, 8, 6);
        write_u32(&mut buf, 12, 1);
        write_u32(&mut buf, 16, 42);
        assert_eq!(decode_send_control(&buf), Ok(Vec::new()));

        // SOL_SOCKET/SCM_CREDENTIALS (2) в этом ядре не поддержан: EINVAL.
        let mut cred = vec![0u8; cmsg_space(12)];
        write_u64(&mut cred, 0, cmsg_len(12) as u64);
        write_u32(&mut cred, 8, SOL_SOCKET as u32);
        write_u32(&mut cred, 12, 2);
        assert_eq!(decode_send_control(&cred), Err(ControlError::Unsupported));
    }

    #[test]
    fn decode_rejects_malformed_lengths() {
        // cmsg_len меньше заголовка.
        let mut short = vec![0u8; cmsg_space(4)];
        write_u64(&mut short, 0, 8);
        assert_eq!(decode_send_control(&short), Err(ControlError::Malformed));
        // cmsg_len за пределами control-буфера.
        let mut over = vec![0u8; cmsg_space(4)];
        write_u64(&mut over, 0, 200);
        assert_eq!(decode_send_control(&over), Err(ControlError::Malformed));
        // Хвост короче заголовка — законен (Linux его игнорирует).
        let mut tail = rights_buf(&[3]);
        tail.extend_from_slice(&[0u8; 8]);
        assert_eq!(decode_send_control(&tail), Ok(vec![3]));
    }

    #[test]
    fn decode_caps_fd_count_at_scm_max_fd() {
        // 254 fd в одном cmsg: `encode_rights` такой cmsg уже не пишет, поэтому
        // буфер собирается вручную — важно, что и разбор его отвергает.
        let count = SCM_MAX_FD + 1;
        let mut buf = vec![0u8; cmsg_space(4 * count)];
        write_u64(&mut buf, 0, cmsg_len(4 * count) as u64);
        write_u32(&mut buf, 8, SOL_SOCKET as u32);
        write_u32(&mut buf, 12, SCM_RIGHTS as u32);
        for i in 0..count {
            write_u32(&mut buf, CMSGHDR_SIZE + 4 * i, i as u32);
        }
        assert_eq!(decode_send_control(&buf), Err(ControlError::TooManyFds));
    }

    /// Таблица повторяет замеры на Linux: control-буфер меньше 20 байт не
    /// доставляет ничего; частичный cmsg получает столько fd, сколько влезло;
    /// `msg_controllen` на выходе — использованная ёмкость.
    #[test]
    fn plan_rights_matches_linux_recvmsg_observations() {
        // (ёмкость, число fd) -> (deliver, cmsg_len, used, truncated)
        let cases = [
            (0usize, 3usize, 0usize, 0usize, 0usize, true),
            (8, 3, 0, 0, 0, true),
            (16, 3, 0, 0, 0, true),
            (19, 3, 0, 0, 0, true),
            (20, 3, 1, 20, 20, true),
            (21, 3, 1, 20, 21, true),
            (23, 3, 1, 20, 23, true),
            (24, 3, 2, 24, 24, true),
            (27, 3, 2, 24, 27, true),
            (28, 3, 3, 28, 28, false),
            (31, 3, 3, 28, 31, false),
            (32, 3, 3, 28, 32, false),
            (256, 3, 3, 28, 32, false),
            (256, 1, 1, 20, 24, false),
        ];
        for (cap, count, deliver, len, used, trunc) in cases {
            let p = plan_rights(count, cap);
            assert_eq!(
                (p.deliver, p.cmsg_len, p.used, p.truncated),
                (deliver, len, used, trunc),
                "capacity={cap} count={count}"
            );
        }
        // Нет fd — нет cmsg.
        assert_eq!(
            plan_rights(0, 256),
            RightsPlan {
                deliver: 0,
                cmsg_len: 0,
                used: 0,
                truncated: false
            }
        );
    }

    #[test]
    fn encode_roundtrips_through_decode() {
        let fds = [3i32, 4, 5];
        let mut buf = vec![0u8; cmsg_space(12)];
        assert_eq!(encode_rights(&mut buf, &fds), Some(28));
        assert_eq!(buf.len(), 32);
        assert_eq!(decode_send_control(&buf), Ok(fds.to_vec()));
        // Буфер меньше cmsg — отказ, а не частичная запись.
        assert_eq!(encode_rights(&mut buf[..27], &fds), None);
        // Слишком много fd — отказ (SCM_MAX_FD).
        let too_many = vec![1i32; SCM_MAX_FD + 1];
        let mut big = vec![0u8; cmsg_space(4 * (SCM_MAX_FD + 1))];
        assert_eq!(encode_rights(&mut big, &too_many), None);
    }
}
