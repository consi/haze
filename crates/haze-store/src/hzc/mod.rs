//! `.hzc` - Haze Chunks.
//!
//! Append-only, gorilla-compressed, zstd-wrapped time-series chunk files.
//! Self-contained: a host's chunks live in one directory and are
//! discoverable by `readdir` + filename parsing alone, no external index
//! required.
//!
//! Phase A: chunk format primitives (`chunk.rs`, `encoding.rs`, `format.rs`).
//! Phase B: writer (`wal.rs`, `writer.rs`), reader (`reader.rs`), and the
//! process-wide handle cache (`store.rs`).

mod bits;
pub mod chunk;
pub mod compactor;
pub mod encoding;
pub mod format;
pub mod reader;
pub mod store;
pub mod wal;
pub mod writer;

pub use chunk::{ChunkDecodeError, ChunkEncodeError, ChunkHeader, decode_chunk, encode_chunk};
pub use compactor::{
    CompactReport, DEFAULT_ROLLUP_SETTLED_AFTER_SECS, MigrationReport, RollupReport, compact_host,
    rollup_host,
};
pub use format::{
    ChunkRef, FilenameError, chunk_filename, is_legacy_chunk_name, parse_chunk_filename,
};
pub use reader::{list_chunks, read_all, read_range, read_range_in_dir};
pub use store::HzcStore;
pub use writer::{
    HostWriter, HzcError, Meta, RetentionTier, chunk_window_bounds, default_retention_tiers,
    host_directory,
};
