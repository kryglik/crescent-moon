//! Quick view (Ctrl+Q): the other panel shows what is under the cursor: the start of a text
//! file, a hex dump of a binary one, what a picture is, or a folder's contents. Files are read
//! on a background thread, so a slow network drive can't hold the screen up, and only the
//! latest request counts when the cursor moves fast.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::SystemTime;

use crate::picture::{self, Picture};

/// How much of a file is read.
const READ_LIMIT: u64 = 64 * 1024;
/// How many lines of text are kept.
const LINE_LIMIT: usize = 2000;
/// How many entries of a folder are listed.
const FOLDER_LIMIT: usize = 2000;

#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    Loading,
    Text { lines: Vec<String>, encoding: &'static str, partial: bool },
    /// A hex dump of the start of the file.
    Binary { lines: Vec<String>, partial: bool },
    Picture(Picture),
    Folder { folders: usize, files: usize, bytes: u64, names: Vec<(String, bool)>, partial: bool },
    Message(String),
}

/// What the preview shows: which file (with its size and time, so a change reloads it).
#[derive(Clone, Debug, PartialEq)]
pub struct Key {
    pub path: PathBuf,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// A file inside a zip: the zip on disk and the file's place in it.
    pub archive: Option<(PathBuf, usize)>,
}

pub struct Preview {
    pub key: Key,
    pub content: Content,
    /// The first line shown.
    pub scroll: usize,
}

pub struct Previewer {
    requests: Sender<Key>,
    results: Receiver<(Key, Content)>,
    pub current: Option<Preview>,
}

impl Previewer {
    pub fn new() -> Previewer {
        let (requests, wanted) = mpsc::channel::<Key>();
        let (done, results) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(mut key) = wanted.recv() {
                // Only the latest request matters.
                while let Ok(newer) = wanted.try_recv() {
                    key = newer;
                }
                let content = load_key(&key);
                if done.send((key, content)).is_err() {
                    break;
                }
            }
        });
        Previewer { requests, results, current: None }
    }

    /// Shows `key`, asking for it unless it is already shown. None shows nothing.
    pub fn show(&mut self, key: Option<Key>) {
        let Some(key) = key else {
            self.current = None;
            return;
        };
        if self.current.as_ref().is_some_and(|p| p.key == key) {
            return;
        }
        // The same file, changed (a log growing): keep the place in it while it reloads.
        let scroll = self.current.as_ref().filter(|p| p.key.path == key.path).map_or(0, |p| p.scroll);
        let _ = self.requests.send(key.clone());
        self.current = Some(Preview { key, content: Content::Loading, scroll });
    }

    /// Takes what the background thread has read. Returns whether the shown preview changed.
    pub fn tick(&mut self) -> bool {
        let mut changed = false;
        while let Ok((key, content)) = self.results.try_recv() {
            if let Some(current) = self.current.as_mut().filter(|p| p.key == key) {
                current.content = content;
                changed = true;
            }
        }
        changed
    }

    pub fn loading(&self) -> bool {
        self.current.as_ref().is_some_and(|p| p.content == Content::Loading)
    }

    /// Scrolls by `delta` lines, within `height` rows on screen.
    pub fn scroll(&mut self, delta: isize, height: usize) {
        let Some(preview) = self.current.as_mut() else { return };
        let max = preview.content.len().saturating_sub(height.max(1));
        preview.scroll = (preview.scroll as isize + delta).clamp(0, max as isize) as usize;
    }
}

impl Content {
    /// Lines it has, for scrolling.
    pub fn len(&self) -> usize {
        match self {
            Content::Text { lines, .. } | Content::Binary { lines, .. } => lines.len(),
            Content::Folder { names, .. } => names.len(),
            Content::Picture(picture) => picture.rows.len(),
            _ => 0,
        }
    }
}

/// A file on disk, or the start of one inside a zip (only what is shown is unpacked).
fn load_key(key: &Key) -> Content {
    let Some((zip, index)) = &key.archive else { return load(&key.path) };
    match crate::archive::read_start(zip, *index, READ_LIMIT) {
        Ok(data) => content_of(&mut io::Cursor::new(data), key.size, &key.path).unwrap_or_else(|e| Content::Message(format!("Cannot read it: {e}"))),
        Err(e) => Content::Message(format!("Cannot read it from the archive: {e}")),
    }
}

pub fn load(path: &Path) -> Content {
    match fs::metadata(path) {
        Ok(meta) if meta.is_dir() => load_folder(path),
        Ok(meta) => load_file(path, meta.len()).unwrap_or_else(|e| Content::Message(format!("Cannot read it: {e}"))),
        Err(e) => Content::Message(format!("Cannot read it: {e}")),
    }
}

fn load_folder(path: &Path) -> Content {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(e) => return Content::Message(format!("Cannot open it: {e}")),
    };
    let (mut folders, mut files, mut bytes, mut names, mut partial) = (0, 0, 0, Vec::new(), false);
    for entry in entries.flatten() {
        if names.len() >= FOLDER_LIMIT {
            partial = true;
            break;
        }
        let is_dir = entry.metadata().map(|m| m.is_dir()).or_else(|_| entry.file_type().map(|t| t.is_dir())).unwrap_or(false);
        if is_dir {
            folders += 1;
        } else {
            files += 1;
            bytes += entry.metadata().map_or(0, |m| m.len());
        }
        names.push((entry.file_name().to_string_lossy().into_owned(), is_dir));
    }
    names.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
    Content::Folder { folders, files, bytes, names, partial }
}

fn load_file(path: &Path, size: u64) -> io::Result<Content> {
    content_of(&mut File::open(path)?, size, path)
}

/// What a file (`size` bytes long, called `name`) is, from its start.
fn content_of<R: Read + Seek>(file: &mut R, size: u64, name: &Path) -> io::Result<Content> {
    let ending = name.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    if let Some(picture) = picture::read(file, ending.as_deref())? {
        return Ok(Content::Picture(picture));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut data = Vec::new();
    file.by_ref().take(READ_LIMIT).read_to_end(&mut data)?;
    let partial = size > data.len() as u64;
    if size == 0 {
        return Ok(Content::Message("The file is empty.".into()));
    }
    Ok(match decode(&data) {
        Some((text, encoding)) => {
            let mut lines: Vec<String> = text.lines().take(LINE_LIMIT).map(printable).collect();
            // A cut-off read may end in the middle of a line; drop the piece.
            if partial && lines.len() > 1 {
                lines.pop();
            }
            Content::Text { lines, encoding, partial }
        }
        None => Content::Binary { lines: hex_dump(&data[..data.len().min(4096)]), partial: size > 4096 },
    })
}

/// Text in UTF-8 (with or without BOM) or UTF-16 with a BOM; None for binary data.
fn decode(data: &[u8]) -> Option<(String, &'static str)> {
    if let Some(rest) = data.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = rest.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return Some((String::from_utf16_lossy(&units), "UTF-16"));
    }
    if let Some(rest) = data.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = rest.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        return Some((String::from_utf16_lossy(&units), "UTF-16 BE"));
    }
    let data = data.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(data);
    // A NUL, or many control characters, means binary.
    let sample = &data[..data.len().min(8192)];
    let controls = sample.iter().filter(|&&b| b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r' | 0x0C | 0x1B)).count();
    if sample.contains(&0) || controls * 20 > sample.len().max(1) {
        return None;
    }
    let text = String::from_utf8_lossy(data).into_owned();
    let encoding = if std::str::from_utf8(data).is_ok() { "UTF-8" } else { "UTF-8 (with invalid bytes)" };
    Some((text, encoding))
}

/// A line made safe to draw: tabs become spaces, other control characters a dot, so nothing
/// in a file can send escape sequences to the terminal.
fn printable(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for c in line.chars().take(1000) {
        match c {
            '\t' => {
                let col = out.chars().count();
                out.extend(std::iter::repeat_n(' ', 4 - col % 4));
            }
            c if c.is_control() => out.push('·'),
            c => out.push(c),
        }
    }
    out
}

fn hex_dump(data: &[u8]) -> Vec<String> {
    data.chunks(16)
        .enumerate()
        .map(|(i, chunk)| {
            let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
            let text: String = chunk.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }).collect();
            format!("{:08x}  {:<47}  {text}", i * 16, hex.join(" "))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn text_files_in_utf8_and_utf16() {
        let tmp = TempDir::new("preview-text");
        let file = tmp.file("a.txt", "první\třádek\r\ndruhý\x1b[31m\n");
        let Content::Text { lines, encoding, partial } = load(&file) else { panic!("{:?}", load(&file)) };
        assert_eq!(lines, vec!["první   řádek".to_string(), "druhý·[31m".to_string()]);
        assert_eq!((encoding, partial), ("UTF-8", false));

        let utf16 = tmp.path().join("u16.txt");
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend("Ahoj\r\nsvěte".encode_utf16().flat_map(u16::to_le_bytes));
        fs::write(&utf16, bytes).unwrap();
        let Content::Text { lines, encoding, .. } = load(&utf16) else { panic!() };
        assert_eq!(lines, vec!["Ahoj".to_string(), "světe".to_string()]);
        assert_eq!(encoding, "UTF-16");
    }

    #[test]
    fn binary_files_as_a_hex_dump() {
        let tmp = TempDir::new("preview-bin");
        let file = tmp.path().join("a.bin");
        fs::write(&file, b"MZ\x90\x00\x03\x00hello world, binary").unwrap();
        let Content::Binary { lines, partial } = load(&file) else { panic!() };
        assert_eq!(lines[0], "00000000  4d 5a 90 00 03 00 68 65 6c 6c 6f 20 77 6f 72 6c  MZ....hello worl");
        assert!(!partial);
    }

    #[test]
    fn picture_sizes() {
        let tmp = TempDir::new("preview-pic");
        let mut png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        png.extend(640u32.to_be_bytes());
        png.extend(480u32.to_be_bytes());
        png.extend([8, 2, 0, 0, 0]);
        let path = tmp.path().join("a.png");
        fs::write(&path, &png).unwrap();
        let Content::Picture(picture) = load(&path) else { panic!("{:?}", load(&path)) };
        assert_eq!((picture.format, picture.row("Size")), ("PNG", Some("640 × 480 pixels, 0.3 megapixels")));

        // A JPEG with an APP1 (EXIF) segment before its frame header.
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x10];
        jpeg.extend([0u8; 14]);
        jpeg.extend([0xFF, 0xC0, 0x00, 0x11, 0x08, 0x0B, 0xD0, 0x0F, 0xC0]);
        jpeg.extend([0u8; 12]);
        let path = tmp.path().join("a.jpg");
        fs::write(&path, &jpeg).unwrap();
        let Content::Picture(picture) = load(&path) else { panic!("{:?}", load(&path)) };
        assert_eq!((picture.format, picture.row("Size")), ("JPEG", Some("4032 × 3024 pixels, 12.2 megapixels")));
    }

    #[test]
    fn folders_list_their_contents() {
        let tmp = TempDir::new("preview-dir");
        tmp.file("b.txt", "12345");
        tmp.file("A.txt", "1");
        tmp.dir("sub");
        let Content::Folder { folders, files, bytes, names, partial } = load(tmp.path()) else { panic!() };
        assert_eq!((folders, files, bytes, partial), (1, 2, 6, false));
        let names: Vec<&str> = names.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["sub", "A.txt", "b.txt"]);
    }

    #[test]
    fn the_previewer_shows_the_latest_request() {
        let tmp = TempDir::new("previewer");
        let a = tmp.file("a.txt", "aaa");
        let b = tmp.file("b.txt", "bbb");
        let key = |path: &Path| Key { path: path.to_path_buf(), size: 3, modified: None, archive: None };
        let mut previewer = Previewer::new();
        previewer.show(Some(key(&a)));
        previewer.show(Some(key(&b)));
        assert!(previewer.loading());
        let start = std::time::Instant::now();
        while previewer.loading() && start.elapsed().as_secs() < 5 {
            previewer.tick();
            thread::sleep(std::time::Duration::from_millis(5));
        }
        let preview = previewer.current.as_ref().unwrap();
        assert_eq!(preview.key.path, b);
        assert!(matches!(&preview.content, Content::Text { lines, .. } if lines == &["bbb".to_string()]));
    }
}
