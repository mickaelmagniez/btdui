//! Spawns the privileged sampler and feeds its output into a [`Model`].

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::model::{Model, Status};
use crate::protocol;

pub struct Session {
    pub path: PathBuf,
    pub model: Arc<Mutex<Model>>,
    stdin: Mutex<Option<ChildStdin>>,
}

/// How to gain root for the sampler: `BTDUI_ELEVATE` (e.g. `sudo -A`,
/// or `none`), defaulting to `pkexec` unless we already are root.
fn sampler_command(path: &Path) -> std::io::Result<Command> {
    // Inside an AppImage, the binary lives in a FUSE mount that root cannot
    // read: hand the AppImage file itself to the elevation helper instead.
    let exe = match std::env::var_os("APPIMAGE") {
        Some(appimage) => PathBuf::from(appimage),
        None => std::env::current_exe()?,
    };
    // SAFETY: geteuid has no preconditions.
    let is_root = unsafe { libc::geteuid() } == 0;
    let elevate = std::env::var("BTDUI_ELEVATE").ok();
    let wrapper: Vec<String> = match elevate.as_deref() {
        Some("none") | Some("") => Vec::new(),
        Some(cmd) => cmd.split_whitespace().map(str::to_owned).collect(),
        None if is_root => Vec::new(),
        None => vec!["pkexec".to_owned()],
    };
    let mut cmd = match wrapper.split_first() {
        Some((prog, args)) => {
            let mut c = Command::new(prog);
            c.args(args).arg(&exe);
            c
        }
        None => Command::new(&exe),
    };
    cmd.arg("--sampler").arg(path);
    if let Ok(threads) = std::env::var("BTDUI_THREADS") {
        cmd.arg("--threads").arg(threads);
    }
    Ok(cmd)
}

impl Session {
    pub fn start(path: PathBuf) -> Arc<Self> {
        let model = Arc::new(Mutex::new(Model::new()));
        let session = Arc::new(Self {
            path: path.clone(),
            model: model.clone(),
            stdin: Mutex::new(None),
        });

        let spawned = sampler_command(&path).and_then(|mut cmd| {
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
        });
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) => {
                model.lock().unwrap().status =
                    Status::Failed(format!("cannot start the sampler: {e}"));
                return session;
            }
        };
        *session.stdin.lock().unwrap() = child.stdin.take();

        let stdout = child.stdout.take().expect("piped stdout");
        let reader = {
            let model = model.clone();
            thread::spawn(move || {
                let mut r = BufReader::with_capacity(1 << 16, stdout);
                loop {
                    match protocol::read(&mut r) {
                        Ok(Some(msg)) => model.lock().unwrap().apply(msg),
                        Ok(None) => break,
                        Err(e) => {
                            model
                                .lock()
                                .unwrap()
                                .log
                                .push(format!("protocol error: {e}"));
                            break;
                        }
                    }
                }
            })
        };

        let stderr = child.stderr.take().expect("piped stderr");
        {
            let model = model.clone();
            thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let mut m = model.lock().unwrap();
                    m.log.push(line);
                    m.version += 1;
                }
            });
        }

        thread::spawn(move || {
            let status = child.wait();
            let _ = reader.join();
            let mut m = model.lock().unwrap();
            if matches!(m.status, Status::Failed(_)) {
                return;
            }
            let last_log = m.log.last().cloned().unwrap_or_default();
            m.status = match status {
                Ok(s) if s.success() => Status::Finished("the sampler exited".into()),
                Ok(s) if s.code() == Some(126) => {
                    Status::Failed("authentication was dismissed (pkexec exit code 126)".into())
                }
                Ok(s) if s.code() == Some(127) => Status::Failed(format!(
                    "not authorized to run the sampler as root. {last_log}"
                )),
                Ok(s) => Status::Failed(format!("the sampler exited with {s}. {last_log}")),
                Err(e) => Status::Failed(format!("cannot wait for the sampler: {e}")),
            };
            m.version += 1;
        });

        session
    }

    pub fn set_paused(&self, paused: bool) {
        if let Some(stdin) = self.stdin.lock().unwrap().as_mut() {
            let _ = stdin.write_all(if paused { b"p" } else { b"r" });
            let _ = stdin.flush();
        }
        let mut m = self.model.lock().unwrap();
        m.paused = paused;
        m.version += 1;
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Closing stdin makes the (possibly root-owned) sampler exit.
        self.stdin.lock().unwrap().take();
    }
}

/// A mounted btrfs filesystem, as listed in `/proc/self/mountinfo`.
#[derive(Clone, Debug)]
pub struct Mount {
    pub mountpoint: PathBuf,
    pub source: String,
    pub subvol: String,
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8)
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One entry per btrfs filesystem, preferring the top-level mount, then the
/// shortest mount point.
pub fn btrfs_mounts() -> Vec<Mount> {
    let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new();
    };
    let mut best: Vec<(String, Mount)> = Vec::new();
    for line in info.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let Some(sep) = fields.iter().position(|&f| f == "-") else {
            continue;
        };
        if fields.get(sep + 1) != Some(&"btrfs") || fields.len() < 5 {
            continue;
        }
        let dev = fields[2].to_owned();
        let mount = Mount {
            mountpoint: PathBuf::from(unescape(fields[4])),
            source: fields.get(sep + 2).map(|s| unescape(s)).unwrap_or_default(),
            subvol: unescape(fields[3]),
        };
        let rank = |m: &Mount| (m.subvol != "/", m.mountpoint.as_os_str().len());
        match best.iter_mut().find(|(d, _)| *d == dev) {
            Some((_, cur)) if rank(&mount) < rank(cur) => *cur = mount,
            Some(_) => {}
            None => best.push((dev, mount)),
        }
    }
    best.into_iter().map(|(_, m)| m).collect()
}
