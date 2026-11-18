//! Still images for the Home app (tiles, notifications): `POST /resource`. No decoding here:
//! the JPEG comes from the camera or NVR (Protect API, or any HTTP snapshot URL) and is
//! returned as is (iOS scales it). Cameras without one can use a user-provided ffmpeg, run
//! per snapshot to grab one frame from an RTSP stream. Cached briefly, since the Home app polls.

use crate::protect::Protect;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub enum Source {
    Protect(Arc<Protect>),
    Url(String),
    /// (ffmpeg binary, RTSP URL): one frame from the stream
    Ffmpeg(String, String),
}

pub struct Snapshots {
    source: Source,
    cache: Mutex<Option<(Instant, Arc<Vec<u8>>)>>,
}

const FRESH: Duration = Duration::from_secs(5);

impl Snapshots {
    pub fn new(source: Source) -> Self {
        Snapshots { source, cache: Mutex::new(None) }
    }

    /// A JPEG, at most a few seconds old. One fetch at a time (the lock is held while fetching).
    pub fn get(&self) -> Result<Arc<Vec<u8>>, String> {
        let mut c = self.cache.lock().unwrap();
        if let Some((t, img)) = c.as_ref() {
            if t.elapsed() < FRESH {
                return Ok(img.clone());
            }
        }
        let img = Arc::new(match &self.source {
            Source::Protect(p) => p.snapshot()?,
            Source::Url(u) => {
                let r = crate::httpc::get(u)?;
                if r.status != 200 {
                    return Err(format!("snapshot URL: HTTP {}", r.status));
                }
                r.body
            }
            Source::Ffmpeg(bin, url) => ffmpeg_frame(bin, url, Duration::from_secs(10))?,
        });
        if !img.starts_with(&[0xff, 0xd8]) {
            return Err("snapshot is not a JPEG".into());
        }
        *c = Some((Instant::now(), img.clone()));
        Ok(img)
    }

    /// Forget the cached image (a ring: the notification should show who is at the door now).
    pub fn invalidate(&self) {
        *self.cache.lock().unwrap() = None;
    }
}

/// Run `ffmpeg` to decode the first frame of an RTSP stream into a JPEG; killed after `timeout`.
pub fn ffmpeg_frame(bin: &str, url: &str, timeout: Duration) -> Result<Vec<u8>, String> {
    let mut child = Command::new(bin)
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-rtsp_transport", "tcp", "-i", url])
        .args(["-an", "-frames:v", "1", "-q:v", "3", "-f", "mjpeg", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("snapshot: can't run {bin}: {e} (install ffmpeg or set snapshot_ffmpeg to its path)"))?;
    let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let reader = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = out.read_to_end(&mut v);
        v
    });
    let errs = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
            break Some(st);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let img = reader.join().unwrap_or_default();
    let msg = errs.join().unwrap_or_default();
    match status {
        None => Err(format!("snapshot: ffmpeg took over {}s", timeout.as_secs())),
        Some(st) if !st.success() || img.is_empty() => {
            // never log the URL: it carries the camera's password
            let first = msg.lines().map(|l| l.replace(url, "<stream>")).next().unwrap_or_default();
            Err(format!("snapshot: ffmpeg failed ({st}): {first}"))
        }
        Some(_) => Ok(img),
    }
}
