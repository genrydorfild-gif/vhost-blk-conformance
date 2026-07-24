// Харнесс: корректный vhost-user frontend + split-virtqueue поверх разделяемой
// памяти + high-level операции virtio-blk. Тесты (tests.rs) строятся на этом.
//
// ВАЖНО про адреса (частый источник багов в бэкендах — и повод для тестов):
//   * адреса буферов В ДЕСКРИПТОРАХ — ГОСТЕВЫЕ ФИЗИЧЕСКИЕ (GPA). Бэкенд транслирует
//     их через region.guest_phys_addr.
//   * адреса колец в SET_VRING_ADDR — ПОЛЬЗОВАТЕЛЬСКИЕ ВИРТУАЛЬНЫЕ (наш VA).
//     Бэкенд транслирует их через region.userspace_addr.
// Мы специально держим GPA_BASE=0, а userspace_addr = base VA региона, чтобы
// значения этих адресов РАЗЛИЧАЛИСЬ — так тест ловит бэкенд, который путает
// две трансляции.

use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use vmm_sys_util::eventfd::EventFd;

use vhost::vhost_user::message::{VhostUserConfigFlags, VhostUserProtocolFeatures};
use vhost::vhost_user::{Frontend, VhostUserFrontend};
use vhost::{VhostBackend, VhostUserMemoryRegionInfo, VringConfigData};

use crate::mem::SharedMem;

// ---- результат теста -------------------------------------------------------

pub enum TestErr {
    Fail(String),
    Skip(String),
}
pub type TR = Result<(), TestErr>;

impl From<String> for TestErr {
    fn from(s: String) -> Self {
        TestErr::Fail(s)
    }
}

// ---- геометрия памяти/очереди ---------------------------------------------

pub const QSZ: u16 = 256;
pub const GPA_BASE: u64 = 0;
pub const REGION_SIZE: usize = 64 * 1024 * 1024;

const DESC_OFF: usize = 0x1000; // таблица дескрипторов (QSZ*16 = 4096)
const AVAIL_OFF: usize = 0x2000; // avail ring
const USED_OFF: usize = 0x3000; // used ring
const DATA_OFF: usize = 0x0010_0000; // буферы данных (с 1 MiB)

pub const SECTOR: usize = 512; // virtio-blk sector — всегда 512, независимо от blk_size
pub const TIMEOUT_MS: u64 = 5000;

// ---- флаги virtqueue -------------------------------------------------------

pub const VRING_DESC_F_NEXT: u16 = 1;
pub const VRING_DESC_F_WRITE: u16 = 2;
pub const VRING_DESC_F_INDIRECT: u16 = 4;
pub const VRING_AVAIL_F_NO_INTERRUPT: u16 = 1;

// ---- virtio feature bits ---------------------------------------------------

pub const VIRTIO_F_RING_INDIRECT_DESC: u64 = 1 << 28;
pub const VIRTIO_F_RING_EVENT_IDX: u64 = 1 << 29;
pub const VHOST_USER_F_PROTOCOL_FEATURES: u64 = 1 << 30;
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

// virtio-blk feature bits (младшие 32)
pub const VIRTIO_BLK_F_RO: u64 = 1 << 5;
pub const VIRTIO_BLK_F_BLK_SIZE: u64 = 1 << 6;
pub const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;
pub const VIRTIO_BLK_F_MQ: u64 = 1 << 12;
pub const VIRTIO_BLK_F_DISCARD: u64 = 1 << 13;
pub const VIRTIO_BLK_F_WRITE_ZEROES: u64 = 1 << 14;

// ---- virtio-blk request types ---------------------------------------------

pub const VIRTIO_BLK_T_IN: u32 = 0;
pub const VIRTIO_BLK_T_OUT: u32 = 1;
pub const VIRTIO_BLK_T_FLUSH: u32 = 4;
pub const VIRTIO_BLK_T_GET_ID: u32 = 8;
pub const VIRTIO_BLK_T_DISCARD: u32 = 11;
pub const VIRTIO_BLK_T_WRITE_ZEROES: u32 = 13;

// status
pub const VIRTIO_BLK_S_OK: u8 = 0;
pub const VIRTIO_BLK_S_IOERR: u8 = 1;
pub const VIRTIO_BLK_S_UNSUPP: u8 = 2;
pub const STATUS_POISON: u8 = 0xff; // кладём в status до запроса — «устройство не тронуло»

// ---- дескриптор (сегмент) --------------------------------------------------

#[derive(Clone, Copy)]
pub struct Desc {
    pub off: usize, // смещение буфера в регионе (оно же GPA, т.к. GPA_BASE=0)
    pub len: u32,
    pub write: bool, // device-writable (для чтения данных устройством наоборот)
}

/// device-readable сегмент (устройство читает: заголовок, данные записи)
pub fn r(off: usize, len: usize) -> Desc {
    Desc { off, len: len as u32, write: false }
}
/// device-writable сегмент (устройство пишет: данные чтения, status)
pub fn w(off: usize, len: usize) -> Desc {
    Desc { off, len: len as u32, write: true }
}

#[derive(Clone, Copy, Debug)]
pub struct UsedElem {
    pub id: u32,
    pub len: u32,
}

// ---- сессия ----------------------------------------------------------------

pub struct Session {
    fe: Frontend,
    pub mem: SharedMem,
    proto: VhostUserProtocolFeatures,
    acked: u64,
    capacity_sectors: u64,
    blk_size: u32,
    offered: u64,
    kick: EventFd,
    call: EventFd,
    next_desc: u16,
    avail_idx: u16,
    used_idx_seen: u16,
    next_data: usize,
}

impl Session {
    /// Полностью готовая сессия: handshake + очередь 0 включена. EVENT_IDX выключен.
    pub fn connect(path: &str) -> Result<Session, String> {
        let mut s = Session::handshake(path, true, false)?;
        s.setup_vring()?;
        Ok(s)
    }

    /// То же, но с согласованным VIRTIO_F_RING_EVENT_IDX (если бэкенд предлагает).
    pub fn connect_event_idx(path: &str) -> Result<Session, String> {
        let mut s = Session::handshake(path, true, true)?;
        s.setup_vring()?;
        Ok(s)
    }

    /// Согласовать все фичи, КРОМЕ переданных в mask_out (и настроить vring).
    /// Позволяет проверить, честно ли бэкенд гейтит запросы по НЕГОЦИИРОВАННЫМ
    /// фичам (а не по тому, что он просто умеет).
    pub fn connect_masking(path: &str, mask_out: u64) -> Result<Session, String> {
        let mut s = Session::handshake_masked(path, true, false, mask_out)?;
        s.setup_vring()?;
        Ok(s)
    }

    /// Готовая сессия, где last_avail и used.idx стартуют с `base` (проверка
    /// корректной 16-битной обёртки индексов колец).
    pub fn connect_at_base(path: &str, base: u16) -> Result<Session, String> {
        let mut s = Session::handshake_masked(path, true, false, 0)?;
        s.setup_vring_impl(base)?;
        Ok(s)
    }

    /// Handshake без настройки vring — для «злых» тестов, дёргающих SET_VRING_* руками.
    /// do_mem_table=false — даже без SET_MEM_TABLE.
    pub fn handshake(path: &str, do_mem_table: bool, want_event_idx: bool) -> Result<Session, String> {
        Session::handshake_masked(path, do_mem_table, want_event_idx, 0)
    }

    pub fn handshake_masked(
        path: &str,
        do_mem_table: bool,
        want_event_idx: bool,
        mask_out: u64,
    ) -> Result<Session, String> {
        // второй аргумент — ЧИСЛО очередей (не размер!). Мы используем только очередь 0.
        let mut fe = Frontend::connect(path, 1).map_err(|e| format!("connect: {:?}", e))?;
        fe.set_owner().map_err(|e| format!("set_owner: {:?}", e))?;
        let offered = fe.get_features().map_err(|e| format!("get_features: {:?}", e))?;

        // Акаем ТОЛЬКО protocol-фичи, которые реально реализуем. Если заакать
        // фичу, требующую сопутствующей настройки, а настройку не сделать —
        // бэкенд (или слой демона над libvhost-server) будет считать её включённой
        // и сломается. В частности НЕЛЬЗЯ акать:
        //   INFLIGHT_SHMFD — потребует GET/SET_INFLIGHT_FD (регион inflight);
        //   LOG_SHMFD      — миграция/dirty log (SET_LOG_BASE);
        //   BACKEND_REQ    — обратный канал (SET_BACKEND_REQ_FD);
        //   INBAND_NOTIFICATIONS, CONFIGURE_MEM_SLOTS, HOST_NOTIFIER, ...
        // Нам нужен только CONFIG (для GET_CONFIG → ёмкость).
        let mut proto = VhostUserProtocolFeatures::empty();
        if offered & VHOST_USER_F_PROTOCOL_FEATURES != 0 {
            let want = VhostUserProtocolFeatures::CONFIG;
            let offered_proto = fe
                .get_protocol_features()
                .map_err(|e| format!("get_protocol_features: {:?}", e))?;
            proto = offered_proto & want; // пересечение: только поддерживаемое обеими сторонами
            fe.set_protocol_features(proto)
                .map_err(|e| format!("set_protocol_features: {:?}", e))?;
        }

        // Acked = всё предложенное, но EVENT_IDX по умолчанию выключаем (упрощает
        // семантику used-ring; тесты, которым он нужен, включают явно).
        let mut acked = offered;
        if !want_event_idx {
            acked &= !VIRTIO_F_RING_EVENT_IDX;
        }
        acked &= !mask_out; // намеренно НЕ согласуем указанные фичи
        fe.set_features(acked).map_err(|e| format!("set_features: {:?}", e))?;

        let (capacity_sectors, blk_size) = if proto.contains(VhostUserProtocolFeatures::CONFIG) {
            match fe.get_config(0, 60, VhostUserConfigFlags::empty(), &vec![0u8; 60]) {
                Ok((_h, payload)) => {
                    let cap = le64(&payload, 0);
                    let bs = le32(&payload, 20);
                    (cap, if bs == 0 { 512 } else { bs })
                }
                Err(_) => (0, 512),
            }
        } else {
            (0, 512)
        };

        let mem = SharedMem::new(REGION_SIZE)?;
        let kick = EventFd::new(0).map_err(|e| format!("eventfd kick: {:?}", e))?;
        // call — неблокирующий, чтобы можно было опрашивать уведомления устройства.
        let call = EventFd::new(libc::EFD_NONBLOCK)
            .map_err(|e| format!("eventfd call: {:?}", e))?;

        let s = Session {
            fe,
            mem,
            proto,
            acked,
            capacity_sectors,
            blk_size,
            offered,
            kick,
            call,
            next_desc: 0,
            avail_idx: 0,
            used_idx_seen: 0,
            next_data: DATA_OFF,
        };

        if do_mem_table {
            let region = s.region();
            s.fe.set_mem_table(&[region])
                .map_err(|e| format!("set_mem_table: {:?}", e))?;
        }
        Ok(s)
    }

    fn region(&self) -> VhostUserMemoryRegionInfo {
        VhostUserMemoryRegionInfo {
            guest_phys_addr: GPA_BASE,
            memory_size: REGION_SIZE as u64,
            userspace_addr: self.mem.base_va(),
            mmap_offset: 0,
            mmap_handle: self.mem.fd(),
        }
    }

    /// Стандартная настройка очереди 0 корректными значениями (base = 0).
    pub fn setup_vring(&mut self) -> Result<(), String> {
        self.setup_vring_impl(0)
    }

    /// Настройка очереди с произвольным начальным индексом колец `base_idx`.
    /// При base_idx != 0 инициализируем avail.idx и used.idx в памяти и теневые
    /// счётчики так, чтобы устройство и мы стартовали от одного значения.
    pub fn setup_vring_impl(&mut self, base_idx: u16) -> Result<(), String> {
        self.mem.zero(DESC_OFF, DATA_OFF - DESC_OFF); // кольца в ноль
        self.fe.set_vring_num(0, QSZ).map_err(|e| format!("set_vring_num: {:?}", e))?;
        let va = self.mem.base_va();
        let cfg = VringConfigData {
            queue_max_size: QSZ,
            queue_size: QSZ,
            flags: 0,
            // адреса колец — В НАШЕМ VA (userspace_addr-относительные)
            desc_table_addr: va + DESC_OFF as u64,
            avail_ring_addr: va + AVAIL_OFF as u64,
            used_ring_addr: va + USED_OFF as u64,
            log_addr: None,
        };
        self.fe.set_vring_addr(0, &cfg).map_err(|e| format!("set_vring_addr: {:?}", e))?;
        self.fe.set_vring_base(0, base_idx).map_err(|e| format!("set_vring_base: {:?}", e))?;
        if base_idx != 0 {
            self.mem.w16(AVAIL_OFF + 2, base_idx); // avail.idx
            self.mem.w16(USED_OFF + 2, base_idx); // used.idx (устройство продолжит отсюда)
            self.avail_idx = base_idx;
            self.used_idx_seen = base_idx;
        }
        self.fe.set_vring_kick(0, &self.kick).map_err(|e| format!("set_vring_kick: {:?}", e))?;
        self.fe.set_vring_call(0, &self.call).map_err(|e| format!("set_vring_call: {:?}", e))?;
        if self.acked & VHOST_USER_F_PROTOCOL_FEATURES != 0 {
            self.fe.set_vring_enable(0, true).map_err(|e| format!("set_vring_enable: {:?}", e))?;
        }
        Ok(())
    }

    // ---- сведения об устройстве --------------------------------------------

    pub fn capacity_sectors(&self) -> u64 {
        self.capacity_sectors
    }
    pub fn blk_size(&self) -> u32 {
        self.blk_size
    }
    pub fn has_feature(&self, bit: u64) -> bool {
        self.acked & bit != 0
    }
    /// Фича ПРЕДЛОЖЕНА устройством (в GET_FEATURES), независимо от того, заакали ли.
    pub fn offered_has(&self, bit: u64) -> bool {
        self.offered & bit != 0
    }
    pub fn has_proto(&self, f: VhostUserProtocolFeatures) -> bool {
        self.proto.contains(f)
    }
    pub fn fe_mut(&mut self) -> &mut Frontend {
        &mut self.fe
    }
    pub fn base_va(&self) -> u64 {
        self.mem.base_va()
    }

    /// GET_VRING_BASE(0): текущий last_avail устройства (останавливает очередь).
    pub fn get_vring_base(&self) -> Result<u32, String> {
        self.fe
            .get_vring_base(0)
            .map_err(|e| format!("get_vring_base: {:?}", e))
    }

    /// Сбросить накопленное уведомление на callfd (не блокирует).
    pub fn drain_call(&mut self) {
        let _ = self.call.read();
    }

    /// Подождать уведомление устройства на callfd до timeout. true = пришло.
    /// Служит для проверки, УВЕДОМЛЯЕТ ли устройство (и подавляет ли при NO_INTERRUPT).
    pub fn wait_call(&mut self, timeout_ms: u64) -> bool {
        let mut pfd = libc::pollfd {
            fd: self.call.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms as libc::c_int) };
        if ret > 0 && (pfd.revents & libc::POLLIN) != 0 {
            let _ = self.call.read();
            true
        } else {
            false
        }
    }

    // ---- низкоуровневый submit ---------------------------------------------

    /// Разложить дескрипторную цепочку и опубликовать в avail. Возвращает head.
    pub fn post(&mut self, descs: &[Desc]) -> u16 {
        let n = descs.len();
        assert!(n >= 1);
        let head = self.next_desc % QSZ;
        for (i, d) in descs.iter().enumerate() {
            let idx = (self.next_desc + i as u16) % QSZ;
            let e = DESC_OFF + idx as usize * 16;
            self.mem.w64(e, GPA_BASE + d.off as u64);
            self.mem.w32(e + 8, d.len);
            let mut flags = 0u16;
            if d.write {
                flags |= VRING_DESC_F_WRITE;
            }
            if i + 1 < n {
                flags |= VRING_DESC_F_NEXT;
            }
            self.mem.w16(e + 12, flags);
            let next = (self.next_desc + i as u16 + 1) % QSZ;
            self.mem.w16(e + 14, next);
        }
        self.next_desc = (self.next_desc + n as u16) % QSZ;
        let ring = AVAIL_OFF + 4 + (self.avail_idx % QSZ) as usize * 2;
        self.mem.w16(ring, head);
        self.avail_idx = self.avail_idx.wrapping_add(1);
        self.mem.w16(AVAIL_OFF + 2, self.avail_idx); // avail.idx
        head
    }

    /// Разложить ОДИН индиректный дескриптор, указывающий на таблицу из descs.
    /// Требует VIRTIO_F_RING_INDIRECT_DESC. table_off — куда положить indirect-таблицу.
    pub fn post_indirect(&mut self, descs: &[Desc], table_off: usize) -> u16 {
        let n = descs.len();
        for (i, d) in descs.iter().enumerate() {
            let e = table_off + i * 16;
            self.mem.w64(e, GPA_BASE + d.off as u64);
            self.mem.w32(e + 8, d.len);
            let mut flags = 0u16;
            if d.write {
                flags |= VRING_DESC_F_WRITE;
            }
            if i + 1 < n {
                flags |= VRING_DESC_F_NEXT;
            }
            self.mem.w16(e + 12, flags);
            self.mem.w16(e + 14, (i + 1) as u16);
        }
        // головной дескриптор в основной таблице: FLAG_INDIRECT, len = n*16
        let head = self.next_desc % QSZ;
        let e = DESC_OFF + head as usize * 16;
        self.mem.w64(e, GPA_BASE + table_off as u64);
        self.mem.w32(e + 8, (n * 16) as u32);
        self.mem.w16(e + 12, VRING_DESC_F_INDIRECT);
        self.mem.w16(e + 14, 0);
        self.next_desc = (self.next_desc + 1) % QSZ;
        let ring = AVAIL_OFF + 4 + (self.avail_idx % QSZ) as usize * 2;
        self.mem.w16(ring, head);
        self.avail_idx = self.avail_idx.wrapping_add(1);
        self.mem.w16(AVAIL_OFF + 2, self.avail_idx);
        head
    }

    pub fn set_avail_flags(&self, flags: u16) {
        self.mem.w16(AVAIL_OFF, flags);
    }

    // ---- сырые примитивы для «злых»/битых дескрипторов --------------------
    // Позволяют собрать намеренно некорректную цепочку (петля, битый next,
    // адрес/длина вне региона, indirect-извраты) для проверки защит бэкенда.

    /// Записать дескриптор с ПОЛНОСТЬЮ произвольными полями (addr — уже GPA).
    pub fn write_raw_desc(&self, idx: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let e = DESC_OFF + (idx % QSZ) as usize * 16;
        self.mem.w64(e, addr);
        self.mem.w32(e + 8, len);
        self.mem.w16(e + 12, flags);
        self.mem.w16(e + 14, next);
    }

    /// Опубликовать в avail произвольный head-индекс (в т.ч. заведомо неверный).
    pub fn push_avail(&mut self, head: u16) {
        let ring = AVAIL_OFF + 4 + (self.avail_idx % QSZ) as usize * 2;
        self.mem.w16(ring, head);
        self.avail_idx = self.avail_idx.wrapping_add(1);
        self.mem.w16(AVAIL_OFF + 2, self.avail_idx);
    }

    pub fn kick(&self) {
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        let _ = self.kick.write(1);
    }

    /// Ждём следующий used-элемент (читаем used.idx из памяти, чтобы не зависеть
    /// от подавления уведомлений). None = зависание (нет завершения за timeout).
    pub fn wait_used(&mut self, timeout_ms: u64) -> Option<UsedElem> {
        let start = Instant::now();
        loop {
            let uidx = self.mem.r16(USED_OFF + 2);
            if uidx != self.used_idx_seen {
                let slot = (self.used_idx_seen % QSZ) as usize;
                let e = USED_OFF + 4 + slot * 8;
                let id = self.mem.r32(e);
                let len = self.mem.r32(e + 4);
                self.used_idx_seen = self.used_idx_seen.wrapping_add(1);
                return Some(UsedElem { id, len });
            }
            if start.elapsed().as_millis() as u64 >= timeout_ms {
                return None;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Синхронный submit: post + kick + ждать одно завершение. Проверяет, что
    /// used.id совпал с head (устройство обязано вернуть индекс головы цепочки).
    pub fn submit(&mut self, descs: &[Desc], timeout_ms: u64) -> Result<UsedElem, String> {
        let head = self.post(descs);
        self.kick();
        match self.wait_used(timeout_ms) {
            Some(u) => {
                if u.id != head as u32 {
                    return Err(format!("used.id={} != head={}", u.id, head));
                }
                Ok(u)
            }
            None => Err(format!("ЗАВИСАНИЕ: нет завершения за {} мс", timeout_ms)),
        }
    }

    // ---- аллокатор буферов данных ------------------------------------------

    /// Bump-аллокатор в области данных, выравнивание 4 KiB. Возвращает смещение (=GPA).
    pub fn alloc(&mut self, len: usize) -> usize {
        let a = (self.next_data + 0xfff) & !0xfff;
        self.next_data = a + len.max(1);
        assert!(self.next_data <= REGION_SIZE, "переполнение региона");
        a
    }

    pub fn wr_hdr(&self, off: usize, req_type: u32, sector: u64) {
        self.mem.w32(off, req_type);
        self.mem.w32(off + 4, 0); // reserved
        self.mem.w64(off + 8, sector);
    }

    pub fn status_at(&self, off: usize) -> u8 {
        self.mem.r8(off)
    }

    // ---- high-level virtio-blk ---------------------------------------------

    pub fn blk_write(&mut self, sector: u64, data: &[u8]) -> Result<u8, String> {
        let hdr = self.alloc(16);
        self.wr_hdr(hdr, VIRTIO_BLK_T_OUT, sector);
        let dbuf = self.alloc(data.len());
        self.mem.wr(dbuf, data);
        let st = self.alloc(1);
        self.mem.wr(st, &[STATUS_POISON]);
        self.submit(&[r(hdr, 16), r(dbuf, data.len()), w(st, 1)], TIMEOUT_MS)?;
        Ok(self.status_at(st))
    }

    pub fn blk_read(&mut self, sector: u64, len: usize) -> Result<(u8, Vec<u8>), String> {
        let hdr = self.alloc(16);
        self.wr_hdr(hdr, VIRTIO_BLK_T_IN, sector);
        let dbuf = self.alloc(len);
        self.mem.zero(dbuf, len);
        let st = self.alloc(1);
        self.mem.wr(st, &[STATUS_POISON]);
        self.submit(&[r(hdr, 16), w(dbuf, len), w(st, 1)], TIMEOUT_MS)?;
        let mut out = vec![0u8; len];
        self.mem.rd(dbuf, &mut out);
        Ok((self.status_at(st), out))
    }

    pub fn blk_flush(&mut self) -> Result<u8, String> {
        let hdr = self.alloc(16);
        self.wr_hdr(hdr, VIRTIO_BLK_T_FLUSH, 0);
        let st = self.alloc(1);
        self.mem.wr(st, &[STATUS_POISON]);
        self.submit(&[r(hdr, 16), w(st, 1)], TIMEOUT_MS)?;
        Ok(self.status_at(st))
    }

    pub fn blk_get_id(&mut self) -> Result<(u8, Vec<u8>), String> {
        let hdr = self.alloc(16);
        self.wr_hdr(hdr, VIRTIO_BLK_T_GET_ID, 0);
        let dbuf = self.alloc(20);
        self.mem.zero(dbuf, 20);
        let st = self.alloc(1);
        self.mem.wr(st, &[STATUS_POISON]);
        self.submit(&[r(hdr, 16), w(dbuf, 20), w(st, 1)], TIMEOUT_MS)?;
        let mut out = vec![0u8; 20];
        self.mem.rd(dbuf, &mut out);
        Ok((self.status_at(st), out))
    }

    /// WRITE_ZEROES / DISCARD: payload = {le64 sector, le32 num_sectors, le32 flags}
    pub fn blk_zeroes_or_discard(
        &mut self,
        req_type: u32,
        sector: u64,
        num_sectors: u32,
        unmap: bool,
    ) -> Result<u8, String> {
        let hdr = self.alloc(16);
        self.wr_hdr(hdr, req_type, 0); // sector заголовка = 0, реальный — в payload
        let payload = self.alloc(16);
        self.mem.w64(payload, sector);
        self.mem.w32(payload + 8, num_sectors);
        self.mem.w32(payload + 12, if unmap { 1 } else { 0 });
        let st = self.alloc(1);
        self.mem.wr(st, &[STATUS_POISON]);
        self.submit(&[r(hdr, 16), r(payload, 16), w(st, 1)], TIMEOUT_MS)?;
        Ok(self.status_at(st))
    }
}

// ---- утилиты ---------------------------------------------------------------

fn le64(b: &[u8], off: usize) -> u64 {
    let mut v = [0u8; 8];
    for i in 0..8 {
        v[i] = b.get(off + i).copied().unwrap_or(0);
    }
    u64::from_le_bytes(v)
}
fn le32(b: &[u8], off: usize) -> u32 {
    let mut v = [0u8; 4];
    for i in 0..4 {
        v[i] = b.get(off + i).copied().unwrap_or(0);
    }
    u32::from_le_bytes(v)
}

static RECONNECT_DELAY_MS: AtomicU64 = AtomicU64::new(0);

/// Пауза «остывания» демона перед переподключением ВНУТРИ теста. Равна значению
/// --delay / $VHOST_TEST_DELAY_MS. Между тестами паузу ставит раннер; эта — для
/// тестов, которые сами переподключаются (persistence, reconnect-стресс,
/// hostile → alive), где раннер не помогает.
pub fn set_reconnect_delay(ms: u64) {
    RECONNECT_DELAY_MS.store(ms, Ordering::Relaxed);
}
pub fn reconnect_cooldown() {
    let ms = RECONNECT_DELAY_MS.load(Ordering::Relaxed);
    if ms > 0 {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

/// Проба живучести демона: свежее подключение + одно чтение сектора 0.
/// Используется после «злых» тестов — демон обязан пережить кривой ввод.
pub fn alive(path: &str) -> TR {
    reconnect_cooldown(); // подождать, пока демон снова готов принимать клиента
    let mut s = Session::connect(path)?;
    let (st, _) = s.blk_read(0, SECTOR)?;
    if st != VIRTIO_BLK_S_OK {
        return Err(TestErr::Fail(format!("liveness-чтение вернуло status={}", st)));
    }
    Ok(())
}

/// Позиционно-зависимый паттерн — ловит misdirected/усечённые записи.
pub fn pat(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| seed ^ (i as u8).wrapping_mul(31).wrapping_add((i >> 8) as u8))
        .collect()
}

/// Смещение первого расхождения (для внятных сообщений).
pub fn first_diff(a: &[u8], b: &[u8]) -> Option<usize> {
    if a.len() != b.len() {
        return Some(a.len().min(b.len()));
    }
    a.iter().zip(b.iter()).position(|(x, y)| x != y)
}

// ---- общие помощники для тестов -------------------------------------------

/// Рабочий сектор (1 MiB от начала) — чтобы не топтать возможный заголовок диска.
pub const WORK: u64 = 2048;

pub fn expect_ok(label: &str, st: u8) -> TR {
    if st == VIRTIO_BLK_S_OK {
        Ok(())
    } else {
        Err(TestErr::Fail(format!("{}: status={} (ожидался OK=0)", label, st)))
    }
}

pub fn same(label: &str, got: &[u8], want: &[u8]) -> TR {
    match first_diff(got, want) {
        None => Ok(()),
        Some(o) => Err(TestErr::Fail(format!(
            "{}: расхождение на байте {} из {} (got={:#04x} want={:#04x})",
            label,
            o,
            want.len(),
            got.get(o).copied().unwrap_or(0),
            want.get(o).copied().unwrap_or(0)
        ))),
    }
}

pub fn all_zero(label: &str, d: &[u8]) -> TR {
    match d.iter().position(|b| *b != 0) {
        None => Ok(()),
        Some(o) => Err(TestErr::Fail(format!(
            "{}: байт {} не ноль ({:#04x})",
            label, o, d[o]
        ))),
    }
}

/// Требуется ёмкость под WORK + sectors (иначе Skip).
pub fn need_cap(s: &Session, sectors: u64) -> Result<u64, TestErr> {
    let c = s.capacity_sectors();
    if c == 0 {
        return Err(TestErr::Skip("ёмкость неизвестна (нет PROTOCOL_F_CONFIG)".into()));
    }
    if c < WORK + sectors + 8 {
        return Err(TestErr::Skip(format!("диск мал: {} секторов", c)));
    }
    Ok(c)
}
