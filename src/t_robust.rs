// Тесты устойчивости к БИТЫМ дескрипторам/кольцу и валидации на уровне blk.
// Основаны на реальных классах багов чужих реализаций:
//   * DPDK CVE-2020-10725 — отсутствие проверки адреса дескриптора → segfault;
//   * DPDK CVE-2021-3839  — невалидированный num_queues в SET_INFLIGHT_FD;
//   * QEMU virtqueue_pop  — «looped descriptor» и зависание на нулевом буфере.
//
// libvhost-server от этих классов защищён (walk_chain/map_buffer/gpa_range_to_ptr,
// max_chain_len, проверки indirect, overflow-safe bounds). Эти тесты — РЕГРЕССИОННЫЕ
// СТОРОЖА: доказывают, что защита срабатывает и демон это переживает.
//
// ВАЖНО про модель проверки «битого дескриптора»: при некорректной цепочке
// libvhost-server делает mark_broken(vq) — очередь фейл-стопит (как QEMU
// virtio_error). Значит завершения по такому запросу НЕ будет (это норма), а
// критерий — ПРОЦЕСС ДЕМОНА ВЫЖИЛ: свежее подключение снова обслуживается.

use crate::dev::{self, r, w, Session, TestErr, TR};

macro_rules! fail {
    ($($a:tt)*) => { return Err(TestErr::Fail(format!($($a)*))) };
}
macro_rules! skip {
    ($($a:tt)*) => { return Err(TestErr::Skip(format!($($a)*))) };
}
macro_rules! check {
    ($c:expr, $($a:tt)*) => { if !($c) { fail!($($a)*); } };
}

const SEC: usize = dev::SECTOR;
const TMO: u64 = dev::TIMEOUT_MS;
const F_NEXT: u16 = dev::VRING_DESC_F_NEXT;
const F_WRITE: u16 = dev::VRING_DESC_F_WRITE;
const F_INDIRECT: u16 = dev::VRING_DESC_F_INDIRECT;

/// Собрать намеренно битую цепочку (через build), пнуть, НЕ ждать завершения
/// (очередь ожидаемо сломается), затем проверить, что демон выжил — свежее
/// подключение работает.
fn run_malformed<F: FnOnce(&mut Session)>(sock: &str, build: F) -> TR {
    {
        let mut s = Session::connect(sock)?;
        build(&mut s);
        s.kick();
        let _ = s.wait_used(400); // ожидаемо None (mark_broken) — это ок
    } // drop → закрытие сокета
    dev::alive(sock) // демон обязан пережить кривой ввод
}

// ==== битые дескрипторы: демон обязан выжить (не крешнуться/не зависнуть) ====

// next указывает за пределы таблицы (>= qsz).
pub fn t_desc_next_oob(sock: &str) -> TR {
    run_malformed(sock, |s| {
        let b = s.alloc(64) as u64;
        s.write_raw_desc(0, b, 16, F_NEXT, dev::QSZ + 5);
        s.push_avail(0);
    })
}

// Петля в цепочке дескрипторов (0→1→0). Должна ограничиться max_chain_len,
// а не крутиться бесконечно (класс QEMU "looped descriptor").
pub fn t_desc_loop(sock: &str) -> TR {
    run_malformed(sock, |s| {
        let b = s.alloc(64) as u64;
        s.write_raw_desc(0, b, 16, F_NEXT, 1);
        s.write_raw_desc(1, b, 16, F_NEXT, 0); // назад на 0
        s.push_avail(0);
    })
}

// Адрес буфера полностью вне отображённой памяти (класс DPDK CVE-2020-10725).
pub fn t_desc_addr_oob(sock: &str) -> TR {
    run_malformed(sock, |s| {
        let bad = dev::REGION_SIZE as u64 + 0x1000;
        s.write_raw_desc(0, bad, 512, 0, 0);
        s.push_avail(0);
    })
}

// Начало валидно, но addr+len выходит за конец региона (частичный OOB).
// Проверяет, что маппится ВЕСЬ диапазон, а не только стартовый адрес.
pub fn t_desc_len_past_region(sock: &str) -> TR {
    run_malformed(sock, |s| {
        let near = dev::REGION_SIZE as u64 - 256;
        s.write_raw_desc(0, near, 4096, 0, 0);
        s.push_avail(0);
    })
}

// Гигантская длина дескриптора (проверка на integer overflow в проверке диапазона).
pub fn t_desc_huge_len(sock: &str) -> TR {
    run_malformed(sock, |s| {
        let b = s.alloc(64) as u64;
        s.write_raw_desc(0, b, 0xFFFF_FFFF, 0, 0);
        s.push_avail(0);
    })
}

// device-readable дескриптор ПОСЛЕ device-writable (нарушение 2.7.4.2).
pub fn t_readable_after_writable(sock: &str) -> TR {
    run_malformed(sock, |s| {
        let b = s.alloc(64) as u64;
        s.write_raw_desc(0, b, 16, F_WRITE | F_NEXT, 1); // writable
        s.write_raw_desc(1, b, 16, 0, 0); // readable после writable
        s.push_avail(0);
    })
}

// Индиректный дескриптор с флагом NEXT (спека запрещает).
pub fn t_indirect_with_next(sock: &str) -> TR {
    run_malformed(sock, |s| {
        let t = s.alloc(64) as u64;
        s.write_raw_desc(0, t, 16, F_INDIRECT | F_NEXT, 1);
        s.push_avail(0);
    })
}

// Вложенный индиректный дескриптор (indirect внутри indirect-таблицы) — запрещён.
pub fn t_nested_indirect(sock: &str) -> TR {
    run_malformed(sock, |s| {
        let b = s.alloc(64) as u64;
        let t = s.alloc(64);
        // запись таблицы: entry0 сам indirect
        s.mem.w64(t, b);
        s.mem.w32(t + 8, 16);
        s.mem.w16(t + 12, F_INDIRECT);
        s.mem.w16(t + 14, 0);
        s.write_raw_desc(0, t as u64, 16, F_INDIRECT, 0);
        s.push_avail(0);
    })
}

// Длина indirect-таблицы не кратна размеру дескриптора (16).
pub fn t_indirect_bad_table_len(sock: &str) -> TR {
    run_malformed(sock, |s| {
        let t = s.alloc(64) as u64;
        s.write_raw_desc(0, t, 20, F_INDIRECT, 0); // 20 % 16 != 0
        s.push_avail(0);
    })
}

// head-индекс в avail-кольце за пределами таблицы (>= qsz).
pub fn t_avail_head_oob(sock: &str) -> TR {
    run_malformed(sock, |s| {
        s.push_avail(dev::QSZ + 5); // без валидного дескриптора
    })
}

// ==== валидация на уровне blk (запрос завершается IOERR, очередь ЖИВА) =======

// GET_ID с буфером неверного размера (не 20) → IOERR (проверка handle_getid).
pub fn t_getid_wrong_size(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    let hdr = s.alloc(16);
    s.wr_hdr(hdr, dev::VIRTIO_BLK_T_GET_ID, 0);
    let d = s.alloc(10);
    s.mem.zero(d, 10);
    let st = s.alloc(1);
    s.mem.wr(st, &[dev::STATUS_POISON]);
    s.submit(&[r(hdr, 16), w(d, 10), w(st, 1)], TMO)?;
    let stv = s.status_at(st);
    check!(
        stv == dev::VIRTIO_BLK_S_IOERR,
        "get_id с буфером 10 байт: status={} (ожидался IOERR)",
        stv
    );
    let (rst, _) = s.blk_read(0, SEC)?; // очередь жива
    dev::expect_ok("после get_id wrong-size", rst)
}

// DISCARD с ДВУМЯ сегментами payload → IOERR (проверяет документированное
// ограничение «поддерживаем один сегмент», niov_out != 2).
pub fn t_discard_multi_segment(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    if !s.has_feature(dev::VIRTIO_BLK_F_DISCARD) {
        skip!("DISCARD не согласован");
    }
    let hdr = s.alloc(16);
    s.wr_hdr(hdr, dev::VIRTIO_BLK_T_DISCARD, 0);
    let s1 = s.alloc(16);
    s.mem.zero(s1, 16);
    let s2 = s.alloc(16);
    s.mem.zero(s2, 16);
    let st = s.alloc(1);
    s.mem.wr(st, &[dev::STATUS_POISON]);
    s.submit(&[r(hdr, 16), r(s1, 16), r(s2, 16), w(st, 1)], TMO)?;
    let stv = s.status_at(st);
    check!(
        stv == dev::VIRTIO_BLK_S_IOERR,
        "discard с 2 сегментами: status={} (ожидался IOERR)",
        stv
    );
    let (rst, _) = s.blk_read(0, SEC)?;
    dev::expect_ok("после discard multi-seg", rst)
}

// DISCARD за пределами ёмкости → IOERR (overflow-safe bounds).
pub fn t_discard_beyond_capacity(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    if !s.has_feature(dev::VIRTIO_BLK_F_DISCARD) {
        skip!("DISCARD не согласован");
    }
    let cap = s.capacity_sectors();
    if cap == 0 {
        skip!("ёмкость неизвестна");
    }
    let st = s.blk_zeroes_or_discard(dev::VIRTIO_BLK_T_DISCARD, cap + 1_000_000, 8, false)?;
    check!(
        st == dev::VIRTIO_BLK_S_IOERR,
        "discard за границей: status={} (ожидался IOERR)",
        st
    );
    let (rst, _) = s.blk_read(0, SEC)?;
    dev::expect_ok("после discard beyond-cap", rst)
}

// Чтение с sector=u64::MAX, len=2 сектора → sector+len переполняет u64.
// overflow-safe проверка (nsectors>cap || sector>cap-nsectors) обязана отвергнуть.
// Наивная проверка (sector+nsectors>cap) ошибочно пропустила бы (wrap в 1).
pub fn t_sector_overflow(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    let (st, _) = s.blk_read(u64::MAX, 2 * SEC)?;
    check!(
        st == dev::VIRTIO_BLK_S_IOERR,
        "чтение sector=u64::MAX (+2): status={} (ожидался IOERR; наивная проверка пропустила бы из-за переполнения)",
        st
    );
    let (rst, _) = s.blk_read(0, SEC)?;
    dev::expect_ok("после overflow-запроса", rst)
}

// DISCARD с нулевым числом секторов — фиксируем поведение (спека допускает,
// libvhost-server не отсекает num_sectors==0 явно). Провал только при креше/зависании.
pub fn t_discard_zero_sectors(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    if !s.has_feature(dev::VIRTIO_BLK_F_DISCARD) {
        skip!("DISCARD не согласован");
    }
    let st = s.blk_zeroes_or_discard(dev::VIRTIO_BLK_T_DISCARD, dev::WORK, 0, false)?;
    check!(st <= 2, "discard 0 секторов: недопустимый status={}", st);
    let (rst, _) = s.blk_read(0, SEC)?;
    dev::expect_ok("после discard 0 секторов", rst)
}

// Запись на readonly-устройство → IOERR (только если согласован RO).
pub fn t_readonly_write(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    if !s.has_feature(dev::VIRTIO_BLK_F_RO) {
        skip!("устройство не readonly (VIRTIO_BLK_F_RO не согласован)");
    }
    let st = s.blk_write(dev::WORK, &dev::pat(0x77, SEC))?;
    check!(
        st == dev::VIRTIO_BLK_S_IOERR,
        "запись на RO-устройство: status={} (ожидался IOERR)",
        st
    );
    Ok(())
}
