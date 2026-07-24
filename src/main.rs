// Конформанс-набор для vhost-user-blk бэкенда, играющий роль КОРРЕКТНОГО мастера
// поверх rust-vmm `vhost` (Frontend). Каждый тест — либо валидный сценарий virtio-blk
// (проверяем корректность данных/семантики), либо спец-определённый error-path /
// «злой» ввод (проверяем, что демон не падает и не виснет).
//
// Запуск:
//   cargo run --release -- /run/vhost-blk-0.sock            # все тесты
//   cargo run --release -- /run/vhost-blk-0.sock scatter    # только имена с "scatter"
//   VHOST_SOCK=/run/d0.sock cargo run --release             # сокет из окружения
//
// Замедлить между тестами (если демону нужен cooldown между подключениями):
//   VHOST_TEST_DELAY_MS=200 cargo run --release -- /run/d0.sock

// Часть констант/хелперов оставлена для полноты (feature-биты, доп. геттеры) —
// глушим предупреждения о неиспользуемом, чтобы вывод сборки был чистым.
#![allow(dead_code)]

mod mem;
mod dev;
mod t_data;
mod t_precision;
mod t_proto;
mod t_robust;
mod tests;

use std::time::Duration;

fn main() {
    let prog = std::env::args().next().unwrap_or_else(|| "vhost-blk-conformance".into());

    // --- разбор аргументов: флаги + позиционные (env — как fallback) ----------
    // Флаги работают под sudo (переменные окружения sudo по умолчанию стирает).
    //   <socket>                 позиционный: путь к сокету
    //   [filter]                 позиционный: имя/категория (только это подмножество)
    //   --only/-o <f>            то же, что позиционный filter
    //   --skip/-s <a,b,c>        пропустить тесты по имени/категории (через запятую)
    //   --delay/-d <ms>          пауза между тестами, мс
    //   list | --list           показать все тесты и выйти
    let mut filter = String::new();
    let mut skip_csv = String::new();
    let mut delay_str = String::new();
    let mut list = false;
    let mut positionals: Vec<String> = Vec::new();

    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        let a = raw[i].clone();
        let take_val = |inline: Option<&str>, i: &mut usize| -> String {
            if let Some(v) = inline {
                v.to_string()
            } else {
                *i += 1;
                raw.get(*i).cloned().unwrap_or_default()
            }
        };
        let (key, inline) = match a.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v)),
            None => (a.clone(), None),
        };
        match key.as_str() {
            "list" | "--list" => list = true,
            "--only" | "-o" | "--filter" => filter = take_val(inline, &mut i),
            "--skip" | "-s" => skip_csv = take_val(inline, &mut i),
            "--delay" | "-d" => delay_str = take_val(inline, &mut i),
            "-h" | "--help" => {
                usage(&prog);
                return;
            }
            s if s.starts_with('-') => {
                eprintln!("неизвестный флаг: {}", s);
                usage(&prog);
                std::process::exit(2);
            }
            _ => positionals.push(a),
        }
        i += 1;
    }

    if list {
        let mut last = "";
        for (name, cat, _) in tests::all() {
            if cat != last {
                println!("\n[{}]", cat);
                last = cat;
            }
            println!("  {}", name);
        }
        return;
    }

    // socket: 1-й позиционный > $VHOST_SOCK
    let sock = positionals
        .get(0)
        .cloned()
        .or_else(|| std::env::var("VHOST_SOCK").ok())
        .unwrap_or_default();
    if sock.is_empty() {
        usage(&prog);
        std::process::exit(2);
    }
    // filter: --only > 2-й позиционный (env для filter нет)
    if filter.is_empty() {
        filter = positionals.get(1).cloned().unwrap_or_default();
    }
    let filter = filter.to_lowercase();
    // skip: --skip > $VHOST_SKIP
    if skip_csv.is_empty() {
        skip_csv = std::env::var("VHOST_SKIP").unwrap_or_default();
    }
    let skip_tokens: Vec<String> = skip_csv
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    // delay: --delay > $VHOST_TEST_DELAY_MS
    if delay_str.is_empty() {
        delay_str = std::env::var("VHOST_TEST_DELAY_MS").unwrap_or_default();
    }
    let delay_ms: u64 = delay_str.parse().unwrap_or(0);
    // та же пауза применяется и к переподключениям ВНУТРИ теста (persistence,
    // reconnect-стресс, hostile→alive), а не только между тестами.
    dev::set_reconnect_delay(delay_ms);

    // Не шуметь дефолтным паник-хендлером: паники в харнессе ловим сами.
    std::panic::set_hook(Box::new(|_| {}));

    let all = tests::all();
    // Test = кортеж Copy-типов, поэтому забираем по значению — без ссылочных слоёв.
    let selected: Vec<tests::Test> = all
        .into_iter()
        .filter(|(name, cat, _)| filter.is_empty() || name_or_cat(name, cat, &filter))
        .filter(|(name, cat, _)| !skip_tokens.iter().any(|t| name_or_cat(name, cat, t)))
        .collect();

    println!("vhost-user-blk conformance (rust-vmm Frontend)");
    println!("socket: {}", sock);
    if !filter.is_empty() {
        println!("filter: {}", filter);
    }
    if !skip_tokens.is_empty() {
        println!("skip:   {}", skip_tokens.join(", "));
    }
    println!("tests:  {}\n", selected.len());

    let (mut pass, mut fail, mut skip) = (0u32, 0u32, 0u32);
    let mut failures: Vec<(String, String)> = Vec::new();
    let mut last_cat = "";

    for (name, cat, f) in selected {
        if cat != last_cat {
            println!("\n== {} ==", cat);
            last_cat = cat;
        }
        if delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(delay_ms));
        }
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&sock)));
        match res {
            Ok(Ok(())) => {
                pass += 1;
                println!("  \x1b[32mPASS\x1b[0m  {}", name);
            }
            Ok(Err(dev::TestErr::Skip(m))) => {
                skip += 1;
                println!("  \x1b[33mSKIP\x1b[0m  {} — {}", name, m);
            }
            Ok(Err(dev::TestErr::Fail(m))) => {
                fail += 1;
                failures.push((name.to_string(), m.clone()));
                println!("  \x1b[31mFAIL\x1b[0m  {} — {}", name, m);
            }
            Err(_) => {
                fail += 1;
                failures.push((name.to_string(), "PANIC в харнессе".into()));
                println!("  \x1b[31mFAIL\x1b[0m  {} — PANIC в харнессе", name);
            }
        }
    }

    println!("\n----------------------------------------");
    println!(
        "PASS {}   \x1b[31mFAIL {}\x1b[0m   \x1b[33mSKIP {}\x1b[0m",
        pass, fail, skip
    );
    if !failures.is_empty() {
        println!("\nПровалы (кандидаты в баги реализации):");
        for (n, m) in &failures {
            println!("  - {}: {}", n, m);
        }
    }
    std::process::exit(if fail > 0 { 1 } else { 0 });
}

/// true, если имя ИЛИ категория теста содержит токен (регистронезависимо).
fn name_or_cat(name: &str, cat: &str, tok: &str) -> bool {
    name.to_lowercase().contains(tok) || cat.to_lowercase().contains(tok)
}

fn usage(prog: &str) {
    eprintln!("usage: {} <socket> [filter] [флаги]", prog);
    eprintln!();
    eprintln!("  <socket>              путь к unix-сокету демона (или $VHOST_SOCK)");
    eprintln!("  [filter]              гнать ТОЛЬКО тесты с этой подстрокой в имени/категории");
    eprintln!();
    eprintln!("флаги (работают под sudo, в отличие от env):");
    eprintln!("  -o, --only <f>        то же, что позиционный filter");
    eprintln!("  -s, --skip <a,b,c>    пропустить тесты по имени/категории (через запятую)");
    eprintln!("  -d, --delay <ms>      пауза между тестами И переподключениями внутри теста, мс");
    eprintln!("      list, --list      показать все тесты и выйти");
    eprintln!("  -h, --help            эта справка");
    eprintln!();
    eprintln!("примеры:");
    eprintln!("  sudo {} /run/d0.sock", prog);
    eprintln!("  sudo {} /run/d0.sock vq-mechanics", prog);
    eprintln!("  sudo {} /run/d0.sock --skip hostile,large-request", prog);
    eprintln!("  sudo {} /run/d0.sock req-types --skip discard --delay 300", prog);
    eprintln!();
    eprintln!("env-эквиваленты (fallback): $VHOST_SOCK, $VHOST_SKIP, $VHOST_TEST_DELAY_MS");
    eprintln!("  (под sudo используй флаги или `sudo -E` / `sudo env VAR=... {}`)", prog);
}
