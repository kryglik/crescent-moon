//! What a picture is, for F3 on one and for the quick view: its format, size and colours, and
//! what the camera wrote into it (EXIF: when, with what, where). Only the parts that say so are
//! read, never the pixels, so a big photo costs a few small reads; and nothing a file claims
//! about itself makes cm read or keep more than a few megabytes of it.

use std::io::{self, BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::Path;

/// Name endings cm takes for pictures: F3 tells about them instead of showing their bytes,
/// and F4 doesn't open them in the editor. SVG is text, so it isn't one.
const ENDINGS: &[&str] = &[
    "jpg", "jpeg", "jpe", "jfif", "png", "apng", "gif", "bmp", "dib", "webp", "tif", "tiff", "heic", "heif", "hif", "avif",
    "ico", "cur", "dng", "cr2", "nef", "nrw", "arw",
];

/// The most of one part of a file (an EXIF block, a colour profile, a text) that is read.
const PART_LIMIT: usize = 1 << 20;
/// How much of a GIF is read to count its frames.
const GIF_LIMIT: u64 = 64 << 20;
/// The longest text kept from a file.
const TEXT_LIMIT: usize = 2000;

/// Whether `path`'s name says it is a picture.
pub fn by_name(path: &Path) -> bool {
    ending(path).is_some_and(|e| ENDINGS.contains(&e.as_str()))
}

fn ending(path: &Path) -> Option<String> {
    path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Picture {
    pub format: &'static str,
    /// What there is to tell, in the order shown: ("Size", "4032 × 3024 pixels, 12.2 megapixels").
    /// The first is always "Format".
    pub rows: Vec<(String, String)>,
}

impl Picture {
    #[cfg(test)]
    pub fn row(&self, label: &str) -> Option<&str> {
        self.rows.iter().find(|(l, _)| l == label).map(|(_, v)| v.as_str())
    }
}

/// The picture at `path`; None when it doesn't start like one cm knows.
pub fn read_path(path: &Path) -> io::Result<Option<Picture>> {
    let mut file = std::fs::File::open(path)?;
    read(&mut file, ending(path).as_deref())
}

/// The picture `file` holds; None when it doesn't start like one cm knows. `ending` (its
/// name's, in lower case) tells apart what says little about itself: icons, and the raw
/// photos of some cameras, which are TIFF files inside.
pub fn read<R: Read + Seek>(file: &mut R, ending: Option<&str>) -> io::Result<Option<Picture>> {
    let mut head = [0u8; 16];
    file.seek(SeekFrom::Start(0))?;
    let n = read_up_to(file, &mut head)?;
    let head = &head[..n];
    let mut facts = Facts::default();
    let format = if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        png(file, &mut facts);
        "PNG"
    } else if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        jpeg(file, &mut facts);
        "JPEG"
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        gif(file, &mut facts);
        "GIF"
    } else if n >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        webp(file, &mut facts);
        "WebP"
    } else if n >= 2 && head.starts_with(b"BM") && bmp(file, &mut facts).is_some() {
        "BMP"
    } else if head.starts_with(b"II*\0") || head.starts_with(b"MM\0*") {
        tiff_file(file, &mut facts, ending)
    } else if n >= 12 && &head[4..8] == b"ftyp" {
        let Some(format) = heif_brand(file) else { return Ok(None) };
        heif(file, &mut facts);
        format
    } else if n >= 6 && head[..2] == [0, 0] && matches!((ending, head[2], head[3]), (Some("ico"), 1, 0) | (Some("cur"), 2, 0)) {
        if icon(file, &mut facts).is_none() {
            return Ok(None);
        }
        if head[2] == 1 { "ICO" } else { "CUR" }
    } else {
        return Ok(None);
    };
    Ok(Some(facts.picture(format)))
}

/// What was found, before it is put in words.
#[derive(Default)]
struct Facts {
    /// What comes after the format's name: "progressive", "colour with transparency".
    kind: Vec<String>,
    size: Option<(u32, u32)>,
    /// How it is turned when shown, as EXIF says it (1: not at all); for HEIF its own say,
    /// which wins over EXIF's.
    turned: Option<u32>,
    frames: Option<Frames>,
    /// An icon's sizes.
    sizes: Vec<u32>,
    colours: Option<String>,
    /// Dots per inch, across and down.
    resolution: Option<(f64, f64)>,
    tags: Tags,
    /// A PNG's texts, a GIF's comment: (label, text).
    texts: Vec<(String, String)>,
    saved: Option<String>,
}

struct Frames {
    count: u32,
    millis: u64,
    /// How many times it plays: 0 over and over, None once.
    plays: Option<u32>,
    /// Whether every frame was counted (a file cut short, or a very big GIF, has more).
    all: bool,
}

impl Facts {
    fn picture(mut self, format: &'static str) -> Picture {
        let mut rows: Vec<(String, String)> = Vec::new();
        let mut add = |label: &str, value: String| {
            if !value.trim().is_empty() {
                rows.push((label.to_string(), value));
            }
        };
        let t = &mut self.tags;
        let largest = t.images.iter().max_by_key(|i| u64::from(i.width) * u64::from(i.height)).map(|i| (i.width, i.height));
        let size = self.size.or(t.pixels).or(largest).filter(|&(w, h)| w > 0 && h > 0);
        let mut kind = vec![format.to_string()];
        kind.append(&mut self.kind);
        add("Format", kind.join(", "));
        if let Some((w, h)) = size {
            add("Size", size_text(w, h));
        }
        if let Some(turned) = self.turned.or(t.orientation) {
            add("Shown", turned_text(turned, size));
        }
        if let Some(frames) = &self.frames
            && frames.count > 1
        {
            add("Frames", frames_text(frames));
        }
        if t.pages > 1 {
            add("Pages", t.pages.to_string());
        }
        if self.sizes.len() > 1 {
            let sizes: Vec<String> = self.sizes.iter().map(u32::to_string).collect();
            add("Sizes", format!("{} pixels", sizes.join(", ")));
        }
        let colours = self.colours.take().or_else(|| t.icc.as_deref().and_then(icc_name)).or_else(|| (t.colour_space == Some(1)).then(|| "sRGB".into()));
        add("Colours", colours.unwrap_or_default());
        if let Some(taken) = t.taken.as_deref().and_then(date_text) {
            let offset = t.offset.as_deref().filter(|o| is_offset(o)).map_or(String::new(), |o| format!(" {o}"));
            add("Taken", format!("{taken}{offset}"));
        }
        add("Camera", camera(t.make.as_deref(), t.model.as_deref()).unwrap_or_default());
        add("Lens", t.lens.take().unwrap_or_default());
        add("Exposure", exposure(t));
        if let Some(focal) = t.focal.filter(|&f| f > 0.0) {
            let full_frame = t.focal35.filter(|&f| f > 0 && f64::from(f) != focal.round());
            add("Focal length", format!("{} mm{}", short(focal, 1), full_frame.map_or(String::new(), |f| format!(" ({f} mm in 35 mm terms)"))));
        }
        if t.flash.is_some_and(|f| f & 1 == 1) {
            add("Flash", "fired".into());
        }
        if let Some((lat, lon)) = t.gps {
            let altitude = t.altitude.map_or(String::new(), |a| {
                format!(", {} m {} sea level", short(a.abs(), 0), if a < 0.0 { "below" } else { "above" })
            });
            let (ns, ew) = (if lat < 0.0 { 'S' } else { 'N' }, if lon < 0.0 { 'W' } else { 'E' });
            add("GPS", format!("{:.6}° {ns}, {:.6}° {ew}{altitude}", lat.abs(), lon.abs()));
        }
        for (label, text) in [
            ("Title", t.title.take()),
            ("Subject", t.subject.take()),
            ("Description", t.description.take()),
            ("Comment", t.comment.take()),
            ("Tags", t.keywords.take()),
        ] {
            add(label, text.unwrap_or_default());
        }
        if let Some(stars) = t.rating.filter(|r| (1..=5).contains(r)) {
            add("Rating", format!("{}{}", "★".repeat(stars as usize), "☆".repeat(5 - stars as usize)));
        }
        add("Artist", t.artist.take().unwrap_or_default());
        add("Copyright", t.copyright.take().unwrap_or_default());
        add("Software", t.software.take().unwrap_or_default());
        let saved = t.saved.as_deref().and_then(date_text).or(self.saved.take());
        if let Some(saved) = saved.filter(|s| t.taken.as_deref().and_then(date_text).as_ref() != Some(s)) {
            add("Saved", saved);
        }
        if let Some((x, y)) = self.resolution.or(t.resolution).filter(|&(x, y)| x >= 1.0 && y >= 1.0) {
            let (x, y) = (short(x, 0), short(y, 0));
            add("Resolution", if x == y { format!("{x} dpi") } else { format!("{x} × {y} dpi") });
        }
        for (label, text) in self.texts {
            add(&label, text);
        }
        Picture { format, rows }
    }
}

fn size_text(w: u32, h: u32) -> String {
    let megapixels = f64::from(w) * f64::from(h) / 1_000_000.0;
    if megapixels >= 0.1 { format!("{w} × {h} pixels, {megapixels:.1} megapixels") } else { format!("{w} × {h} pixels") }
}

/// EXIF's orientation: how the picture is turned to be shown upright.
fn turned_text(turned: u32, size: Option<(u32, u32)>) -> String {
    let (how, quarter) = match turned {
        2 => ("mirrored", false),
        3 => ("turned 180°", false),
        4 => ("turned 180° and mirrored", false),
        5 => ("turned 90° anticlockwise and mirrored", true),
        6 => ("turned 90° clockwise", true),
        7 => ("turned 90° clockwise and mirrored", true),
        8 => ("turned 90° anticlockwise", true),
        _ => return String::new(),
    };
    match size {
        Some((w, h)) if quarter => format!("{how}, so {h} × {w}"),
        _ => how.into(),
    }
}

fn frames_text(frames: &Frames) -> String {
    let mut text = format!("{}{} frames", if frames.all { "" } else { "at least " }, frames.count);
    if frames.millis > 0 && frames.all {
        let seconds = frames.millis as f64 / 1000.0;
        text += &if seconds < 10.0 { format!(", {seconds:.1} s") } else { format!(", {seconds:.0} s") };
    }
    text += &match frames.plays {
        Some(0) => ", over and over".to_string(),
        None | Some(1) => ", once".to_string(),
        Some(n) => format!(", {n} times"),
    };
    text
}

/// "Canon EOS R6", not "Canon Canon EOS R6"; "NIKON D750", not "NIKON CORPORATION NIKON D750".
fn camera(make: Option<&str>, model: Option<&str>) -> Option<String> {
    match (make, model) {
        (Some(make), Some(model)) => {
            let brand = make.split_whitespace().next().unwrap_or(make).to_lowercase();
            Some(if model.to_lowercase().starts_with(&brand) { model.to_string() } else { format!("{make} {model}") })
        }
        (Some(one), None) | (None, Some(one)) => Some(one.to_string()),
        (None, None) => None,
    }
}

/// "1/125 s · f/1.8 · ISO 400 · -0.7 EV"
fn exposure(t: &Tags) -> String {
    let mut parts = Vec::new();
    if let Some(time) = t.exposure.filter(|&e| e > 0.0) {
        let per = 1.0 / time;
        parts.push(if time < 1.0 && (per - per.round()).abs() < per * 0.03 {
            format!("1/{} s", per.round())
        } else {
            format!("{} s", short(time, 1))
        });
    }
    if let Some(f) = t.f_number.filter(|&f| f > 0.0) {
        parts.push(format!("f/{}", short(f, 1)));
    }
    if let Some(iso) = t.iso.filter(|&i| i > 0) {
        parts.push(format!("ISO {iso}"));
    }
    if let Some(bias) = t.bias.filter(|b| b.abs() >= 0.05) {
        parts.push(format!("{}{} EV", if bias > 0.0 { "+" } else { "" }, short(bias, 1)));
    }
    parts.join(" · ")
}

/// `x` with at most `decimals` decimals, and none that are 0: 1.8, 50, -0.7.
fn short(x: f64, decimals: usize) -> String {
    let text = format!("{x:.decimals$}");
    let text = if text.contains('.') { text.trim_end_matches('0').trim_end_matches('.').to_string() } else { text };
    if text == "-0" { "0".into() } else { text }
}

/// EXIF's "2025:07:14 18:32:05" as "2025-07-14 18:32:05".
fn date_text(s: &str) -> Option<String> {
    let b = s.as_bytes();
    if b.len() < 19 || !b[..19].is_ascii() || b[4] != b':' || b[7] != b':' || b[10] != b' ' || s.starts_with("0000") {
        return None;
    }
    Some(format!("{}-{}-{} {}", &s[..4], &s[5..7], &s[8..10], &s[11..19]))
}

fn is_offset(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 6 && matches!(b[0], b'+' | b'-') && b[3] == b':' && [1, 2, 4, 5].iter().all(|&i| b[i].is_ascii_digit())
}

/// Text from a file made safe to show: control characters (an escape sequence would reach
/// the terminal) and line breaks become spaces, runs of spaces one; cut at `TEXT_LIMIT`.
fn clean(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut space = false;
    for c in s.chars().take(TEXT_LIMIT) {
        if c.is_control() || c.is_whitespace() || c == '\u{FEFF}' {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.push(c);
    }
    (!out.is_empty()).then_some(out)
}

fn latin1(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn le24(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], 0])
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn read_up_to<R: Read>(file: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// `len` bytes at `offset`, if the file has them all.
fn read_at<R: Read + Seek>(r: &mut R, offset: u64, len: usize) -> Option<Vec<u8>> {
    if len > 16 * PART_LIMIT {
        return None;
    }
    r.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = Vec::new();
    r.take(len as u64).read_to_end(&mut buf).ok()?;
    (buf.len() == len).then_some(buf)
}

/// Unpacks zlib data (a PNG's compressed text or colour profile), at most `PART_LIMIT` of it.
fn inflate(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data).take(PART_LIMIT as u64).read_to_end(&mut out).ok()?;
    Some(out)
}

// ---- JPEG ----

fn jpeg<R: Read + Seek>(r: &mut R, f: &mut Facts) {
    let mut icc: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut pos = 2u64;
    for _ in 0..1000 {
        let Some(marker) = read_at(r, pos, 2) else { break };
        if marker[0] != 0xFF {
            break;
        }
        let kind = marker[1];
        // Fill bytes, and the markers that have no length.
        if kind == 0xFF {
            pos += 1;
            continue;
        }
        if kind == 0x01 || (0xD0..=0xD8).contains(&kind) {
            pos += 2;
            continue;
        }
        // The pixels start (or end): nothing more about the picture comes.
        if kind == 0xD9 || kind == 0xDA {
            break;
        }
        let Some(length) = read_at(r, pos + 2, 2).map(|b| u64::from(be16(&b))) else { break };
        if length < 2 {
            break;
        }
        let (at, len) = (pos + 4, (length - 2) as usize);
        match kind {
            0xE0 if len >= 12 => {
                if let Some(d) = read_at(r, at, 12).filter(|d| d.starts_with(b"JFIF\0")) {
                    let (x, y) = (f64::from(be16(&d[8..10])), f64::from(be16(&d[10..12])));
                    f.resolution = match d[7] {
                        1 => Some((x, y)),
                        2 => Some((x * 2.54, y * 2.54)),
                        _ => None,
                    };
                }
            }
            0xE1 if len > 6 => {
                if read_at(r, at, 6).is_some_and(|d| d == b"Exif\0\0")
                    && let Some(block) = read_at(r, at + 6, len - 6)
                {
                    tiff(&mut Cursor::new(block), 0, &mut f.tags, false);
                }
            }
            0xE2 if len > 14 => {
                if let Some(d) = read_at(r, at, len).filter(|d| d.starts_with(b"ICC_PROFILE\0"))
                    && icc.len() < 64
                {
                    icc.push((d[12], d[14..].to_vec()));
                }
            }
            // Start of frame, all but DHT (C4), JPG (C8) and DAC (CC).
            0xC0..=0xCF if !matches!(kind, 0xC4 | 0xC8 | 0xCC) => {
                if let Some(d) = read_at(r, at, 6)
                    && f.size.is_none()
                {
                    f.size = Some((u32::from(be16(&d[3..5])), u32::from(be16(&d[1..3]))));
                    if matches!(kind, 0xC2 | 0xC6 | 0xCA | 0xCE) {
                        f.kind.push("progressive".into());
                    } else if matches!(kind, 0xC3 | 0xC7 | 0xCB | 0xCF) {
                        f.kind.push("lossless".into());
                    }
                    match d[5] {
                        1 => f.kind.push("greyscale".into()),
                        4 => f.kind.push("CMYK".into()),
                        _ => {}
                    }
                    if d[0] != 8 {
                        f.kind.push(format!("{} bits per channel", d[0]));
                    }
                }
            }
            _ => {}
        }
        pos = at + len as u64;
    }
    if !icc.is_empty() {
        icc.sort_by_key(|(seq, _)| *seq);
        let profile: Vec<u8> = icc.into_iter().flat_map(|(_, part)| part).collect();
        f.colours = icc_name(&profile);
    }
}

// ---- PNG ----

fn png<R: Read + Seek>(r: &mut R, f: &mut Facts) {
    let (mut colour_type, mut depth, mut interlaced, mut transparency, mut palette) = (None, 8, false, false, 0);
    let mut frames: Option<Frames> = None;
    let mut millis = 0u64;
    let mut pos = 8u64;
    for _ in 0..100_000 {
        let Some(head) = read_at(r, pos, 8) else { break };
        let len = be32(&head[..4]) as usize;
        let kind: [u8; 4] = [head[4], head[5], head[6], head[7]];
        let at = pos + 8;
        // Its data, when it is at least `least` and at most `most` bytes long.
        let mut data = |least: usize, most: usize| if (least..=most).contains(&len) { read_at(r, at, len) } else { None };
        match &kind {
            b"IHDR" => {
                if let Some(d) = data(13, 64) {
                    f.size = Some((be32(&d[..4]), be32(&d[4..8])));
                    (depth, colour_type, interlaced) = (d[8], Some(d[9]), d[12] == 1);
                }
            }
            b"PLTE" => palette = len / 3,
            b"tRNS" => transparency = true,
            b"acTL" => {
                if let Some(d) = data(8, 64) {
                    frames = Some(Frames { count: be32(&d[..4]), millis: 0, plays: Some(be32(&d[4..8])), all: true });
                }
            }
            b"fcTL" => {
                if let Some(d) = data(26, 64) {
                    let (num, den) = (u64::from(be16(&d[20..22])), u64::from(be16(&d[22..24])));
                    millis += num * 1000 / if den == 0 { 100 } else { den };
                }
            }
            b"pHYs" => {
                if let Some(d) = data(9, 64).filter(|d| d[8] == 1) {
                    f.resolution = Some((f64::from(be32(&d[..4])) * 0.0254, f64::from(be32(&d[4..8])) * 0.0254));
                }
            }
            b"sRGB" => {
                f.colours.get_or_insert_with(|| "sRGB".into());
            }
            b"iCCP" => {
                if let Some(d) = data(0, PART_LIMIT) {
                    let name_end = d.iter().position(|&c| c == 0).unwrap_or(d.len());
                    let profile = d.get(name_end + 2..).and_then(inflate);
                    f.colours = profile.as_deref().and_then(icc_name).or_else(|| clean(&latin1(&d[..name_end])));
                }
            }
            b"tEXt" | b"zTXt" | b"iTXt" => {
                if let Some(d) = data(0, PART_LIMIT)
                    && let Some((key, text)) = png_text(&kind, &d)
                {
                    f.texts.push((key, text));
                }
            }
            b"eXIf" => {
                if let Some(d) = data(0, PART_LIMIT) {
                    tiff(&mut Cursor::new(d), 0, &mut f.tags, false);
                }
            }
            b"tIME" => {
                if let Some(d) = data(7, 64) {
                    f.saved = Some(format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC", be16(&d[..2]), d[2], d[3], d[4], d[5], d[6]));
                }
            }
            b"IEND" => break,
            _ => {}
        }
        pos = at + len as u64 + 4;
    }
    let with = |what: &str| if transparency { format!("{what} with transparency") } else { what.to_string() };
    let channel_bits = if depth != 8 { Some(format!("{depth} bits per channel")) } else { None };
    match colour_type {
        Some(0) if depth == 1 && !transparency => f.kind.push("black and white".into()),
        Some(0) => f.kind.extend([Some(with("greyscale")), channel_bits].into_iter().flatten()),
        Some(2) => f.kind.extend([Some(with("colour")), channel_bits].into_iter().flatten()),
        Some(3) => f.kind.push(with(&format!("{} colours", if palette > 0 { palette } else { 1 << depth }))),
        Some(4) => f.kind.extend([Some("greyscale with transparency".to_string()), channel_bits].into_iter().flatten()),
        Some(6) => f.kind.extend([Some("colour with transparency".to_string()), channel_bits].into_iter().flatten()),
        _ => {}
    }
    if interlaced {
        f.kind.push("interlaced".into());
    }
    if let Some(mut frames) = frames.filter(|fr| fr.count > 1) {
        frames.millis = millis;
        f.kind.push("animated".into());
        f.frames = Some(frames);
    }
}

/// A PNG text chunk's keyword (in words a person would use) and text. XMP and the hex dumps
/// some programs keep their data in are left out.
fn png_text(kind: &[u8; 4], d: &[u8]) -> Option<(String, String)> {
    let key_end = d.iter().position(|&c| c == 0)?;
    let key = latin1(&d[..key_end]);
    if key.starts_with("XML:") || key.starts_with("Raw profile type") {
        return None;
    }
    let rest = &d[key_end + 1..];
    let text = match kind {
        b"tEXt" => latin1(rest),
        b"zTXt" => latin1(&inflate(rest.get(1..)?)?),
        _ => {
            // Compressed or not, its method, its language and its translated keyword; then UTF-8.
            let (compressed, rest) = (*rest.first()? == 1, rest.get(2..)?);
            let language_end = rest.iter().position(|&c| c == 0)?;
            let rest = &rest[language_end + 1..];
            let translated_end = rest.iter().position(|&c| c == 0)?;
            let text = &rest[translated_end + 1..];
            if compressed { String::from_utf8_lossy(&inflate(text)?).into_owned() } else { String::from_utf8_lossy(text).into_owned() }
        }
    };
    let label = match key.as_str() {
        "Creation Time" => "Created".to_string(),
        "Author" => "Artist".to_string(),
        other => clean(other)?,
    };
    Some((label, clean(&text)?))
}

// ---- GIF ----

fn gif<R: Read + Seek>(r: &mut R, f: &mut Facts) {
    if r.seek(SeekFrom::Start(0)).is_err() {
        return;
    }
    let mut r = BufReader::with_capacity(1 << 16, (&mut *r).take(GIF_LIMIT));
    let mut screen = [0u8; 13];
    if r.read_exact(&mut screen).is_err() {
        return;
    }
    f.size = Some((u32::from(le16(&screen[6..8])), u32::from(le16(&screen[8..10]))));
    let mut frames = Frames { count: 0, millis: 0, plays: None, all: false };
    let mut delay = 0u64;
    if screen[10] & 0x80 != 0 && skip(&mut r, 3 << ((screen[10] & 7) + 1)).is_none() {
        return;
    }
    let mut comment = None;
    loop {
        let Some(kind) = byte(&mut r) else { break };
        match kind {
            // An image: its descriptor, its own colours if it has them, then its data.
            0x2C => {
                let mut d = [0u8; 9];
                if r.read_exact(&mut d).is_err()
                    || (d[8] & 0x80 != 0 && skip(&mut r, 3 << ((d[8] & 7) + 1)).is_none())
                    || byte(&mut r).is_none()
                    || blocks(&mut r, 0).is_none()
                {
                    break;
                }
                frames.count += 1;
                // As browsers do, a delay of 0 or 1 hundredth is played as 10.
                frames.millis += if delay <= 1 { 100 } else { delay * 10 };
                delay = 0;
            }
            0x21 => {
                let Some(label) = byte(&mut r) else { break };
                let keep = match label {
                    0xFE => TEXT_LIMIT,
                    _ => 64,
                };
                let Some(d) = blocks(&mut r, keep) else { break };
                match label {
                    0xF9 if d.len() >= 3 => delay = u64::from(le16(&d[1..3])),
                    0xFF if d.starts_with(b"NETSCAPE2.0") && d.len() >= 14 && d[11] == 1 => {
                        // How many more times it plays after the first; 0, over and over.
                        let more = u32::from(le16(&d[12..14]));
                        frames.plays = Some(if more == 0 { 0 } else { more + 1 });
                    }
                    0xFE => comment = clean(&latin1(&d)),
                    _ => {}
                }
            }
            0x3B => {
                frames.all = true;
                break;
            }
            _ => break,
        }
    }
    if let Some(comment) = comment {
        f.texts.push(("Comment".into(), comment));
    }
    if frames.count > 1 {
        f.kind.push("animated".into());
        f.frames = Some(frames);
    }
}

fn byte<R: Read>(r: &mut R) -> Option<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b).ok()?;
    Some(b[0])
}

fn skip<R: Read>(r: &mut R, n: u64) -> Option<()> {
    (io::copy(&mut r.take(n), &mut io::sink()).ok()? == n).then_some(())
}

/// A GIF's data blocks, up to the empty one that ends them; the first `keep` bytes of them.
fn blocks<R: Read>(r: &mut R, keep: usize) -> Option<Vec<u8>> {
    let mut kept = Vec::new();
    let mut block = [0u8; 255];
    loop {
        let size = byte(r)? as usize;
        if size == 0 {
            return Some(kept);
        }
        r.read_exact(&mut block[..size]).ok()?;
        let room = keep.saturating_sub(kept.len()).min(size);
        kept.extend_from_slice(&block[..room]);
    }
}

// ---- WebP ----

fn webp<R: Read + Seek>(r: &mut R, f: &mut Facts) {
    let Some(riff) = read_at(r, 4, 4) else { return };
    let end = u64::from(le32(&riff)) + 8;
    let (mut lossless, mut alpha) = (None, false);
    let mut frames = Frames { count: 0, millis: 0, plays: None, all: true };
    let mut pos = 12u64;
    for _ in 0..100_000 {
        if pos + 8 > end {
            break;
        }
        let Some(head) = read_at(r, pos, 8) else {
            frames.all = false;
            break;
        };
        let len = le32(&head[4..8]) as usize;
        let at = pos + 8;
        let mut data = |least: usize, most: usize| if (least..=most).contains(&len) { read_at(r, at, len) } else { None };
        match &head[..4] {
            b"VP8X" => {
                if let Some(d) = data(10, 64) {
                    alpha |= d[0] & 0x10 != 0;
                    f.size = Some((le24(&d[4..7]) + 1, le24(&d[7..10]) + 1));
                }
            }
            b"VP8 " => {
                lossless.get_or_insert(false);
                if let Some(d) = read_at(r, at, 10).filter(|d| d[3..6] == [0x9D, 0x01, 0x2A]) {
                    f.size.get_or_insert((u32::from(le16(&d[6..8]) & 0x3FFF), u32::from(le16(&d[8..10]) & 0x3FFF)));
                }
            }
            b"VP8L" => {
                lossless.get_or_insert(true);
                if let Some(d) = read_at(r, at, 5).filter(|d| d[0] == 0x2F) {
                    let bits = le32(&d[1..5]);
                    f.size.get_or_insert(((bits & 0x3FFF) + 1, (bits >> 14 & 0x3FFF) + 1));
                    alpha |= bits >> 28 & 1 == 1;
                }
            }
            b"ALPH" => alpha = true,
            b"ANIM" => {
                if let Some(d) = data(6, 64) {
                    frames.plays = Some(u32::from(le16(&d[4..6])));
                }
            }
            b"ANMF" => {
                if let Some(d) = read_at(r, at, 20) {
                    frames.count += 1;
                    frames.millis += u64::from(le24(&d[12..15]));
                    match &d[16..20] {
                        b"VP8L" => lossless.get_or_insert(true),
                        _ => lossless.get_or_insert(false),
                    };
                }
            }
            b"ICCP" => f.colours = data(0, PART_LIMIT).as_deref().and_then(icc_name),
            b"EXIF" => {
                if let Some(d) = data(0, PART_LIMIT) {
                    let start = if d.starts_with(b"Exif\0\0") { 6 } else { 0 };
                    tiff(&mut Cursor::new(d), start, &mut f.tags, false);
                }
            }
            _ => {}
        }
        pos = at + len as u64 + (len as u64 & 1);
    }
    match lossless {
        Some(true) => f.kind.push("lossless".into()),
        Some(false) => f.kind.push("lossy".into()),
        None => {}
    }
    if alpha {
        f.kind.push("with transparency".into());
    }
    if frames.count > 1 {
        f.kind.push("animated".into());
        f.frames = Some(frames);
    }
}

// ---- BMP ----

fn bmp<R: Read + Seek>(r: &mut R, f: &mut Facts) -> Option<()> {
    let d = read_at(r, 0, 26)?;
    let header = le32(&d[14..18]);
    if header == 12 {
        f.size = Some((u32::from(le16(&d[18..20])), u32::from(le16(&d[20..22]))));
        f.kind.push(format!("{} bits per pixel", le16(&read_at(r, 24, 2)?)));
        return Some(());
    }
    if !(40..=124).contains(&header) {
        return None;
    }
    // A negative height: the rows go top to bottom.
    f.size = Some(((le32(&d[18..22]) as i32).unsigned_abs(), (le32(&d[22..26]) as i32).unsigned_abs()));
    let e = read_at(r, 28, 18)?;
    f.kind.push(format!("{} bits per pixel", le16(&e[..2])));
    let (x, y) = (le32(&e[10..14]) as i32, le32(&e[14..18]) as i32);
    if x > 0 && y > 0 {
        f.resolution = Some((f64::from(x) * 0.0254, f64::from(y) * 0.0254));
    }
    Some(())
}

// ---- ICO, CUR ----

fn icon<R: Read + Seek>(r: &mut R, f: &mut Facts) -> Option<()> {
    let count = le16(&read_at(r, 4, 2)?) as usize;
    if count == 0 || count > 256 {
        return None;
    }
    let entries = read_at(r, 6, count * 16)?;
    let mut largest = (0, 0);
    for e in entries.chunks_exact(16) {
        let (mut w, mut h) = (if e[0] == 0 { 256 } else { u32::from(e[0]) }, if e[1] == 0 { 256 } else { u32::from(e[1]) });
        // A big one is often a PNG, which knows its own size.
        if let Some(png) = read_at(r, u64::from(le32(&e[12..16])), 24).filter(|d| d.starts_with(b"\x89PNG")) {
            (w, h) = (be32(&png[16..20]), be32(&png[20..24]));
        }
        f.sizes.push(w);
        if u64::from(w) * u64::from(h) > u64::from(largest.0) * u64::from(largest.1) {
            largest = (w, h);
        }
    }
    f.sizes.sort_unstable();
    f.sizes.dedup();
    f.size = Some(largest);
    Some(())
}

// ---- HEIF: HEIC, AVIF ----

/// What a file that starts with an ISO "ftyp" box is: a picture in one of HEIF's formats, or
/// None (a video, a Canon raw photo, …).
fn heif_brand<R: Read + Seek>(r: &mut R) -> Option<&'static str> {
    let size = be32(&read_at(r, 0, 4)?) as usize;
    if !(16..=4096).contains(&size) {
        return None;
    }
    let d = read_at(r, 8, size - 8)?;
    let brands: Vec<&[u8]> = std::iter::once(&d[..4]).chain(d[8..].chunks_exact(4)).collect();
    let has = |names: &[&[u8]]| brands.iter().any(|b| names.contains(b));
    if has(&[b"avif", b"avis"]) {
        Some("AVIF")
    } else if has(&[b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx"]) {
        Some("HEIC")
    } else if has(&[b"mif1", b"msf1"]) {
        Some("HEIF")
    } else {
        None
    }
}

/// The boxes in `d`: (type, contents).
fn boxes(mut d: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    while d.len() >= 8 && out.len() < 10_000 {
        let size = u64::from(be32(&d[..4]));
        let kind = [d[4], d[5], d[6], d[7]];
        let (head, size) = match size {
            1 if d.len() >= 16 => (16, u64::from_be_bytes(d[8..16].try_into().unwrap())),
            0 => (8, d.len() as u64),
            size => (8, size),
        };
        if size < head || size > d.len() as u64 {
            break;
        }
        out.push((kind, &d[head as usize..size as usize]));
        d = &d[size as usize..];
    }
    out
}

fn find<'a>(boxes: &[([u8; 4], &'a [u8])], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes.iter().find(|(k, _)| k == kind).map(|(_, d)| *d)
}

/// A big-endian number of `size` bytes (0, 4 or 8) at `*at`, which moves past it.
fn number(d: &[u8], at: &mut usize, size: usize) -> Option<u64> {
    let bytes = d.get(*at..*at + size)?;
    *at += size;
    Some(bytes.iter().fold(0u64, |n, &b| n << 8 | u64::from(b)))
}

fn heif<R: Read + Seek>(r: &mut R, f: &mut Facts) -> Option<()> {
    // The top-level boxes, without reading them, up to "meta".
    let mut pos = 0u64;
    let mut meta = None;
    for _ in 0..1000 {
        let head = read_at(r, pos, 16).or_else(|| read_at(r, pos, 8))?;
        let (header, size) = match u64::from(be32(&head[..4])) {
            1 if head.len() == 16 => (16, u64::from_be_bytes(head[8..16].try_into().unwrap())),
            size => (8, size),
        };
        if size < header {
            return None;
        }
        if &head[4..8] == b"meta" {
            let len = usize::try_from(size - header).ok().filter(|&s| s <= 4 * PART_LIMIT)?;
            meta = Some(read_at(r, pos + header, len)?);
            break;
        }
        pos = pos.checked_add(size)?;
    }
    let meta = meta?;
    let children = boxes(meta.get(4..)?);
    // The primary picture, the items there are, and where each one's data is.
    let primary = find(&children, b"pitm")
        .and_then(|d| if d.first() == Some(&0) { d.get(4..6).map(|b| u32::from(be16(b))) } else { d.get(4..8).map(be32) });
    let mut exif_item = None;
    if let Some(iinf) = find(&children, b"iinf") {
        let start = if iinf.first() == Some(&0) { 6 } else { 8 };
        for (kind, infe) in boxes(iinf.get(start..).unwrap_or_default()) {
            let (id, item_type) = match infe.first() {
                Some(2) if kind == *b"infe" => (infe.get(4..6).map(|b| u32::from(be16(b))), infe.get(8..12)),
                Some(3) if kind == *b"infe" => (infe.get(4..8).map(be32), infe.get(10..14)),
                _ => continue,
            };
            if item_type == Some(b"Exif") {
                exif_item = id;
            }
        }
    }
    let mut properties = Vec::new();
    let mut of_primary = Vec::new();
    if let Some(iprp) = find(&children, b"iprp") {
        let iprp = boxes(iprp);
        properties = find(&iprp, b"ipco").map(boxes).unwrap_or_default();
        if let Some(ipma) = find(&iprp, b"ipma") {
            of_primary = associations(ipma, primary).unwrap_or_default();
        }
    }
    let mine: Vec<&([u8; 4], &[u8])> = of_primary.iter().filter_map(|&i| properties.get(i.checked_sub(1)?)).collect();
    let property = |kind: &[u8; 4]| mine.iter().find(|(k, _)| k == kind).map(|(_, d)| *d).or_else(|| find(&properties, kind));
    if let Some(ispe) = property(b"ispe").filter(|d| d.len() >= 12) {
        f.size = Some((be32(&ispe[4..8]), be32(&ispe[8..12])));
    }
    if let Some(pixi) = property(b"pixi").filter(|d| d.len() >= 6)
        && pixi[5] != 8
    {
        f.kind.push(format!("{} bits per channel", pixi[5]));
    }
    let alpha = properties.iter().any(|(k, d)| k == b"auxC" && String::from_utf8_lossy(d.get(4..).unwrap_or_default()).contains("alpha"));
    if alpha {
        f.kind.push("with transparency".into());
    }
    // HEIF says itself how it is turned, and its readers go by that, not by EXIF.
    f.turned = Some(match property(b"irot").and_then(|d| d.first()).map(|a| a & 3) {
        Some(1) => 8,
        Some(2) => 3,
        Some(3) => 6,
        _ => 1,
    });
    if let Some(colr) = property(b"colr").filter(|d| d.len() >= 8) {
        f.colours = match &colr[..4] {
            b"nclx" => {
                let primaries = match be16(&colr[4..6]) {
                    1 => Some("sRGB"),
                    9 => Some("BT.2020"),
                    12 => Some("Display P3"),
                    _ => None,
                };
                let hdr = matches!(be16(&colr[6..8]), 16 | 18);
                primaries.map(|p| if hdr { format!("{p}, HDR") } else { p.to_string() })
            }
            b"prof" | b"rICC" => icc_name(&colr[4..]),
            _ => None,
        };
    }
    if let (Some(id), Some(iloc)) = (exif_item, find(&children, b"iloc"))
        && let Some((offset, len)) = location(iloc, id)
        && let Some(d) = read_at(r, offset, len.min(PART_LIMIT as u64) as usize)
        && d.len() >= 4
    {
        // The data starts with how far after these 4 bytes its TIFF header is.
        let start = 4 + u64::from(be32(&d[..4]));
        tiff(&mut Cursor::new(d), start, &mut f.tags, false);
    }
    Some(())
}

/// The (1-based) properties `ipma` gives the item `id`.
fn associations(ipma: &[u8], id: Option<u32>) -> Option<Vec<usize>> {
    let (version, wide) = (*ipma.first()?, ipma.get(3)? & 1 == 1);
    let count = be32(ipma.get(4..8)?);
    let mut at = 8;
    for _ in 0..count.min(100_000) {
        let item = number(ipma, &mut at, if version < 1 { 2 } else { 4 })? as u32;
        let n = *ipma.get(at)? as usize;
        at += 1;
        let mut found = Vec::new();
        for _ in 0..n {
            found.push(if wide { (number(ipma, &mut at, 2)? & 0x7FFF) as usize } else { (number(ipma, &mut at, 1)? & 0x7F) as usize });
        }
        if Some(item) == id {
            return Some(found);
        }
    }
    None
}

/// Where item `id`'s data is in the file, as `iloc` says: (offset, length) of its first piece.
fn location(iloc: &[u8], id: u32) -> Option<(u64, u64)> {
    let version = *iloc.first()?;
    let sizes = *iloc.get(4)?;
    let (offset_size, length_size) = ((sizes >> 4) as usize, (sizes & 15) as usize);
    let more = *iloc.get(5)?;
    let (base_size, index_size) = ((more >> 4) as usize, if version >= 1 { (more & 15) as usize } else { 0 });
    let mut at = 6;
    let count = number(iloc, &mut at, if version < 2 { 2 } else { 4 })?;
    for _ in 0..count.min(100_000) {
        let item = number(iloc, &mut at, if version < 2 { 2 } else { 4 })?;
        let method = if version >= 1 { number(iloc, &mut at, 2)? & 15 } else { 0 };
        at += 2; // the data reference
        let base = number(iloc, &mut at, base_size)?;
        let extents = number(iloc, &mut at, 2)?;
        let mut first = None;
        for _ in 0..extents {
            number(iloc, &mut at, index_size)?;
            let offset = number(iloc, &mut at, offset_size)?;
            let length = number(iloc, &mut at, length_size)?;
            first.get_or_insert((base.checked_add(offset)?, length));
        }
        if item == u64::from(id) {
            // Only data in the file itself; not in the "idat" box.
            return first.filter(|_| method == 0);
        }
    }
    None
}

// ---- TIFF, and EXIF, which is TIFF inside other files ----

#[derive(Default)]
struct Tags {
    make: Option<String>,
    model: Option<String>,
    lens: Option<String>,
    software: Option<String>,
    artist: Option<String>,
    copyright: Option<String>,
    description: Option<String>,
    title: Option<String>,
    comment: Option<String>,
    subject: Option<String>,
    keywords: Option<String>,
    rating: Option<u32>,
    taken: Option<String>,
    offset: Option<String>,
    saved: Option<String>,
    exposure: Option<f64>,
    f_number: Option<f64>,
    iso: Option<u32>,
    bias: Option<f64>,
    focal: Option<f64>,
    focal35: Option<u32>,
    flash: Option<u32>,
    orientation: Option<u32>,
    resolution: Option<(f64, f64)>,
    colour_space: Option<u32>,
    gps: Option<(f64, f64)>,
    altitude: Option<f64>,
    /// The size EXIF says the picture is.
    pixels: Option<(u32, u32)>,
    // A TIFF file's own:
    images: Vec<Image>,
    pages: usize,
    dng: bool,
    icc: Option<Vec<u8>>,
}

struct Image {
    width: u32,
    height: u32,
    bits: Vec<u32>,
    compression: Option<u32>,
    photometric: Option<u32>,
}

#[derive(Clone, Copy)]
struct Entry {
    tag: u16,
    kind: u16,
    count: u32,
    value: [u8; 4],
}

struct Tiff<'a, R> {
    r: &'a mut R,
    base: u64,
    le: bool,
}

/// Reads the TIFF structure at `base`: an EXIF block in another file (`file` false: its second
/// directory is a thumbnail), or a TIFF file, each of whose directories is a page.
fn tiff<R: Read + Seek>(r: &mut R, base: u64, tags: &mut Tags, file: bool) {
    let Some(head) = read_at(r, base, 8) else { return };
    let le = match &head[..4] {
        b"II*\0" => true,
        b"MM\0*" => false,
        _ => return,
    };
    let mut t = Tiff { r, base, le };
    let mut next = t.u32(&head[4..8]);
    let mut seen = Vec::new();
    while next != 0 && !seen.contains(&next) && seen.len() < 64 {
        seen.push(next);
        let Some((entries, after)) = t.dir(next) else { break };
        if seen.len() == 1 {
            t.main(&entries, tags, file);
        }
        if !file {
            break;
        }
        if t.image(&entries, tags) {
            tags.pages += 1;
        }
        next = after;
    }
}

impl<R: Read + Seek> Tiff<'_, R> {
    fn u16(&self, b: &[u8]) -> u16 {
        if self.le { le16(b) } else { be16(b) }
    }

    fn u32(&self, b: &[u8]) -> u32 {
        if self.le { le32(b) } else { be32(b) }
    }

    /// The entries of the directory at `offset`, and where the next one is (0: none).
    fn dir(&mut self, offset: u32) -> Option<(Vec<Entry>, u32)> {
        let at = self.base + u64::from(offset);
        let count = read_at(self.r, at, 2)?;
        let count = self.u16(&count) as usize;
        if count == 0 || count > 1000 {
            return None;
        }
        let data = read_at(self.r, at + 2, count * 12)?;
        let entries = data
            .chunks_exact(12)
            .map(|e| Entry { tag: self.u16(&e[..2]), kind: self.u16(&e[2..4]), count: self.u32(&e[4..8]), value: [e[8], e[9], e[10], e[11]] })
            .collect();
        let next = read_at(self.r, at + 2 + count as u64 * 12, 4).map_or(0, |b| self.u32(&b));
        Some((entries, next))
    }

    fn bytes(&mut self, e: &Entry) -> Option<Vec<u8>> {
        let unit = match e.kind {
            1 | 2 | 6 | 7 => 1,
            3 | 8 => 2,
            4 | 9 | 11 | 13 => 4,
            5 | 10 | 12 => 8,
            _ => return None,
        };
        let len = (e.count as usize).checked_mul(unit)?;
        if len <= 4 {
            return Some(e.value[..len].to_vec());
        }
        if len > PART_LIMIT {
            return None;
        }
        let at = self.base + u64::from(self.u32(&e.value));
        read_at(self.r, at, len)
    }

    fn text(&mut self, e: &Entry) -> Option<String> {
        let b = self.bytes(e)?;
        let b = b.split(|&c| c == 0).next()?;
        clean(&String::from_utf8_lossy(b))
    }

    /// The Windows tags (title, comment, keywords…): UTF-16, little-endian whatever the file is.
    fn utf16(&mut self, e: &Entry) -> Option<String> {
        let b = self.bytes(e)?;
        let units: Vec<u16> = b.chunks_exact(2).map(le16).take_while(|&u| u != 0).collect();
        clean(&String::from_utf16_lossy(&units))
    }

    /// EXIF's user comment: 8 bytes saying how it is written, then the text.
    fn comment(&mut self, e: &Entry) -> Option<String> {
        let b = self.bytes(e)?;
        let (how, text) = (b.get(..8)?, &b[8..]);
        if how.starts_with(b"UNICODE") {
            let units: Vec<u16> = text.chunks_exact(2).map(|c| self.u16(c)).take_while(|&u| u != 0).collect();
            clean(&String::from_utf16_lossy(&units))
        } else if how.starts_with(b"ASCII") || how.iter().all(|&c| c == 0) {
            clean(&String::from_utf8_lossy(text.split(|&c| c == 0).next()?))
        } else {
            None
        }
    }

    fn numbers(&mut self, e: &Entry) -> Vec<f64> {
        let Some(b) = self.bytes(e) else { return Vec::new() };
        let numbers: Vec<f64> = match e.kind {
            1 | 7 => b.iter().map(|&x| f64::from(x)).collect(),
            6 => b.iter().map(|&x| f64::from(x as i8)).collect(),
            3 => b.chunks_exact(2).map(|c| f64::from(self.u16(c))).collect(),
            8 => b.chunks_exact(2).map(|c| f64::from(self.u16(c) as i16)).collect(),
            4 | 13 => b.chunks_exact(4).map(|c| f64::from(self.u32(c))).collect(),
            9 => b.chunks_exact(4).map(|c| f64::from(self.u32(c) as i32)).collect(),
            5 => b.chunks_exact(8).filter(|c| self.u32(&c[4..]) != 0).map(|c| f64::from(self.u32(c)) / f64::from(self.u32(&c[4..]))).collect(),
            10 => b
                .chunks_exact(8)
                .filter(|c| self.u32(&c[4..]) != 0)
                .map(|c| f64::from(self.u32(c) as i32) / f64::from(self.u32(&c[4..]) as i32))
                .collect(),
            11 => b.chunks_exact(4).map(|c| f64::from(f32::from_bits(self.u32(c)))).collect(),
            12 => b
                .chunks_exact(8)
                .map(|c| {
                    let (hi, lo) = if self.le { (self.u32(&c[4..]), self.u32(c)) } else { (self.u32(c), self.u32(&c[4..])) };
                    f64::from_bits(u64::from(hi) << 32 | u64::from(lo))
                })
                .collect(),
            _ => Vec::new(),
        };
        numbers.into_iter().filter(|n| n.is_finite()).collect()
    }

    fn number(&mut self, e: &Entry) -> Option<f64> {
        self.numbers(e).first().copied()
    }

    fn whole(&mut self, e: &Entry) -> Option<u32> {
        self.number(e).filter(|&n| n >= 0.0 && n <= f64::from(u32::MAX)).map(|n| n as u32)
    }

    /// The first directory: what the picture is and who made it, and where EXIF's and the
    /// GPS's own directories are.
    fn main(&mut self, entries: &[Entry], tags: &mut Tags, file: bool) {
        let mut unit = 2;
        let mut resolution = (None, None);
        for e in entries {
            match e.tag {
                0x010E => tags.description = self.text(e),
                0x010F => tags.make = self.text(e),
                0x0110 => tags.model = self.text(e),
                0x0112 => tags.orientation = self.whole(e),
                0x011A => resolution.0 = self.number(e),
                0x011B => resolution.1 = self.number(e),
                0x0128 => unit = self.whole(e).unwrap_or(2),
                0x0131 => tags.software = self.text(e),
                0x0132 => tags.saved = self.text(e),
                0x013B => tags.artist = self.text(e),
                0x4746 => tags.rating = self.whole(e),
                0x8298 => tags.copyright = self.text(e),
                0x8773 => tags.icc = self.bytes(e),
                0x8769 => {
                    if let Some(at) = self.whole(e) {
                        self.exif(at, tags);
                    }
                }
                0x8825 => {
                    if let Some(at) = self.whole(e) {
                        self.gps(at, tags);
                    }
                }
                0x9C9B => tags.title = self.utf16(e),
                0x9C9C => tags.comment = tags.comment.take().or_else(|| self.utf16(e)),
                0x9C9D => tags.artist = tags.artist.take().or_else(|| self.utf16(e)),
                0x9C9E => tags.keywords = self.utf16(e),
                0x9C9F => tags.subject = self.utf16(e),
                0xC612 => tags.dng = true,
                // A raw photo's full picture is often in a directory of its own.
                0x014A if file => {
                    for at in self.numbers(e).into_iter().take(8) {
                        if let Some((sub, _)) = self.dir(at as u32) {
                            self.image(&sub, tags);
                        }
                    }
                }
                _ => {}
            }
        }
        let inches = match unit {
            2 => Some(1.0),
            3 => Some(2.54),
            _ => None,
        };
        if let (Some(x), Some(y), Some(per)) = (resolution.0, resolution.1, inches) {
            tags.resolution = Some((x * per, y * per));
        }
    }

    /// A TIFF file's picture in this directory, if it has one; returns whether it is a page
    /// of its own (not a smaller copy of one).
    fn image(&mut self, entries: &[Entry], tags: &mut Tags) -> bool {
        let mut image = Image { width: 0, height: 0, bits: Vec::new(), compression: None, photometric: None };
        let mut smaller = false;
        for e in entries {
            match e.tag {
                0x00FE => smaller = self.whole(e).is_some_and(|kind| kind & 1 == 1),
                0x0100 => image.width = self.whole(e).unwrap_or(0),
                0x0101 => image.height = self.whole(e).unwrap_or(0),
                0x0102 => image.bits = self.numbers(e).into_iter().map(|b| b as u32).collect(),
                0x0103 => image.compression = self.whole(e),
                0x0106 => image.photometric = self.whole(e),
                _ => {}
            }
        }
        if image.width == 0 || image.height == 0 {
            return false;
        }
        tags.images.push(image);
        !smaller
    }

    fn exif(&mut self, at: u32, tags: &mut Tags) {
        let Some((entries, _)) = self.dir(at) else { return };
        let (mut width, mut height, mut digitized, mut iso_too) = (None, None, None, None);
        for e in &entries {
            match e.tag {
                0x829A => tags.exposure = self.number(e),
                0x829D => tags.f_number = self.number(e),
                0x8827 => tags.iso = self.whole(e),
                0x8832 | 0x8833 => iso_too = iso_too.or(self.whole(e)),
                0x9003 => tags.taken = self.text(e),
                0x9004 => digitized = self.text(e),
                0x9011 => tags.offset = self.text(e),
                0x9204 => tags.bias = self.number(e),
                0x9209 => tags.flash = self.whole(e),
                0x920A => tags.focal = self.number(e),
                0x9286 => tags.comment = self.comment(e),
                0xA001 => tags.colour_space = self.whole(e),
                0xA002 => width = self.whole(e),
                0xA003 => height = self.whole(e),
                0xA405 => tags.focal35 = self.whole(e),
                0xA434 => tags.lens = self.text(e),
                _ => {}
            }
        }
        // ISO 65535 means "more than that": the real one is in another tag.
        if tags.iso.is_none_or(|iso| iso == 65535) && iso_too.is_some() {
            tags.iso = iso_too;
        }
        tags.taken = tags.taken.take().or(digitized);
        tags.pixels = width.zip(height);
    }

    fn gps(&mut self, at: u32, tags: &mut Tags) {
        let Some((entries, _)) = self.dir(at) else { return };
        let (mut lat, mut lon, mut lat_ref, mut lon_ref, mut altitude, mut below) = (None, None, None, None, None, false);
        let degrees = |n: Vec<f64>| n.first().map(|d| d + n.get(1).unwrap_or(&0.0) / 60.0 + n.get(2).unwrap_or(&0.0) / 3600.0);
        for e in &entries {
            match e.tag {
                1 => lat_ref = self.text(e),
                2 => lat = degrees(self.numbers(e)),
                3 => lon_ref = self.text(e),
                4 => lon = degrees(self.numbers(e)),
                5 => below = self.whole(e) == Some(1),
                6 => altitude = self.number(e),
                _ => {}
            }
        }
        let sign = |r: Option<String>, negative: &str| if r.as_deref().is_some_and(|r| r.eq_ignore_ascii_case(negative)) { -1.0 } else { 1.0 };
        if let (Some(lat), Some(lon)) = (lat, lon)
            && (lat != 0.0 || lon != 0.0)
            && lat.abs() <= 90.0
            && lon.abs() <= 180.0
        {
            tags.gps = Some((lat * sign(lat_ref, "S"), lon * sign(lon_ref, "W")));
            tags.altitude = altitude.map(|a| if below { -a } else { a });
        }
    }
}

/// A TIFF file: a picture of its own, or a camera's raw photo.
fn tiff_file<R: Read + Seek>(r: &mut R, f: &mut Facts, ending: Option<&str>) -> &'static str {
    tiff(r, 0, &mut f.tags, true);
    let t = &f.tags;
    let format = if t.dng {
        "DNG"
    } else {
        match ending {
            Some("cr2") => "CR2",
            Some("nef" | "nrw") => "NEF",
            Some("arw") => "ARW",
            _ => "TIFF",
        }
    };
    let Some(image) = t.images.iter().max_by_key(|i| u64::from(i.width) * u64::from(i.height)) else { return format };
    let bits = image.bits.first().copied().filter(|&b| image.bits.iter().all(|&o| o == b));
    match image.photometric {
        Some(32803 | 34892) => f.kind.push("raw sensor data".into()),
        Some(0 | 1) if bits == Some(1) => f.kind.push("black and white".into()),
        Some(0 | 1) => f.kind.push("greyscale".into()),
        Some(2 | 6) => f.kind.push("colour".into()),
        Some(3) => f.kind.push("palette".into()),
        Some(5) => f.kind.push("CMYK".into()),
        _ => {}
    }
    if let Some(bits) = bits.filter(|&b| b != 8 && b != 1) {
        f.kind.push(format!("{bits} bits per channel"));
    }
    match image.compression {
        Some(5) => f.kind.push("LZW".into()),
        Some(7) => f.kind.push("JPEG-compressed".into()),
        Some(8 | 32946) => f.kind.push("Deflate".into()),
        Some(32773) => f.kind.push("PackBits".into()),
        Some(3 | 4) => f.kind.push("fax-compressed".into()),
        _ => {}
    }
    format
}

// ---- Colour profiles ----

/// The name an ICC colour profile gives itself ("Display P3", "sRGB IEC61966-2.1").
fn icc_name(p: &[u8]) -> Option<String> {
    if p.len() < 132 || &p[36..40] != b"acsp" {
        return None;
    }
    let count = be32(&p[128..132]) as usize;
    for i in 0..count.min(100) {
        let e = p.get(132 + i * 12..144 + i * 12)?;
        if &e[..4] != b"desc" {
            continue;
        }
        let (at, len) = (be32(&e[4..8]) as usize, be32(&e[8..12]) as usize);
        let tag = p.get(at..at.checked_add(len)?)?;
        return match tag.get(..4)? {
            b"desc" => {
                let n = be32(tag.get(8..12)?) as usize;
                let text = tag.get(12..12usize.checked_add(n)?)?;
                clean(&latin1(text.split(|&c| c == 0).next()?))
            }
            b"mluc" => {
                let record = tag.get(16..28)?;
                let (n, from) = (be32(&record[4..8]) as usize, be32(&record[8..12]) as usize);
                let text = tag.get(from..from.checked_add(n)?)?;
                let units: Vec<u16> = text.chunks_exact(2).map(be16).take_while(|&u| u != 0).collect();
                clean(&String::from_utf16_lossy(&units))
            }
            _ => None,
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(data: &[u8], ending: &str) -> Picture {
        read(&mut Cursor::new(data), Some(ending)).unwrap().unwrap_or_else(|| panic!("not a picture: {ending}"))
    }

    /// A TIFF value: (type, count, bytes in the order `le` says).
    enum V {
        Ascii(&'static str),
        Short(u16),
        Rationals(Vec<(u32, u32)>),
        Signed(i32, i32),
        Undefined(Vec<u8>),
        Utf16(&'static str),
    }

    fn encode(v: &V, le: bool) -> (u16, u32, Vec<u8>) {
        let u16b = |n: u16| if le { n.to_le_bytes().to_vec() } else { n.to_be_bytes().to_vec() };
        let u32b = |n: u32| if le { n.to_le_bytes().to_vec() } else { n.to_be_bytes().to_vec() };
        match v {
            V::Ascii(s) => (2, s.len() as u32 + 1, [s.as_bytes(), &[0]].concat()),
            V::Short(n) => (3, 1, u16b(*n)),
            V::Rationals(list) => (5, list.len() as u32, list.iter().flat_map(|&(n, d)| [u32b(n), u32b(d)].concat()).collect()),
            V::Signed(n, d) => (10, 1, [u32b(*n as u32), u32b(*d as u32)].concat()),
            V::Undefined(b) => (7, b.len() as u32, b.clone()),
            V::Utf16(s) => {
                let b: Vec<u8> = s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
                (1, b.len() as u32, b)
            }
        }
    }

    /// A TIFF structure: its first directory, and EXIF's and the GPS's when not empty.
    fn tiff_block(le: bool, ifd0: Vec<(u16, V)>, exif: Vec<(u16, V)>, gps: Vec<(u16, V)>) -> Vec<u8> {
        let u16b = |n: u16| if le { n.to_le_bytes() } else { n.to_be_bytes() };
        let u32b = |n: u32| if le { n.to_le_bytes() } else { n.to_be_bytes() };
        let dir_len = |entries: usize, data: usize| 2 + entries * 12 + 4 + data;
        let encoded = |dir: &[(u16, V)]| -> Vec<(u16, u16, u32, Vec<u8>)> {
            dir.iter()
                .map(|(tag, v)| {
                    let (kind, count, bytes) = encode(v, le);
                    (*tag, kind, count, bytes)
                })
                .collect()
        };
        let data_len = |dir: &[(u16, u16, u32, Vec<u8>)]| dir.iter().map(|e| if e.3.len() > 4 { e.3.len() } else { 0 }).sum::<usize>();
        let (mut ifd0, exif, gps) = (encoded(&ifd0), encoded(&exif), encoded(&gps));
        let extra = usize::from(!exif.is_empty()) + usize::from(!gps.is_empty());
        let ifd0_len = dir_len(ifd0.len() + extra, data_len(&ifd0));
        let exif_at = 8 + ifd0_len;
        let gps_at = exif_at + if exif.is_empty() { 0 } else { dir_len(exif.len(), data_len(&exif)) };
        if !exif.is_empty() {
            ifd0.push((0x8769, 4, 1, u32b(exif_at as u32).to_vec()));
        }
        if !gps.is_empty() {
            ifd0.push((0x8825, 4, 1, u32b(gps_at as u32).to_vec()));
        }
        let mut out = if le { b"II*\0".to_vec() } else { b"MM\0*".to_vec() };
        out.extend(u32b(8));
        for (dir, at) in [(&ifd0, 8), (&exif, exif_at), (&gps, gps_at)] {
            if dir.is_empty() {
                continue;
            }
            assert_eq!(out.len(), at);
            let mut data_at = at + 2 + dir.len() * 12 + 4;
            let mut data: Vec<u8> = Vec::new();
            out.extend(u16b(dir.len() as u16));
            for (tag, kind, count, bytes) in dir {
                out.extend(u16b(*tag));
                out.extend(u16b(*kind));
                out.extend(u32b(*count));
                if bytes.len() <= 4 {
                    let mut value = bytes.clone();
                    value.resize(4, 0);
                    out.extend(value);
                } else {
                    out.extend(u32b(data_at as u32));
                    data_at += bytes.len();
                    data.extend(bytes);
                }
            }
            out.extend([0; 4]);
            out.extend(data);
        }
        out
    }

    /// A small ICC profile that calls itself `name`.
    fn profile(name: &str) -> Vec<u8> {
        let mut p = vec![0u8; 128];
        p[36..40].copy_from_slice(b"acsp");
        p.extend(1u32.to_be_bytes());
        let desc_at = 132 + 12;
        let mut desc = b"desc\0\0\0\0".to_vec();
        desc.extend((name.len() as u32 + 1).to_be_bytes());
        desc.extend(name.as_bytes());
        desc.push(0);
        p.extend(b"desc");
        p.extend((desc_at as u32).to_be_bytes());
        p.extend((desc.len() as u32).to_be_bytes());
        p.extend(desc);
        p
    }

    fn segment(marker: u8, data: &[u8]) -> Vec<u8> {
        let mut out = vec![0xFF, marker];
        out.extend((data.len() as u16 + 2).to_be_bytes());
        out.extend(data);
        out
    }

    fn photo_exif(le: bool, orientation: u16) -> Vec<u8> {
        tiff_block(
            le,
            vec![
                (0x010F, V::Ascii("Canon")),
                (0x0110, V::Ascii("Canon EOS R6")),
                (0x0112, V::Short(orientation)),
                (0x0131, V::Ascii("Firmware 1.8.1")),
                (0x0132, V::Ascii("2025:07:15 09:00:00")),
                (0x9C9B, V::Utf16("Západ slunce")),
                (0x4746, V::Short(4)),
            ],
            vec![
                (0x829A, V::Rationals(vec![(1, 125)])),
                (0x829D, V::Rationals(vec![(18, 10)])),
                (0x8827, V::Short(400)),
                (0x9003, V::Ascii("2025:07:14 18:32:05")),
                (0x9011, V::Ascii("+02:00")),
                (0x9204, V::Signed(-2, 3)),
                (0x9209, V::Short(0x19)),
                (0x920A, V::Rationals(vec![(50, 1)])),
                (0xA434, V::Ascii("RF50mm F1.8 STM")),
                (0x9286, V::Undefined(b"ASCII\0\0\0Evening\x1b[31m walk\0".to_vec())),
            ],
            vec![
                (1, V::Ascii("N")),
                (2, V::Rationals(vec![(50, 1), (4, 1), (31944, 1000)])),
                (3, V::Ascii("E")),
                (4, V::Rationals(vec![(14, 1), (26, 1), (1608, 100)])),
                (5, V::Undefined(vec![0])),
                (6, V::Rationals(vec![(245, 1)])),
            ],
        )
    }

    fn photo_jpeg(le: bool) -> Vec<u8> {
        let mut jpeg = vec![0xFF, 0xD8];
        jpeg.extend(segment(0xE0, b"JFIF\0\x01\x02\x01\x01\x2C\x01\x2C\0\0"));
        jpeg.extend(segment(0xE1, &[b"Exif\0\0".as_slice(), &photo_exif(le, 6)].concat()));
        jpeg.extend(segment(0xE2, &[b"ICC_PROFILE\0\x01\x01".as_slice(), &profile("Display P3")].concat()));
        jpeg.extend(segment(0xDB, &[0; 65]));
        jpeg.extend(segment(0xC2, &[8, 0x0B, 0xD0, 0x0F, 0xC0, 3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]));
        jpeg.extend(segment(0xDA, &[3, 1, 0, 2, 0x11, 3, 0x11, 0, 0x3F, 0]));
        jpeg.extend([0x12, 0x34, 0xFF, 0xD9]);
        jpeg
    }

    #[test]
    fn a_photo_from_a_camera() {
        for le in [false, true] {
            let p = picture(&photo_jpeg(le), "jpg");
            let rows: Vec<(&str, &str)> = p.rows.iter().map(|(l, v)| (l.as_str(), v.as_str())).collect();
            assert_eq!(
                rows,
                [
                    ("Format", "JPEG, progressive"),
                    ("Size", "4032 × 3024 pixels, 12.2 megapixels"),
                    ("Shown", "turned 90° clockwise, so 3024 × 4032"),
                    ("Colours", "Display P3"),
                    ("Taken", "2025-07-14 18:32:05 +02:00"),
                    ("Camera", "Canon EOS R6"),
                    ("Lens", "RF50mm F1.8 STM"),
                    ("Exposure", "1/125 s · f/1.8 · ISO 400 · -0.7 EV"),
                    ("Focal length", "50 mm"),
                    ("Flash", "fired"),
                    ("GPS", "50.075540° N, 14.437800° E, 245 m above sea level"),
                    ("Title", "Západ slunce"),
                    ("Comment", "Evening [31m walk"),
                    ("Rating", "★★★★☆"),
                    ("Software", "Firmware 1.8.1"),
                    ("Saved", "2025-07-15 09:00:00"),
                    ("Resolution", "300 dpi"),
                ],
                "little-endian: {le}"
            );
        }
    }

    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = (data.len() as u32).to_be_bytes().to_vec();
        out.extend(kind);
        out.extend(data);
        out.extend([0; 4]);
        out
    }

    fn zlib(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn animated_png() -> Vec<u8> {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend(chunk(b"IHDR", &[0, 0, 2, 0x80, 0, 0, 1, 0xE0, 8, 6, 0, 0, 0]));
        png.extend(chunk(b"acTL", &[0, 0, 0, 3, 0, 0, 0, 0]));
        png.extend(chunk(b"iCCP", &[b"ICC profile\0\0".as_slice(), &zlib(&profile("sRGB IEC61966-2.1"))].concat()));
        png.extend(chunk(b"pHYs", &[0, 0, 0x2E, 0x23, 0, 0, 0x2E, 0x23, 1]));
        png.extend(chunk(b"tEXt", b"Software\0GIMP 2.10"));
        png.extend(chunk(b"zTXt", &[b"Comment\0\0".as_slice(), &zlib(b"made\nby hand")].concat()));
        png.extend(chunk(b"iTXt", "Title\0\0\0cs\0Název\0Západ".as_bytes()));
        png.extend(chunk(b"tEXt", b"XML:com.adobe.xmp\0<x:xmpmeta/>"));
        png.extend(chunk(b"tIME", &[0x07, 0xE9, 3, 4, 5, 6, 7]));
        for _ in 0..3 {
            let mut fctl = vec![0u8; 26];
            fctl[20..24].copy_from_slice(&[0, 1, 0, 10]);
            png.extend(chunk(b"fcTL", &fctl));
            png.extend(chunk(b"IDAT", &[0; 40]));
        }
        png.extend(chunk(b"IEND", &[]));
        png
    }

    #[test]
    fn a_png_with_texts_and_frames() {
        let p = picture(&animated_png(), "png");
        assert_eq!(p.row("Format"), Some("PNG, colour with transparency, animated"));
        assert_eq!(p.row("Size"), Some("640 × 480 pixels, 0.3 megapixels"));
        assert_eq!(p.row("Frames"), Some("3 frames, 0.3 s, over and over"));
        assert_eq!(p.row("Colours"), Some("sRGB IEC61966-2.1"));
        assert_eq!(p.row("Resolution"), Some("300 dpi"));
        assert_eq!(p.row("Software"), Some("GIMP 2.10"));
        assert_eq!(p.row("Comment"), Some("made by hand"));
        assert_eq!(p.row("Title"), Some("Západ"));
        assert_eq!(p.row("Saved"), Some("2025-03-04 05:06:07 UTC"));
        assert!(!p.rows.iter().any(|(l, _)| l.starts_with("XML")), "{:?}", p.rows);

        // A palette and a black-and-white one.
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend(chunk(b"IHDR", &[0, 0, 0, 16, 0, 0, 0, 16, 8, 3, 0, 0, 1]));
        png.extend(chunk(b"PLTE", &[0; 48]));
        png.extend(chunk(b"tRNS", &[0]));
        assert_eq!(picture(&png, "png").row("Format"), Some("PNG, 16 colours with transparency, interlaced"));
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend(chunk(b"IHDR", &[0, 0, 0, 16, 0, 0, 0, 16, 1, 0, 0, 0, 0]));
        assert_eq!(picture(&png, "png").row("Format"), Some("PNG, black and white"));
    }

    fn animated_gif(trailer: bool) -> Vec<u8> {
        let mut gif = b"GIF89a\x0a\x00\x0b\x00\x80\x00\x00".to_vec();
        gif.extend([0u8; 6]);
        gif.extend(b"\x21\xFF\x0bNETSCAPE2.0\x03\x01\x00\x00\x00");
        gif.extend(b"\x21\xFE\x05hello\x00");
        for _ in 0..3 {
            gif.extend(b"\x21\xF9\x04\x00\x0a\x00\x00\x00");
            gif.extend(b"\x2C\x00\x00\x00\x00\x0a\x00\x0b\x00\x00\x02\x02\x4c\x01\x00");
        }
        if trailer {
            gif.push(0x3B);
        } else {
            gif.truncate(gif.len() - 3);
        }
        gif
    }

    #[test]
    fn an_animated_gif() {
        let p = picture(&animated_gif(true), "gif");
        assert_eq!(p.row("Format"), Some("GIF, animated"));
        assert_eq!(p.row("Size"), Some("10 × 11 pixels"));
        assert_eq!(p.row("Frames"), Some("3 frames, 0.3 s, over and over"));
        assert_eq!(p.row("Comment"), Some("hello"));
        // Cut short: what is there.
        assert_eq!(picture(&animated_gif(false), "gif").row("Frames"), Some("at least 2 frames, over and over"));
    }

    fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = chunks.concat();
        let mut out = b"RIFF".to_vec();
        out.extend((body.len() as u32 + 4).to_le_bytes());
        out.extend(b"WEBP");
        out.extend(body);
        out
    }

    fn riff_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = kind.to_vec();
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(data);
        if data.len() % 2 == 1 {
            out.push(0);
        }
        out
    }

    #[test]
    fn webp_pictures() {
        let exif = tiff_block(true, vec![(0x0110, V::Ascii("Pixel 9"))], vec![], vec![]);
        let frame = |millis: u32| {
            let mut d = vec![0u8; 12];
            d.extend(&millis.to_le_bytes()[..3]);
            d.push(0);
            d.extend(riff_chunk(b"VP8L", &[0x2F, 0, 0, 0, 0]));
            riff_chunk(b"ANMF", &d)
        };
        let webp = riff(&[
            riff_chunk(b"VP8X", &[0x1A, 0, 0, 0, 0x1F, 0x03, 0, 0x57, 0x02, 0]),
            riff_chunk(b"ANIM", &[0, 0, 0, 0, 2, 0]),
            frame(100),
            frame(300),
            riff_chunk(b"EXIF", &exif),
        ]);
        let p = picture(&webp, "webp");
        assert_eq!(p.row("Format"), Some("WebP, lossless, with transparency, animated"));
        assert_eq!(p.row("Size"), Some("800 × 600 pixels, 0.5 megapixels"));
        assert_eq!(p.row("Frames"), Some("2 frames, 0.4 s, 2 times"));
        assert_eq!(p.row("Camera"), Some("Pixel 9"));

        // A plain lossy one.
        let mut vp8 = vec![0x30, 0x01, 0x00, 0x9D, 0x01, 0x2A];
        vp8.extend(320u16.to_le_bytes());
        vp8.extend(200u16.to_le_bytes());
        let p = picture(&riff(&[riff_chunk(b"VP8 ", &vp8)]), "webp");
        assert_eq!((p.row("Format"), p.row("Size")), (Some("WebP, lossy"), Some("320 × 200 pixels")));
    }

    #[test]
    fn bmp_and_icons() {
        let mut bmp = b"BM".to_vec();
        bmp.extend([0u8; 12]);
        bmp.extend(40u32.to_le_bytes());
        bmp.extend(1024i32.to_le_bytes());
        bmp.extend((-768i32).to_le_bytes());
        bmp.extend(1u16.to_le_bytes());
        bmp.extend(24u16.to_le_bytes());
        bmp.extend([0u8; 8]);
        bmp.extend(2835i32.to_le_bytes());
        bmp.extend(2835i32.to_le_bytes());
        bmp.extend([0u8; 8]);
        let p = picture(&bmp, "bmp");
        assert_eq!(p.row("Format"), Some("BMP, 24 bits per pixel"));
        assert_eq!(p.row("Size"), Some("1024 × 768 pixels, 0.8 megapixels"));
        assert_eq!(p.row("Resolution"), Some("72 dpi"));

        let mut ico = vec![0, 0, 1, 0, 3, 0];
        let png_at = 6 + 3 * 16;
        for (size, at) in [(16u8, 0u32), (32, 0), (0, png_at as u32)] {
            ico.extend([size, size, 0, 0, 1, 0, 32, 0]);
            ico.extend(100u32.to_le_bytes());
            ico.extend(at.to_le_bytes());
        }
        ico.extend(b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR");
        ico.extend(512u32.to_be_bytes());
        ico.extend(512u32.to_be_bytes());
        let p = picture(&ico, "ico");
        assert_eq!(p.row("Sizes"), Some("16, 32, 512 pixels"));
        assert_eq!(p.row("Size"), Some("512 × 512 pixels, 0.3 megapixels"));
        // The same bytes in a file not called an icon aren't taken for one.
        assert_eq!(read(&mut Cursor::new(&ico), Some("bin")).unwrap(), None);
    }

    #[test]
    fn tiff_files_and_their_pages() {
        let page = |next: u32| {
            let mut d = Vec::new();
            let entries: [(u16, u16, u32, u32); 6] =
                [(0x100, 3, 1, 1200), (0x101, 3, 1, 800), (0x102, 3, 1, 16), (0x103, 3, 1, 5), (0x106, 3, 1, 2), (0x10F, 2, 4, u32::from_le_bytes(*b"HP\0\0"))];
            d.extend((entries.len() as u16).to_le_bytes());
            for (tag, kind, count, value) in entries {
                d.extend(tag.to_le_bytes());
                d.extend(kind.to_le_bytes());
                d.extend(count.to_le_bytes());
                d.extend(value.to_le_bytes());
            }
            d.extend(next.to_le_bytes());
            d
        };
        let mut tiff = b"II*\0\x08\0\0\0".to_vec();
        let first = page(8 + 2 + 6 * 12 + 4);
        tiff.extend(&first);
        tiff.extend(page(0));
        let p = picture(&tiff, "tif");
        assert_eq!(p.row("Format"), Some("TIFF, colour, 16 bits per channel, LZW"));
        assert_eq!(p.row("Size"), Some("1200 × 800 pixels, 1.0 megapixels"));
        assert_eq!(p.row("Pages"), Some("2"));
        assert_eq!(p.row("Camera"), Some("HP"));
        // A directory that points back at itself is read once.
        let mut looped = b"II*\0\x08\0\0\0".to_vec();
        looped.extend(page(8));
        assert_eq!(picture(&looped, "tif").row("Pages"), None);
    }

    fn heif_box(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = (data.len() as u32 + 8).to_be_bytes().to_vec();
        out.extend(kind);
        out.extend(data);
        out
    }

    fn full(version: u8, flags: u32, data: &[u8]) -> Vec<u8> {
        let mut out = vec![version];
        out.extend(&flags.to_be_bytes()[1..]);
        out.extend(data);
        out
    }

    fn iphone_heic() -> Vec<u8> {
        let ftyp = heif_box(b"ftyp", b"heic\0\0\0\0mif1heic");
        let exif = [&[0u8, 0, 0, 6][..], b"Exif\0\0", &tiff_block(false, vec![(0x010F, V::Ascii("Apple")), (0x0110, V::Ascii("iPhone 15 Pro")), (0x0112, V::Short(1))], vec![(0x829D, V::Rationals(vec![(178, 100)]))], vec![])].concat();
        let iinf = heif_box(
            b"iinf",
            &full(0, 0, &[&[0u8, 2][..], &heif_box(b"infe", &full(2, 0, b"\0\x01\0\0hvc1\0")), &heif_box(b"infe", &full(2, 0, b"\0\x02\0\0Exif\0"))].concat()),
        );
        let ipco = heif_box(
            b"ipco",
            &[
                heif_box(b"ispe", &full(0, 0, &[4032u32.to_be_bytes(), 3024u32.to_be_bytes()].concat())),
                heif_box(b"irot", &[3]),
                heif_box(b"pixi", &full(0, 0, &[3, 10, 10, 10])),
                heif_box(b"colr", b"nclx\0\x0c\0\x10\0\x01\x80"),
            ]
            .concat(),
        );
        let ipma = heif_box(b"ipma", &full(0, 0, &[0, 0, 0, 1, 0, 1, 4, 0x81, 2, 0x83, 4]));
        let iprp = heif_box(b"iprp", &[ipco, ipma].concat());
        let pitm = heif_box(b"pitm", &full(0, 0, &[0, 1]));
        let hdlr = heif_box(b"hdlr", &full(0, 0, b"\0\0\0\0pict\0\0\0\0\0\0\0\0\0\0\0\0\0"));
        // iloc, version 0: 4-byte offsets and lengths, no base offset.
        let iloc_len = 8 + full(0, 0, &[0x44, 0, 0, 1, 0, 2, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]).len();
        let meta_len = 8 + 4 + hdlr.len() + pitm.len() + iinf.len() + iloc_len + iprp.len();
        let exif_at = (ftyp.len() + meta_len + 8) as u32;
        let iloc = heif_box(b"iloc", &full(0, 0, &[&[0x44u8, 0, 0, 1, 0, 2, 0, 0, 0, 1][..], &exif_at.to_be_bytes(), &(exif.len() as u32).to_be_bytes()].concat()));
        let meta = heif_box(b"meta", &full(0, 0, &[hdlr, pitm, iinf, iloc, iprp].concat()));
        assert_eq!(meta.len(), meta_len);
        [ftyp, meta, heif_box(b"mdat", &exif)].concat()
    }

    #[test]
    fn a_heic_photo() {
        let p = picture(&iphone_heic(), "heic");
        assert_eq!(p.row("Format"), Some("HEIC, 10 bits per channel"));
        assert_eq!(p.row("Size"), Some("4032 × 3024 pixels, 12.2 megapixels"));
        // HEIF's own turn, though EXIF says it isn't turned.
        assert_eq!(p.row("Shown"), Some("turned 90° clockwise, so 3024 × 4032"));
        assert_eq!(p.row("Colours"), Some("Display P3, HDR"));
        assert_eq!(p.row("Camera"), Some("Apple iPhone 15 Pro"));
        assert_eq!(p.row("Exposure"), Some("f/1.8"));
        // A video starts the same way, and isn't a picture.
        assert_eq!(read(&mut Cursor::new(heif_box(b"ftyp", b"isom\0\0\0\0isomiso2mp41")), Some("heic")).unwrap(), None);
    }

    #[test]
    fn what_isnt_a_picture() {
        assert!(by_name(Path::new("a/B.JPG")) && by_name(Path::new("x.heic")) && by_name(Path::new("y.webp")));
        assert!(!by_name(Path::new("logo.svg")) && !by_name(Path::new("jpg")) && !by_name(Path::new("notes.txt")));
        assert_eq!(read(&mut Cursor::new(b"just some text"), Some("jpg")).unwrap(), None);
        assert_eq!(read(&mut Cursor::new(b""), Some("png")).unwrap(), None);
    }

    /// Every file cut short at every length, and with bytes changed here and there, is read
    /// without a panic (and soon: nothing it says makes cm read much).
    #[test]
    fn broken_files_dont_break_it() {
        let files = [
            (photo_jpeg(false), "jpg"),
            (photo_jpeg(true), "jpg"),
            (animated_png(), "png"),
            (animated_gif(true), "gif"),
            (iphone_heic(), "heic"),
            (riff(&[riff_chunk(b"VP8X", &[0x1A, 0, 0, 0, 0x1F, 0x03, 0, 0x57, 0x02, 0]), riff_chunk(b"EXIF", &photo_exif(true, 3))]), "webp"),
            ([b"II*\0\x08\0\0\0".as_slice(), &photo_exif(true, 1)[8..]].concat(), "tif"),
        ];
        let mut seed = 0x2545F491u32;
        for (data, ending) in &files {
            for len in 0..=data.len() {
                let _ = read(&mut Cursor::new(&data[..len]), Some(ending));
            }
            for _ in 0..2000 {
                let mut changed = data.clone();
                for _ in 0..3 {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    let at = seed as usize % changed.len();
                    changed[at] = (seed >> 8) as u8;
                }
                let _ = read(&mut Cursor::new(&changed), Some(ending));
            }
        }
    }

    #[test]
    fn numbers_in_words() {
        assert_eq!(short(1.8, 1), "1.8");
        assert_eq!(short(50.0, 1), "50");
        assert_eq!(short(-0.04, 1), "0");
        assert_eq!(camera(Some("NIKON CORPORATION"), Some("NIKON D750")), Some("NIKON D750".into()));
        assert_eq!(camera(Some("SONY"), Some("ILCE-7M3")), Some("SONY ILCE-7M3".into()));
        let mut t = Tags { exposure: Some(0.4), ..Tags::default() };
        assert_eq!(exposure(&t), "0.4 s");
        t.exposure = Some(1.0 / 3.0);
        assert_eq!(exposure(&t), "1/3 s");
        t.exposure = Some(30.0);
        assert_eq!(exposure(&t), "30 s");
        assert_eq!(date_text("0000:00:00 00:00:00"), None);
        assert_eq!(date_text("2025:01:02 03:04:05.123"), Some("2025-01-02 03:04:05".into()));
        assert_eq!(clean("  a\u{1b}[2J\n b  "), Some("a [2J b".into()));
    }
}
