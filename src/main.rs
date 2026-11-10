//! btdui: a sampling disk usage profiler for btrfs, with a gpui front-end.
//!
//! `btdui [PATH]` opens the GUI. It starts `btdui --sampler PATH`
//! as root (through pkexec by default), which streams samples back.

mod btrfs;
mod model;
mod protocol;
mod sampler;
mod session;
mod sunburst;
mod treemap;
mod ui;

use std::path::PathBuf;
use std::process::ExitCode;

use gpui::{Application, Bounds, WindowBounds, WindowOptions, prelude::*, px, size};

const USAGE: &str = "usage: btdui [--view rings|treemap|list] [PATH]\n       btdui --report PATH [SECONDS]\n       btdui --sampler PATH [--threads N]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("-h") | Some("--help") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("--sampler") => {
            let Some(path) = args.get(1) else {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            };
            let threads = match args.get(2).map(String::as_str) {
                Some("--threads") => args.get(3).and_then(|n| n.parse().ok()),
                _ => None,
            };
            match sampler::run(path.as_ref(), threads) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("btdui sampler: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("--report") => {
            let Some(path) = args.get(1) else {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            };
            let secs = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
            report(PathBuf::from(path), secs)
        }
        _ => {
            let mut view = ui::View::Rings;
            let mut path = None;
            let mut it = args.iter();
            while let Some(arg) = it.next() {
                match arg.as_str() {
                    "--view" => match it.next().map(String::as_str) {
                        Some("rings") => view = ui::View::Rings,
                        Some("treemap") => view = ui::View::Treemap,
                        Some("list") => view = ui::View::List,
                        _ => {
                            eprintln!("{USAGE}");
                            return ExitCode::from(2);
                        }
                    },
                    other => path = Some(PathBuf::from(other)),
                }
            }
            run_gui(path, view);
            ExitCode::SUCCESS
        }
    }
}

/// Samples for `secs` seconds, then prints the two top levels of the tree.
fn report(path: PathBuf, secs: u64) -> ExitCode {
    let session = session::Session::start(path);
    std::thread::sleep(std::time::Duration::from_secs(secs));
    let m = session.model.lock().unwrap();
    if let model::Status::Failed(msg) = &m.status {
        eprintln!("error: {msg}");
        return ExitCode::FAILURE;
    }
    println!(
        "{} samples, total {}, resolution {}",
        m.samples,
        ui::format_size(m.total_bytes() as f64),
        ui::format_size(m.total_bytes() as f64 / m.samples.max(1) as f64)
    );
    println!(
        "{:>11} {:>11} {:>11}  path",
        "represented", "exclusive", "shared"
    );
    let mut stack = vec![(model::ROOT, 0usize)];
    while let Some((id, depth)) = stack.pop() {
        let n = &m.nodes[id as usize];
        if id != model::ROOT {
            println!(
                "{:>11} {:>11} {:>11}  {}{}",
                ui::format_size(m.bytes(n.represented)),
                ui::format_size(m.bytes(n.exclusive)),
                ui::format_size(m.bytes(n.shared)),
                "  ".repeat(depth - 1),
                n.name
            );
        }
        if depth < 2 {
            let mut kids: Vec<_> = n.children.values().copied().collect();
            kids.sort_by_key(|&k| std::cmp::Reverse(m.nodes[k as usize].represented));
            kids.truncate(12);
            stack.extend(kids.into_iter().rev().map(|k| (k, depth + 1)));
        }
    }
    ExitCode::SUCCESS
}

fn run_gui(path: Option<PathBuf>, view: ui::View) {
    Application::new().run(move |cx| {
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                app_id: Some("btdui".into()),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| ui::BtduApp::new(path, view, window, cx)),
        )
        .expect("failed to open the window");
        cx.activate(true);
    });
}
