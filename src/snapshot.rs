//! Still images for the Home app (tiles, notifications): `POST /resource`. No decoding here:
//! the JPEG comes from the camera or NVR (Protect API, or any HTTP snapshot URL) and is
//! returned as is (iOS scales it). Cached briefly, since the Home app polls.

use crate::protect::Protect;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub enum Source {
    Protect(Arc<Protect>),
    Url(String),
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
