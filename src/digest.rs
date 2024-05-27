// Copyright 2015-2019 Brian Smith.
//
// Permission to use, copy, modify, and/or distribute this software for any
// purpose with or without fee is hereby granted, provided that the above
// copyright notice and this permission notice appear in all copies.
//
// THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHORS DISCLAIM ALL WARRANTIES
// WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHORS BE LIABLE FOR ANY
// SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION
// OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN
// CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

//! SHA-2 and the legacy SHA-1 digest algorithm.
//!
//! If all the data is available in a single contiguous slice then the `digest`
//! function should be used. Otherwise, the digest can be calculated in
//! multiple steps using `Context`.

// Note on why are we doing things the hard way: It would be easy to implement
// this using the C `EVP_MD`/`EVP_MD_CTX` interface. However, if we were to do
// things that way, we'd have a hard dependency on `malloc` and other overhead.
// The goal for this implementation is to drive the overhead as close to zero
// as possible.

use self::{
    dynstate::DynState,
    sha2::{SHA256_BLOCK_LEN, SHA512_BLOCK_LEN},
};
use crate::digest::sha2::{State32, State64};
use crate::{
    bits::{BitLength, FromByteLen as _},
    cpu, debug,
    polyfill::{self, slice, sliceutil},
};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::num::Wrapping;

mod dynstate;
mod sha1;
mod sha2;

#[derive(Clone)]
pub(crate) struct BlockContext {
    state: DynState,

    // Note that SHA-512 has a 128-bit input bit counter, but this
    // implementation only supports up to 2^64-1 input bits for all algorithms,
    // so a 64-bit counter is more than sufficient.
    completed_bytes: u64,

    /// The context's algorithm.
    pub algorithm: &'static Algorithm,
}

impl BlockContext {
    pub(crate) fn new(algorithm: &'static Algorithm) -> Self {
        Self {
            state: algorithm.initial_state.clone(),
            completed_bytes: 0,
            algorithm,
        }
    }

    /// Processes all the full blocks in `input`, returning the partial block
    /// at the end, which may be empty.
    pub(crate) fn update<'i>(&mut self, input: &'i [u8], cpu_features: cpu::Features) -> &'i [u8] {
        let (completed_bytes, leftover) = self.block_data_order(input, cpu_features);
        // Using saturated addition here allows `update` to be infallible and
        // panic-free. If we were to reach the maximum value here then `finish`
        // will detect that we processed too much data when it converts this to
        // a bit length.
        self.completed_bytes = self
            .completed_bytes
            .saturating_add(polyfill::u64_from_usize(completed_bytes));
        leftover
    }

    pub(crate) fn finish(
        mut self,
        pending: &mut [u8],
        num_pending: usize,
        cpu_features: cpu::Features,
    ) -> Digest {
        let block_len = self.algorithm.block_len();
        assert_eq!(pending.len(), block_len);
        assert!(num_pending < pending.len());
        let pending = &mut pending[..block_len];

        let mut padding_pos = num_pending;
        pending[padding_pos] = 0x80;
        padding_pos += 1;

        if padding_pos > pending.len() - self.algorithm.len_len {
            pending[padding_pos..].fill(0);
            let (completed_bytes, leftover) = self.block_data_order(pending, cpu_features);
            debug_assert_eq!((completed_bytes, leftover.len()), (block_len, 0));
            // We don't increase |self.completed_bytes| because the padding
            // isn't data, and so it isn't included in the data length.
            padding_pos = 0;
        }

        pending[padding_pos..(block_len - 8)].fill(0);

        // Output the length, in bits, in big endian order.
        let completed_bytes = self
            .completed_bytes
            .checked_add(polyfill::u64_from_usize(num_pending))
            .unwrap();
        let copmleted_bits = BitLength::from_byte_len(completed_bytes).unwrap();
        pending[(block_len - 8)..].copy_from_slice(&copmleted_bits.to_be_bytes());

        let (completed_bytes, leftover) = self.block_data_order(pending, cpu_features);
        debug_assert_eq!((completed_bytes, leftover.len()), (block_len, 0));

        Digest {
            algorithm: self.algorithm,
            value: (self.algorithm.format_output)(self.state),
        }
    }

    #[must_use]
    fn block_data_order<'d>(
        &mut self,
        data: &'d [u8],
        cpu_features: cpu::Features,
    ) -> (usize, &'d [u8]) {
        (self.algorithm.block_data_order)(&mut self.state, data, cpu_features)
    }
}

/// A context for multi-step (Init-Update-Finish) digest calculations.
///
/// # Examples
///
/// ```
/// use ring::digest;
///
/// let one_shot = digest::digest(&digest::SHA384, b"hello, world");
///
/// let mut ctx = digest::Context::new(&digest::SHA384);
/// ctx.update(b"hello");
/// ctx.update(b", ");
/// ctx.update(b"world");
/// let multi_part = ctx.finish();
///
/// assert_eq!(&one_shot.as_ref(), &multi_part.as_ref());
/// ```
#[derive(Clone)]
pub struct Context {
    block: BlockContext,
    // TODO: More explicitly force 64-bit alignment for |pending|.
    pending: [u8; MAX_BLOCK_LEN],

    // Invariant: `self.num_pending < self.block.algorithm.block_len`.
    num_pending: usize,
}

/// Structure to store and restore BlockContext state
#[derive(Clone)]
pub struct ContextState {
    /// Field used to determine state enum name
    pub name: String,
    /// State data
    pub data: Vec<u64>,
}

/// Structure to store and restore Context
#[derive(Clone)]
pub struct ContextData {
    /// Context state
    pub state: ContextState,
    /// Completed bytes
    pub completed_bytes: u64,
    /// Digest algorithm name = AlgorithmID
    pub algorithm: String,
    /// Number of pending bytes
    pub num_pending: usize,
    /// Pending bytes
    pub pending: Vec<u8>,
}
impl Context {
    /// Retrieves context data from current context states
    pub fn serialize(&self) -> ContextData {
        let (state_name, state_data) = match self.block.state {
            DynState::As64(as64) => ("as64", as64.iter().map(|w| w.0).collect::<Vec<_>>()),
            DynState::As32(as32) => (
                "as32",
                as32.iter().map(|w| u64::from(w.0)).collect::<Vec<_>>(),
            ),
        };

        let algo = match self.block.algorithm.id {
            AlgorithmID::SHA1 => "SHA1",
            AlgorithmID::SHA256 => "SHA256",
            AlgorithmID::SHA384 => "SHA384",
            AlgorithmID::SHA512 => "SHA512",
            AlgorithmID::SHA512_256 => "SHA512_256",
        };

        ContextData {
            completed_bytes: self.block.completed_bytes,
            state: ContextState {
                name: state_name.to_string(),
                data: state_data,
            },
            algorithm: algo.to_string(),
            num_pending: self.num_pending,
            pending: self.pending.to_vec(),
        }
    }

    /// Create context from stored context data
    pub fn deserialize(data: ContextData) -> Self {
        let algo = match data.algorithm.as_str() {
            "SHA1" => &SHA1_FOR_LEGACY_USE_ONLY,
            "SHA256" => &SHA256,
            "SHA384" => &SHA384,
            "SHA512" => &SHA512,
            "SHA512_256" => &SHA512_256,
            _ => &SHA256,
        };

        let mut block = BlockContext::new(algo);
        block.completed_bytes = data.completed_bytes;
        block.state = match data.state.name.as_str() {
            "as64" => {
                let state: State64 = data
                    .state
                    .data
                    .iter()
                    .map(|b| Wrapping(*b))
                    .collect::<Vec<_>>()
                    .try_into()
                    .unwrap();
                DynState::As64(state)
            }
            _ => {
                let state: State32 = data
                    .state
                    .data
                    .iter()
                    .map(|b| Wrapping(u32::try_from(*b).unwrap()))
                    .collect::<Vec<_>>()
                    .try_into()
                    .unwrap();
                DynState::As32(state)
            }
        };

        Self {
            block,
            pending: data.pending.try_into().unwrap(),
            num_pending: data.num_pending,
        }
    }

    /// Constructs a new context.
    pub fn new(algorithm: &'static Algorithm) -> Self {
        Self {
            block: BlockContext::new(algorithm),
            pending: [0u8; MAX_BLOCK_LEN],
            num_pending: 0,
        }
    }

    pub(crate) fn clone_from(block: &BlockContext) -> Self {
        Self {
            block: block.clone(),
            pending: [0u8; MAX_BLOCK_LEN],
            num_pending: 0,
        }
    }

    /// Updates the digest with all the data in `data`.
    pub fn update(&mut self, data: &[u8]) {
        let cpu_features = cpu::features();

        let block_len = self.block.algorithm.block_len();
        let buffer = &mut self.pending[..block_len];

        let to_digest = if self.num_pending == 0 {
            data
        } else {
            let buffer_to_fill = match buffer.get_mut(self.num_pending..) {
                Some(buffer_to_fill) => buffer_to_fill,
                None => {
                    // Impossible because of the invariant.
                    unreachable!();
                }
            };
            sliceutil::overwrite_at_start(buffer_to_fill, data);
            match slice::split_at_checked(data, buffer_to_fill.len()) {
                Some((just_copied, to_digest)) => {
                    debug_assert_eq!(buffer_to_fill.len(), just_copied.len());
                    debug_assert_eq!(self.num_pending + just_copied.len(), block_len);
                    let leftover = self.block.update(buffer, cpu_features);
                    debug_assert_eq!(leftover.len(), 0);
                    self.num_pending = 0;
                    to_digest
                }
                None => {
                    self.num_pending += data.len();
                    // If `data` isn't enough to complete a block, buffer it and stop.
                    debug_assert!(self.num_pending < block_len);
                    return;
                }
            }
        };

        let leftover = self.block.update(to_digest, cpu_features);
        sliceutil::overwrite_at_start(buffer, leftover);
        self.num_pending = leftover.len();
        debug_assert!(self.num_pending < block_len);
    }

    /// Finalizes the digest calculation and returns the digest value.
    ///
    /// `finish` consumes the context so it cannot be (mis-)used after `finish`
    /// has been called.
    pub fn finish(mut self) -> Digest {
        let cpu_features = cpu::features();

        let block_len = self.block.algorithm.block_len();
        self.block.finish(
            &mut self.pending[..block_len],
            self.num_pending,
            cpu_features,
        )
    }

    /// The algorithm that this context is using.
    #[inline(always)]
    pub fn algorithm(&self) -> &'static Algorithm {
        self.block.algorithm
    }
}

/// Returns the digest of `data` using the given digest algorithm.
///
/// # Examples:
///
/// ```
/// # #[cfg(feature = "alloc")]
/// # {
/// use ring::{digest, test};
/// let expected_hex = "09ca7e4eaa6e8ae9c7d261167129184883644d07dfba7cbfbc4c8a2e08360d5b";
/// let expected: Vec<u8> = test::from_hex(expected_hex).unwrap();
/// let actual = digest::digest(&digest::SHA256, b"hello, world");
///
/// assert_eq!(&expected, &actual.as_ref());
/// # }
/// ```
pub fn digest(algorithm: &'static Algorithm, data: &[u8]) -> Digest {
    let mut ctx = Context::new(algorithm);
    ctx.update(data);
    ctx.finish()
}

/// A calculated digest value.
///
/// Use [`Self::as_ref`] to get the value as a `&[u8]`.
#[derive(Clone, Copy)]
pub struct Digest {
    value: Output,
    algorithm: &'static Algorithm,
}

impl Digest {
    /// The algorithm that was used to calculate the digest value.
    #[inline(always)]
    pub fn algorithm(&self) -> &'static Algorithm {
        self.algorithm
    }
}

impl AsRef<[u8]> for Digest {
    #[inline(always)]
    fn as_ref(&self) -> &[u8] {
        &self.value.0[..self.algorithm.output_len()]
    }
}

impl core::fmt::Debug for Digest {
    fn fmt(&self, fmt: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(fmt, "{:?}:", self.algorithm)?;
        debug::write_hex_bytes(fmt, self.as_ref())
    }
}

/// A digest algorithm.
pub struct Algorithm {
    output_len: OutputLen,
    chaining_len: usize,
    block_len: BlockLen,

    /// The length of the length in the padding.
    len_len: usize,

    /// `block_data_order` processes all the full blocks of data in `data`. It
    /// returns the number of bytes processed and the unprocessed data, which
    /// is guaranteed to be less than `block_len` bytes long.
    block_data_order: for<'d> fn(
        state: &mut DynState,
        data: &'d [u8],
        cpu_features: cpu::Features,
    ) -> (usize, &'d [u8]),

    format_output: fn(input: DynState) -> Output,

    initial_state: DynState,

    id: AlgorithmID,
}

#[derive(Debug, Eq, PartialEq)]
enum AlgorithmID {
    SHA1,
    SHA256,
    SHA384,
    SHA512,
    SHA512_256,
}

impl PartialEq for Algorithm {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Algorithm {}

derive_debug_via_id!(Algorithm);

impl Algorithm {
    /// The internal block length.
    pub fn block_len(&self) -> usize {
        self.block_len.into()
    }

    /// The size of the chaining value of the digest function, in bytes.
    ///
    /// For non-truncated algorithms (SHA-1, SHA-256, SHA-512), this is equal
    /// to [`Self::output_len()`]. For truncated algorithms (e.g. SHA-384,
    /// SHA-512/256), this is equal to the length before truncation. This is
    /// mostly helpful for determining the size of an HMAC key that is
    /// appropriate for the digest algorithm.
    pub fn chaining_len(&self) -> usize {
        self.chaining_len
    }

    /// The length of a finalized digest.
    pub fn output_len(&self) -> usize {
        self.output_len.into()
    }
}

/// SHA-1 as specified in [FIPS 180-4]. Deprecated.
///
/// [FIPS 180-4]: http://nvlpubs.nist.gov/nistpubs/FIPS/NIST.FIPS.180-4.pdf
pub static SHA1_FOR_LEGACY_USE_ONLY: Algorithm = Algorithm {
    output_len: sha1::OUTPUT_LEN,
    chaining_len: sha1::CHAINING_LEN,
    block_len: sha1::BLOCK_LEN,
    len_len: 64 / 8,
    block_data_order: dynstate::sha1_block_data_order,
    format_output: dynstate::sha256_format_output,
    initial_state: DynState::new32([
        Wrapping(0x67452301u32),
        Wrapping(0xefcdab89u32),
        Wrapping(0x98badcfeu32),
        Wrapping(0x10325476u32),
        Wrapping(0xc3d2e1f0u32),
        Wrapping(0),
        Wrapping(0),
        Wrapping(0),
    ]),
    id: AlgorithmID::SHA1,
};

/// SHA-256 as specified in [FIPS 180-4].
///
/// [FIPS 180-4]: http://nvlpubs.nist.gov/nistpubs/FIPS/NIST.FIPS.180-4.pdf
pub static SHA256: Algorithm = Algorithm {
    output_len: OutputLen::_256,
    chaining_len: SHA256_OUTPUT_LEN,
    block_len: SHA256_BLOCK_LEN,
    len_len: 64 / 8,
    block_data_order: dynstate::sha256_block_data_order,
    format_output: dynstate::sha256_format_output,
    initial_state: DynState::new32([
        Wrapping(0x6a09e667u32),
        Wrapping(0xbb67ae85u32),
        Wrapping(0x3c6ef372u32),
        Wrapping(0xa54ff53au32),
        Wrapping(0x510e527fu32),
        Wrapping(0x9b05688cu32),
        Wrapping(0x1f83d9abu32),
        Wrapping(0x5be0cd19u32),
    ]),
    id: AlgorithmID::SHA256,
};

/// SHA-384 as specified in [FIPS 180-4].
///
/// [FIPS 180-4]: http://nvlpubs.nist.gov/nistpubs/FIPS/NIST.FIPS.180-4.pdf
pub static SHA384: Algorithm = Algorithm {
    output_len: OutputLen::_384,
    chaining_len: SHA512_OUTPUT_LEN,
    block_len: SHA512_BLOCK_LEN,
    len_len: SHA512_LEN_LEN,
    block_data_order: dynstate::sha512_block_data_order,
    format_output: dynstate::sha512_format_output,
    initial_state: DynState::new64([
        Wrapping(0xcbbb9d5dc1059ed8),
        Wrapping(0x629a292a367cd507),
        Wrapping(0x9159015a3070dd17),
        Wrapping(0x152fecd8f70e5939),
        Wrapping(0x67332667ffc00b31),
        Wrapping(0x8eb44a8768581511),
        Wrapping(0xdb0c2e0d64f98fa7),
        Wrapping(0x47b5481dbefa4fa4),
    ]),
    id: AlgorithmID::SHA384,
};

/// SHA-512 as specified in [FIPS 180-4].
///
/// [FIPS 180-4]: http://nvlpubs.nist.gov/nistpubs/FIPS/NIST.FIPS.180-4.pdf
pub static SHA512: Algorithm = Algorithm {
    output_len: OutputLen::_512,
    chaining_len: SHA512_OUTPUT_LEN,
    block_len: SHA512_BLOCK_LEN,
    len_len: SHA512_LEN_LEN,
    block_data_order: dynstate::sha512_block_data_order,
    format_output: dynstate::sha512_format_output,
    initial_state: DynState::new64([
        Wrapping(0x6a09e667f3bcc908),
        Wrapping(0xbb67ae8584caa73b),
        Wrapping(0x3c6ef372fe94f82b),
        Wrapping(0xa54ff53a5f1d36f1),
        Wrapping(0x510e527fade682d1),
        Wrapping(0x9b05688c2b3e6c1f),
        Wrapping(0x1f83d9abfb41bd6b),
        Wrapping(0x5be0cd19137e2179),
    ]),
    id: AlgorithmID::SHA512,
};

/// SHA-512/256 as specified in [FIPS 180-4].
///
/// This is *not* the same as just truncating the output of SHA-512, as
/// SHA-512/256 has its own initial state distinct from SHA-512's initial
/// state.
///
/// [FIPS 180-4]: http://nvlpubs.nist.gov/nistpubs/FIPS/NIST.FIPS.180-4.pdf
pub static SHA512_256: Algorithm = Algorithm {
    output_len: OutputLen::_256,
    chaining_len: SHA512_OUTPUT_LEN,
    block_len: SHA512_BLOCK_LEN,
    len_len: SHA512_LEN_LEN,
    block_data_order: dynstate::sha512_block_data_order,
    format_output: dynstate::sha512_format_output,
    initial_state: DynState::new64([
        Wrapping(0x22312194fc2bf72c),
        Wrapping(0x9f555fa3c84c64c2),
        Wrapping(0x2393b86b6f53b151),
        Wrapping(0x963877195940eabd),
        Wrapping(0x96283ee2a88effe3),
        Wrapping(0xbe5e1e2553863992),
        Wrapping(0x2b0199fc2c85b8aa),
        Wrapping(0x0eb72ddc81c52ca2),
    ]),
    id: AlgorithmID::SHA512_256,
};

#[derive(Clone, Copy)]
struct Output([u8; MAX_OUTPUT_LEN]);

/// The maximum block length ([`Algorithm::block_len()`]) of all the algorithms
/// in this module.
pub const MAX_BLOCK_LEN: usize = BlockLen::MAX.into();

/// The maximum output length ([`Algorithm::output_len()`]) of all the
/// algorithms in this module.
pub const MAX_OUTPUT_LEN: usize = OutputLen::MAX.into();

/// The maximum chaining length ([`Algorithm::chaining_len()`]) of all the
/// algorithms in this module.
pub const MAX_CHAINING_LEN: usize = MAX_OUTPUT_LEN;

#[inline]
fn format_output<T, F, const N: usize>(input: [Wrapping<T>; sha2::CHAINING_WORDS], f: F) -> Output
where
    F: Fn(T) -> [u8; N],
    T: Copy,
{
    let mut output = Output([0; MAX_OUTPUT_LEN]);
    output
        .0
        .chunks_mut(N)
        .zip(input.iter().copied().map(|Wrapping(w)| f(w)))
        .for_each(|(o, i)| {
            o.copy_from_slice(&i);
        });
    output
}

/// The length of the output of SHA-1, in bytes.
pub const SHA1_OUTPUT_LEN: usize = sha1::OUTPUT_LEN.into();

/// The length of the output of SHA-256, in bytes.
pub const SHA256_OUTPUT_LEN: usize = OutputLen::_256.into();

/// The length of the output of SHA-384, in bytes.
pub const SHA384_OUTPUT_LEN: usize = OutputLen::_384.into();

/// The length of the output of SHA-512, in bytes.
pub const SHA512_OUTPUT_LEN: usize = OutputLen::_512.into();

/// The length of the output of SHA-512/256, in bytes.
pub const SHA512_256_OUTPUT_LEN: usize = OutputLen::_256.into();

/// The length of the length field for SHA-512-based algorithms, in bytes.
const SHA512_LEN_LEN: usize = 128 / 8;

#[derive(Clone, Copy)]
enum BlockLen {
    _512 = 512 / 8,
    _1024 = 1024 / 8, // MAX
}

impl BlockLen {
    const MAX: Self = Self::_1024;
    #[inline(always)]
    const fn into(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy)]
enum OutputLen {
    _160 = 160 / 8,
    _256 = 256 / 8,
    _384 = 384 / 8,
    _512 = 512 / 8, // MAX
}

impl OutputLen {
    const MAX: Self = Self::_512;

    #[inline(always)]
    const fn into(self) -> usize {
        self as usize
    }
}

#[cfg(test)]
mod tests {
    mod store_restore_context {}

    mod max_input {
        extern crate alloc;
        use super::super::super::digest;
        use crate::polyfill::u64_from_usize;
        use alloc::vec;

        macro_rules! max_input_tests {
            ( $algorithm_name:ident ) => {
                mod $algorithm_name {
                    use super::super::super::super::digest;

                    #[test]
                    fn max_input_test() {
                        super::max_input_test(&digest::$algorithm_name);
                    }

                    #[test]
                    #[should_panic]
                    fn too_long_input_test_block() {
                        super::too_long_input_test_block(&digest::$algorithm_name);
                    }

                    #[test]
                    #[should_panic]
                    fn too_long_input_test_byte() {
                        super::too_long_input_test_byte(&digest::$algorithm_name);
                    }
                }
            };
        }

        fn max_input_test(alg: &'static digest::Algorithm) {
            let mut context = nearly_full_context(alg);
            let next_input = vec![0u8; alg.block_len() - 1];
            context.update(&next_input);
            let _ = context.finish(); // no panic
        }

        fn too_long_input_test_block(alg: &'static digest::Algorithm) {
            let mut context = nearly_full_context(alg);
            let next_input = vec![0u8; alg.block_len()];
            context.update(&next_input);
            let _ = context.finish(); // should panic
        }

        fn too_long_input_test_byte(alg: &'static digest::Algorithm) {
            let mut context = nearly_full_context(alg);
            let next_input = vec![0u8; alg.block_len() - 1];
            context.update(&next_input);
            context.update(&[0]);
            let _ = context.finish(); // should panic
        }

        fn nearly_full_context(alg: &'static digest::Algorithm) -> digest::Context {
            // All implementations currently support up to 2^64-1 bits
            // of input; according to the spec, SHA-384 and SHA-512
            // support up to 2^128-1, but that's not implemented yet.
            let max_bytes = 1u64 << (64 - 3);
            let max_blocks = max_bytes / u64_from_usize(alg.block_len());
            let completed_bytes = (max_blocks - 1) * u64_from_usize(alg.block_len());
            digest::Context {
                block: digest::BlockContext {
                    state: alg.initial_state.clone(),
                    completed_bytes,
                    algorithm: alg,
                },
                pending: [0u8; digest::MAX_BLOCK_LEN],
                num_pending: 0,
            }
        }

        max_input_tests!(SHA1_FOR_LEGACY_USE_ONLY);
        max_input_tests!(SHA256);
        max_input_tests!(SHA384);
        max_input_tests!(SHA512);
    }
}
