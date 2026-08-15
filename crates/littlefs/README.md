# hc32-littlefs

`hc32-littlefs` is a small `no_std`, no-allocation filesystem for NOR flash.
It is designed for the HC32F460 internal flash geometry and for applications
where recovery behavior matters more than POSIX features or write throughput.

The on-disk format is intentionally **not compatible** with littlefs. The
littlefs 2.11.3 design informed the ordering rules, CRC validation, serial
generation comparison, and fault tests. This crate removes CTZ lists,
metadata-pair append logs, FCRC, orphan state, and move state by using a bounded
directory tree inside complete immutable snapshots.

## Supported operations

- atomic whole-file create or replace;
- bounded reads, typed `stat`, write-size preflight, and callback-based listing;
- explicit directories with `mkdir`, empty-directory `rmdir`, and `read_dir`;
- atomic file rename and whole-directory-tree rename;
- format, mount, verification, and device recovery after an I/O error;
- erase-count wear leveling: dynamic placement on every mutation (least-worn
  non-overlapping run) plus an explicit `level()` static relocation; per-block
  counters live in each snapshot and survive remounts and reformats.

The root directory is implicit. Public paths are canonical UTF-8 paths relative
to that root: no leading or trailing `/`, empty component, `.` component, `..`
component, or NUL is accepted. The empty path denotes root for `stat` and
`read_dir`; persistent paths are limited to 63 bytes. A snapshot contains at
most 32 total file and directory entries. Directory records use the flag field
already present in the version-1 record header, so existing flag-zero root files
remain mountable.

There are no open file handles, random writes, recursive removal, implicit
parent creation, symbolic links, attributes, timestamps, permissions, or
disk-format compatibility layer. Current-working-directory and absolute/relative
input normalization belong to the caller; the filesystem accepts only canonical
root-relative paths.

See [DESIGN.md](DESIGN.md) for the disk format, durability contract, and safety
argument.
