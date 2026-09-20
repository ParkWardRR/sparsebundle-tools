# Roadmap

Where this is going, in four phases. Each one makes the crate useful to a
wider set of people.

---

## Phase 1 — Format completeness

The crate reads XML plists and the GPT/APM/APFS structures that show up in
every Time Machine backup. That covers 95% of sparsebundles in the wild. The
remaining 5% hit one of these:

| Gap | What happens today | Fix |
|---|---|---|
| **Binary plists** | Errors out with "binary plist — not parsed" | Add a binary plist reader (the format is [documented](https://medium.com/@karaiskc/understanding-apples-binary-property-list-format-281e6da00dbd) and small) |
| **UDIF / flat DMG** | Not supported at all | UDIF is the non-sparse variant — a single `.dmg` with a resource fork trailer. Same partition structures inside, different container. |
| **CoreStorage LVG** | Partitions parse but LVG volumes are invisible | Pre-FileVault2 encryption scheme. Detect the LVG GUID and report it, even if we can't read through it. |
| **GUID table** | Only HFS, APFS, EFI recognised | Add Linux filesystems (ext4, XFS) and Windows (NTFS, FAT32). People put non-Mac images in sparsebundles. |

**When it's done:** `sparsebundle info` gives a useful answer for every
sparsebundle, not just well-formed macOS backup images.

---

## Phase 2 — Filesystem awareness

Right now the crate stops at the partition map. It tells you "this partition
is HFS+" but can't list what's inside without mounting. This phase adds
read-only filesystem parsing so you can browse the contents directly.

| Feature | What it unlocks |
|---|---|
| **HFS+ catalog tree** | List files and directories by reading B-tree nodes from the band data. No mount, no FUSE, no root. |
| **APFS object map** | Read the container's object map and volume superblocks. Enumerate volumes, snapshots, and the root directory tree. |
| **File extraction by path** | `sparsebundle cat MyMac.sparsebundle:/Users/me/Documents/notes.txt` — read a file out of an unmounted bundle. |
| **Snapshot enumeration** | List Time Machine snapshots (APFS) or dated directories (HFS+) and report which machine each belongs to. |

This is the hard phase. HFS+ B-trees are well-documented but fiddly. APFS
is reverse-engineered from the [Apple File System Reference](https://developer.apple.com/support/downloads/Apple-File-System-Reference.pdf)
and community work. Read-only is the constraint that keeps it tractable.

**When it's done:** You can list and extract files from a sparsebundle on any
platform, without any mount infrastructure.

---

## Phase 3 — CI, distribution, and API polish

The crate works. This phase makes it usable by other people's projects.

| Item | Detail |
|---|---|
| **GitHub Actions CI** | Build + test on macOS and Linux, clippy, rustfmt check. The CI badge in the README currently points nowhere. |
| **crates.io publish** | Publish as `sparsebundle` on crates.io. Pin MSRV, add `rust-version` to Cargo.toml. |
| **Cross-compilation** | Test that it builds for `x86_64-unknown-linux-gnu` and `aarch64-apple-darwin`. No platform-specific code today, but best to prove it. |
| **API docs** | `cargo doc` with examples for every public function. The doc comments exist but the examples don't run as doctests yet. |
| **Error types** | Replace `anyhow::Result` in the public API with a proper `sparsebundle::Error` enum. Library consumers shouldn't depend on `anyhow`. |
| **`no_std` feasibility** | Evaluate whether the core analysis path can work without `std::fs`. Probably not worth it, but worth knowing. |

**When it's done:** `cargo add sparsebundle` works, CI is green, docs are on
docs.rs, and downstream crates get typed errors.

---

## Phase 4 — Power tools

Features for people doing forensics, data recovery, or bulk operations across
many bundles.

| Feature | Why |
|---|---|
| **Band integrity check** | Read every band and verify it's the expected size. Catches truncated copies that `analyse` can't see without reading every file. |
| **Bundle diff** | Compare two sparsebundles band-by-band. "Which bands changed since last backup?" — useful for incremental copies. |
| **Logical address map** | Given a byte offset in the logical image, report which band file and offset within it. Inverse of what `read_at` does. Useful for correlating filesystem tools' output with band files. |
| **Parallel streaming** | `stream_band` is single-threaded today. For local disk (not NAS), parallel reads across bands would cut wall time. Configurable worker count, same as the private tool learned the hard way. |
| **JSON output** | `sparsebundle info --json` for scripting and piping into `jq`. |

**When it's done:** The crate is a serious tool for anyone working with
sparsebundles at scale.

---

## Not planned

- **Write support.** This is a read-only tool. Creating or modifying
  sparsebundles is Apple's job.
- **Encryption cracking.** Detecting encryption: yes. Breaking it: no.
- **Full filesystem driver.** Phase 2 adds read-only browsing, not a FUSE
  mount. If you need a mount, use `sparsebundlefs` + the OS filesystem
  driver.
