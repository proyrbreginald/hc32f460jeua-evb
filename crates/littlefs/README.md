# hc32-littlefs

`hc32-littlefs` is a small `no_std`, no-allocation filesystem for NOR flash.
It is designed for the HC32F460 internal flash geometry and for applications
where recovery behavior matters more than POSIX features or write throughput.

The on-disk format is intentionally **not compatible** with littlefs. The
littlefs 2.11.3 design informed the ordering rules, CRC validation, serial
generation comparison, and fault tests. This crate removes its directory tree,
CTZ lists, metadata-pair append log, FCRC, orphan state, and move state by using
a bounded flat namespace and complete immutable snapshots.

## Supported operations

- atomic whole-file create or replace;
- bounded reads, `stat`, and callback-based listing;
- atomic remove and rename;
- format, mount, verification, and device recovery after an I/O error.

There are no subdirectories, open file handles, random writes, attributes,
timestamps, permissions, or disk-format compatibility layer. Names are UTF-8,
must not contain `/` or NUL, and are limited to 63 bytes. A snapshot contains at
most 32 files.

See [DESIGN.md](DESIGN.md) for the disk format, durability contract, and safety
argument.

