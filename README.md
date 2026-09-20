<![CDATA[<div align="center">

# sparsebundle-tools

**Read, analyse and stream Apple sparsebundle disk images without mounting them.**

[![License: BlueOak-1.0.0](https://img.shields.io/badge/license-BlueOak--1.0.0-blue)](https://blueoakcouncil.org/license/1.0.0)
[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey)](https://github.com/ParkWardRR/sparsebundle-tools)
[![Tests](https://img.shields.io/badge/tests-13%20passing-brightgreen)](#tests)
[![Roadmap](https://img.shields.io/badge/roadmap-4%20phases-informational)](ROADMAP.md)
[![GitHub last commit](https://img.shields.io/github/last-commit/ParkWardRR/sparsebundle-tools)](https://github.com/ParkWardRR/sparsebundle-tools/commits/main)
[![GitHub stars](https://img.shields.io/github/stars/ParkWardRR/sparsebundle-tools)](https://github.com/ParkWardRR/sparsebundle-tools/stargazers)

</div>

---

Born out of a week of debugging old Time Machine backups on a Synology NAS, trying to figure out which ones are worth keeping and which are structurally dead. `hdiutil attach` on a 4.5 TB bundle takes thirty minutes and then says "no mountable file systems", which tells you nothing. This tool answers the same questions from a few kilobytes of reads.

Rust library + CLI. No dependencies beyond `anyhow` and `clap`. No unsafe code.

---

## What it does

| Command | What you get |
|---|---|
| `sparsebundle-tools info <path>` | Full structural analysis: health, partition map (GPT/APM), filesystem (HFS+/APFS), encryption state, band inventory |
| `sparsebundle-tools bands <path>` | List every band file in correct numerical order with sizes |
| `sparsebundle-tools read <path> --offset N --len N` | Read arbitrary bytes at any logical offset — resolves the right band automatically |

As a library: `analyse()`, `read_at()`, `bands()`, `stream_band()` — everything you need to build tools on top of the sparsebundle format.

---

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

---

## Install

```sh
cargo install sparsebundle-tools
```

Or as a library dependency:

```toml
[dependencies]
sparsebundle-tools = "0.1"
```

Or build from source:

```sh
git clone https://github.com/ParkWardRR/sparsebundle-tools.git
cd sparsebundle-tools
cargo build --release
```

---

## CLI usage

### Structural analysis

```sh
sparsebundle-tools info /Volumes/NAS/Backups/MyMac.sparsebundle
```

```
  Sound · bands 2956 (739.0 GiB allocated of 4500.0 GiB declared) · info from Info.plist
    GPT Apple_APFS @ LBA 40 (4499.9 GiB) fs=APFS [9374660 blocks of 4096B · no container keybag observed]
```

### List bands

```sh
sparsebundle-tools bands /Volumes/NAS/Backups/MyMac.sparsebundle
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
sparsebundle-tools read /path/to/bundle --offset 0 --len 512 | xxd | head

# Read the GPT header
sparsebundle-tools read /path/to/bundle --offset 512 --len 512 | xxd
```

The `read` command writes raw bytes to stdout. Pipe to `xxd`, `hexdump`, or a file.

---

## Library API

```rust
use std::path::Path;

fn main() -> anyhow::Result<()> {
    let bundle = Path::new("/Volumes/NAS/Backups/MyMac.sparsebundle");

    // Structural analysis
    let analysis = sparsebundle_tools::analyse(bundle)?;
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
    let mbr = sparsebundle_tools::read_at(bundle, analysis.band_size, 0, 512)?;
    println!("MBR signature: {:02x}{:02x}", mbr[510], mbr[511]);

    // Enumerate bands in correct order
    let bands = sparsebundle_tools::bands(bundle)?;
    println!("First band: index={}, path={}", bands[0].index, bands[0].path.display());

    // Stream a band in bounded memory (8 MiB windows, 64 KiB overlap)
    sparsebundle_tools::stream_band(&bands[0], None, |offset, window| {
        println!("Window at offset {}, {} bytes", offset, window.len());
    })?;

    Ok(())
}
```

### Band streaming

The streaming API reads each band in bounded 8 MiB windows with 64 KiB overlap between consecutive windows, so content that straddles a window boundary is seen whole. Cross-band overlap works the same way — when two bands are numerically adjacent, the tail of one is spliced onto the head of the next. Non-adjacent bands (a gap in the sparse image) are never spliced, because that would join unrelated data.

This is the API you want for processing every byte of a multi-terabyte bundle without blowing memory.

---

## What `hdiutil` errors actually mean

### `no mountable file systems`

This has at least four different causes and the error message distinguishes none of them:

1. **Genuinely truncated band set.** Band 0 is missing or the bands directory is empty. The partition map is gone. Data is unrecoverable. Reports `Health::Unusable`.

2. **Encrypted image.** The APFS container has a keybag. Everything reads fine at the byte level but the content is ciphertext. Reports `Health::Suspect` with "container keybag observed".

3. **Unrecognised filesystem.** The partition map is intact but the filesystem is not HFS+ or APFS. Partitions parse but `filesystem` comes back `None`.

4. **Damaged partition map.** Band 0 exists but contains neither a GPT nor APM signature. Reports `Health::Unusable` with "no GPT or APM signature".

All four are decidable from a few kilobytes of reads. You do not need to wait thirty minutes for `hdiutil attach` to time out.

### `resource busy`

The bundle's band files are open by another process. On a NAS, this usually means Spotlight indexing or an `mdworker` that followed a symlink.

### `image not recognized`

Info.plist is missing, unreadable, or a binary plist. This library falls back to Info.bckup automatically. If both are gone, the bundle is structurally dead.

---

## Platform notes

**macOS**: Works natively. Point it at any `.sparsebundle` directory. No `hdiutil` involvement.

**Linux**: Sparsebundles on a NAS mount as regular directories over SMB/NFS. This tool reads them directly. For *mounting* the image after analysis, see [timemachine-mount](https://github.com/ParkWardRR/timemachine-mount).

**Windows**: Should work (just directory and file I/O) but untested. PRs welcome.

---

## Tests

```sh
cargo test
```

13 unit tests covering:
- Plist parsing and value extraction
- Band filename hex sorting (the trap)
- Missing-band sparse semantics (reads as zeros)
- Offset-to-band resolution
- HFS+ vs APFS filesystem probing
- APFS superblock parsing and encryption evidence
- Band adjacency and streaming overlap
- Cross-window boundary handling
- Window offset correctness

---

## What this does not do

- Does not mount anything. Read-only analysis of the raw structure.
- Does not handle binary plists. Apple sometimes writes these; this library requires XML plists (the normal case for sparsebundles).
- Does not decrypt APFS volumes. It detects whether encryption is present but cannot read through it.
- Does not write or modify sparsebundles. Read-only.

See [ROADMAP.md](ROADMAP.md) for what's planned.

---

## License

[Blue Oak Model License 1.0.0](LICENSE.md)
]]>