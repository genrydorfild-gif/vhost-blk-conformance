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
mod t_proto;
mod tests;

use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Режим списка: показать все тесты (категория + имя) и выйти. Сокет не нужен.
    let a1 = args.get(1).map(String::as_str).unwrap_or("");
    let a2 = args.get(2).map(String::as_str).unwrap_or("");
    if a1 == "list" || a1 == "--list" || a2 == "list" || a2 == "--list" {
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

    let sock = args
        .get(1)
        .cloned()
        .or_else(|| std::env::var("VHOST_SOCK").ok())
        .unwrap_or_default();
    if sock.is_empty() {
        eprintln!("usage: {} <socket> [name-или-category-фильтр]", args[0]);
        eprintln!("  все тесты:        {} /run/d0.sock", args[0]);
        eprintln!("  список тестов:    {} list", args[0]);
        eprintln!("  только подмнож.:  {} /run/d0.sock vq-mechanics", args[0]);
        eprintln!(
            "  пропустить:       VHOST_SKIP=hostile,large-request {} /run/d0.sock",
            args[0]
        );
        std::process::exit(2);
    }
    // Включающий фильтр (позиц. аргумент): имя ИЛИ категория содержит подстроку.
    let filter = args.get(2).cloned().unwrap_or_default().to_lowercase();
    // Исключение: VHOST_SKIP="tok1,tok2" — пропустить тесты, у которых имя ИЛИ
    // категория содержит любой из токенов (регистронезависимо).
    let skip_tokens: Vec<String> = std::env::var("VHOST_SKIP")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    let delay_ms: u64 = std::env::var("VHOST_TEST_DELAY_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

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
