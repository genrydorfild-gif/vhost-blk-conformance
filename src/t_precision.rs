// Тесты на НЕТОЧНУЮ имплементацию протокола → функциональные ошибки (не уязвимости).
// Здесь мастер корректный; проверяем, что бэкенд точно соблюдает семантику:
// учёт индексов колец, уведомления, гейтинг по согласованным фичам, поля config.

use crate::dev::{self, r, w, Session, TestErr, TR};
use vhost::VhostBackend; // для set_vring_num / set_vring_base через fe_mut()

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

// SET_VRING_BASE(n) → GET_VRING_BASE обязан вернуть n (round-trip last_avail).
pub fn t_vring_base_roundtrip(sock: &str) -> TR {
    let mut s = Session::handshake(sock, true, false)?;
    s.fe_mut()
        .set_vring_num(0, dev::QSZ)
        .map_err(|e| format!("set_vring_num: {:?}", e))?;
    s.fe_mut()
        .set_vring_base(0, 7)
        .map_err(|e| format!("set_vring_base: {:?}", e))?;
    let got = s.get_vring_base()?;
    check!(
        got == 7,
        "GET_VRING_BASE вернул {} после SET_VRING_BASE(7) — неверный учёт last_avail",
        got
    );
    Ok(())
}

// После N обработанных запросов GET_VRING_BASE == N (устройство точно двигает
// last_avail). Критично для остановки/миграции: неверный last_avail = потеря или
// повтор запросов при возобновлении.
pub fn t_vring_base_tracks_consumed(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    for _ in 0..3 {
        let (st, _) = s.blk_read(0, SEC)?;
        dev::expect_ok("read", st)?;
    }
    let got = s.get_vring_base()?;
    check!(
        got == 3,
        "GET_VRING_BASE={} после 3 запросов (ожидалось 3 = last_avail); \
         неверный учёт сломает остановку/миграцию",
        got
    );
    Ok(())
}

// При обычных условиях (без NO_INTERRUPT) устройство ОБЯЗАНО уведомить callfd.
pub fn t_notify_signaled(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    s.drain_call();
    let hdr = s.alloc(16);
    s.wr_hdr(hdr, dev::VIRTIO_BLK_T_IN, 0);
    let d = s.alloc(SEC);
    s.mem.zero(d, SEC);
    let st = s.alloc(1);
    s.mem.wr(st, &[dev::STATUS_POISON]);
    let head = s.post(&[r(hdr, 16), w(d, SEC), w(st, 1)]);
    s.kick();
    let u = s
        .wait_used(TMO)
        .ok_or(TestErr::Fail("ЗАВИСАНИЕ".into()))?;
    check!(u.id == head as u32, "used.id={} != head={}", u.id, head);
    check!(
        s.wait_call(1000),
        "устройство НЕ уведомило (callfd) при разрешённых уведомлениях — драйвер завис бы"
    );
    Ok(())
}

// При VRING_AVAIL_F_NO_INTERRUPT устройство обязано ОБРАБОТАТЬ, но НЕ уведомлять.
pub fn t_notify_suppressed(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    s.set_avail_flags(dev::VRING_AVAIL_F_NO_INTERRUPT);
    s.drain_call();
    let hdr = s.alloc(16);
    s.wr_hdr(hdr, dev::VIRTIO_BLK_T_IN, 0);
    let d = s.alloc(SEC);
    s.mem.zero(d, SEC);
    let st = s.alloc(1);
    s.mem.wr(st, &[dev::STATUS_POISON]);
    let head = s.post(&[r(hdr, 16), w(d, SEC), w(st, 1)]);
    s.kick();
    let u = s
        .wait_used(TMO)
        .ok_or(TestErr::Fail("ЗАВИСАНИЕ (обработка должна идти и при NO_INTERRUPT)".into()))?;
    check!(u.id == head as u32, "used.id={} != head={}", u.id, head);
    check!(
        !s.wait_call(300),
        "устройство уведомило (callfd) ВОПРЕКИ VRING_AVAIL_F_NO_INTERRUPT — лишние прерывания"
    );
    Ok(())
}

// Гейтинг по НЕГОЦИИРОВАННЫМ фичам: если драйвер НЕ заакал F_DISCARD, запрос
// DISCARD обязан быть отвергнут (UNSUPP), а не выполнен. Ловит бэкенд, который
// смотрит на «что умею», а не «что согласовано».
pub fn t_feature_gating_discard(sock: &str) -> TR {
    let mut s = Session::connect_masking(sock, dev::VIRTIO_BLK_F_DISCARD)?;
    if !s.offered_has(dev::VIRTIO_BLK_F_DISCARD) {
        skip!("устройство не предлагает DISCARD — гейтинг не проверить");
    }
    // F_DISCARD НЕ согласован (masked), но устройство его предлагает. Шлём DISCARD.
    let st = s.blk_zeroes_or_discard(dev::VIRTIO_BLK_T_DISCARD, dev::WORK, 8, false)?;
    check!(
        st == dev::VIRTIO_BLK_S_UNSUPP,
        "DISCARD при НЕсогласованном F_DISCARD → status={} (ожидался UNSUPP=2). \
         Устройство гейтит по ПРЕДЛОЖЕННЫМ, а не НЕГОЦИИРОВАННЫМ фичам — \
         функциональная неточность (обрабатывает несогласованную команду)",
        st
    );
    Ok(())
}

// config.blk_size (если F_BLK_SIZE) обязан быть степенью двойки и >= 512.
pub fn t_config_blk_size_sane(sock: &str) -> TR {
    let s = Session::connect(sock)?;
    if !s.has_feature(dev::VIRTIO_BLK_F_BLK_SIZE) {
        skip!("F_BLK_SIZE не согласован");
    }
    let bs = s.blk_size();
    check!(bs >= 512, "config.blk_size={} (< 512)", bs);
    check!(
        bs & (bs - 1) == 0,
        "config.blk_size={} не степень двойки",
        bs
    );
    Ok(())
}

// 16-битная обёртка индексов колец: стартуем last_avail/used.idx у 0xFFF0 и
// пересекаем 0x10000 десятками операций. Неверная обёртка = порча/зависание.
pub fn t_used_index_wrap(sock: &str) -> TR {
    let mut s = Session::connect_at_base(sock, 0xFFF0)?;
    dev::need_cap(&s, 4)?;
    let sector = dev::WORK;
    let pat = dev::pat(0x5c, SEC);
    // 24 пары write+read = 48 операций: 0xFFF0 + 48 переваливает за 0x10000.
    for i in 0..24u64 {
        dev::expect_ok("write across wrap", s.blk_write(sector, &pat)?)?;
        let (rst, data) = s.blk_read(sector, SEC)?;
        dev::expect_ok("read across wrap", rst)?;
        dev::same(&format!("wrap iter {}", i), &data, &pat)?;
    }
    Ok(())
}
