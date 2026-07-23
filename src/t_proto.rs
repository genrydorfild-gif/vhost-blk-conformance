// Тесты уровня протокола/конфигурации/жизненного цикла и «злого» ввода.
//
// Для «злых» тестов главный критерий — ЖИВУЧЕСТЬ ДЕМОНА: после кривого сообщения
// демон обязан не упасть и не зависнуть (соединение он вправе закрыть — это
// корректная реакция). Проверяем свежим подключением + чтением (dev::alive).
// Мы НЕ утверждаем, что конкретный вызов вернул Err: без REPLY_ACK клиент этого
// достоверно не видит. Ловим именно креши/зависания всего демона.

use crate::dev::{self, Session, TestErr, TR};

use vhost::vhost_user::message::VhostUserConfigFlags;
use vhost::vhost_user::VhostUserFrontend;
use vhost::{VhostBackend, VringConfigData};

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

// ==== config ================================================================

// GET_CONFIG (capacity) согласован с фактическим поведением на границе:
// последний сектор читается, а выход за него — IOERR.
pub fn t_config_capacity_consistency(sock: &str) -> TR {
    let mut s = Session::connect(sock)?;
    let cap = s.capacity_sectors();
    if cap == 0 {
        skip!("нет PROTOCOL_F_CONFIG — ёмкость неизвестна");
    }
    let (st1, _) = s.blk_read(cap - 1, SEC)?;
    dev::expect_ok("чтение последнего сектора из config", st1)?;
    let (st2, _) = s.blk_read(cap - 1, 2 * SEC)?;
    check!(
        st2 != dev::VIRTIO_BLK_S_OK,
        "config.capacity={} секторов, но чтение за границей вернуло OK",
        cap
    );
    Ok(())
}

// Частичное чтение config-пространства совпадает с полным.
pub fn t_config_partial_read(sock: &str) -> TR {
    let mut s = Session::handshake(sock, false, false)?;
    if !s.has_proto(vhost::vhost_user::message::VhostUserProtocolFeatures::CONFIG) {
        skip!("PROTOCOL_F_CONFIG не согласован");
    }
    let full = s
        .fe_mut()
        .get_config(0, 60, VhostUserConfigFlags::empty(), &vec![0u8; 60])
        .map_err(|e| format!("get_config(60): {:?}", e))?
        .1;
    let head = s
        .fe_mut()
        .get_config(0, 8, VhostUserConfigFlags::empty(), &vec![0u8; 8])
        .map_err(|e| format!("get_config(8): {:?}", e))?
        .1;
    check!(head.len() >= 8 && full.len() >= 8, "config короче 8 байт");
    check!(
        head[0..8] == full[0..8],
        "частичный GET_CONFIG(0,8) != первые 8 байт GET_CONFIG(0,60): {:?} vs {:?}",
        &head[0..8],
        &full[0..8]
    );
    Ok(())
}

// GET_FEATURES идемпотентен: два подряд запроса дают одно и то же.
pub fn t_get_features_stable(sock: &str) -> TR {
    let mut s = Session::handshake(sock, false, false)?;
    let a = s.fe_mut().get_features().map_err(|e| format!("get_features a: {:?}", e))?;
    let b = s.fe_mut().get_features().map_err(|e| format!("get_features b: {:?}", e))?;
    check!(a == b, "GET_FEATURES нестабилен: {:#x} != {:#x}", a, b);
    Ok(())
}

// ==== жизненный цикл ========================================================

// Данные, записанные с OK, переживают ПЕРЕПОДКЛЮЧЕНИЕ (новый мастер, новая сессия).
pub fn t_persistence_reconnect(sock: &str) -> TR {
    let sector = dev::WORK;
    let pat = dev::pat(0x9e, SEC);
    {
        let mut s = Session::connect(sock)?;
        dev::need_cap(&s, 4)?;
        dev::expect_ok("write (сессия A)", s.blk_write(sector, &pat)?)?;
    } // drop → закрытие сокета (бэкенд видит disconnect/GET_VRING_BASE)
    let mut s = Session::connect(sock)?;
    let (st, data) = s.blk_read(sector, SEC)?;
    dev::expect_ok("read (сессия B)", st)?;
    dev::same("данные пережили reconnect", &data, &pat)
}

// Стресс переподключения: несколько циклов connect → read.
pub fn t_reconnect_stress(sock: &str) -> TR {
    for i in 0..5 {
        let mut s = Session::connect(sock)?;
        let (st, _) = s
            .blk_read(0, SEC)
            .map_err(|e| format!("цикл {}: {}", i, e))?;
        dev::expect_ok(&format!("reconnect #{}", i), st)?;
    }
    Ok(())
}

// ==== «злой» ввод: демон обязан выжить =====================================

// Повторный SET_OWNER без RESET_OWNER (по спеке — ошибка). Демон не должен падать.
pub fn t_double_set_owner(sock: &str) -> TR {
    {
        let mut s = Session::handshake(sock, false, false)?;
        let _ = s.fe_mut().set_owner(); // второй раз
    }
    dev::alive(sock)
}

// SET_VRING_NUM = 0 (недопустимо).
pub fn t_vring_num_zero(sock: &str) -> TR {
    {
        let mut s = Session::handshake(sock, true, false)?;
        let _ = s.fe_mut().set_vring_num(0, 0);
    }
    dev::alive(sock)
}

// SET_VRING_NUM не степень двойки (спека требует pow2).
pub fn t_vring_num_not_pow2(sock: &str) -> TR {
    {
        let mut s = Session::handshake(sock, true, false)?;
        let _ = s.fe_mut().set_vring_num(0, 100);
    }
    dev::alive(sock)
}

// SET_VRING_NUM больше максимума устройства.
pub fn t_vring_num_too_big(sock: &str) -> TR {
    {
        let mut s = Session::handshake(sock, true, false)?;
        let _ = s.fe_mut().set_vring_num(0, 32768);
    }
    dev::alive(sock)
}

// SET_VRING_ADDR с невыровненным адресом таблицы дескрипторов (нужно 16).
pub fn t_vring_addr_unaligned(sock: &str) -> TR {
    {
        let mut s = Session::handshake(sock, true, false)?;
        let _ = s.fe_mut().set_vring_num(0, dev::QSZ);
        let base = s.base_va();
        let cfg = VringConfigData {
            queue_max_size: dev::QSZ,
            queue_size: dev::QSZ,
            flags: 0,
            desc_table_addr: base + 0x1001, // невыровнено
            avail_ring_addr: base + 0x2000,
            used_ring_addr: base + 0x3000,
            log_addr: None,
        };
        let _ = s.fe_mut().set_vring_addr(0, &cfg);
    }
    dev::alive(sock)
}

// SET_MEM_TABLE с нулём регионов.
pub fn t_mem_table_empty(sock: &str) -> TR {
    {
        let mut s = Session::handshake(sock, false, false)?;
        let _ = s.fe_mut().set_mem_table(&[]);
    }
    dev::alive(sock)
}

// Настройка vring и kick БЕЗ предварительного SET_MEM_TABLE: у бэкенда нет
// отображения памяти — он не должен упасть, разбирая кольца по неотображённым адресам.
pub fn t_vring_before_mem(sock: &str) -> TR {
    {
        let mut s = Session::handshake(sock, false, false)?; // без mem table
        let _ = s.setup_vring(); // адреса валидные по форме, но память не отдана
        let h = s.alloc(16);
        s.wr_hdr(h, dev::VIRTIO_BLK_T_IN, 0);
        let d = s.alloc(SEC);
        let st = s.alloc(1);
        s.post(&[dev::r(h, 16), dev::w(d, SEC), dev::w(st, 1)]);
        s.kick();
        let _ = s.wait_used(300); // ожидаемо ничего (или разрыв) — важно, что демон жив
    }
    dev::alive(sock)
}
