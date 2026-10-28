# mkfs-mfs

Create Macintosh File System (MFS) disk images in Rust.

MFS was the original Macintosh filesystem (1984), a flat filesystem where
"folders" were a Finder-only illusion not reflected in the on-disk
structure. It was superseded by HFS in 1985 but is still readable by
classic Mac emulators (Mini vMac, Basilisk II) and modern tools like
[CiderPress2][cp2].

[cp2]: https://github.com/fadden/CiderPress2

## Install

From crates.io:

```bash
cargo install --locked mkfs-mfs
```

This installs a binary called `mkfs-mfs`. Pre-built binaries for Linux
(x86_64 / aarch64), macOS (universal), and Windows are also attached to
each [GitHub release][releases].

[releases]: https://github.com/bitplane/mkfs-mfs/releases

## Usage

```bash
# Create an empty 400 KB disk image (a Sony 3.5" SS floppy).
mkfs-mfs out.dsk

# Custom size, custom volume name.
mkfs-mfs -s 800 -v "MyDisk" out.dsk

# Populate from a directory tree. Subdirectories are flattened with ':'
# as the path separator (the Mac convention), since MFS has no real
# folders.
mkfs-mfs -d ./payload -s 800 out.dsk
```

Source filenames containing `:` are rejected (would clash with the
flattening path separator).

## Limits

- 4094 files maximum (12-bit map).
- 640 allocation blocks maximum for compatibility with two-sector MDB readers.
  Allocation blocks grow with image size (minimum 1 KB).
- Filenames up to 255 bytes (truncated longer with a warning).
  Names and volume labels are NFC-normalized and use MacRoman; unrepresentable characters are rejected.
  Names that collide after truncation or classic Mac case-insensitive comparison are rejected.
- File-extension → Type/Creator mapping for `.txt`, `.text`, `.c`, `.h`
  (TEXT/ttxt) and `.bin` (BINA/hDmp). Everything else is `????`/`????`.

## License

WTFPL with one extra clause: don't blame me. See [LICENSE](LICENSE).
