//! Read, analyse and stream Apple sparsebundle disk images.
//!
//! A sparsebundle is a directory containing an `Info.plist` and a `bands/`
//! subdirectory full of numbered band files. The band index is
//! `offset / band_size`, so **band 0 contains the partition map**. Reading it
//! tells you whether the image is structurally sound without attaching it.
//!
//! Format details that matter, and that are easy to get wrong:
//!
//! - Band files are named `bands/%x` -- **lowercase hex, no zero padding**.
//! - **A missing band is normal.** The reference implementation memsets the
//!   buffer to zero on ENOENT; that is what "sparse" means. Absent bands are
//!   never evidence of damage.
//! - **`token` is an ordinary empty file** in every sparsebundle, not a lock.
//! - `Info.bckup` is a copy of `Info.plist`, and is a real recovery path when
//!   the primary is unreadable.

use anyhow::{anyhow, Context, Result};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// GPT partition type GUIDs, stored mixed-endian on disk (first three fields
/// little-endian), so these are the byte sequences as they actually appear.
const GUID_HFS: [u8; 16] = [
    0x00, 0x53, 0x46, 0x48, 0x00, 0x00, 0xaa, 0x11, 0xaa, 0x11, 0x00, 0x30, 0x65, 0x43, 0xec,
    0xac,
];
const GUID_APFS: [u8; 16] = [
    0xef, 0x57, 0x34, 0x7c, 0x00, 0x00, 0xaa, 0x11, 0xaa, 0x11, 0x00, 0x30, 0x65, 0x43, 0xec,
    0xac,
];
const GUID_EFI: [u8; 16] = [
    0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9,
    0x3b,
];

const SECTOR: u64 = 512;

/// Bytes of the following band appended to each streaming read, so content
/// spanning a band boundary is still seen whole.
pub const OVERLAP: usize = 64 * 1024;

/// How much of a band is held in memory at once during streaming.
pub const CHUNK: usize = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Analysis types
// ---------------------------------------------------------------------------

/// Structural health of a sparsebundle.
#[derive(Debug, Clone, PartialEq)]
pub enum Health {
    /// Partition map parsed and a known filesystem found.
    Sound,
    /// Structure readable but something is off; attach may still work.
    Suspect,
    /// Attaching is pointless or the contents are unreachable.
    Unusable,
}

/// Result of analysing a sparsebundle's structure.
#[derive(Debug, Clone)]
pub struct Analysis {
    pub health: Health,
    pub band_size: u64,
    pub declared_size: u64,
    pub band_count: usize,
    pub zero_length_bands: usize,
    pub info_source: &'static str,
    pub partitions: Vec<Partition>,
    pub notes: Vec<String>,
}

/// A partition found in the image's partition map.
#[derive(Debug, Clone)]
pub struct Partition {
    pub scheme: &'static str,
    pub kind: String,
    pub start_lba: u64,
    pub sectors: u64,
    pub filesystem: Option<&'static str>,
    pub apfs: Option<ApfsInfo>,
}

impl Analysis {
    /// Estimated allocated bytes (band_count * band_size).
    ///
    /// This is *allocated*, not the declared size. A sparsebundle's declared
    /// size is an upper bound it may never approach, so the two differing is
    /// expected, not a fault.
    pub fn allocated_bytes(&self) -> u64 {
        self.band_count as u64 * self.band_size
    }

    /// Render a human-readable summary.
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "  {:?} · bands {} ({:.1} GiB allocated of {:.1} GiB declared) · info from {}\n",
            self.health,
            self.band_count,
            self.allocated_bytes() as f64 / 1_073_741_824.0,
            self.declared_size as f64 / 1_073_741_824.0,
            self.info_source
        ));
        for p in &self.partitions {
            s.push_str(&format!(
                "    {} {} @ LBA {} ({:.1} GiB) fs={}{}\n",
                p.scheme,
                p.kind,
                p.start_lba,
                p.sectors as f64 * SECTOR as f64 / 1_073_741_824.0,
                p.filesystem.unwrap_or("unrecognised"),
                match &p.apfs {
                    Some(a) => format!(
                        " [{} blocks of {}B · {}]",
                        a.block_count,
                        a.block_size,
                        a.encryption_evidence().label()
                    ),
                    None => String::new(),
                }
            ));
        }
        for n in &self.notes {
            s.push_str(&format!("    note: {n}\n"));
        }
        s
    }
}

// ---------------------------------------------------------------------------
// APFS types
// ---------------------------------------------------------------------------

/// Details read out of an APFS container superblock.
#[derive(Debug, Clone, Default)]
pub struct ApfsInfo {
    pub block_size: u32,
    pub block_count: u64,
    pub incompatible_features: u64,
    /// The container keybag (`nx_keylocker`, offset 0x608). A non-empty range
    /// means encryption key material exists for this container.
    pub keylocker_blocks: u64,
}

/// What the container superblock alone can tell us about encryption.
///
/// Deliberately not a boolean. A container-level keybag says nothing
/// definitive about every volume inside, and its absence is not proof that
/// everything is readable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EncryptionEvidence {
    /// nx_keylocker range is empty. Suggestive, not conclusive.
    NoContainerKeybagObserved,
    /// A container keybag exists: encryption is in play somewhere.
    ContainerKeybagObserved,
    /// Superblock unreadable or unparsed.
    Unknown,
}

impl EncryptionEvidence {
    pub fn label(self) -> &'static str {
        match self {
            EncryptionEvidence::NoContainerKeybagObserved => "no container keybag observed",
            EncryptionEvidence::ContainerKeybagObserved => "CONTAINER KEYBAG OBSERVED",
            EncryptionEvidence::Unknown => "encryption state unknown",
        }
    }
}

impl ApfsInfo {
    pub fn encryption_evidence(&self) -> EncryptionEvidence {
        if self.keylocker_blocks > 0 {
            EncryptionEvidence::ContainerKeybagObserved
        } else {
            EncryptionEvidence::NoContainerKeybagObserved
        }
    }
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// Read `len` bytes at absolute image `offset`, resolving which band holds it.
///
/// Returns zeros for an absent band, matching sparse semantics. This is the
/// fundamental read primitive for the format: band index = offset / band_size,
/// offset within band = offset % band_size.
pub fn read_at(bundle: &Path, band_size: u64, offset: u64, len: usize) -> Result<Vec<u8>> {
    let idx = offset / band_size;
    let within = offset % band_size;
    let path = bundle.join("bands").join(format!("{idx:x}"));
    if !path.exists() {
        // Not an error: an unallocated region legitimately reads as zeros.
        return Ok(vec![0u8; len]);
    }
    let mut f = fs::File::open(&path)?;
    f.seek(SeekFrom::Start(within))?;
    let mut buf = vec![0u8; len];
    let mut got = 0;
    while got < len {
        match f.read(&mut buf[got..])? {
            0 => break,
            n => got += n,
        }
    }
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Plist helpers
// ---------------------------------------------------------------------------

/// Pull an integer value out of an XML plist by key.
fn plist_int(xml: &str, key: &str) -> Option<u64> {
    let k = format!("<key>{key}</key>");
    let i = xml.find(&k)? + k.len();
    let rest = &xml[i..];
    let s = rest.find("<integer>")? + "<integer>".len();
    let e = rest[s..].find("</integer>")?;
    rest[s..s + e].trim().parse().ok()
}

fn plist_str(xml: &str, key: &str) -> Option<String> {
    let k = format!("<key>{key}</key>");
    let i = xml.find(&k)? + k.len();
    let rest = &xml[i..];
    let s = rest.find("<string>")? + "<string>".len();
    let e = rest[s..].find("</string>")?;
    Some(rest[s..s + e].trim().to_string())
}

// ---------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------

/// Analyse the structure of a sparsebundle without attaching it.
///
/// Parses `Info.plist` (falling back to `Info.bckup`), counts bands, reads
/// the partition map from band 0, identifies the filesystem, and checks for
/// APFS encryption evidence.
pub fn analyse(bundle: &Path) -> Result<Analysis> {
    let mut notes: Vec<String> = Vec::new();
    let mut health = Health::Sound;

    if !bundle.is_dir() {
        return Err(anyhow!("not a directory — bundle missing"));
    }

    // Info.plist, falling back to the Info.bckup copy Apple writes alongside
    // it. That fallback is the entire reason the backup file exists.
    let (xml, info_source) = match fs::read_to_string(bundle.join("Info.plist")) {
        Ok(x) => (x, "Info.plist"),
        Err(_) => match fs::read_to_string(bundle.join("Info.bckup")) {
            Ok(x) => {
                notes.push("Info.plist unreadable — recovered from Info.bckup".into());
                health = Health::Suspect;
                (x, "Info.bckup")
            }
            Err(e) => return Err(anyhow!("neither Info.plist nor Info.bckup readable: {e}")),
        },
    };
    if xml.starts_with("bplist00") {
        return Err(anyhow!("binary plist — not parsed by this reader"));
    }

    let band_size = plist_int(&xml, "band-size")
        .ok_or_else(|| anyhow!("no band-size in plist — band map unusable"))?;
    let declared_size = plist_int(&xml, "size").unwrap_or(0);
    if let Some(v) = plist_int(&xml, "bundle-backingstore-version") {
        if v != 1 {
            notes.push(format!(
                "unexpected bundle-backingstore-version {v} (expected 1)"
            ));
            health = Health::Suspect;
        }
    }
    if let Some(t) = plist_str(&xml, "diskimage-bundle-type") {
        if !t.contains("sparsebundle") {
            notes.push(format!("unexpected bundle type {t}"));
        }
    }

    // Band inventory.
    let bands_dir = bundle.join("bands");
    let mut band_count = 0usize;
    let mut zero_length_bands = 0usize;
    let mut have_band_zero = false;
    match fs::read_dir(&bands_dir) {
        Ok(rd) => {
            for e in rd.flatten() {
                let md = match e.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if !md.is_file() {
                    continue;
                }
                band_count += 1;
                if md.len() == 0 {
                    zero_length_bands += 1;
                }
                if e.file_name() == "0" {
                    have_band_zero = true;
                }
            }
        }
        Err(e) => {
            return Err(anyhow!("no bands directory: {e}"));
        }
    }
    if band_count == 0 {
        notes.push("bands/ is empty — no data at all".into());
        health = Health::Unusable;
    }
    if zero_length_bands > 0 {
        notes.push(format!(
            "{zero_length_bands} zero-length bands — signature of a truncated copy"
        ));
        health = Health::Unusable;
    }
    if !have_band_zero && band_count > 0 {
        notes.push("band 0 absent — partition map unreadable, image cannot mount".into());
        health = Health::Unusable;
    }

    // Partition map lives at image offset 0, therefore in band 0.
    let mut partitions = Vec::new();
    if have_band_zero {
        let head = read_at(bundle, band_size, 0, 34 * SECTOR as usize)?;
        if &head[SECTOR as usize..SECTOR as usize + 8] == b"EFI PART" {
            partitions = parse_gpt(bundle, band_size, &head)?;
        } else if head[0] == 0x45 && head[1] == 0x52 {
            partitions = parse_apm(bundle, band_size, &head)?;
            notes.push("Apple Partition Map (pre-GPT layout)".into());
        } else if head[0] == 0x00 && head.iter().take(512).all(|&b| b == 0) {
            notes.push("band 0 is entirely zero — image was never formatted".into());
            health = Health::Unusable;
        } else {
            notes.push("no GPT or APM signature at offset 0 — partition map damaged".into());
            health = Health::Unusable;
        }
    }

    // Check for APFS encryption.
    if partitions.iter().any(|p| {
        p.apfs.as_ref().is_some_and(|a| {
            a.encryption_evidence() == EncryptionEvidence::ContainerKeybagObserved
        })
    }) {
        notes.push(
            "container keybag observed — encryption is in play; contents may not be \
             readable without a key"
                .into(),
        );
        health = Health::Suspect;
    }

    if !partitions.is_empty() && !partitions.iter().any(|p| p.filesystem.is_some()) {
        notes.push(
            "partitions found but no recognised filesystem — this is what \
             'no mountable file systems' means"
                .into(),
        );
        health = Health::Unusable;
    }

    Ok(Analysis {
        health,
        band_size,
        declared_size,
        band_count,
        zero_length_bands,
        info_source,
        partitions,
        notes,
    })
}

// ---------------------------------------------------------------------------
// Partition parsing
// ---------------------------------------------------------------------------

fn parse_gpt(bundle: &Path, band_size: u64, head: &[u8]) -> Result<Vec<Partition>> {
    let h = &head[SECTOR as usize..];
    let entry_lba = u64::from_le_bytes(h[72..80].try_into()?);
    let n_entries = u32::from_le_bytes(h[80..84].try_into()?) as usize;
    let entry_size = u32::from_le_bytes(h[84..88].try_into()?) as usize;
    if entry_size < 128 || entry_size > 4096 || n_entries > 1024 {
        return Ok(Vec::new());
    }

    let table = read_at(bundle, band_size, entry_lba * SECTOR, n_entries * entry_size)?;
    let mut out = Vec::new();
    for i in 0..n_entries {
        let e = &table[i * entry_size..(i + 1) * entry_size];
        let guid: [u8; 16] = e[0..16].try_into()?;
        if guid == [0u8; 16] {
            continue;
        }
        let start = u64::from_le_bytes(e[32..40].try_into()?);
        let end = u64::from_le_bytes(e[40..48].try_into()?);
        let kind = match guid {
            g if g == GUID_HFS => "Apple_HFS".to_string(),
            g if g == GUID_APFS => "Apple_APFS".to_string(),
            g if g == GUID_EFI => "EFI".to_string(),
            _ => "other".to_string(),
        };
        let filesystem = probe_fs(bundle, band_size, start)?;
        let apfs = if filesystem == Some("APFS") {
            parse_apfs_superblock(&read_at(bundle, band_size, start * SECTOR, 0x618)?)
        } else {
            None
        };
        out.push(Partition {
            scheme: "GPT",
            kind,
            start_lba: start,
            sectors: end.saturating_sub(start) + 1,
            filesystem,
            apfs,
        });
    }
    Ok(out)
}

fn parse_apm(bundle: &Path, band_size: u64, head: &[u8]) -> Result<Vec<Partition>> {
    let mut out = Vec::new();
    for i in 1..16u64 {
        let off = (i * SECTOR) as usize;
        if off + SECTOR as usize > head.len() {
            break;
        }
        let e = &head[off..off + SECTOR as usize];
        if !(e[0] == 0x50 && e[1] == 0x4d) {
            break; // "PM"
        }
        let start = u32::from_be_bytes(e[8..12].try_into()?) as u64;
        let blocks = u32::from_be_bytes(e[12..16].try_into()?) as u64;
        let kind = String::from_utf8_lossy(&e[48..80])
            .trim_end_matches('\0')
            .trim()
            .to_string();
        let filesystem = probe_fs(bundle, band_size, start)?;
        out.push(Partition {
            scheme: "APM",
            kind,
            start_lba: start,
            sectors: blocks,
            filesystem,
            apfs: None,
        });
    }
    Ok(out)
}

/// Parse the APFS container superblock.
///
/// Offsets from the Apple File System Reference: nx_magic 0x20,
/// nx_block_size 0x24, nx_block_count 0x28, nx_incompatible_features 0x40,
/// nx_keylocker 0x608 (prange: paddr, count).
pub fn parse_apfs_superblock(sb: &[u8]) -> Option<ApfsInfo> {
    if sb.len() < 0x618 || &sb[0x20..0x24] != b"NXSB" {
        return None;
    }
    Some(ApfsInfo {
        block_size: u32::from_le_bytes(sb[0x24..0x28].try_into().ok()?),
        block_count: u64::from_le_bytes(sb[0x28..0x30].try_into().ok()?),
        incompatible_features: u64::from_le_bytes(sb[0x40..0x48].try_into().ok()?),
        keylocker_blocks: u64::from_le_bytes(sb[0x610..0x618].try_into().ok()?),
    })
}

/// Identify the filesystem at a partition start.
fn probe_fs(bundle: &Path, band_size: u64, start_lba: u64) -> Result<Option<&'static str>> {
    let base = start_lba * SECTOR;
    let sb = read_at(bundle, band_size, base, 64)?;
    if &sb[32..36] == b"NXSB" {
        return Ok(Some("APFS"));
    }
    // HFS+ volume header is at offset 1024 from the partition start.
    let vh = read_at(bundle, band_size, base + 1024, 2)?;
    match &vh[..2] {
        b"H+" => return Ok(Some("HFS+")),
        b"HX" => return Ok(Some("HFSX")),
        _ => {}
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Band enumeration and streaming
// ---------------------------------------------------------------------------

/// One band file, with the index parsed from its hex filename.
#[derive(Debug)]
pub struct Band {
    pub index: u64,
    pub path: PathBuf,
}

/// List all bands in a sparsebundle in logical (numerical) order.
///
/// Band files are named in unpadded lowercase hex. Sorting filenames as
/// strings would put `10` before `a`, scrambling the address space. This
/// function parses the hex index and sorts numerically.
pub fn bands(bundle: &Path) -> Result<Vec<Band>> {
    let dir = bundle.join("bands");
    let mut out = Vec::new();
    for e in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
        let e = e?;
        let name = e.file_name().to_string_lossy().to_string();
        if let Ok(index) = u64::from_str_radix(&name, 16) {
            out.push(Band {
                index,
                path: e.path(),
            });
        }
    }
    out.sort_by_key(|b| b.index);
    Ok(out)
}

/// Read the head of the next adjacent band for cross-boundary content.
///
/// Only reads when the next band's index is exactly one greater. A sparse
/// gap between bands means the intervening region is zeros -- splicing
/// non-adjacent bands would join data that is megabytes apart.
pub fn adjacent_head(b: &Band, next: Option<&Band>) -> Result<Option<Vec<u8>>> {
    let Some(n) = next else {
        return Ok(None);
    };
    if n.index != b.index + 1 {
        return Ok(None);
    }
    let mut f = fs::File::open(&n.path)?;
    let mut head = vec![0u8; OVERLAP];
    let got = f.read(&mut head)?;
    head.truncate(got);
    Ok(Some(head))
}

/// Stream a band in bounded memory windows, calling `on_window` with each
/// chunk and its byte offset within the band.
///
/// Consecutive windows overlap by [`OVERLAP`] bytes, so content lying across
/// a window boundary is still seen whole. The caller deduplicates by logical
/// offset if needed.
pub fn stream_band<F>(b: &Band, tail: Option<Vec<u8>>, mut on_window: F) -> Result<u64>
where
    F: FnMut(u64, &[u8]),
{
    let mut f = fs::File::open(&b.path)?;
    let mut carry: Vec<u8> = Vec::new();
    let mut carry_at: u64 = 0;
    let mut read_total: u64 = 0;
    let mut chunk = vec![0u8; CHUNK];

    loop {
        let mut filled = 0usize;
        while filled < chunk.len() {
            match f.read(&mut chunk[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        if filled == 0 {
            break;
        }
        read_total += filled as u64;

        let mut buf = std::mem::take(&mut carry);
        let buf_at = if buf.is_empty() {
            read_total - filled as u64
        } else {
            carry_at
        };
        buf.extend_from_slice(&chunk[..filled]);
        on_window(buf_at, &buf);

        let keep = OVERLAP.min(buf.len());
        carry_at = buf_at + (buf.len() - keep) as u64;
        carry = buf[buf.len() - keep..].to_vec();

        if filled < CHUNK {
            break;
        }
    }

    // Splice the next band's head onto the final carry, so content spanning
    // the band boundary is seen. Only reached when the bands are adjacent.
    if let Some(t) = tail {
        let mut buf = carry;
        let at = carry_at;
        buf.extend_from_slice(&t);
        on_window(at, &buf);
    }
    Ok(read_total)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_extracts_band_size_and_size() {
        let xml = r#"<plist><dict>
            <key>band-size</key><integer>268435456</integer>
            <key>size</key><integer>38291861393408</integer>
            <key>diskimage-bundle-type</key><string>com.apple.diskimage.sparsebundle</string>
        </dict></plist>"#;
        assert_eq!(plist_int(xml, "band-size"), Some(268435456));
        assert_eq!(plist_int(xml, "size"), Some(38291861393408));
        assert!(plist_str(xml, "diskimage-bundle-type")
            .unwrap()
            .contains("sparsebundle"));
        assert_eq!(plist_int(xml, "absent"), None);
    }

    #[test]
    fn band_filenames_are_unpadded_lowercase_hex() {
        assert_eq!(format!("{:x}", 0u64), "0");
        assert_eq!(format!("{:x}", 255u64), "ff");
        assert_eq!(format!("{:x}", 4096u64), "1000");
    }

    #[test]
    fn missing_band_reads_as_zeros_not_an_error() {
        let dir = std::env::temp_dir().join(format!("sb-band-{}", std::process::id()));
        fs::create_dir_all(dir.join("bands")).unwrap();
        let got = read_at(&dir, 1024, 8192, 16).unwrap();
        assert_eq!(got, vec![0u8; 16]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_at_resolves_offset_to_the_right_band() {
        let dir = std::env::temp_dir().join(format!("sb-band2-{}", std::process::id()));
        fs::create_dir_all(dir.join("bands")).unwrap();
        // band-size 256, so offset 512 is band 2 ("2"), 16 bytes in.
        fs::write(dir.join("bands/2"), vec![0xAB; 256]).unwrap();
        let got = read_at(&dir, 256, 512 + 16, 4).unwrap();
        assert_eq!(got, vec![0xAB; 4]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hfs_and_apfs_magics_are_distinguished() {
        let dir = std::env::temp_dir().join(format!("sb-fs-{}", std::process::id()));
        fs::create_dir_all(dir.join("bands")).unwrap();
        let mut band = vec![0u8; 4096];
        band[1024] = b'H';
        band[1025] = b'+';
        fs::write(dir.join("bands/0"), &band).unwrap();
        assert_eq!(probe_fs(&dir, 1 << 20, 0).unwrap(), Some("HFS+"));

        band[1024] = 0;
        band[1025] = 0;
        band[32..36].copy_from_slice(b"NXSB");
        fs::write(dir.join("bands/0"), &band).unwrap();
        assert_eq!(probe_fs(&dir, 1 << 20, 0).unwrap(), Some("APFS"));
        fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod apfs_tests {
    use super::*;

    fn superblock(keylocker_blocks: u64) -> Vec<u8> {
        let mut sb = vec![0u8; 0x618];
        sb[0x20..0x24].copy_from_slice(b"NXSB");
        sb[0x24..0x28].copy_from_slice(&4096u32.to_le_bytes());
        sb[0x28..0x30].copy_from_slice(&1_000_000u64.to_le_bytes());
        sb[0x610..0x618].copy_from_slice(&keylocker_blocks.to_le_bytes());
        sb
    }

    #[test]
    fn reads_container_geometry() {
        let a = parse_apfs_superblock(&superblock(0)).unwrap();
        assert_eq!(a.block_size, 4096);
        assert_eq!(a.block_count, 1_000_000);
    }

    #[test]
    fn keybag_is_evidence_not_a_verdict() {
        let none = parse_apfs_superblock(&superblock(0)).unwrap();
        assert_eq!(
            none.encryption_evidence(),
            EncryptionEvidence::NoContainerKeybagObserved,
            "absence of a keybag is an observation, not proof of readability"
        );
        let some = parse_apfs_superblock(&superblock(8)).unwrap();
        assert_eq!(
            some.encryption_evidence(),
            EncryptionEvidence::ContainerKeybagObserved
        );
        assert!(!none.encryption_evidence().label().contains("unencrypted"));
    }

    #[test]
    fn rejects_non_apfs_and_short_buffers() {
        assert!(
            parse_apfs_superblock(&vec![0u8; 0x618]).is_none(),
            "no NXSB magic"
        );
        assert!(
            parse_apfs_superblock(&superblock(0)[..0x100]).is_none(),
            "truncated"
        );
    }
}

#[cfg(test)]
mod band_tests {
    use super::*;

    fn mk_bundle(dir: &Path, band_list: &[(&str, &[u8])]) {
        fs::create_dir_all(dir.join("bands")).unwrap();
        for (name, data) in band_list {
            fs::write(dir.join("bands").join(name), data).unwrap();
        }
    }

    #[test]
    fn bands_sort_numerically_not_lexically() {
        let d = std::env::temp_dir().join(format!("sb-sort-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        mk_bundle(&d, &[("0", b"a"), ("a", b"b"), ("10", b"c"), ("2", b"d")]);
        let got: Vec<u64> = bands(&d).unwrap().iter().map(|b| b.index).collect();
        assert_eq!(got, vec![0, 2, 10, 16]);
        fs::remove_dir_all(&d).ok();
    }

    /// Collect every window `stream_band` produces, for assertions.
    fn windows(d: &Path, which: usize) -> Vec<(u64, Vec<u8>)> {
        let list = bands(d).unwrap();
        let tail = adjacent_head(&list[which], list.get(which + 1)).unwrap();
        let mut got = Vec::new();
        stream_band(&list[which], tail, |at, buf| got.push((at, buf.to_vec()))).unwrap();
        got
    }

    #[test]
    fn adjacent_bands_are_spliced() {
        let d = std::env::temp_dir().join(format!("sb-adj-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        mk_bundle(&d, &[("0", b"aaaa"), ("1", b"bbbb")]);
        let got = windows(&d, 0);
        assert!(
            got.iter()
                .any(|(_, b)| b.windows(8).any(|w| w == b"aaaabbbb")),
            "consecutive bands must be seen spliced: {got:?}"
        );
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn non_adjacent_bands_are_never_spliced() {
        let d = std::env::temp_dir().join(format!("sb-gap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        mk_bundle(&d, &[("0", b"aaaa"), ("5", b"bbbb")]);
        let list = bands(&d).unwrap();
        assert!(
            adjacent_head(&list[0], list.get(1)).unwrap().is_none(),
            "a gap must break adjacency"
        );
        let got = windows(&d, 0);
        assert!(
            !got.iter().any(|(_, b)| b.windows(2).any(|w| w == b"ab")),
            "band 0 and band 5 must never appear joined"
        );
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_secret_split_across_a_window_boundary_is_still_seen() {
        let d = std::env::temp_dir().join(format!("sb-win-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let mut data = vec![b'.'; CHUNK - 10];
        data.extend_from_slice(b"SPLIT-ACROSS-THE-WINDOW-BOUNDARY");
        data.extend(vec![b'.'; 1000]);
        mk_bundle(&d, &[("0", &data)]);
        let got = windows(&d, 0);
        assert!(
            got.iter().any(|(_, b)| b
                .windows(32)
                .any(|w| w == b"SPLIT-ACROSS-THE-WINDOW-BOUNDARY")),
            "the overlap between windows must keep a straddling secret intact"
        );
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn window_offsets_are_absolute_within_the_band() {
        let d = std::env::temp_dir().join(format!("sb-off-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let data = vec![b'x'; CHUNK * 2 + 4096];
        mk_bundle(&d, &[("0", &data)]);
        let got = windows(&d, 0);
        assert!(
            got.len() >= 2,
            "expected multiple windows, got {}",
            got.len()
        );
        assert_eq!(got[0].0, 0, "first window starts at 0");
        for w in got.windows(2) {
            assert!(
                w[1].0 > w[0].0,
                "window offsets must advance: {:?}",
                (w[0].0, w[1].0)
            );
            let ends_at = w[0].0 + w[0].1.len() as u64;
            assert!(
                w[1].0 < ends_at,
                "gap between windows: {} ends at {ends_at}, next starts at {}",
                w[0].0,
                w[1].0
            );
            assert_eq!(
                ends_at - w[1].0,
                OVERLAP as u64,
                "consecutive windows must overlap by exactly OVERLAP"
            );
        }
        fs::remove_dir_all(&d).ok();
    }
}
