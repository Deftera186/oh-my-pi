//! Bounded multi-format archive reading with deterministic ZIP/TAR writing.
//!
//! [`Archive`] indexes a seekable source without materializing ordinary
//! member payloads: ZIP (incl. ZIP64), TAR (ustar/GNU/PAX), Electron ASAR,
//! RAR 4/5, 7z, ISO 9660, CAB, cpio, RPM, Unix ar, Debian packages, LZH, and
//! ARJ containers, plus gzip/bzip2/xz/zstd/`.Z`/LZMA-compressed tars and
//! single-stream files (one stem-named member). Whole-stream decompression
//! happens once under [`Limits`]; format-specific writers live in [`zip`] and
//! [`tar`]; every other format is read-only.
//!
//! # Example
//!
//! ```
//! use omp_ar::{Archive, Format, tar};
//!
//! let encoded = tar::encode([("hello.txt", b"hello".as_slice())])?;
//! let mut archive = Archive::from_bytes_with_format(&encoded, Format::Tar)?;
//! assert_eq!(archive.read("hello.txt")?, b"hello");
//! # Ok::<(), omp_ar::Error>(())
//! ```

mod archive;
pub mod arj;
pub mod asar;
pub mod cab;
mod codec;
pub mod cpio;
pub mod deb;
mod entry;
mod error;
pub mod iso;
mod links;
pub mod lzh;
mod path;
pub mod rar;
pub mod rpm;
pub mod sevenzip;
pub mod tar;
pub mod unix_ar;
pub mod zip;

pub use archive::{
	Archive, EXTENSION_TABLE, Files, Format, Limits, PathCandidate, path_candidates, unpack,
	unpack_with_format,
};
pub use entry::Entry;
pub use error::{Error, Result};
