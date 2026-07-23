// Тесты плоскости данных virtio-blk: корректность на нетривиальных (но валидных)
// раскладках дескрипторов, типы запросов, границы, механика очереди.
// Каждый тест открывает СВОЮ сессию (отдельное подключение к сокету).

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

// ==== smoke =================================================================

pub fn t_handshake(sock: &str) -> TR {
    let s = Session::connect(sock)?;
    // Доход до сюда = handshake прошёл. Ёмкость печатаем как инфо (0 = нет CONFIG).
    let _ = s.capacity_sectors();
    Ok(())
}

pub fn t_read0(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    let (st, _) = s.blk_read(0, SEC)?;
    dev::expect_ok("read sector 0", st)
}

// ==== целостность ===========================================================

pub fn t_roundtrip(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    dev::need_cap(&s, 4)?;
    let pat = dev::pat(0xa5, SEC);
    dev::expect_ok("write", s.blk_write(dev::WORK, &pat)?)?;
    let (st, data) = s.blk_read(dev::WORK, SEC)?;
    dev::expect_ok("read", st)?;
    dev::same("roundtrip", &data, &pat)
}

pub fn t_multiblock(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    dev::need_cap(&s, 16)?;
    // 4 соседних сектора разными паттернами — ловит перепутанные смещения.
    for i in 0..4u64 {
        let p = dev::pat((0x10 + i) as u8, SEC);
        dev::expect_ok("write blk", s.blk_write(dev::WORK + i, &p)?)?;
    }
    for i in 0..4u64 {
        let p = dev::pat((0x10 + i) as u8, SEC);
        let (st, data) = s.blk_read(dev::WORK + i, SEC)?;
        dev::expect_ok("read blk", st)?;
        dev::same(&format!("blk {}", i), &data, &p)?;
    }
    Ok(())
}

pub fn t_overwrite(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    dev::need_cap(&s, 4)?;
    dev::expect_ok("write A", s.blk_write(dev::WORK, &dev::pat(0x01, SEC))?)?;
    let b = dev::pat(0x02, SEC);
    dev::expect_ok("write B", s.blk_write(dev::WORK, &b)?)?;
    let (st, data) = s.blk_read(dev::WORK, SEC)?;
    dev::expect_ok("read", st)?;
    dev::same("overwrite", &data, &b)
}

// ==== раскладки дескрипторов (⭐ неочевидные, но по спеке) ===================

// Заголовок virtio_blk_outhdr, разбитый на 2 device-readable дескриптора 8+8.
// Спека допускает произвольное дробление запроса по дескрипторам; многие бэкенды
// ошибочно считают, что 16-байтный заголовок — ровно один дескриптор.
pub fn t_hdr_split2(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    dev::need_cap(&s, 4)?;
    let sector = dev::WORK;
    let pat = dev::pat(0xb1, SEC);

    let h = s.alloc(16);
    s.wr_hdr(h, dev::VIRTIO_BLK_T_OUT, sector);
    let d = s.alloc(SEC);
    s.mem.wr(d, &pat);
    let st = s.alloc(1);
    s.mem.wr(st, &[dev::STATUS_POISON]);
    s.submit(&[r(h, 8), r(h + 8, 8), r(d, SEC), w(st, 1)], TMO)?;
    dev::expect_ok("write (split hdr 8+8)", s.status_at(st))?;

    let (rst, data) = s.blk_read(sector, SEC)?;
    dev::expect_ok("read back", rst)?;
    dev::same("split-hdr data", &data, &pat)
}

// Заголовок, разбитый на 4 дескриптора по 4 байта.
pub fn t_hdr_split4(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    dev::need_cap(&s, 4)?;
    let sector = dev::WORK + 1;
    let pat = dev::pat(0xb2, SEC);
    let h = s.alloc(16);
    s.wr_hdr(h, dev::VIRTIO_BLK_T_OUT, sector);
    let d = s.alloc(SEC);
    s.mem.wr(d, &pat);
    let st = s.alloc(1);
    s.mem.wr(st, &[dev::STATUS_POISON]);
    s.submit(
        &[
            r(h, 4),
            r(h + 4, 4),
            r(h + 8, 4),
            r(h + 12, 4),
            r(d, SEC),
            w(st, 1),
        ],
        TMO,
    )?;
    dev::expect_ok("write (split hdr 4x4)", s.status_at(st))?;
    let (rst, data) = s.blk_read(sector, SEC)?;
    dev::expect_ok("read back", rst)?;
    dev::same("split4-hdr data", &data, &pat)
}

// WRITE, где заголовок И данные лежат в ОДНОМ device-readable дескрипторе.
// Проверяет, что бэкенд режет поток запроса по байтам, а не по границам дескрипторов.
pub fn t_hdr_data_contiguous_write(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    dev::need_cap(&s, 4)?;
    let sector = dev::WORK + 2;
    let pat = dev::pat(0xb3, SEC);
    let buf = s.alloc(16 + SEC);
    s.wr_hdr(buf, dev::VIRTIO_BLK_T_OUT, sector);
    s.mem.wr(buf + 16, &pat);
    let st = s.alloc(1);
    s.mem.wr(st, &[dev::STATUS_POISON]);
    s.submit(&[r(buf, 16 + SEC), w(st, 1)], TMO)?;
    dev::expect_ok("write (hdr+data в одном desc)", s.status_at(st))?;
    let (rst, data) = s.blk_read(sector, SEC)?;
    dev::expect_ok("read back", rst)?;
    dev::same("contiguous hdr+data", &data, &pat)
}

// WRITE, где данные разбросаны по 8 device-readable дескрипторам по 512.
pub fn t_scatter_write(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    dev::need_cap(&s, 16)?;
    let sector = dev::WORK;
    let total = 8 * SEC;
    let pat = dev::pat(0x5a, total);
    let h = s.alloc(16);
    s.wr_hdr(h, dev::VIRTIO_BLK_T_OUT, sector);
    let d = s.alloc(total);
    s.mem.wr(d, &pat);
    let st = s.alloc(1);
    s.mem.wr(st, &[dev::STATUS_POISON]);
    let mut descs = vec![r(h, 16)];
    let mut o = 0;
    while o < total {
        descs.push(r(d + o, SEC));
        o += SEC;
    }
    descs.push(w(st, 1));
    s.submit(&descs, TMO)?;
    dev::expect_ok("scatter write", s.status_at(st))?;
    let (rst, data) = s.blk_read(sector, total)?;
    dev::expect_ok("read back", rst)?;
    dev::same("scatter-write data", &data, &pat)
}

// READ в 8 разбросанных device-writable дескрипторов по 512.
pub fn t_scatter_read(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    dev::need_cap(&s, 16)?;
    let sector = dev::WORK + 8;
    let total = 8 * SEC;
    let pat = dev::pat(0x6b, total);
    dev::expect_ok("prewrite", s.blk_write(sector, &pat)?)?;

    let h = s.alloc(16);
    s.wr_hdr(h, dev::VIRTIO_BLK_T_IN, sector);
    let d = s.alloc(total);
    s.mem.zero(d, total);
    let st = s.alloc(1);
    s.mem.wr(st, &[dev::STATUS_POISON]);
    let mut descs = vec![r(h, 16)];
    let mut o = 0;
    while o < total {
        descs.push(w(d + o, SEC));
        o += SEC;
    }
    descs.push(w(st, 1));
    s.submit(&descs, TMO)?;
    dev::expect_ok("scatter read", s.status_at(st))?;
    let mut got = vec![0u8; total];
    s.mem.rd(d, &mut got);
    dev::same("scatter-read data", &got, &pat)
}

// READ в сегменты неравной длины 512+1024+512+2048 = 4096.
pub fn t_uneven_segments(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    dev::need_cap(&s, 16)?;
    let sector = dev::WORK + 16;
    let total = 4096;
    let pat = dev::pat(0x7c, total);
    dev::expect_ok("prewrite", s.blk_write(sector, &pat)?)?;
    let h = s.alloc(16);
    s.wr_hdr(h, dev::VIRTIO_BLK_T_IN, sector);
    let d = s.alloc(total);
    s.mem.zero(d, total);
    let st = s.alloc(1);
    s.mem.wr(st, &[dev::STATUS_POISON]);
    let sizes = [512usize, 1024, 512, 2048];
    let mut descs = vec![r(h, 16)];
    let mut o = 0;
    for sz in sizes {
        descs.push(w(d + o, sz));
        o += sz;
    }
    descs.push(w(st, 1));
    s.submit(&descs, TMO)?;
    dev::expect_ok("uneven read", s.status_at(st))?;
    let mut got = vec![0u8; total];
    s.mem.rd(d, &mut got);
    dev::same("uneven-seg data", &got, &pat)
}

// READ в 64 сегмента по 512 — длинная цепочка (проверка seg_max/итератора).
pub fn t_many_segments(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    dev::need_cap(&s, 80)?;
    let sector = dev::WORK + 24;
    let segs = 64usize;
    let total = segs * SEC;
    let pat = dev::pat(0x8d, total);
    dev::expect_ok("prewrite", s.blk_write(sector, &pat)?)?;
    let h = s.alloc(16);
    s.wr_hdr(h, dev::VIRTIO_BLK_T_IN, sector);
    let d = s.alloc(total);
    s.mem.zero(d, total);
    let st = s.alloc(1);
    s.mem.wr(st, &[dev::STATUS_POISON]);
    let mut descs = vec![r(h, 16)];
    for i in 0..segs {
        descs.push(w(d + i * SEC, SEC));
    }
    descs.push(w(st, 1));
    s.submit(&descs, TMO)?;
    dev::expect_ok("64-seg read", s.status_at(st))?;
    let mut got = vec![0u8; total];
    s.mem.rd(d, &mut got);
    dev::same("64-seg data", &got, &pat)
}

// Дескриптор status длиннее 1 байта — устройство пишет ровно 1 байт статуса.
pub fn t_status_oversized(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    dev::need_cap(&s, 4)?;
    let sector = dev::WORK + 3;
    let pat = dev::pat(0x9e, SEC);
    dev::expect_ok("prewrite", s.blk_write(sector, &pat)?)?;
    let h = s.alloc(16);
    s.wr_hdr(h, dev::VIRTIO_BLK_T_IN, sector);
    let d = s.alloc(SEC);
    s.mem.zero(d, SEC);
    let st = s.alloc(4);
    s.mem.wr(st, &[dev::STATUS_POISON; 4]);
    s.submit(&[r(h, 16), w(d, SEC), w(st, 4)], TMO)?;
    dev::expect_ok("status в 4-байтном desc", s.status_at(st))?;
    let mut got = vec![0u8; SEC];
    s.mem.rd(d, &mut got);
    dev::same("oversized-status data", &got, &pat)
}

// Индиректная дескрипторная таблица (VIRTIO_F_RING_INDIRECT_DESC).
pub fn t_indirect_rw(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    if !s.has_feature(dev::VIRTIO_F_RING_INDIRECT_DESC) {
        skip!("INDIRECT_DESC не согласован");
    }
    dev::need_cap(&s, 4)?;
    let sector = dev::WORK + 4;
    let pat = dev::pat(0xc4, SEC);
    dev::expect_ok("prewrite", s.blk_write(sector, &pat)?)?;

    let h = s.alloc(16);
    s.wr_hdr(h, dev::VIRTIO_BLK_T_IN, sector);
    let d = s.alloc(SEC);
    s.mem.zero(d, SEC);
    let st = s.alloc(1);
    s.mem.wr(st, &[dev::STATUS_POISON]);
    let table = s.alloc(3 * 16);
    let head = s.post_indirect(&[r(h, 16), w(d, SEC), w(st, 1)], table);
    s.kick();
    let u = s
        .wait_used(TMO)
        .ok_or(TestErr::Fail("ЗАВИСАНИЕ (indirect)".into()))?;
    check!(u.id == head as u32, "indirect: used.id={} != head={}", u.id, head);
    dev::expect_ok("indirect read", s.status_at(st))?;
    let mut got = vec![0u8; SEC];
    s.mem.rd(d, &mut got);
    dev::same("indirect data", &got, &pat)
}

// ==== типы запросов =========================================================

pub fn t_flush(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    if !s.has_feature(dev::VIRTIO_BLK_F_FLUSH) {
        skip!("FLUSH не согласован");
    }
    dev::need_cap(&s, 4)?;
    dev::expect_ok("write", s.blk_write(dev::WORK, &dev::pat(0x33, SEC))?)?;
    dev::expect_ok("flush", s.blk_flush()?)
}

// FLUSH с ненулевым sector в заголовке: sector зарезервирован, устройство игнорирует.
pub fn t_flush_nonzero_sector(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    if !s.has_feature(dev::VIRTIO_BLK_F_FLUSH) {
        skip!("FLUSH не согласован");
    }
    let h = s.alloc(16);
    s.wr_hdr(h, dev::VIRTIO_BLK_T_FLUSH, dev::WORK); // sector != 0
    let st = s.alloc(1);
    s.mem.wr(st, &[dev::STATUS_POISON]);
    s.submit(&[r(h, 16), w(st, 1)], TMO)?;
    dev::expect_ok("flush с ненулевым sector (должен игнорироваться)", s.status_at(st))
}

pub fn t_get_id(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    let (st, id) = s.blk_get_id()?;
    dev::expect_ok("get_id", st)?;
    // ID — до 20 байт, обычно ASCII-строка с нулём. Проверяем лишь адекватность.
    check!(id.len() == 20, "get_id вернул {} байт (ожидалось 20)", id.len());
    Ok(())
}

pub fn t_write_zeroes(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    if !s.has_feature(dev::VIRTIO_BLK_F_WRITE_ZEROES) {
        skip!("WRITE_ZEROES не согласован");
    }
    dev::need_cap(&s, 16)?;
    let sector = dev::WORK;
    let n = 8u32;
    let total = n as usize * SEC;
    dev::expect_ok("prewrite", s.blk_write(sector, &dev::pat(0xee, total))?)?;
    dev::expect_ok(
        "write-zeroes",
        s.blk_zeroes_or_discard(dev::VIRTIO_BLK_T_WRITE_ZEROES, sector, n, false)?,
    )?;
    let (rst, data) = s.blk_read(sector, total)?;
    dev::expect_ok("read after zeroes", rst)?;
    dev::all_zero("зона после write-zeroes", &data)
}

pub fn t_write_zeroes_unmap(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    if !s.has_feature(dev::VIRTIO_BLK_F_WRITE_ZEROES) {
        skip!("WRITE_ZEROES не согласован");
    }
    dev::need_cap(&s, 16)?;
    let sector = dev::WORK + 8;
    let n = 8u32;
    let total = n as usize * SEC;
    dev::expect_ok("prewrite", s.blk_write(sector, &dev::pat(0xef, total))?)?;
    dev::expect_ok(
        "write-zeroes -u",
        s.blk_zeroes_or_discard(dev::VIRTIO_BLK_T_WRITE_ZEROES, sector, n, true)?,
    )?;
    let (rst, data) = s.blk_read(sector, total)?;
    dev::expect_ok("read after zeroes -u", rst)?;
    dev::all_zero("зона после write-zeroes -u", &data)
}

pub fn t_discard(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    if !s.has_feature(dev::VIRTIO_BLK_F_DISCARD) {
        skip!("DISCARD не согласован");
    }
    dev::need_cap(&s, 16)?;
    let sector = dev::WORK;
    let n = 8u32;
    let total = n as usize * SEC;
    dev::expect_ok("prewrite", s.blk_write(sector, &dev::pat(0xab, total))?)?;
    // Содержимое после discard не определено — проверяем лишь, что область снова
    // записываема и перезапись видна.
    dev::expect_ok(
        "discard",
        s.blk_zeroes_or_discard(dev::VIRTIO_BLK_T_DISCARD, sector, n, false)?,
    )?;
    let np = dev::pat(0xba, total);
    dev::expect_ok("rewrite after discard", s.blk_write(sector, &np)?)?;
    let (rst, data) = s.blk_read(sector, total)?;
    dev::expect_ok("read after rewrite", rst)?;
    dev::same("перезапись после discard", &data, &np)
}

// Неизвестный тип запроса → устройство обязано ответить UNSUPP/IOERR, не упасть.
pub fn t_unsupported_type(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    let h = s.alloc(16);
    s.wr_hdr(h, 0x5EAD_BEEF, 0);
    let d = s.alloc(SEC);
    s.mem.zero(d, SEC);
    let st = s.alloc(1);
    s.mem.wr(st, &[dev::STATUS_POISON]);
    s.submit(&[r(h, 16), w(d, SEC), w(st, 1)], TMO)?;
    let stv = s.status_at(st);
    check!(
        stv == dev::VIRTIO_BLK_S_UNSUPP || stv == dev::VIRTIO_BLK_S_IOERR,
        "неизвестный тип: status={} (ожидался UNSUPP=2 или IOERR=1)",
        stv
    );
    // устройство живо в той же сессии
    let (rst, _) = s.blk_read(0, SEC)?;
    dev::expect_ok("после неизвестного типа", rst)
}

// Запрос IN без единого data-дескриптора (0 секторов).
pub fn t_zero_length_read(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    let h = s.alloc(16);
    s.wr_hdr(h, dev::VIRTIO_BLK_T_IN, dev::WORK);
    let st = s.alloc(1);
    s.mem.wr(st, &[dev::STATUS_POISON]);
    s.submit(&[r(h, 16), w(st, 1)], TMO)?;
    let stv = s.status_at(st);
    check!(stv <= 2, "0-длина: недопустимый status={}", stv);
    let (rst, _) = s.blk_read(0, SEC)?;
    dev::expect_ok("после 0-длины жив", rst)
}

// Длина данных не кратна сектору (512+100). Устройство должно обработать корректно
// (OK либо IOERR), но не зависнуть и не выдать мусорный статус.
pub fn t_nonmultiple_length(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    dev::need_cap(&s, 4)?;
    let h = s.alloc(16);
    s.wr_hdr(h, dev::VIRTIO_BLK_T_IN, dev::WORK);
    let len = SEC + 100;
    let d = s.alloc(len);
    s.mem.zero(d, len);
    let st = s.alloc(1);
    s.mem.wr(st, &[dev::STATUS_POISON]);
    s.submit(&[r(h, 16), w(d, len), w(st, 1)], TMO)?;
    let stv = s.status_at(st);
    check!(stv <= 2, "некратная длина: недопустимый status={}", stv);
    let (rst, _) = s.blk_read(0, SEC)?;
    dev::expect_ok("после некратной длины жив", rst)
}

// ==== границы ===============================================================

pub fn t_last_sector(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    let cap = dev::need_cap(&s, 0)?;
    let sector = cap - 1;
    let pat = dev::pat(0x11, SEC);
    dev::expect_ok("write last", s.blk_write(sector, &pat)?)?;
    let (st, data) = s.blk_read(sector, SEC)?;
    dev::expect_ok("read last", st)?;
    dev::same("last-sector", &data, &pat)
}

// Чтение, выходящее за ёмкость (последний сектор + ещё один) → IOERR, не OK.
pub fn t_cross_capacity(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    let cap = dev::need_cap(&s, 0)?;
    let (st, _) = s.blk_read(cap - 1, 2 * SEC)?;
    check!(
        st != dev::VIRTIO_BLK_S_OK,
        "чтение через границу вернуло OK (ожидался IOERR)"
    );
    let (rst, _) = s.blk_read(0, SEC)?;
    dev::expect_ok("после cross-границы жив", rst)
}

pub fn t_beyond_capacity(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    let cap = dev::need_cap(&s, 0)?;
    let (st, _) = s.blk_read(cap + 1000, SEC)?;
    check!(
        st != dev::VIRTIO_BLK_S_OK,
        "чтение за пределами ёмкости вернуло OK (ожидался IOERR)"
    );
    let (rst, _) = s.blk_read(0, SEC)?;
    dev::expect_ok("после out-of-range жив", rst)
}

// Крупный запрос: 128 KiB одной цепочкой из 32×4KiB — длинная цепочка + объём.
pub fn t_large_request(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    dev::need_cap(&s, 300)?;
    let sector = dev::WORK + 100;
    let seg = 4096usize;
    let segs = 32usize;
    let total = seg * segs;
    let pat = dev::pat(0xd7, total);

    // запись цепочкой
    let h = s.alloc(16);
    s.wr_hdr(h, dev::VIRTIO_BLK_T_OUT, sector);
    let d = s.alloc(total);
    s.mem.wr(d, &pat);
    let st = s.alloc(1);
    s.mem.wr(st, &[dev::STATUS_POISON]);
    let mut descs = vec![r(h, 16)];
    for i in 0..segs {
        descs.push(r(d + i * seg, seg));
    }
    descs.push(w(st, 1));
    s.submit(&descs, TMO)?;
    dev::expect_ok("large write", s.status_at(st))?;

    let (rst, data) = s.blk_read(sector, total)?;
    dev::expect_ok("large read", rst)?;
    dev::same("large-request", &data, &pat)
}

// ==== механика очереди ======================================================

// Много запросов in-flight сразу (16 записей), затем дренаж; проверяем completion
// каждого и что данные осели.
pub fn t_multi_outstanding(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    dev::need_cap(&s, 80)?;
    let n = 16u64;
    let mut plan: Vec<(u32, u64, usize, Vec<u8>)> = Vec::new();
    for i in 0..n {
        let sector = dev::WORK + i * 4;
        let pat = dev::pat((0x40 + i) as u8, SEC);
        let h = s.alloc(16);
        s.wr_hdr(h, dev::VIRTIO_BLK_T_OUT, sector);
        let d = s.alloc(SEC);
        s.mem.wr(d, &pat);
        let st = s.alloc(1);
        s.mem.wr(st, &[dev::STATUS_POISON]);
        let head = s.post(&[r(h, 16), r(d, SEC), w(st, 1)]);
        plan.push((head as u32, sector, st, pat));
    }
    s.kick();
    let mut done = 0u64;
    while done < n {
        match s.wait_used(TMO) {
            Some(u) => {
                let e = plan
                    .iter()
                    .find(|e| e.0 == u.id)
                    .ok_or(TestErr::Fail(format!("неизвестный used.id={}", u.id)))?;
                let stv = s.status_at(e.2);
                check!(stv == 0, "write sector {} status={}", e.1, stv);
                done += 1;
            }
            None => fail!("ЗАВИСАНИЕ: получено {}/{} завершений", done, n),
        }
    }
    // всё осело
    for (_, sector, _, pat) in &plan {
        let (stv, data) = s.blk_read(*sector, SEC)?;
        dev::expect_ok("readback", stv)?;
        dev::same("multi-outstanding", &data, pat)?;
    }
    Ok(())
}

// Несколько чтений in-flight; сопоставляем завершения по used.id и проверяем,
// что каждое вернуло данные СВОЕГО сектора (ловит перепутанные завершения).
pub fn t_out_of_order(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    dev::need_cap(&s, 64)?;
    let n = 8u64;
    for i in 0..n {
        dev::expect_ok(
            "prewrite",
            s.blk_write(dev::WORK + i * 2, &dev::pat((0x70 + i) as u8, SEC))?,
        )?;
    }
    let mut plan: Vec<(u32, u64, usize, Vec<u8>)> = Vec::new();
    for i in 0..n {
        let sector = dev::WORK + i * 2;
        let h = s.alloc(16);
        s.wr_hdr(h, dev::VIRTIO_BLK_T_IN, sector);
        let d = s.alloc(SEC);
        s.mem.zero(d, SEC);
        let st = s.alloc(1);
        s.mem.wr(st, &[dev::STATUS_POISON]);
        let head = s.post(&[r(h, 16), w(d, SEC), w(st, 1)]);
        plan.push((head as u32, sector, d, dev::pat((0x70 + i) as u8, SEC)));
    }
    s.kick();
    let mut done = 0u64;
    while done < n {
        match s.wait_used(TMO) {
            Some(u) => {
                let e = plan
                    .iter()
                    .find(|e| e.0 == u.id)
                    .ok_or(TestErr::Fail(format!("неизвестный used.id={}", u.id)))?;
                let mut got = vec![0u8; SEC];
                s.mem.rd(e.2, &mut got);
                dev::same(&format!("read sector {}", e.1), &got, &e.3)?;
                done += 1;
            }
            None => fail!("ЗАВИСАНИЕ: {}/{} завершений", done, n),
        }
    }
    Ok(())
}

// used.len для чтения обязан быть (данные + 1 байт status). Многие бэкенды
// ошибочно ставят только длину данных или 0 — это несоответствие спеке.
pub fn t_used_len_read(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    dev::need_cap(&s, 16)?;
    let sector = dev::WORK;
    let n = 8 * SEC; // 4096
    dev::expect_ok("prewrite", s.blk_write(sector, &dev::pat(0x21, n))?)?;
    let h = s.alloc(16);
    s.wr_hdr(h, dev::VIRTIO_BLK_T_IN, sector);
    let d = s.alloc(n);
    s.mem.zero(d, n);
    let st = s.alloc(1);
    s.mem.wr(st, &[dev::STATUS_POISON]);
    let u = s.submit(&[r(h, 16), w(d, n), w(st, 1)], TMO)?;
    dev::expect_ok("read", s.status_at(st))?;
    let want = (n + 1) as u32;
    check!(
        u.len == want,
        "used.len={} (спека: данные+status={}). Значение {} (только данные) или 0 — известное нарушение",
        u.len,
        want,
        n
    );
    Ok(())
}

// used.len для записи обязан быть 1 (записан только байт status).
pub fn t_used_len_write(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    dev::need_cap(&s, 8)?;
    let sector = dev::WORK + 200;
    let n = 4 * SEC;
    let h = s.alloc(16);
    s.wr_hdr(h, dev::VIRTIO_BLK_T_OUT, sector);
    let d = s.alloc(n);
    s.mem.wr(d, &dev::pat(0x22, n));
    let st = s.alloc(1);
    s.mem.wr(st, &[dev::STATUS_POISON]);
    let u = s.submit(&[r(h, 16), r(d, n), w(st, 1)], TMO)?;
    dev::expect_ok("write", s.status_at(st))?;
    check!(
        u.len == 1,
        "used.len={} для записи (спека: только status = 1 байт)",
        u.len
    );
    Ok(())
}

// Kick без новых avail-элементов: устройство не должно фабриковать завершение.
pub fn t_spurious_kick(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    s.kick();
    if let Some(u) = s.wait_used(300) {
        fail!("устройство вернуло завершение id={} без запроса", u.id);
    }
    // всё ещё обслуживает
    let (st, _) = s.blk_read(0, SEC)?;
    dev::expect_ok("после spurious kick", st)
}

// Флаг VRING_AVAIL_F_NO_INTERRUPT: устройство всё равно обязано обработать запрос
// и обновить used ring (уведомление подавляется, обработка — нет).
pub fn t_no_interrupt(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    dev::need_cap(&s, 4)?;
    let sector = dev::WORK;
    let pat = dev::pat(0x55, SEC);
    dev::expect_ok("prewrite", s.blk_write(sector, &pat)?)?;
    s.set_avail_flags(dev::VRING_AVAIL_F_NO_INTERRUPT);
    let (st, data) = s.blk_read(sector, SEC)?; // wait_used опрашивает память, не callfd
    dev::expect_ok("read с NO_INTERRUPT", st)?;
    dev::same("данные с NO_INTERRUPT", &data, &pat)
}

// Двойной kick на один запрос: ровно одно завершение.
pub fn t_double_kick(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    dev::need_cap(&s, 4)?;
    let sector = dev::WORK;
    let pat = dev::pat(0x66, SEC);
    dev::expect_ok("prewrite", s.blk_write(sector, &pat)?)?;
    let h = s.alloc(16);
    s.wr_hdr(h, dev::VIRTIO_BLK_T_IN, sector);
    let d = s.alloc(SEC);
    s.mem.zero(d, SEC);
    let st = s.alloc(1);
    s.mem.wr(st, &[dev::STATUS_POISON]);
    let head = s.post(&[r(h, 16), w(d, SEC), w(st, 1)]);
    s.kick();
    s.kick();
    let u = s
        .wait_used(TMO)
        .ok_or(TestErr::Fail("ЗАВИСАНИЕ (double kick)".into()))?;
    check!(u.id == head as u32, "used.id={} != head={}", u.id, head);
    if let Some(u2) = s.wait_used(300) {
        fail!("двойное завершение: лишний used id={}", u2.id);
    }
    let mut got = vec![0u8; SEC];
    s.mem.rd(d, &mut got);
    dev::same("double-kick data", &got, &pat)
}
