//! Reading what a HEIF file says about its primary image, without decoding any pixels: the
//! EXIF block, the ICC colour profile, and the rotation and mirror to apply (DESIGN.md section
//! 7.6). The file is untrusted, so every read is bounds-checked and every size is capped.
//!
//! The file is a tree of boxes. Only the path to what we need is followed:
//! `meta` -> `pitm` (the primary item), `iinf` (item types), `iloc` (where items are),
//! `iref` (which item describes which), `iprp` -> `ipco` (properties) and `ipma` (which
//! properties belong to which item).
#![forbid(unsafe_code)]

use std::io::{Read, Seek, SeekFrom};

/// Most bytes of `meta` read into memory. Real files have a few hundred KB at most.
const MAX_META: u64 = 64 * 1024 * 1024;
/// Most bytes of EXIF read. A JPEG's EXIF segment can't hold more than 64 KB anyway.
const MAX_EXIF: u64 = 1024 * 1024;
/// Most bytes of ICC profile kept. Real profiles are a few KB to a few hundred KB.
const MAX_ICC: usize = 16 * 1024 * 1024;
/// Most top-level boxes `find_meta` steps over before giving up.
const MAX_TOP_BOXES: u32 = 10_000;
/// Most extents one `iloc` item may have.
const MAX_EXTENTS: usize = 1024;
/// Most items or properties looked at in one list.
const MAX_ENTRIES: u32 = 1_000_000;

/// One step of the display transform, in the order the file lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    /// `irot`: this many quarter turns anticlockwise (1 to 3).
    Rotate(u8),
    /// `imir`: flip left to right (`Horizontal`) or top to bottom (`Vertical`).
    Mirror(Flip),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flip {
    Horizontal,
    Vertical,
}

/// What the container says about the primary image.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Properties {
    /// The EXIF block, starting at its TIFF header (`II*\0` or `MM\0*`), if there is one.
    pub exif: Option<Vec<u8>>,
    /// The ICC profile (`colr` of type `prof` or `rICC`), if there is one.
    pub icc: Option<Vec<u8>>,
    /// The rotation and mirror steps for the primary image, in order.
    pub transforms: Vec<Transform>,
    /// The stored size before the transforms (`ispe`), if given.
    pub stored_size: Option<(u32, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Not a HEIF file, or damaged.
    Damaged(String),
    /// A HEIF file this app does not handle (no still image, e.g. an image sequence).
    Unsupported(String),
}

fn damaged<T>(why: &str) -> Result<T, ParseError> {
    Err(ParseError::Damaged(why.to_string()))
}

/// A bounds-checked cursor over a byte slice.
struct Cur<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Cur<'a> {
    fn new(b: &'a [u8]) -> Self {
        Cur { b, at: 0 }
    }
    fn left(&self) -> usize {
        self.b.len() - self.at
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.left() {
            return None;
        }
        let s = &self.b[self.at..self.at + n];
        self.at += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|s| u16::from_be_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8).map(|s| {
            let mut a = [0u8; 8];
            a.copy_from_slice(s);
            u64::from_be_bytes(a)
        })
    }
    /// An unsigned big-endian number of 0, 4 or 8 bytes (`iloc`'s sizes).
    fn sized(&mut self, n: u8) -> Option<u64> {
        match n {
            0 => Some(0),
            4 => self.u32().map(u64::from),
            8 => self.u64(),
            _ => None,
        }
    }
    fn fourcc(&mut self) -> Option<[u8; 4]> {
        self.take(4).map(|s| [s[0], s[1], s[2], s[3]])
    }
    fn rest(&mut self) -> &'a [u8] {
        let s = &self.b[self.at..];
        self.at = self.b.len();
        s
    }
    /// Version and flags of a "full" box.
    fn full(&mut self) -> Option<(u8, u32)> {
        let v = self.u32()?;
        Some(((v >> 24) as u8, v & 0x00FF_FFFF))
    }
    /// The next box: its type and payload. `None` at the end or when the box is damaged.
    fn next_box(&mut self) -> Option<([u8; 4], &'a [u8])> {
        let size = self.u32()?;
        let kind = self.fourcc()?;
        let (header, size) = match size {
            0 => (8, self.left() as u64 + 8),
            1 => (16, self.u64()?),
            n => (8, u64::from(n)),
        };
        let body = size.checked_sub(header)?;
        if body > self.left() as u64 {
            return None;
        }
        Some((kind, self.take(body as usize)?))
    }
}

/// Where a part of an item lives in the file.
#[derive(Debug, Clone, Copy)]
struct Extent {
    offset: u64,
    length: u64,
}

/// The `iloc` entry for one item (file-offset storage only).
fn item_extents(iloc: &[u8], want: u32) -> Option<Vec<Extent>> {
    let mut c = Cur::new(iloc);
    let (version, _) = c.full()?;
    if version > 2 {
        return None;
    }
    let sizes = c.u8()?;
    let (offset_size, length_size) = (sizes >> 4, sizes & 15);
    let sizes = c.u8()?;
    let base_size = sizes >> 4;
    let index_size = if version == 0 { 0 } else { sizes & 15 };
    // An extent that takes no bytes to describe would let a tiny box claim 65535 per item.
    let per_extent = usize::from(offset_size) + usize::from(length_size) + usize::from(index_size);
    if per_extent == 0 {
        return None;
    }
    let count = if version < 2 {
        u32::from(c.u16()?)
    } else {
        c.u32()?
    };
    for _ in 0..count.min(MAX_ENTRIES) {
        let id = if version < 2 {
            u32::from(c.u16()?)
        } else {
            c.u32()?
        };
        let method = if version == 0 { 0 } else { c.u16()? & 15 };
        c.u16()?; // data reference index
        let base = c.sized(base_size)?;
        let n = usize::from(c.u16()?);
        if n > MAX_EXTENTS || n.checked_mul(per_extent)? > c.left() {
            return None;
        }
        let mut out = Vec::new();
        for _ in 0..n {
            if index_size > 0 {
                c.sized(index_size)?;
            }
            let offset = c.sized(offset_size)?;
            let length = c.sized(length_size)?;
            out.push(Extent {
                offset: base.checked_add(offset)?,
                length,
            });
        }
        if id == want {
            // Data kept inside `idat` or in another item is not read.
            return (method == 0).then_some(out);
        }
    }
    None
}

#[derive(Debug, Clone)]
struct Item {
    id: u32,
    kind: [u8; 4],
}

fn items(iinf: &[u8]) -> Option<Vec<Item>> {
    let mut c = Cur::new(iinf);
    let (version, _) = c.full()?;
    let count = if version == 0 {
        u32::from(c.u16()?)
    } else {
        c.u32()?
    };
    let mut out = Vec::new();
    for _ in 0..count.min(MAX_ENTRIES) {
        let Some((kind, body)) = c.next_box() else {
            break;
        };
        if &kind != b"infe" {
            continue;
        }
        let mut b = Cur::new(body);
        let Some((v, _)) = b.full() else { continue };
        let id = match v {
            2 => b.u16().map(u32::from),
            3 => b.u32(),
            _ => None,
        };
        let (Some(id), Some(_), Some(kind)) = (id, b.u16(), b.fourcc()) else {
            continue;
        };
        out.push(Item { id, kind });
    }
    Some(out)
}

/// `(from, to)` pairs of the `cdsc` ("content describes") references.
fn describes(iref: &[u8]) -> Vec<(u32, u32)> {
    let mut c = Cur::new(iref);
    let Some((version, _)) = c.full() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    while let Some((kind, body)) = c.next_box() {
        if &kind != b"cdsc" {
            continue;
        }
        let mut b = Cur::new(body);
        let id = |b: &mut Cur| {
            if version == 0 {
                b.u16().map(u32::from)
            } else {
                b.u32()
            }
        };
        let (Some(from), Some(n)) = (id(&mut b), b.u16()) else {
            continue;
        };
        for _ in 0..n {
            let Some(to) = id(&mut b) else { break };
            out.push((from, to));
        }
    }
    out
}

/// A property as the file stores it, enough to know what we want from it.
#[derive(Debug, Clone)]
enum Prop {
    Icc(Vec<u8>),
    Transform(Transform),
    Size(u32, u32),
    Other,
}

fn property(kind: [u8; 4], body: &[u8]) -> Prop {
    let mut c = Cur::new(body);
    match &kind {
        b"colr" => match c.fourcc() {
            Some(t) if &t == b"prof" || &t == b"rICC" => Prop::Icc(c.rest().to_vec()),
            _ => Prop::Other,
        },
        b"irot" => match c.u8() {
            Some(a) if a & 3 != 0 => Prop::Transform(Transform::Rotate(a & 3)),
            _ => Prop::Other,
        },
        b"imir" => match c.u8() {
            // As libheif and WIC read it: axis 0 swaps top and bottom, axis 1 left and right.
            Some(a) => Prop::Transform(Transform::Mirror(if a & 1 == 0 {
                Flip::Vertical
            } else {
                Flip::Horizontal
            })),
            None => Prop::Other,
        },
        b"ispe" => {
            if c.full().is_some()
                && let (Some(w), Some(h)) = (c.u32(), c.u32())
            {
                return Prop::Size(w, h);
            }
            Prop::Other
        }
        _ => Prop::Other,
    }
}

/// The 1-based property numbers the `ipma` box gives to `item`, in order.
fn associations(ipma: &[u8], item: u32) -> Vec<usize> {
    let mut c = Cur::new(ipma);
    let Some((version, flags)) = c.full() else {
        return Vec::new();
    };
    let Some(count) = c.u32() else {
        return Vec::new();
    };
    for _ in 0..count.min(MAX_ENTRIES) {
        let id = if version < 1 {
            c.u16().map(u32::from)
        } else {
            c.u32()
        };
        let (Some(id), Some(n)) = (id, c.u8()) else {
            break;
        };
        let mut out = Vec::new();
        for _ in 0..n {
            let index = if flags & 1 == 1 {
                c.u16().map(|v| usize::from(v & 0x7FFF))
            } else {
                c.u8().map(|v| usize::from(v & 0x7F))
            };
            let Some(index) = index else { break };
            out.push(index);
        }
        if id == item {
            return out;
        }
    }
    Vec::new()
}

/// The parts of `meta` that matter, with where the EXIF item's bytes are.
struct MetaInfo {
    props: Properties,
    exif_at: Option<Vec<Extent>>,
}

fn read_meta(meta: &[u8]) -> Result<MetaInfo, ParseError> {
    let mut c = Cur::new(meta);
    if c.full().is_none() {
        return damaged("the meta box is cut short");
    }
    let (mut pitm, mut iinf, mut iloc, mut iref, mut iprp) = (None, None, None, None, None);
    while let Some((kind, body)) = c.next_box() {
        match &kind {
            b"pitm" => pitm = Some(body),
            b"iinf" => iinf = Some(body),
            b"iloc" => iloc = Some(body),
            b"iref" => iref = Some(body),
            b"iprp" => iprp = Some(body),
            _ => {}
        }
    }
    let Some(pitm) = pitm else {
        return Err(ParseError::Unsupported(
            "the file has no single main image".into(),
        ));
    };
    let mut p = Cur::new(pitm);
    let primary = match p.full() {
        Some((0, _)) => p.u16().map(u32::from),
        Some(_) => p.u32(),
        None => None,
    };
    let Some(primary) = primary else {
        return damaged("the main-image box is cut short");
    };

    let mut props = Properties::default();
    if let Some(iprp) = iprp {
        let (mut ipco, mut ipma) = (None, Vec::new());
        let mut c = Cur::new(iprp);
        while let Some((kind, body)) = c.next_box() {
            match &kind {
                b"ipco" => ipco = Some(body),
                b"ipma" => ipma.push(body),
                _ => {}
            }
        }
        let mut list = Vec::new();
        if let Some(ipco) = ipco {
            let mut c = Cur::new(ipco);
            while let Some((kind, body)) = c.next_box() {
                if list.len() >= MAX_ENTRIES as usize {
                    break;
                }
                list.push(property(kind, body));
            }
        }
        for ipma in ipma {
            for index in associations(ipma, primary) {
                match index.checked_sub(1).and_then(|i| list.get(i)) {
                    Some(Prop::Icc(p)) if props.icc.is_none() && p.len() <= MAX_ICC => {
                        props.icc = Some(p.clone())
                    }
                    Some(Prop::Transform(t)) => props.transforms.push(*t),
                    Some(Prop::Size(w, h)) if props.stored_size.is_none() => {
                        props.stored_size = Some((*w, *h));
                    }
                    _ => {}
                }
            }
        }
    }

    let mut exif_at = None;
    if let (Some(iinf), Some(iloc)) = (iinf, iloc) {
        let all = items(iinf).unwrap_or_default();
        let described = iref.map(describes).unwrap_or_default();
        let exif: Vec<&Item> = all.iter().filter(|i| &i.kind == b"Exif").collect();
        let chosen = exif
            .iter()
            .find(|i| described.contains(&(i.id, primary)))
            .or_else(|| exif.first());
        if let Some(item) = chosen {
            exif_at = item_extents(iloc, item.id);
        }
    }
    Ok(MetaInfo { props, exif_at })
}

/// Reads the box headers from the start of the file to find `meta`; returns its payload.
fn find_meta<R: Read + Seek>(r: &mut R, file_len: u64) -> Result<Vec<u8>, ParseError> {
    let io = |e: std::io::Error| ParseError::Damaged(format!("could not read the file: {e}"));
    let mut at = 0u64;
    let mut first = true;
    let mut seen = 0u32;
    while at + 8 <= file_len {
        seen += 1;
        if seen > MAX_TOP_BOXES {
            return damaged("the file has far too many boxes");
        }
        r.seek(SeekFrom::Start(at)).map_err(io)?;
        let mut head = [0u8; 16];
        let n = (file_len - at).min(16) as usize;
        r.read_exact(&mut head[..n]).map_err(io)?;
        let mut c = Cur::new(&head[..n]);
        let (Some(size), Some(kind)) = (c.u32(), c.fourcc()) else {
            break;
        };
        let (header, size) = match size {
            0 => (8, file_len - at),
            1 => match c.u64() {
                Some(s) => (16, s),
                None => break,
            },
            s => (8, u64::from(s)),
        };
        if size < header || size > file_len - at {
            return damaged("a box in the file runs past its end");
        }
        if first {
            if &kind != b"ftyp" {
                return damaged("it does not start like a HEIF photo");
            }
            first = false;
        }
        if &kind == b"meta" {
            let body = size - header;
            if body > MAX_META {
                return damaged("the file's description is far too large");
            }
            r.seek(SeekFrom::Start(at + header)).map_err(io)?;
            let mut buf = vec![0u8; body as usize];
            r.read_exact(&mut buf).map_err(io)?;
            return Ok(buf);
        }
        at += size;
    }
    if first {
        return damaged("it does not start like a HEIF photo");
    }
    Err(ParseError::Unsupported(
        "the file has no still image (an image sequence or video)".into(),
    ))
}

/// The TIFF data inside an EXIF item: the item starts with the offset of the TIFF header.
fn exif_tiff(item: &[u8]) -> Option<Vec<u8>> {
    let mut c = Cur::new(item);
    let skip = c.u32()? as usize;
    c.take(skip)?;
    let tiff = c.rest();
    let ok = tiff.len() >= 8 && (tiff.starts_with(b"II*\0") || tiff.starts_with(b"MM\0*"));
    ok.then(|| tiff.to_vec())
}

/// Reads the EXIF item's bytes; `None` if they can't be had.
fn read_exif<R: Read + Seek>(r: &mut R, file_len: u64, extents: &[Extent]) -> Option<Vec<u8>> {
    let total = extents
        .iter()
        .try_fold(0u64, |a, e| a.checked_add(e.length))?;
    if total > MAX_EXIF {
        return None;
    }
    let mut item = Vec::with_capacity(total as usize);
    for e in extents {
        let end = e.offset.checked_add(e.length)?;
        if end > file_len {
            return None;
        }
        r.seek(SeekFrom::Start(e.offset)).ok()?;
        let at = item.len();
        item.resize(at + e.length as usize, 0);
        r.read_exact(&mut item[at..]).ok()?;
    }
    exif_tiff(&item)
}

/// Reads the properties of the primary image of the HEIF file `r` of `file_len` bytes. An EXIF
/// block that can't be read is left out; the photo itself is still fine.
pub fn read_properties<R: Read + Seek>(r: &mut R, file_len: u64) -> Result<Properties, ParseError> {
    let meta = find_meta(r, file_len)?;
    let MetaInfo { mut props, exif_at } = read_meta(&meta)?;
    if let Some(extents) = exif_at {
        props.exif = read_exif(r, file_len, &extents);
    }
    Ok(props)
}

/// The type a reference gets when it is retired ([`retire_dangling_references`]): one no HEIF
/// reader knows, so it is skipped.
const RETIRED_REF: &[u8; 4] = b"pnk?";

/// The boxes at `start..end` of `file`, as `(type, box start, payload start, end)`, stopping at
/// the first one that doesn't fit.
fn child_boxes(file: &[u8], start: usize, end: usize) -> Vec<([u8; 4], usize, usize, usize)> {
    let mut out = Vec::new();
    let mut at = start;
    while let Some(head_end) = at.checked_add(8).filter(|h| *h <= end) {
        let mut c = Cur::new(&file[at..head_end]);
        let (Some(size), Some(kind)) = (c.u32(), c.fourcc()) else {
            break;
        };
        let (header, size) = match size {
            0 => (8, end - at),
            1 => match file
                .get(head_end..head_end.saturating_add(8))
                .and_then(|b| Cur::new(b).u64())
                .and_then(|s| usize::try_from(s).ok())
            {
                Some(s) => (16, s),
                None => break,
            },
            n => (8, n as usize),
        };
        let Some(next) = at.checked_add(size).filter(|n| size >= header && *n <= end) else {
            break;
        };
        out.push((kind, at, at + header, next));
        if out.len() >= MAX_ENTRIES as usize {
            break;
        }
        at = next;
    }
    out
}

/// Makes a photo that `libheif` 1.17 refuses readable by it. Some iPhone photos have an `auxl`
/// reference in `iref` (for an aux image: a matte, a depth map) that also names an item that
/// `libheif` has no image for: one that `iinf` doesn't list, or the HDR tone-map item (`tmap`),
/// a type `libheif` 1.17 doesn't know. It fails the whole photo with "Non-existing item ID
/// referenced". Other readers (WIC) skip such a reference. This finds each reference that names
/// such an item and changes its type to one nobody knows ([`RETIRED_REF`]), so it is skipped.
/// Returns how many were retired.
///
/// Only those four bytes of a reference change. The file doesn't grow or shrink and no box
/// moves, so every offset in `iloc` (which are offsets into the file) stays right and no box
/// size needs fixing. The input is untrusted: every read is bounds-checked, and a file that
/// doesn't parse is left alone (the decoder then says what is wrong with it).
pub fn retire_dangling_references(file: &mut [u8]) -> usize {
    let top = child_boxes(file, 0, file.len());
    let Some(&(_, _, meta, meta_end)) = top.iter().find(|(k, _, _, _)| k == b"meta") else {
        return 0;
    };
    // `meta` is a full box: four bytes of version and flags, then its children.
    let children = child_boxes(file, meta.saturating_add(4), meta_end);
    let find = |kind: &[u8; 4]| children.iter().find(|(k, _, _, _)| k == kind).copied();
    let (Some((_, _, iinf, iinf_end)), Some((_, _, iref, iref_end))) =
        (find(b"iinf"), find(b"iref"))
    else {
        return 0;
    };
    let Some(all) = items(&file[iinf..iinf_end]) else {
        return 0;
    };
    if all.is_empty() {
        return 0;
    }
    // Items `libheif` 1.17 has no image for count as missing, too: the tone map `tmap`.
    let ids: std::collections::HashSet<u32> = all
        .iter()
        .filter(|i| &i.kind != b"tmap")
        .map(|i| i.id)
        .collect();
    let Some(version) = file.get(iref).copied().filter(|_| iref_end > iref + 4) else {
        return 0;
    };
    let wide = version != 0;
    let mut retired = Vec::new();
    for (kind, at, body, end) in child_boxes(file, iref + 4, iref_end) {
        if &kind == RETIRED_REF {
            continue;
        }
        let mut c = Cur::new(&file[body..end]);
        let read = |c: &mut Cur| {
            if wide {
                c.u32()
            } else {
                c.u16().map(u32::from)
            }
        };
        let Some(from) = read(&mut c) else { continue };
        let Some(n) = c.u16() else { continue };
        let mut missing = !ids.contains(&from);
        for _ in 0..n {
            match read(&mut c) {
                Some(to) => missing |= !ids.contains(&to),
                None => break,
            }
        }
        if missing {
            // The type follows the four bytes of size.
            retired.push(at + 4);
        }
    }
    for at in &retired {
        file[*at..*at + 4].copy_from_slice(RETIRED_REF);
    }
    retired.len()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Cursor;

    pub(crate) fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    pub(crate) fn full(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
        let mut b = ((u32::from(version) << 24) | flags).to_be_bytes().to_vec();
        b.extend_from_slice(body);
        boxed(kind, &b)
    }

    /// A tiny EXIF TIFF block, little-endian, with Orientation = `o`.
    pub(crate) fn tiff_le(o: u16) -> Vec<u8> {
        let mut t = b"II*\0".to_vec();
        t.extend_from_slice(&8u32.to_le_bytes());
        t.extend_from_slice(&1u16.to_le_bytes());
        t.extend_from_slice(&0x0112u16.to_le_bytes());
        t.extend_from_slice(&3u16.to_le_bytes());
        t.extend_from_slice(&1u32.to_le_bytes());
        t.extend_from_slice(&o.to_le_bytes());
        t.extend_from_slice(&[0, 0]);
        t.extend_from_slice(&0u32.to_le_bytes());
        t
    }

    /// A HEIF file with one primary item (id 1), an Exif item (id 2) and the given properties.
    pub(crate) fn heif(tiff: &[u8], props: &[Vec<u8>], assoc: &[u8]) -> Vec<u8> {
        let ftyp = boxed(b"ftyp", b"heic\0\0\0\0mif1heic");
        let pitm = full(b"pitm", 0, 0, &1u16.to_be_bytes());
        let infe = |id: u16, kind: &[u8; 4]| {
            let mut b = id.to_be_bytes().to_vec();
            b.extend_from_slice(&[0, 0]);
            b.extend_from_slice(kind);
            b.push(0);
            full(b"infe", 2, 0, &b)
        };
        let mut iinf_body = 2u16.to_be_bytes().to_vec();
        iinf_body.extend(infe(1, b"hvc1"));
        iinf_body.extend(infe(2, b"Exif"));
        let iinf = full(b"iinf", 0, 0, &iinf_body);
        let mut cdsc = 2u16.to_be_bytes().to_vec();
        cdsc.extend_from_slice(&1u16.to_be_bytes());
        cdsc.extend_from_slice(&1u16.to_be_bytes());
        let iref = full(b"iref", 0, 0, &boxed(b"cdsc", &cdsc));
        let ipco = boxed(b"ipco", &props.concat());
        let mut ipma_body = 1u32.to_be_bytes().to_vec();
        ipma_body.extend_from_slice(&1u16.to_be_bytes());
        ipma_body.push(assoc.len() as u8);
        ipma_body.extend_from_slice(assoc);
        let ipma = full(b"ipma", 0, 0, &ipma_body);
        let iprp = boxed(b"iprp", &[ipco, ipma].concat());
        // The Exif item's bytes go in an mdat after meta; the offset is fixed up below.
        let mut item = 0u32.to_be_bytes().to_vec();
        item.extend_from_slice(tiff);
        let iloc_for = |offset: u32| {
            let mut b = vec![0x44, 0x00];
            b.extend_from_slice(&1u16.to_be_bytes()); // one item: the Exif one
            b.extend_from_slice(&2u16.to_be_bytes());
            b.extend_from_slice(&0u16.to_be_bytes());
            b.extend_from_slice(&1u16.to_be_bytes());
            b.extend_from_slice(&offset.to_be_bytes());
            b.extend_from_slice(&(item.len() as u32).to_be_bytes());
            full(b"iloc", 0, 0, &b)
        };
        let meta_with = |offset: u32| {
            let mut m = 0u32.to_be_bytes().to_vec();
            for part in [&pitm, &iinf, &iloc_for(offset), &iref, &iprp] {
                m.extend_from_slice(part);
            }
            boxed(b"meta", &m)
        };
        let meta_len = meta_with(0).len();
        let offset = (ftyp.len() + meta_len + 8) as u32;
        let mut file = ftyp;
        file.extend(meta_with(offset));
        file.extend(boxed(b"mdat", &item));
        file
    }

    /// An `iloc` body (version 2) of `items` items, each with `extents` extents, all sizes zero.
    fn hostile_iloc(items: u32, extents: u16, sizes: [u8; 2]) -> Vec<u8> {
        let mut b = vec![2, 0, 0, 0, sizes[0], sizes[1]];
        b.extend_from_slice(&items.to_be_bytes());
        for id in 1..=items {
            b.extend_from_slice(&id.to_be_bytes());
            b.extend_from_slice(&0u16.to_be_bytes()); // construction method
            b.extend_from_slice(&0u16.to_be_bytes()); // data reference
            b.extend_from_slice(&extents.to_be_bytes());
        }
        b
    }

    #[test]
    fn an_iloc_of_zero_size_extents_is_refused_quickly() {
        let started = std::time::Instant::now();
        let iloc = hostile_iloc(100_000, 65535, [0, 0]);
        assert!(item_extents(&iloc, 1).is_none());
        assert!(item_extents(&iloc, 99_999).is_none());
        assert!(started.elapsed().as_secs() < 5);
    }

    #[test]
    fn too_many_extents_for_the_bytes_left_are_refused() {
        // Real sizes, but 60000 extents claimed with nothing behind them.
        let iloc = hostile_iloc(1, 60000, [0x44, 0x00]);
        assert!(item_extents(&iloc, 1).is_none());
        let iloc = hostile_iloc(1, 2000, [0x44, 0x00]);
        assert!(item_extents(&iloc, 1).is_none());
    }

    fn parse(file: &[u8]) -> Result<Properties, ParseError> {
        read_properties(&mut Cursor::new(file), file.len() as u64)
    }

    #[test]
    fn finds_exif_icc_and_transforms_of_the_primary_item() {
        let ispe = full(b"ispe", 0, 0, &[0, 0, 0, 40, 0, 0, 0, 30]);
        let colr = boxed(b"colr", b"profABCDEF");
        let irot = boxed(b"irot", &[3]);
        let imir = boxed(b"imir", &[1]);
        let file = heif(&tiff_le(6), &[ispe, colr, irot, imir], &[1, 2, 3, 4]);
        let p = parse(&file).unwrap();
        assert_eq!(p.exif.unwrap(), tiff_le(6));
        assert_eq!(p.icc.unwrap(), b"ABCDEF");
        assert_eq!(p.stored_size, Some((40, 30)));
        assert_eq!(
            p.transforms,
            vec![Transform::Rotate(3), Transform::Mirror(Flip::Horizontal)]
        );
    }

    #[test]
    fn a_file_without_properties_has_none() {
        let file = heif(&tiff_le(1), &[], &[]);
        let p = parse(&file).unwrap();
        assert!(p.icc.is_none() && p.transforms.is_empty() && p.stored_size.is_none());
        assert!(p.exif.is_some());
    }

    #[test]
    fn nclx_colour_info_is_not_a_profile() {
        let colr = boxed(b"colr", b"nclx\0\x0c\0\x0c\0\x0c\0");
        let file = heif(&tiff_le(1), &[colr], &[1]);
        assert!(parse(&file).unwrap().icc.is_none());
    }

    #[test]
    fn exif_that_is_not_tiff_is_left_out() {
        let file = heif(b"not tiff at all!", &[], &[]);
        assert!(parse(&file).unwrap().exif.is_none());
    }

    #[test]
    fn not_heif_and_cut_off_files_are_errors_never_panics() {
        assert!(matches!(
            parse(b"just some text, not a photo"),
            Err(ParseError::Damaged(_))
        ));
        assert!(parse(b"").is_err());
        let good = heif(&tiff_le(1), &[boxed(b"irot", &[1])], &[1]);
        for cut in 0..good.len() {
            let _ = parse(&good[..cut]);
        }
        // Every single-byte change must be survived too.
        for i in 0..good.len() {
            for v in [0u8, 0xFF, 0x7F] {
                let mut bad = good.clone();
                bad[i] = v;
                let _ = parse(&bad);
            }
        }
    }

    #[test]
    fn a_box_larger_than_the_file_is_damaged() {
        let mut f = boxed(b"ftyp", b"heic\0\0\0\0");
        f.extend_from_slice(&0x7FFF_FFFFu32.to_be_bytes());
        f.extend_from_slice(b"meta");
        assert!(matches!(parse(&f), Err(ParseError::Damaged(_))));
    }

    #[test]
    fn a_file_with_no_still_image_is_unsupported() {
        let f = boxed(b"ftyp", b"msf1\0\0\0\0msf1");
        assert!(matches!(parse(&f), Err(ParseError::Unsupported(_))));
        let mut meta = 0u32.to_be_bytes().to_vec();
        meta.extend(full(b"hdlr", 0, 0, &[0; 20]));
        let mut f = boxed(b"ftyp", b"msf1\0\0\0\0msf1");
        f.extend(boxed(b"meta", &meta));
        assert!(matches!(parse(&f), Err(ParseError::Unsupported(_))));
    }

    const DANGLING: &[u8] = include_bytes!("fixtures/dangling.heic");
    const PLAIN: &[u8] = include_bytes!("fixtures/plain.heic");
    const WITH_EXIF: &[u8] = include_bytes!("fixtures/rotated.heic");

    /// A reference to an item that isn't in the file is retired by changing its type, and by
    /// nothing else: not one box moves, so the offsets in `iloc` stay right. Every reference to
    /// an item that does exist is left as it is.
    #[test]
    fn a_reference_to_a_missing_item_is_retired_and_nothing_else_changes() {
        let mut f = DANGLING.to_vec();
        assert!(f.windows(4).any(|w| w == b"auxl"));
        assert_eq!(retire_dangling_references(&mut f), 1);
        assert_eq!(f.len(), DANGLING.len());
        let changed: Vec<usize> = (0..f.len()).filter(|i| f[*i] != DANGLING[*i]).collect();
        assert_eq!(
            changed.len(),
            4,
            "only the four bytes of the type: {changed:?}"
        );
        assert!(!f.windows(4).any(|w| w == b"auxl"));
        assert!(
            f.windows(4).any(|w| w == b"cdsc"),
            "the good reference stays"
        );
        // Nothing more to do the second time, and what we read from the file is the same.
        assert_eq!(retire_dangling_references(&mut f), 0);
        let before = read_properties(&mut Cursor::new(DANGLING), DANGLING.len() as u64).unwrap();
        let after = read_properties(&mut Cursor::new(&f), f.len() as u64).unwrap();
        assert_eq!(before, after);
        assert!(after.exif.is_some());
    }

    /// What counts as missing: an item that isn't listed, or the tone-map item `libheif` 1.17
    /// has no image for. A reference to an ordinary image item is left alone.
    #[test]
    fn only_references_to_missing_or_tone_map_items_are_retired() {
        // The reference is `auxl 3 -> (1, 4)`, and item 4 is a `tmap`.
        let at = |f: &[u8], needle: &[u8]| f.windows(needle.len()).position(|w| w == needle);
        let infe4 = at(DANGLING, b"tmap").unwrap();
        // Item 4 is an ordinary image: nothing is missing.
        let mut f = DANGLING.to_vec();
        f[infe4..infe4 + 4].copy_from_slice(b"hvc1");
        assert_eq!(retire_dangling_references(&mut f), 0);
        // Item 4 is a tone map but nothing refers to it: nothing to retire either.
        let mut f = DANGLING.to_vec();
        let auxl = at(&f, b"auxl").unwrap();
        // `auxl` is the type, at the start of: type(4) from(2) count(2) to1(2) to2(2)
        f[auxl + 10..auxl + 12].copy_from_slice(&1u16.to_be_bytes());
        assert_eq!(retire_dangling_references(&mut f), 0);
        // Item 9 is in no list at all: retired.
        let mut f = DANGLING.to_vec();
        let auxl = at(&f, b"auxl").unwrap();
        f[auxl + 10..auxl + 12].copy_from_slice(&9u16.to_be_bytes());
        assert_eq!(retire_dangling_references(&mut f), 1);
    }

    #[test]
    fn a_file_with_no_missing_references_is_left_alone() {
        for good in [PLAIN, WITH_EXIF] {
            let mut f = good.to_vec();
            assert_eq!(retire_dangling_references(&mut f), 0);
            assert_eq!(f, good);
        }
        let mut text = b"not a photo at all".to_vec();
        assert_eq!(retire_dangling_references(&mut text), 0);
        assert_eq!(retire_dangling_references(&mut []), 0);
    }

    /// The file is untrusted: cut off or changed anywhere, cleaning it never panics, and never
    /// changes its length.
    #[test]
    fn cleaning_survives_any_cut_or_change() {
        for cut in 0..DANGLING.len() {
            let mut f = DANGLING[..cut].to_vec();
            let _ = retire_dangling_references(&mut f);
            assert_eq!(f.len(), cut);
        }
        for i in 0..DANGLING.len() {
            for v in [0u8, 0xFF, 0x7F, 1] {
                let mut f = DANGLING.to_vec();
                f[i] = v;
                let _ = retire_dangling_references(&mut f);
                assert_eq!(f.len(), DANGLING.len());
            }
        }
        // A huge count of references, in a box that ends early.
        let mut f = DANGLING.to_vec();
        let at = f.windows(4).position(|w| w == b"auxl").unwrap();
        f[at + 4 + 2] = 0xFF;
        f[at + 4 + 3] = 0xFF;
        let _ = retire_dangling_references(&mut f);
    }
}
