//! Media domain: inline images, uploads, downloads.

use super::{Event, Session};
use std::path::PathBuf;
use std::sync::mpsc::Sender;

const MAX_EDGE: u32 = 1600;

/// `Command::FetchImage` — fetch + decode one image for inline display.
pub async fn fetch_image(ses: &Session, tx: &Sender<Event>, url: &str) {
    let Some(c) = ses.client.as_ref() else {
        let _ = tx.send(Event::ImageFailed(url.to_string()));
        return;
    };
    let client = c.clone();
    let url_owned = url.to_string();
    let tx2 = tx.clone();
    let url_for_decode = url_owned.clone();

    // Fetch on the runtime; decode off it.
    let fetched = ost::api::fetch_media_data(&client, &url_owned).await;
    tokio::task::spawn_blocking(move || match fetched {
        Ok(mb) => match decode_rgba(&mb.data) {
            Ok((rgba, size)) => {
                let _ = tx2.send(Event::ImageReady {
                    url: url_for_decode,
                    rgba,
                    size,
                });
            }
            Err(e) => {
                log::warn!("image decode failed: {e}");
                let _ = tx2.send(Event::ImageFailed(url_for_decode));
            }
        },
        Err(e) => {
            log::warn!("image fetch failed: {e:#}");
            let _ = tx2.send(Event::ImageFailed(url_for_decode));
        }
    });
}

/// Decode to RGBA8, downscaling to `MAX_EDGE` on the long edge.
fn decode_rgba(bytes: &[u8]) -> anyhow::Result<(Vec<u8>, [usize; 2])> {
    let img = image::load_from_memory(bytes)?;
    let (w, h) = (img.width(), img.height());
    let img = if w.max(h) > MAX_EDGE {
        let scale = MAX_EDGE as f32 / w.max(h) as f32;
        let (nw, nh) = ((w as f32 * scale) as u32, (h as f32 * scale) as u32);
        img.resize(nw, nh, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let size = [img.width() as usize, img.height() as usize];
    Ok((img.to_rgba8().into_raw(), size))
}

/// `Command::UploadFile` — send a local file into a chat.
pub async fn upload(ses: &Session, tx: &Sender<Event>, chat_id: &str, path: &PathBuf) {
    let Some(c) = ses.client.as_ref() else {
        return;
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    let total = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let _ = tx.send(Event::UploadProgress {
        chat_id: chat_id.to_string(),
        name: name.clone(),
        sent: 0,
        total,
    });
    let path_str = path.to_string_lossy().to_string();
    match ost::api::upload_file_data(c, chat_id, &path_str).await {
        Ok(_file) => {
            let _ = tx.send(Event::UploadDone {
                chat_id: chat_id.to_string(),
                name,
            });
            // Refetch newest history so the file message shows up.
            if let Ok(page) = ost::api::read_messages_page(c, chat_id, 50, None).await {
                let _ = tx.send(Event::Messages {
                    older_link: page.backward_link,
                    prepend: false,
                    chat_id: chat_id.to_string(),
                    messages: page.messages,
                    members: HashMap::new(),
                    resolved_name: None,
                });
            }
        }
        Err(e) => {
            let _ = tx.send(Event::Error(format!("upload: {e:#}")));
        }
    }
}

/// `Command::DownloadFile` — fetch a shared file to disk and open it.
pub async fn download(ses: &Session, tx: &Sender<Event>, url: &str, name: &str) {
    let Some(c) = ses.client.as_ref() else {
        return;
    };
    let result = ost::api::fetch_media_data(c, url).await;
    match result {
        Ok(mb) => {
            let dir = dirs_home().join("Downloads");
            let _ = std::fs::create_dir_all(&dir);
            let path = numbered_variant(&dir, &safe_file_name(name));
            match std::fs::write(&path, &mb.data) {
                Ok(()) => {
                    let _ = tx.send(Event::DownloadDone {
                        name: name.to_string(),
                        path: path.clone(),
                    });
                    let p = path.clone();
                    std::thread::spawn(move || {
                        let _ = open::that(&p);
                    });
                }
                Err(e) => {
                    let _ = tx.send(Event::Error(format!("save: {e}")));
                }
            }
        }
        Err(e) => {
            let _ = tx.send(Event::Error(format!("download: {e:#}")));
        }
    }
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// A file name from a message or a drive listing, safe to join to the
/// Downloads folder. The sender chooses it, so `../.bashrc` or an
/// absolute path once wrote outside Downloads (`PathBuf::join` replaces
/// the folder with an absolute path). Keeps only the last component
/// (either separator), drops control characters and leading dots, caps
/// the length, and never comes back empty.
pub(super) fn safe_file_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let printable: String = last
        .chars()
        .filter(|c| !c.is_control() && !is_bidi_control(*c))
        .take(120)
        .collect();
    let name = printable.trim().trim_start_matches('.').trim();
    if name.is_empty() {
        "download".into()
    } else {
        name.to_string()
    }
}

/// Direction marks and overrides, which can make `evil\u{202e}fdp.exe`
/// display as `evilexe.pdf`.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// `name.ext` → `name.ext`, then `name (1).ext`, `name (2).ext`, …
fn numbered_variant(dir: &PathBuf, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let stem = std::path::Path::new(name)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    let ext = std::path::Path::new(name)
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    for i in 1..1000u32 {
        let candidate = dir.join(format!("{stem} ({i}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem}-{timestamp}{ext}", timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)))
}

use std::collections::HashMap;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_names_stay_in_the_folder() {
        let dir = PathBuf::from("/home/u/Downloads");
        for (name, want) in [
            ("Plan.docx", "Plan.docx"),
            ("../.bashrc", "bashrc"),
            ("../../.config/autostart/x.desktop", "x.desktop"),
            ("/home/u/.ssh/authorized_keys", "authorized_keys"),
            ("..\\..\\evil.bat", "evil.bat"),
            ("..", "download"),
            (".", "download"),
            ("", "download"),
            ("a/", "download"),
            ("re\u{202e}port\n.pdf", "report.pdf"),
        ] {
            let got = safe_file_name(name);
            assert_eq!(got, want, "{name:?}");
            let path = dir.join(&got);
            assert_eq!(
                path.parent(),
                Some(dir.as_path()),
                "{name:?} stays in Downloads"
            );
        }
        assert_eq!(safe_file_name("evil\u{202e}fdp.exe"), "evilfdp.exe");
        assert_eq!(safe_file_name(&"x".repeat(500)).len(), 120);
    }
}
