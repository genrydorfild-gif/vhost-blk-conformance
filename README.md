# vhost-user-blk conformance (rust-vmm Frontend)

Конформанс-тесты для твоего **vhost-user-blk бэкенда**, где мы играем роль
**корректного мастера** поверх rust-vmm крейта `vhost` (тип `Frontend`, бывш.
`Master`). Бьём напрямую в сокет демона — **без ВМ и без libblkio**.

Идея: каждый тест — это либо валидный по спеке сценарий virtio-blk на **нетривиальной
раскладке** (которую фиксированный мастер вроде libblkio/qemu-io никогда не пришлёт),
либо спец-определённый **error-path / «злой» ввод**. Цель — ловить баги реализации:
неверный разбор дескрипторных цепочек, перепутанные завершения, кривой `used.len`,
падения/зависания на граничных и некорректных сообщениях.

> ⚠️ **Тесты ДЕСТРУКТИВНЫ**: пишут на диск, начиная примерно с сектора `WORK=2048`
> (1 MiB), а также в последний сектор. Запускай только против **скретч-диска**.

## Сборка и запуск

Нужен Rust (stable) + доступ к crates.io для крейта `vhost`.

```bash
cd /tmp/vhost-blk-conformance
cargo build --release

BIN=./target/release/vhost-blk-conformance

# все тесты против одного сокета (обычно нужен sudo — сокет root-only):
sudo $BIN /run/vhost-blk-0.sock

# список тестов и категорий (сокет не нужен):
$BIN list

# ТОЛЬКО подмножество — позиц. фильтр по имени ИЛИ категории (подстрока, регистр не важен):
sudo $BIN /run/d0.sock scatter          # тесты с "scatter" в имени
sudo $BIN /run/d0.sock vq-mechanics     # вся категория vq-mechanics
sudo $BIN /run/d0.sock --only vq-mechanics   # то же флагом

# ПРОПУСТИТЬ тесты — флаг --skip (список через запятую, матч по имени ИЛИ категории):
sudo $BIN /run/d0.sock --skip hostile                        # без всей категории hostile
sudo $BIN /run/d0.sock --skip hostile,large-request,many-segments

# фильтр и skip комбинируются (сначала подмножество, потом вычитаем skip):
sudo $BIN /run/d0.sock req-types --skip discard --delay 300
```

### ⚠️ Флаги vs переменные окружения под `sudo`

`sudo` по умолчанию (`env_reset`) **стирает окружение**, поэтому
`VHOST_SKIP=... sudo $BIN` НЕ сработает. Отсюда флаги. Env-эквиваленты
(`$VHOST_SOCK`, `$VHOST_SKIP`, `$VHOST_TEST_DELAY_MS`) остаются как fallback, но под
sudo их надо прокидывать явно:

```bash
sudo $BIN /run/d0.sock --skip hostile          # рекомендуется: флаги
sudo -E VHOST_SKIP=hostile $BIN /run/d0.sock   # -E сохраняет твоё окружение
sudo env VHOST_SKIP=hostile $BIN /run/d0.sock  # прокинуть конкретную переменную
```

Флаги: `-o/--only <f>`, `-s/--skip <a,b,c>`, `-d/--delay <ms>`, `list`, `-h/--help`.
Приоритет: флаг > позиционный аргумент > переменная окружения.

**`--delay` применяется в двух местах:** между тестами (раннер) И между
переподключениями ВНУТРИ одного теста (`persistence-across-reconnect`,
`reconnect-stress`, `hostile → alive`). Если твоему демону нужно ~5 сек на
подготовку после разрыва — ставь `--delay 5000`, иначе эти тесты словят
недоготовый демон и упадут/зависнут на втором подключении.

Демон должен уже **слушать** сокет. `Frontend::connect` сам ретраит подключение,
пока бэкенд не готов — это закрывает твой сценарий «~5 сек на подготовку после разрыва».

Код возврата: `0` — все прошли/скип, `1` — есть провалы (кандидаты в баги).

## Что и как проверяется

Каждый тест открывает своё подключение, делает корректный handshake
(`SET_OWNER → GET/SET_FEATURES → GET/SET_PROTOCOL_FEATURES → GET_CONFIG →
SET_MEM_TABLE → SET_VRING_* → SET_VRING_ENABLE`) и настраивает split-очередь в
разделяемой памяти (memfd). Дальше — сам сценарий.

**Ключевые «неочевидные, но по спеке» проверки** (то, ради чего это писалось):

| Тест | Что ловит |
|---|---|
| `header-split-8+8`, `header-split-4x4` | бэкенд, считающий 16-байтный заголовок ровно одним дескриптором |
| `header+data-one-desc (write)` | бэкенд, режущий запрос по границам дескрипторов, а не по байтам |
| `scatter-write`, `scatter-read`, `uneven-segments`, `many-segments-64x512` | сборку/разбор SG-цепочек, отслеживание смещений по неравным сегментам |
| `oversized-status-desc` | переполнение при записи статуса в дескриптор длиннее 1 байта |
| `indirect-descriptors` | ветку `VIRTIO_F_RING_INDIRECT_DESC` (часто недотестирована) |
| `unsupported-request-type` | UNSUPP/IOERR вместо падения на неизвестном типе |
| `cross-capacity-boundary`, `beyond-capacity` | валидацию границ (IOERR, а не тихий OK/креш) |
| `used.len-read=data+status`, `used.len-write=1` | корректность поля `used.len` (частый баг) |
| `out-of-order-completion` | перепутанные завершения при нескольких in-flight |
| `spurious-kick`, `double-kick` | фабрикацию лишних завершений |
| `no-interrupt-flag` | обработку запроса при `VRING_AVAIL_F_NO_INTERRUPT` |
| `flush-nonzero-sector`, `zero-length-read`, `non-multiple-length` | краевые формы валидных запросов |
| `persistence-across-reconnect` | durability данных через разрыв соединения |
| `vring-num-*`, `vring-addr-unaligned`, `mem-table-empty`, `vring-before-mem-table`, `double-set-owner` | **живучесть демона** на кривых протокольных сообщениях |

### Модель проверки «злых» тестов (категория `hostile`)

Главный критерий — **демон обязан выжить**. Мы шлём кривое сообщение, роняем
сессию и проверяем свежим подключением + чтением (`dev::alive`), что демон
по-прежнему обслуживает. Закрыть соединение в ответ на мусор — **корректно**;
мы ловим именно **креши и зависания всего демона**.

> Если тест из `hostile` **завис** (а не FAIL) — вероятно, демон упал и
> `Frontend::connect` не может переподключиться. Это тоже находка. Прерви (Ctrl-C)
> и смотри лог демона.

### Строгие проверки, которые могут «шуметь»

- `used.len-read=data+status` требует `used.len == длина_данных + 1` (байт статуса).
  Часть бэкендов кладёт только длину данных или `0` — Linux-драйвер это терпит, но
  спеке (§ split virtqueue: *len — число байт, записанных устройством в цепочку*)
  соответствует именно `данные+status`. Если у тебя иначе — это осознанное
  отклонение, реши сам, ослаблять ли тест.
- `flush-nonzero-sector` ждёт `OK`: поле `sector` для FLUSH зарезервировано и
  устройство должно его игнорировать.

## Аудит защит (сверка с чужими реализациями)

Категории `robustness` и `validation` — это **регрессионные сторожа**: они бьют по
классам багов, реально закрытых в других vhost-user/virtio реализациях, и проверяют,
что у libvhost-server защита есть и срабатывает без креша/зависания.

Сверка кода libvhost-server (на момент написания) показала, что защита **есть** от:

| Класс (где был баг) | Защита в libvhost-server |
|---|---|
| адрес дескриптора вне памяти → segfault (DPDK CVE-2020-10725) | `gpa_range_to_ptr` → `-EFAULT` |
| частичный OOB (addr+len за регионом) | маппится весь диапазон, не только старт |
| петля дескрипторов / «looped descriptor» (QEMU) | `max_chain_len = MAX(qsz, 515)` → `-ENOBUFS` |
| next за пределами таблицы | `next >= qsz` → `-ERANGE` |
| вложенный indirect / indirect+NEXT / кривая длина таблицы | явные проверки в `walk_indirect_table` |
| readable-после-writable (наруш. 2.7.4.2) | проверка порядка в `add_buffer` |
| выход за ёмкость + integer overflow (`sector+len`) | overflow-safe `nsectors>cap \|\| sector>cap-nsectors` |
| нулевой/некратный размер запроса | `is_valid_req` отвергает |
| GET_ID с кривым буфером | проверка длины == 20 |

**Поведение при битом дескрипторе:** libvhost-server делает `mark_broken(vq)` —
очередь фейл-стопит (как QEMU `virtio_error`). Это спека-совместимо
(DEVICE_NEEDS_RESET), но означает: один некорректный дескриптор от драйвера кладёт
всю очередь до переподключения. Для доверенного контура с корректным QEMU это не
проблема; для недоверенного гостя — DoS-поверхность (как и у QEMU). Тесты `robustness`
поэтому проверяют не «очередь ожила», а «**процесс демона выжил**» (свежее
подключение работает).

**Замеченные мелкие расхождения** (низкая важность, но на заметку):
- поле `flags` в discard/write-zeroes **не проверяется** — бит `unmap` и reserved-биты
  игнорируются (спека допускает проверку);
- `num_sectors == 0` в discard проходит валидацию (тест `discard-zero-sectors`
  фиксирует поведение);
- стоит вручную свериться с **DPDK CVE-2021-3839** (валидация `num_queues` в
  `GET/SET_INFLIGHT_FD`): `vhost_get_inflight_fd` считает `mmap_size` из
  `idesc->num_queues` — проверь верхнюю границу (тест это не покрывает, т.к. крейт
  `vhost` не даёт слать произвольный inflight-запрос).

### Категория `precision` — точность протокола (функциональные ошибки, не CVE)

Проверяет не «упадёт ли», а «точно ли соблюдена семантика»:

- `vring-base-roundtrip`, `vring-base-tracks-consumed` — SET/GET_VRING_BASE и учёт
  `last_avail` (неверный → потеря/повтор запросов при остановке/миграции). У
  libvhost-server корректно (`last_avail++` на запрос).
- `notify-signaled-when-enabled` / `notify-suppressed-no-interrupt` — устройство
  уведомляет callfd когда должно и молчит при `VRING_AVAIL_F_NO_INTERRUPT`.
- `used-index-wrap` — 16-битная обёртка индексов колец (старт у `0xFFF0`).
- `config-blk-size-sane` — `blk_size` степень двойки ≥ 512.
- **`feature-gating-discard` — ОЖИДАЕМО ПАДАЕТ против libvhost-server.** Это находка,
  которую ты просил: `dev->features` в [virtio_blk.c](../../yc-libvhost-server/virtio/virtio_blk.c)
  хранит **предложенный** набор и не сужается до **согласованного**, поэтому
  `dev_supports_req` пропускает DISCARD, даже если драйвер не заакал `F_DISCARD`.
  Устройство выполняет несогласованную команду вместо `UNSUPP` — функциональная
  неточность (безвредная с корректным QEMU, но нарушает модель негоциации). Тест
  падает со `status != UNSUPP`, фиксируя это.

**Ещё наблюдение (не баг):** libvhost-server **не предлагает `VIRTIO_BLK_F_FLUSH`** —
гость не может форсировать durability через FLUSH (полагается на storage-слой). Тест
`flush` поэтому скипается. Если твой backend не гарантирует немедленную durability —
это стоит держать в голове.

## Границы применимости

Это по-прежнему **корректный мастер поверх одной очереди**. Он НЕ покрывает:

- **inflight recovery / рестарт демона при живом мастере** — `GET/SET_INFLIGHT_FD`;
- **live migration** (dirty log, `SET_LOG_BASE`);
- **RESET_DEVICE**, **memory hotplug** (`ADD/REM_MEM_REG`);
- **multiqueue data plane** (настраивается только очередь 0).

Эти сценарии — через настоящий QEMU/libvirt (VM + `reconnect=` + QMP `migrate`),
как в отдельном разговоре про «вариант (б)».

## Структура

```
src/
  main.rs      раннер: аргументы, прогон, сводка, коды возврата
  mem.rs       memfd + mmap(MAP_SHARED) + volatile LE-доступ к памяти
  dev.rs       ХАРНЕСС: Frontend-handshake, split-vring, high-level virtio-blk,
               примитивы (alloc/post/kick/wait_used) и хелперы тестов
  t_data.rs    тесты плоскости данных (раскладки, типы запросов, границы, очередь)
  t_proto.rs   тесты config / lifecycle / «злого» ввода
  tests.rs     реестр (имя, категория, функция)
```

Важная деталь про адреса (и повод для тестов): адреса **буферов в дескрипторах** —
гостевые физические (GPA), а адреса **колец в `SET_VRING_ADDR`** — пользовательские
виртуальные (наш VA). Харнесс держит `GPA_BASE=0`, а `userspace_addr = base VA`,
чтобы значения различались и тест ловил бэкенд, путающий две трансляции. См.
шапку `dev.rs`.

## Статус сборки

✅ **Собирается без ошибок и предупреждений** на Rust stable против:
`vhost 0.17.0`, `vmm-sys-util 0.15`, `vm-memory 0.18`, `libc 0.2`
(точные версии зафиксированы в `Cargo.lock`).

Скомпилировано и проверено, что бинарь стартует и печатает usage. **Runtime против
живого демона не гонялся** (в среде сборки демона нет) — это твой шаг.

### Если API разойдётся в другой версии крейта

Две точки, где `vhost` менял поверхность (обе уже учтены под 0.17):
1. **`VhostUserMemoryRegionInfo`** импортируется из корня — `vhost::VhostUserMemoryRegionInfo`
   (не из `::message`). Поля: `guest_phys_addr, memory_size, userspace_addr,
   mmap_offset, mmap_handle` (+2 под фичей `xen`, её не включаем).
2. **`vmm-sys-util`** должен быть той же версии, что тянет `vhost` (иначе два разных
   типа `EventFd` в графе и ошибка в `set_vring_kick/call`). Сейчас `0.15`.
