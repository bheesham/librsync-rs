//! librsync bindings for Rust.
//!
//! This library contains bindings to librsync [1], encapsulating the algorithms of the rsync
//! protocol, which computes differences between files efficiently.
//!
//! The rsync protocol, when computes differences, does not require the presence of both files.
//! It needs instead the new file and a set of checksums of the first file (the signature).
//! Computed differences can be stored in a delta file. The rsync protocol is then able to
//! reproduce the new file, by having the old one and the delta.
//!
//! [1]: http://librsync.sourcefrog.net/
//!
//!
//! # Overview of types and modules
//!
//! This crate provides the streaming operations to produce signatures, delta and patches in the
//! top-level module with `Signature`, `Delta` and `Patch` structs. Those structs take some input
//! stream (`Read` or `Read + Seek` traits) and implement another stream (`Read` trait) from which
//! the output can be read.
//!
//! Higher level operations are provided within the `whole` submodule. If the application does not
//! need fine-grained control over IO operations, `signature`, `delta` and `patch` functions can be
//! used. Those functions apply the results to an output stream (implementing the `Write` trait)
//! in a single call.
//!
//!
//! # Example: streams
//!
//! This example shows how to go trough the streaming APIs, starting from an input string and a
//! modified string which act as old and new files. The example simulates a real world scenario, in
//! which the signature of a base file is computed, used as input to compute differencies between
//! the base file and the new one, and finally the new file is reconstructed, by using the base
//! file and the delta.
//!
//! ```rust
//! use std::io::prelude::*;
//! use std::io::Cursor;
//! use librsync::{Delta, Patch, Signature};
//!
//! let base = "base file".as_bytes();
//! let new = "modified base file".as_bytes();
//!
//! // create signature starting from base file
//! let mut sig = Signature::new(base).unwrap();
//! // create delta from new file and the base signature
//! let delta = Delta::new(new, &mut sig).unwrap();
//! // create and store the new file from the base one and the delta
//! let mut patch = Patch::new(Cursor::new(base), delta).unwrap();
//! let mut computed_new = Vec::new();
//! patch.read_to_end(&mut computed_new).unwrap();
//!
//! // test whether the computed file is exactly the new file, as expected
//! assert_eq!(computed_new, new);
//! ```
//!
//! Note that intermediate results are not stored in temporary containers. This is possible because
//! the operations implement the `Read` trait. In this way the results does not need to be fully in
//! memory, during computation.
//!
//!
//! # Example: whole file API
//!
//! This example shows how to go trough the whole file APIs, starting from an input string and a
//! modified string which act as old and new files. Unlike the streaming example, here we call a
//! single function, to get the computation result of signature, delta and patch operations. This
//! is convenient when an output stream (like a network socket or a file) is used as output for an
//! operation.
//!
//! ```rust
//! use std::io::Cursor;
//! use librsync::whole::*;
//!
//! let base = "base file".as_bytes();
//! let new = "modified base file".as_bytes();
//!
//! // signature
//! let mut sig = Vec::new();
//! signature(&mut Cursor::new(base), &mut sig).unwrap();
//!
//! // delta
//! let mut dlt = Vec::new();
//! delta(&mut Cursor::new(new), &mut Cursor::new(sig), &mut dlt).unwrap();
//!
//! // patch
//! let mut out = Vec::new();
//! patch(&mut Cursor::new(base), &mut Cursor::new(dlt), &mut out).unwrap();
//!
//! assert_eq!(out, new);
//! ```

#![deny(missing_copy_implementations,
        missing_docs,
        trivial_casts, trivial_numeric_casts,
        unstable_features,
        unused_import_braces, unused_qualifications)]

#![cfg_attr(feature = "nightly", allow(unstable_features))]
#![cfg_attr(feature = "lints", feature(plugin))]
#![cfg_attr(feature = "lints", plugin(clippy))]

extern crate librsync_sys as raw;
extern crate libc;
#[cfg(feature = "log")]
#[macro_use]
extern crate log;

mod macros;
mod job;
mod logfwd;
mod unstrait;
pub mod whole;

use job::{Job, JobDriver};

use std::error;
use std::fmt::{self, Display, Formatter};
use std::io::{self, BufReader, Read, Seek};
use std::mem;
use std::ops::Deref;
use std::ptr;
use std::slice;


/// The signature type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignatureType {
    /// A signature file with MD4 signatures.
    ///
    /// Backward compatible with librsync < 1.0, but deprecated because of a security
    /// vulnerability.
    MD4,
    /// A signature file using BLAKE2 hash.
    Blake2,
}

/// Enumeration of all possible errors in this crate.
#[derive(Debug)]
pub enum Error {
    /// An IO error.
    Io(io::Error),
    /// Out of memory.
    Mem,
    /// Bad magic number at start of stream.
    BadMagic,
    /// The feature is not available yet.
    Unimplemented,
    /// Probably a library bug.
    Internal,
    /// All the other error numbers.
    ///
    /// This error should never occur, as it is an indication of a bug.
    Unknown(i32),
}

/// A `Result` type alias for this crate's `Error` type.
pub type Result<T> = std::result::Result<T, Error>;

/// A struct to generate a signature.
///
/// This type takes a `Read` stream for the input from which compute the signatures, and implements
/// another `Read` stream from which get the result.
pub struct Signature<R> {
    driver: JobDriver<BufReader<R>>,
}

/// A struct to generate a delta between two files.
///
/// This type takes two `Read` streams, one for the signature of the base file and one for the new
/// file. It then provides another `Read` stream from which get the result.
pub struct Delta<R> {
    driver: JobDriver<BufReader<R>>,
    _sumset: Sumset,
}

/// A struct to apply a delta to a basis file, to recreate the new file.
///
/// This type takes a `Read + Seek` stream for the base file, and a `Read` stream for the delta
/// file. It then provides another `Read` stream from which get the resulting patched file.
pub struct Patch<'a, B: 'a, D> {
    driver: JobDriver<BufReader<D>>,
    base: Box<B>,
    _raw: Box<UnsReadAndSeek<'a>>,
}


struct Sumset(*mut raw::rs_signature_t);

// Ok, here be dragons. I have two opposed needs for `Patch` struct:
// * take an input stream preserving its type, to be used by `into_inner`;
// * provide a `Read + Seek` trait object to `patch_copy_cb`, since C callbacks cannot use generic
//   parameters.
//
// So, what have I done? I box the stream with its concrete type for the first requirement, and I
// use an unsafe trait object to that type. This pointer can't exist in safe Rust, since its
// lifetime paramter is not expressible (a struct with a field pointing to another field). I then
// pass a pointer to that fat pointer to the C callback, which can safely unwrap it and get the
// needed `Read + Seek` trait. Boxing both fields is important, because moving a `Patch` object
// will change the addresses of its fields. However their content will not move, since it is on the
// heap.
type UnsReadAndSeek<'a> = unstrait::UnsafeTraitObject<ReadAndSeek + 'a>;

// workaround for E0225
trait ReadAndSeek: Read + Seek {}
impl<T: Read + Seek> ReadAndSeek for T {}


impl<R: Read> Signature<R> {
    /// Creates a new signature stream with default parameters.
    ///
    /// This constructor takes an input stream for the file from which compute the signatures.
    /// Default options are used for the signature format: BLAKE2 for the hashing, 2048 bytes for
    /// the block length and full length for the strong signature size.
    pub fn new(input: R) -> Result<Self> {
        Self::with_options(input, raw::RS_DEFAULT_BLOCK_LEN, 0, SignatureType::Blake2)
    }

    /// Creates a new signature stream by specifying custom parameters.
    ///
    /// This constructor takes the input stream for the file from which compute the signatures, the
    /// size of checksum blocks as `block_len` parameter (larger values make the signature shorter
    /// and the delta longer), and the size of strong signatures in bytes as `strong_len`
    /// parameter. If it is non-zero the signature will be truncated to that amount of bytes.
    /// The last parameter specifies which version of the signature format to be used.
    pub fn with_options(input: R,
                        block_len: usize,
                        strong_len: usize,
                        sig_magic: SignatureType)
                        -> Result<Self> {
        Self::with_buf_reader(BufReader::new(input), block_len, strong_len, sig_magic)
    }

    /// Creates a new signature stream by using a `BufReader`.
    ///
    /// This constructor takes an already built `BufReader` instance. Prefer this constructor if
    /// you already have a `BufReader` as input stream, since it avoids wrapping the input stream
    /// into another `BufReader` instance. See `with_options` constructor for details on the other
    /// parameters.
    pub fn with_buf_reader(input: BufReader<R>,
                           block_len: usize,
                           strong_len: usize,
                           sig_magic: SignatureType)
                           -> Result<Self> {
        logfwd::init();
        let job = unsafe { raw::rs_sig_begin(block_len, strong_len, sig_magic.as_raw()) };
        if job.is_null() {
            return Err(Error::BadMagic);
        }
        Ok(Signature { driver: JobDriver::new(input, Job(job)) })
    }

    /// Unwraps this stream, returning the underlying input stream.
    pub fn into_inner(self) -> R {
        self.driver.into_inner().into_inner()
    }
}

impl<R: Read> Read for Signature<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.driver.read(buf)
    }
}


impl<R: Read> Delta<R> {
    /// Creates a new delta stream.
    ///
    /// This constructor takes two `Read` streams for the new file (`new` parameter) and for the
    /// signatures of the base file (`base_sig` parameter). It produces a delta stream from which
    /// read the resulting delta file.
    pub fn new<S: Read + ?Sized>(new: R, base_sig: &mut S) -> Result<Self> {
        Self::with_buf_reader(BufReader::new(new), base_sig)
    }

    /// Creates a new delta stream by using a `BufReader` as new file.
    ///
    /// This constructor specializes the `new` constructor by taking a `BufReader` instance as
    /// `new` parameter. Prefer this constructor if you already have a `BufReader` as input stream,
    /// since it avoids wrapping the input stream into another `BufReader` instance. See `new`
    /// constructor for more details on the parameters.
    pub fn with_buf_reader<S: Read + ?Sized>(new: BufReader<R>, base_sig: &mut S) -> Result<Self> {
        logfwd::init();
        // load the signature
        let sumset = unsafe {
            let mut sumset = ptr::null_mut();
            let job = raw::rs_loadsig_begin(&mut sumset);
            assert!(!job.is_null());
            let mut job = JobDriver::new(BufReader::new(base_sig), Job(job));
            try!(job.consume_input());
            let sumset = Sumset(sumset);
            let res = raw::rs_build_hash_table(*sumset);
            if res != raw::RS_DONE {
                return Err(Error::from(res));
            }
            sumset
        };
        let job = unsafe { raw::rs_delta_begin(*sumset) };
        if job.is_null() {
            return Err(io_err(io::ErrorKind::InvalidData, "invalid signature given"));
        }
        Ok(Delta {
            driver: JobDriver::new(new, Job(job)),
            _sumset: sumset,
        })
    }

    /// Unwraps this stream, returning the underlying new file stream.
    pub fn into_inner(self) -> R {
        self.driver.into_inner().into_inner()
    }
}

impl<R: Read> Read for Delta<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.driver.read(buf)
    }
}


impl<'a, B: Read + Seek + 'a, D: Read> Patch<'a, B, D> {
    /// Creates a new patch stream.
    ///
    /// This constructor takes a `Read + Seek` stream for the basis file (`base` parameter), and a
    /// `Read` stream for the delta file (`delta` parameter). It produces a stream from which read
    /// the resulting patched file.
    pub fn new(base: B, delta: D) -> Result<Self> {
        Self::with_buf_reader(base, BufReader::new(delta))
    }

    /// Creates a new patch stream by using a `BufReader` as delta stream.
    ///
    /// This constructor specializes the `new` constructor by taking a `BufReader` instance as
    /// `delta` parameter. Prefer this constructor if you already have a `BufReader` as input
    /// stream, since it avoids wrapping the input stream into another `BufReader` instance. See
    /// `new` constructor for more details on the parameters.
    pub fn with_buf_reader(base: B, delta: BufReader<D>) -> Result<Self> {
        logfwd::init();

        let base = Box::new(base);
        let cb_data = Box::new(UnsReadAndSeek::new(&*base));
        let job = unsafe {
            let data = mem::transmute(&*cb_data);
            raw::rs_patch_begin(patch_copy_cb, data)
        };
        assert!(!job.is_null());
        Ok(Patch {
            driver: JobDriver::new(delta, Job(job)),
            base: base,
            _raw: cb_data,
        })
    }

    /// Unwraps this stream and returns the underlying streams.
    pub fn into_inner(self) -> (B, D) {
        (*self.base, self.driver.into_inner().into_inner())
    }
}

impl<'a, B, D: Read> Read for Patch<'a, B, D> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.driver.read(buf)
    }
}


impl error::Error for Error {
    fn description(&self) -> &str {
        match *self {
            Error::Io(ref err) => err.description(),
            Error::Mem => "out of memory",
            Error::BadMagic => "bad magic number given",
            Error::Unimplemented => "unimplemented feature",
            Error::Internal => "internal error",
            Error::Unknown(_) => "unknown error from librsync",
        }
    }
}

impl Display for Error {
    fn fmt(&self, fmt: &mut Formatter) -> fmt::Result {
        match *self {
            Error::Io(ref e) => write!(fmt, "{}", e),
            Error::Unknown(n) => write!(fmt, "unknown error {} from native library", n),
            _ => write!(fmt, "{}", std::error::Error::description(self)),
        }
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Error {
        Error::Io(err)
    }
}

impl From<raw::rs_result> for Error {
    fn from(err: raw::rs_result) -> Error {
        match err {
            raw::RS_BLOCKED => io_err(io::ErrorKind::WouldBlock, "blocked waiting for more data"),
            raw::RS_IO_ERROR => io_err(io::ErrorKind::Other, "unknown IO error from librsync"),
            raw::RS_MEM_ERROR => Error::Mem,
            raw::RS_INPUT_ENDED => {
                io_err(io::ErrorKind::UnexpectedEof, "unexpected end of input file")
            }
            raw::RS_BAD_MAGIC => Error::BadMagic,
            raw::RS_UNIMPLEMENTED => Error::Unimplemented,
            raw::RS_CORRUPT => io_err(io::ErrorKind::InvalidData, "unbelievable value in stream"),
            raw::RS_INTERNAL_ERROR => Error::Internal,
            raw::RS_PARAM_ERROR => io_err(io::ErrorKind::InvalidInput, "bad parameter"),
            n => Error::Unknown(n),
        }
    }
}


impl SignatureType {
    fn as_raw(&self) -> raw::rs_magic_number {
        match *self {
            SignatureType::MD4 => raw::RS_MD4_SIG_MAGIC,
            SignatureType::Blake2 => raw::RS_BLAKE2_SIG_MAGIC,
        }
    }
}


impl Drop for Sumset {
    fn drop(&mut self) {
        unsafe {
            if !self.0.is_null() {
                raw::rs_free_sumset(self.0);
            }
        }
    }
}

impl Deref for Sumset {
    type Target = *mut raw::rs_signature_t;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}


extern "C" fn patch_copy_cb(opaque: *mut libc::c_void,
                            pos: raw::rs_long_t,
                            len: *mut libc::size_t,
                            buf: *mut *mut libc::c_void)
                            -> raw::rs_result {
    let input = unsafe {
        let h: *mut UnsReadAndSeek = mem::transmute(opaque);
        (*h).as_inner_mut()
    };
    let output = unsafe {
        let buf: *mut u8 = mem::transmute(*buf);
        slice::from_raw_parts_mut(buf, *len)
    };
    try_or_rs_error!(input.seek(io::SeekFrom::Start(pos as u64)));
    try_or_rs_error!(input.read(output));
    raw::RS_DONE
}


fn io_err<E>(kind: io::ErrorKind, e: E) -> Error
    where E: Into<Box<error::Error + Send + Sync>>
{
    Error::Io(io::Error::new(kind, e))
}


#[cfg(test)]
mod test {
    use super::*;
    use std::io::{Cursor, Read};

    const DATA: &'static str = "this is a string to be tested";
    const DATA2: &'static str = "this is another string to be tested";

    // generated with `rdiff signature -b 10 -S 5 data data.sig`
    fn data_signature() -> Vec<u8> {
        vec![0x72, 0x73, 0x01, 0x36, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x00, 0x00, 0x05, 0x1b, 0x21,
             0x04, 0x8b, 0xad, 0x3c, 0xbd, 0x19, 0x09, 0x1d, 0x1b, 0x04, 0xf0, 0x9d, 0x1f, 0x64,
             0x31, 0xde, 0x15, 0xf4, 0x04, 0x87, 0x60, 0x96, 0x19, 0x50, 0x39]
    }

    // generated with `rdiff delta data.sig data2 data2.delta`
    fn data2_delta() -> Vec<u8> {
        vec![0x72, 0x73, 0x02, 0x36, 0x41, 0x10, 0x74, 0x68, 0x69, 0x73, 0x20, 0x69, 0x73, 0x20,
             0x61, 0x6e, 0x6f, 0x74, 0x68, 0x65, 0x72, 0x20, 0x45, 0x0a, 0x13, 0x00]
    }


    #[test]
    fn signature() {
        let cursor = Cursor::new(DATA);
        let mut sig = Signature::with_options(cursor, 10, 5, SignatureType::MD4).unwrap();
        let mut signature = Vec::new();
        let read = sig.read_to_end(&mut signature).unwrap();
        assert_eq!(read, signature.len());
        assert_eq!(signature, data_signature());
    }

    #[test]
    fn delta() {
        let sig = data_signature();
        let input = Cursor::new(DATA2);
        let mut job = Delta::new(input, &mut Cursor::new(sig)).unwrap();
        let mut delta = Vec::new();
        let read = job.read_to_end(&mut delta).unwrap();
        assert_eq!(read, delta.len());
        assert_eq!(delta, data2_delta());
    }

    #[test]
    fn patch() {
        let base = Cursor::new(DATA);
        let delta = data2_delta();
        let delta = Cursor::new(delta);
        let mut patch = Patch::new(base, delta).unwrap();
        let mut computed_new = String::new();
        patch.read_to_string(&mut computed_new).unwrap();
        assert_eq!(computed_new, DATA2);
    }

    #[test]
    fn integration() {
        let base = Cursor::new(DATA);
        let new = Cursor::new(DATA2);
        let mut sig = Signature::with_options(base, 10, 5, SignatureType::MD4).unwrap();
        let delta = Delta::new(new, &mut sig).unwrap();
        let base = Cursor::new(DATA);
        let mut patch = Patch::new(base, delta).unwrap();
        let mut computed_new = String::new();
        patch.read_to_string(&mut computed_new).unwrap();
        assert_eq!(computed_new, DATA2);
    }
}
