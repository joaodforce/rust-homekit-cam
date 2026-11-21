//! Still images for the Home app (tiles, notifications): `POST /resource`. The JPEG comes from
//! the camera or NVR (Protect API, or any HTTP snapshot URL), or from a user-provided ffmpeg run
//! per snapshot to grab one frame from an RTSP stream. Cached briefly, since the Home app polls.
//!
//! Last step (`snapshot_resize = true`): when the controller asks for a much smaller image (the
//! Apple Watch asks for ~320x240, through a home hub), the JPEG is decoded straight at 1/2, 1/4
//! or 1/8 scale, area-averaged to fit and re-encoded: a few KB instead of a few hundred.

use crate::protect::Protect;
use std::collections::HashMap;
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
    /// resize for small requests (`snapshot_resize`)
    resize: bool,
    /// resized copies per requested size, tagged with the source image's fetch time
    small: Mutex<HashMap<(u32, u32), (Instant, Arc<Vec<u8>>)>>,
}

const FRESH: Duration = Duration::from_secs(5);

impl Snapshots {
    pub fn new(source: Source) -> Self {
        Snapshots { source, cache: Mutex::new(None), resize: false, small: Mutex::default() }
    }

    pub fn with_resize(mut self, on: bool) -> Self {
        self.resize = on;
        self
    }

    /// A JPEG for a `w`x`h` request: the source image, or (with resizing on) a smaller copy.
    pub fn get_sized(&self, w: u32, h: u32) -> Result<Arc<Vec<u8>>, String> {
        let (t, img) = self.fetch()?;
        if !self.resize || w == 0 || h == 0 {
            return Ok(img);
        }
        let mut small = self.small.lock().unwrap();
        if let Some((ts, s)) = small.get(&(w, h)) {
            if *ts == t {
                return Ok(s.clone());
            }
        }
        let start = Instant::now();
        let out = match resize_jpeg(&img, w, h) {
            Ok(Some((jpeg, (sw, sh), (tw, th)))) => {
                if !small.contains_key(&(w, h)) {
                    crate::log(format!(
                        "snapshot: {w}x{h} requested: {sw}x{sh} {} KB → {tw}x{th} {} KB in {} ms",
                        img.len() / 1024,
                        jpeg.len() / 1024,
                        start.elapsed().as_millis()
                    ));
                }
                Arc::new(jpeg)
            }
            Ok(None) => img, // not much smaller: send the original
            Err(e) => {
                crate::log(format!("snapshot: resize to {w}x{h} failed ({e}), sending the original"));
                img
            }
        };
        small.insert((w, h), (t, out.clone()));
        Ok(out)
    }

    /// A JPEG, at most a few seconds old.
    pub fn get(&self) -> Result<Arc<Vec<u8>>, String> {
        Ok(self.fetch()?.1)
    }

    /// The source JPEG and when it was fetched. One fetch at a time (the lock is held meanwhile).
    fn fetch(&self) -> Result<(Instant, Arc<Vec<u8>>), String> {
        let mut c = self.cache.lock().unwrap();
        if let Some((t, img)) = c.as_ref() {
            if t.elapsed() < FRESH {
                return Ok((*t, img.clone()));
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
        let t = Instant::now();
        *c = Some((t, img.clone()));
        Ok((t, img))
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

/// Shrink a JPEG to fit `w`x`h` (aspect kept). None when it wouldn't get much smaller: the target
/// is over half the source's size, or the re-encoded image isn't clearly smaller than the source.
/// Returns (jpeg, source size, new size).
#[allow(clippy::type_complexity)]
pub fn resize_jpeg(jpeg: &[u8], w: u32, h: u32) -> Result<Option<(Vec<u8>, (u32, u32), (u32, u32))>, String> {
    let Some((buf, src, dst)) = scale_jpeg(jpeg, 80, |sw, sh| {
        let k = (w as f64 / sw as f64).min(h as f64 / sh as f64);
        (k <= 0.5).then_some(k)
    })?
    else {
        return Ok(None);
    };
    if buf.len() * 10 > jpeg.len() * 7 {
        return Ok(None); // under 30% saved: not worth a different image
    }
    Ok(Some((buf, src, dst)))
}

/// A JPEG at most `width` wide (aspect kept) at `quality` (1–100): the source when it is
/// already that small. For thumbnails sent with motion events.
pub fn shrink_jpeg(jpeg: &[u8], width: u32, quality: u8) -> Result<Vec<u8>, String> {
    let r = scale_jpeg(jpeg, quality.clamp(1, 100), |sw, _| (sw > width).then(|| width as f64 / sw as f64))?;
    Ok(r.map(|(buf, _, _)| buf).unwrap_or_else(|| jpeg.to_vec()))
}

/// Decode at the smallest 1/2, 1/4, 1/8 scale still at least the target, area-average to the
/// target and re-encode. `factor(source w, h)` gives the scale, or None to leave it alone.
#[allow(clippy::type_complexity)]
fn scale_jpeg(jpeg: &[u8], quality: u8, factor: impl Fn(u32, u32) -> Option<f64>) -> Result<Option<(Vec<u8>, (u32, u32), (u32, u32))>, String> {
    let mut d = jpeg_decoder::Decoder::new(jpeg);
    d.read_info().map_err(|e| e.to_string())?;
    let info = d.info().ok_or("no image info")?;
    let (sw, sh) = (info.width as u32, info.height as u32);
    let Some(k) = factor(sw, sh) else { return Ok(None) };
    let (tw, th) = (((sw as f64 * k).round() as u32).max(1), ((sh as f64 * k).round() as u32).max(1));
    let (dw, dh) = d.scale(tw as u16, th as u16).map_err(|e| e.to_string())?;
    let px = d.decode().map_err(|e| e.to_string())?;
    let (c, color) = match d.info().map(|i| i.pixel_format) {
        Some(jpeg_decoder::PixelFormat::RGB24) => (3, jpeg_encoder::ColorType::Rgb),
        Some(jpeg_decoder::PixelFormat::L8) => (1, jpeg_encoder::ColorType::Luma),
        other => return Err(format!("unsupported pixel format {other:?}")),
    };
    let out = area_resample(&px, dw as usize, dh as usize, c, tw as usize, th as usize);
    let mut buf = Vec::with_capacity(out.len() / 8);
    jpeg_encoder::Encoder::new(&mut buf, quality).encode(&out, tw as u16, th as u16, color).map_err(|e| e.to_string())?;
    Ok(Some((buf, (sw, sh), (tw, th))))
}

/// Box-filter downscale: each output pixel averages the source pixels it covers.
fn area_resample(src: &[u8], sw: usize, sh: usize, c: usize, tw: usize, th: usize) -> Vec<u8> {
    let mut out = vec![0u8; tw * th * c];
    for y in 0..th {
        let (y0, y1) = (y * sh / th, ((y + 1) * sh / th).max(y * sh / th + 1));
        for x in 0..tw {
            let (x0, x1) = (x * sw / tw, ((x + 1) * sw / tw).max(x * sw / tw + 1));
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            for ch in 0..c {
                let mut sum = 0u32;
                for yy in y0..y1 {
                    let row = &src[(yy * sw + x0) * c..(yy * sw + x1) * c];
                    sum += row.iter().skip(ch).step_by(c).map(|&v| v as u32).sum::<u32>();
                }
                out[(y * tw + x) * c + ch] = (sum / n) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_jpeg(w: u16, h: u16) -> Vec<u8> {
        // a gradient with a bright square, so averaging is visible
        let mut px = Vec::with_capacity(w as usize * h as usize * 3);
        for y in 0..h as u32 {
            for x in 0..w as u32 {
                let inside = x > w as u32 / 4 && x < w as u32 / 2 && y > h as u32 / 4 && y < h as u32 / 2;
                px.extend_from_slice(&if inside { [250, 250, 250] } else { [(x * 255 / w as u32) as u8, (y * 255 / h as u32) as u8, 60] });
            }
        }
        let mut buf = Vec::new();
        jpeg_encoder::Encoder::new(&mut buf, 85).encode(&px, w, h, jpeg_encoder::ColorType::Rgb).unwrap();
        buf
    }

    #[test]
    fn shrinks_for_the_watch_keeps_aspect() {
        let big = test_jpeg(1920, 1080);
        let (small, src, dst) = resize_jpeg(&big, 320, 240).unwrap().unwrap();
        assert_eq!(src, (1920, 1080));
        assert_eq!(dst, (320, 180)); // 16:9 kept, fits 320x240
        assert!(small.len() * 5 < big.len(), "{} vs {}", small.len(), big.len());
        let mut d = jpeg_decoder::Decoder::new(&small[..]);
        let px = d.decode().unwrap();
        let i = d.info().unwrap();
        assert_eq!((i.width, i.height), (320, 180));
        // the bright square survives at the same place: (120, 60) is inside it
        let p = &px[(60 * 320 + 120) * 3..][..3];
        assert!(p.iter().all(|&v| v > 200), "{p:?}");
    }

    #[test]
    fn leaves_big_requests_alone() {
        let img = test_jpeg(640, 480);
        assert!(resize_jpeg(&img, 640, 480).unwrap().is_none());
        assert!(resize_jpeg(&img, 600, 450).unwrap().is_none());
        // seen from iPhones: a portrait 480x640 tile for a 640x360 source (0.75): not worth it
        assert!(resize_jpeg(&test_jpeg(640, 360), 480, 640).unwrap().is_none());
        assert!(resize_jpeg(&img, 320, 240).unwrap().is_some());
        assert!(resize_jpeg(b"not a jpeg", 320, 240).is_err());
    }

    #[test]
    fn thumbnails_for_motion_events() {
        let big = test_jpeg(1280, 960);
        let t = shrink_jpeg(&big, 320, 60).unwrap();
        let mut d = jpeg_decoder::Decoder::new(&t[..]);
        d.read_info().unwrap();
        let i = d.info().unwrap();
        assert_eq!((i.width, i.height), (320, 240));
        assert!(t.len() < 20_000, "{}", t.len());
        // already small: sent as is
        let small = test_jpeg(300, 200);
        assert_eq!(shrink_jpeg(&small, 320, 60).unwrap(), small);
    }
}
