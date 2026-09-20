# sparsebundle

[![Crates.io](https://img.shields.io/crates/v/sparsebundle)](https://crates.io/crates/sparsebundle)
[![License: BlueOak-1.0.0](https://img.shields.io/badge/license-BlueOak--1.0.0-blue)](https://blueoakcouncil.org/license/1.0.0)
[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange)](https://www.rust-lang.org)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey)](https://github.com/ParkWardRR/sparsebundle)
[![CI](https://img.shields.io/github/actions/workflow/status/ParkWardRR/sparsebundle/ci.yml?label=CI)](https://github.com/ParkWardRR/sparsebundle/actions)
[![Roadmap](https://img.shields.io/badge/roadmap-4%20phases-informational)](ROADMAP.md)

Read, analyse and stream Apple sparsebundle disk images without mounting them.

Born out of a week of debugging old Time Machine backups on a Synology NAS, trying to figure out which ones are worth keeping and which are structurally dead. `hdiutil attach` on a 4.5 TB bundle takes thirty minutes and then says "no mountable file systems", which tells you nothing. This tool answers the same questions from a few kilobytes of reads.

## The format

A `.sparsebundle` is a directory, not a file. Inside:

```
MyBackup.sparsebundle/
  Info.plist          # band size, declared image size, bundle type
  Info.bckup          # copy of Info.plist (recovery path if the primary is damaged)
  bands/
    0                 # first 256 MiB (or whatever the band size is)
    1
    2
    ...
    ff
    100
    ...
  token               # empty file, always present, not a lock
```

Each band holds one slice of the logical disk image. Band index = byte offset / band size. Band 0 contains the partition map. The declared size in Info.plist is a ceiling the image may never reach. A sparsebundle with 200 bands out of a possible 18,000 is normal, not damaged.

A missing band is not an error. It reads as zeros. That is what "sparse" means.

## The trap

Band files are named in unpadded lowercase hex. Sort them as strings and you get `10` before `a`. Your address space is now scrambled and every read returns wrong data. Ask me how I know.

The correct sort order for bands `0, 2, a, 10` is numeric: 0, 2, 10 (decimal), 16 (decimal). String sort gives `0, 10, 2, a`. Every adjacency check fails, every streaming read crosses a gap, and no error is raised because each individual band reads fine.

This library parses the hex index and sorts numerically.

## Install

```sh
cargo install sparsebundle
```

Or as a library dependency:

```toml
[dependencies]
sparsebundle = "0.1"
```

## CLI usage

### Structural analysis

```sh
sparsebundle info /Volumes/NAS/Backups/MyMac.sparsebundle
```

Output:

```
  Sound · bands 2956 (739.0 GiB allocated of 4500.0 GiB declared) · info from Info.plist
    GPT Apple_APFS @ LBA 40 (4499.9 GiB) fs=APFS [9374660 blocks of 4096B · no container keybag observed]
```

### List bands

```sh
sparsebundle bands /Volumes/NAS/Backups/MyMac.sparsebundle
```

```
2956 bands, band size 268435456 bytes (256 MiB)
       0  268435456 bytes
       1  268435456 bytes
       2  268435456 bytes
      ...
     b8b  134217728 bytes
```

### Read raw bytes

```sh
# Dump the first 512 bytes (the protective MBR)
sparsebundle read /path/to/bundle --offset 0 --len 512 | xxd | head

# Read the GPT header
sparsebundle read /path/to/bundle --offset 512 --len 512 | xxd
```

The `read` command writes raw bytes to stdout. Pipe to `xxd`, `hexdump`, or a file.

## Library API

```rust
use std::path::Path;

fn main() -> anyhow::Result<()> {
    let bundle = Path::new("/Volumes/NAS/Backups/MyMac.sparsebundle");

    // Structural analysis
    let analysis = sparsebundle::analyse(bundle)?;
    println!("Health: {:?}", analysis.health);
    println!("Band size: {} bytes", analysis.band_size);
    println!("Bands: {}", analysis.band_count);
    println!("Allocated: {:.1} GiB", analysis.allocated_bytes() as f64 / 1_073_741_824.0);

    for p in &analysis.partitions {
        println!("{} {} fs={:?}", p.scheme, p.kind, p.filesystem);
        if let Some(apfs) = &p.apfs {
            println!("  APFS: {} blocks, encryption: {}",
                apfs.block_count, apfs.encryption_evidence().label());
        }
    }

    // Read arbitrary bytes by logical offset
    let mbr = sparsebundle::read_at(bundle, analysis.band_size, 0, 512)?;
    println!("MBR signature: {:02x}{:02x}", mbr[510], mbr[511]);

    // Enumerate bands in correct order
    let bands = sparsebundle::bands(bundle)?;
    println!("First band: index={}, path={}", bands[0].index, bands[0].path.display());

    // Stream a band in bounded memory
    sparsebundle::stream_band(&bands[0], None, |offset, window| {
        // Process each window. Consecutive windows overlap by OVERLAP bytes.
        println!("Window at offset {}, {} bytes", offset, window.len());
    })?;

    Ok(())
}
```

## What `hdiutil` errors actually mean

### `no mountable file systems`

This has at least four different causes and the error message distinguishes none of them:

1. **Genuinely truncated band set.** Band 0 is missing or the bands directory is empty. The partition map is gone. Data is unrecoverable. This library reports `Health::Unusable` with a note about the missing band.

2. **Encrypted image.** The APFS container has a keybag. Everything reads fine at the byte level but the content is ciphertext. Without the password, the volume cannot mount. This library reports `Health::Suspect` with "container keybag observed".

3. **Unrecognised filesystem.** The partition map is intact but the filesystem is not HFS+ or APFS. Maybe it is an old UFS image, or something custom. The partitions parse but `filesystem` comes back `None`.

4. **Damaged partition map.** Band 0 exists but contains neither a GPT nor APM signature. The image was either never formatted or its metadata is corrupted. This library reports `Health::Unusable` with "no GPT or APM signature".

All four are decidable from a few kilobytes of reads. You do not need to wait thirty minutes for `hdiutil attach` to time out.

### `resource busy`

The bundle's band files are open by another process. On a NAS, this usually means Spotlight indexing or an `mdworker` that followed a symlink.

### `image not recognized`

Info.plist is missing, unreadable, or a binary plist. This library falls back to Info.bckup automatically. If both are gone, the bundle is structurally dead.

## Platform notes

**macOS**: Works natively. You can point it at any `.sparsebundle` directory, whether it is mounted or not. No `hdiutil` involvement.

**Linux**: Sparsebundles on a NAS mount as regular directories over SMB/NFS/AFP. This tool reads them directly. If you want to *mount* the image after analysis, use [sparsebundlefs](https://github.com/torarnv/sparsebundlefs) to expose it as a single block device, then `mount -t hfsplus` or similar.

**Windows**: Should work (it is just directory and file I/O) but untested. PRs welcome.

## What this does not do

- Does not mount anything. Read-only analysis of the raw structure.
- Does not handle binary plists. Apple sometimes writes these; this library requires XML plists (the normal case for sparsebundles).
- Does not decrypt APFS volumes. It detects whether encryption is present but cannot read through it.
- Does not write or modify sparsebundles. Read-only.

## License

[Blue Oak Model License 1.0.0](https://blueoakcouncil.org/license/1.0.0)
