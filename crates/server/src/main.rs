//! steadycore · P0 骨架
//!
//! 起来的东西：一个常驻进程、一条 HTTP、一个生活状态文档、一个 tick 循环、一份日志。
//! 还没有的东西：记忆的读写与检索（P1）、自我层（P2）、完整动作集与产出回流（P3）。
//!
//! **一条底线（2026-11-02 13:09 用户提的）：折腾端口这种事不许把生命体弄死。**
//! - 端口被「另一个我」占着 → 说明我已经在跑了，安静退出（`exit 0`），不 panic。
//! - 端口被别的程序占着 → 往上找空端口，找到就接着活，换到哪个端口都写在日志和
//!   `data/port.txt` 里。
//! - 一个端口都绑不上 → 进「无声模式」：tick 照跑、状态照落库，只是没 HTTP。
//!   聋了，但还活着。
//! - `main` 不再返回 `Result`：anyhow 会把一整屏 backtrace 打出来，那对判断没帮助。

mod app;
mod config;
mod http;

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rand::Rng;
use serde_json::json;
use tokio::net::TcpListener;
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use app::App;
use config::Config;

fn main() {
    // 不用 `fn main() -> Result<()>`：那样 anyhow 在 RUST_BACKTRACE 开着的时候会
    // 打印一整页 backtrace，看着像崩了，其实只是一句「端口被占了」。
    let code = match real_main() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("steadycore 没能起来：{e:#}");
            2
        }
    };
    std::process::exit(code);
}

fn real_main() -> Result<()> {
    // 日志要最先起来，不然启动失败连原因都留不下
    let mut cfg = Config::load_or_create()?;
    let guard = init_logging(&cfg)?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("建 tokio runtime 失败")?;

    let r = rt.block_on(run(&mut cfg));
    drop(guard);
    r
}

async fn run(cfg: &mut Config) -> Result<()> {
    info!(
        version = steadycore::model::VERSION,
        pid = std::process::id(),
        home = %config::home().display(),
        "steadycore 启动"
    );

    // ── 1. 先问一句：是不是已经有一个我在跑了 ──────────────────
    if let Some((port, pid)) = find_running_core(cfg).await {
        info!(
            port,
            pid, "已经有一个 steadycore 在跑，这次只是被多叫了一次，安静退出（不是错误）"
        );
        return Ok(());
    }

    // ── 2. 绑端口：占着就往上挪，一个都绑不上就无声模式 ────────
    let want = cfg.port;
    let mut listener = None;
    let mut bound = None;
    for p in want..=want.saturating_add(cfg.port_scan) {
        match TcpListener::bind((cfg.host.as_str(), p)).await {
            Ok(l) => {
                listener = Some(l);
                bound = Some(p);
                break;
            }
            Err(e) => {
                warn!(port = p, error = %e, "这个端口绑不上，试下一个");
            }
        }
    }

    let silent = listener.is_none();
    if let Some(p) = bound {
        cfg.port = p;
        if p != want {
            warn!(want, got = p, "原端口被占，换到 {} 上继续活着", p);
        }
    } else {
        warn!(
            want,
            scan = cfg.port_scan,
            "一个端口都没绑上，进无声模式：tick 照跑、状态照落库，只是没有 HTTP"
        );
    }

    let port_file = config::data_dir().join("port.txt");
    let _ = std::fs::write(&port_file, cfg.port.to_string());
    write_lock(cfg.port);

    // ── 3. 存储 ────────────────────────────────────────────────
    let store = Arc::new(steadycore_store::Store::connect(&cfg.mongo_uri, &cfg.db).await);
    if !store.up() {
        warn!("起步时 mongo 不可用，核心继续跑，只是不落库；/v1/health 会报 down");
    } else {
        store.ensure_indexes().await;
    }

    let app = App::new(cfg.clone(), store);
    app.restore().await;

    // ── 4. 定时 tick ───────────────────────────────────────────
    spawn_ticker(app.clone());

    // ── 5. HTTP（如果有） ──────────────────────────────────────
    match listener {
        Some(l) => {
            let addr = cfg.addr();
            let router = http::router(app.clone());
            info!(
                addr = %addr,
                tick = cfg.tick_interval_sec,
                jitter = cfg.tick_jitter_sec,
                "HTTP 就绪  →  http://{addr}/v1/health"
            );
            if let Err(e) = axum::serve(l, router)
                .with_graceful_shutdown(shutdown_signal())
                .await
            {
                // 服务挂了也不许把进程带走：tick 还在跑，它还活着
                error!(error = %e, "HTTP 服务异常退出，核心继续以无声模式活着");
                shutdown_signal().await;
            }
        }
        None => {
            info!("无声模式：等着 Ctrl+C，期间 tick 照常走");
            shutdown_signal().await;
        }
    }

    // ── 6. 关停：把最后一份状态落一下 ──────────────────────────
    let s = app.life_snapshot();
    let _ = app.store.save_life(&s).await;
    clear_lock();
    info!(seq = s.seq, silent, "已落最后一份状态，再见");
    Ok(())
}

/// 找一找是不是已经有一个我在跑。
///
/// 两条线索都试：`data/core.lock` 里记的端口，以及配置里的端口（锁文件可能被删了）。
/// 只有对面真的回一句 `"name":"steadycore"` 才算，不然同端口上坐着别的程序会被误判。
async fn find_running_core(cfg: &Config) -> Option<(u16, i64)> {
    let mut ports: Vec<u16> = Vec::new();
    if let Some(p) = read_lock_port() {
        ports.push(p);
    }
    if !ports.contains(&cfg.port) {
        ports.push(cfg.port);
    }

    for p in ports {
        if let Some(pid) = probe_core(&cfg.host, p).await {
            return Some((p, pid));
        }
    }
    None
}

/// 手搓一条最短的 HTTP/1.0 GET。为了探个活把 reqwest 拖进来不值当。
async fn probe_core(host: &str, port: u16) -> Option<i64> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let addr = format!("{host}:{port}");
    let out = tokio::time::timeout(Duration::from_millis(1500), async {
        let mut s = tokio::net::TcpStream::connect(&addr).await.ok()?;
        let req = format!("GET /v1/health HTTP/1.0\r\nHost: {host}\r\n\r\n");
        s.write_all(req.as_bytes()).await.ok()?;
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf).await;
        Some(String::from_utf8_lossy(&buf).to_string())
    })
    .await;

    let text = out.ok()??;
    if !text.contains("\"name\":\"steadycore\"") {
        return None;
    }
    Some(extract_num(&text, "\"pid\":").unwrap_or(0))
}

/// 从一段 JSON 文本里抠一个整数出来，抠不到就 None。够用就行，不引 serde 解析半个响应。
fn extract_num(text: &str, key: &str) -> Option<i64> {
    let i = text.find(key)? + key.len();
    let rest = &text[i..];
    let digits: String = rest
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

fn lock_path() -> std::path::PathBuf {
    config::data_dir().join("core.lock")
}

fn write_lock(port: u16) {
    let body = json!({
        "pid": std::process::id(),
        "port": port,
        "version": steadycore::model::VERSION,
        "startedAt": config::fmt_ms(app::now_ms()),
    });
    let _ = std::fs::write(lock_path(), body.to_string());
}

fn clear_lock() {
    let p = lock_path();
    // 只删自己写的那份：pid 对不上说明是别人的锁，别乱动
    if let Ok(text) = std::fs::read_to_string(&p) {
        if extract_num(&text, "\"pid\":").is_some_and(|pid| pid == std::process::id() as i64) {
            let _ = std::fs::remove_file(&p);
        }
    }
}

fn read_lock_port() -> Option<u16> {
    let text = std::fs::read_to_string(lock_path()).ok()?;
    let p = extract_num(&text, "\"port\":")?;
    if (1..=65535).contains(&p) {
        Some(p as u16)
    } else {
        None
    }
}

fn spawn_ticker(app: Arc<App>) {
    tokio::spawn(async move {
        let base = app.cfg.tick_interval_sec;
        let jit = app.cfg.tick_jitter_sec;

        // 起手先走一步，这样 /v1/health 一上来就有数可看
        tokio::time::sleep(Duration::from_secs(1)).await;
        app.do_tick(None, "boot").await;

        loop {
            // 间隔 = 基准 ± 抖动（§10.2 不确定性之三：不是精确定时器）
            let secs = if jit == 0 {
                base
            } else {
                let lo = base.saturating_sub(jit);
                let span = jit.saturating_mul(2);
                lo + if span > 0 {
                    rand::thread_rng().gen_range(0..=span)
                } else {
                    0
                }
            }
            .max(5);

            tokio::time::sleep(Duration::from_secs(secs)).await;
            app.do_tick(None, "timed").await;
        }
    });
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("收到 Ctrl+C，准备收工");
}

/// 日志：`data/logs/core.log.YYYY-MM-DD`，同时打一份到控制台。
fn init_logging(cfg: &Config) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    let dir = config::log_dir();
    std::fs::create_dir_all(&dir)?;
    prune_old_logs(&dir, cfg.log_keep_days);

    let file_appender = tracing_appender::rolling::daily(&dir, "core.log");
    let (nb, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,steadycore_server=debug,steadycore=debug"));

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(nb)
        .with_ansi(false)
        .with_target(true);

    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stdout)
        .with_ansi(false)
        .with_target(false);

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stdout_layer)
        .try_init()
        .ok();

    Ok(guard)
}

/// 按天滚动的文件，名字里带日期，直接按名字清就是了，不用读文件 mtime。
fn prune_old_logs(dir: &Path, keep_days: u32) {
    if keep_days == 0 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let cutoff = chrono::Local::now().date_naive() - chrono::Duration::days(keep_days as i64);

    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // core.log.2026-11-02
        let Some(date_part) = name.rsplit('.').next() else {
            continue;
        };
        if let Ok(d) = chrono::NaiveDate::parse_from_str(date_part, "%Y-%m-%d") {
            if d < cutoff {
                let p = entry.path();
                if std::fs::remove_file(&p).is_ok() {
                    let _ = writeln!(std::io::stdout(), "清理旧日志 {}", p.display());
                }
            }
        }
    }
}
