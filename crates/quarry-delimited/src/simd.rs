//! 64-byte SIMD structural block classifier and carryless prefix-XOR helpers.
//!
//! Supported native backends on Rust 1.88+:
//! - `SimdBackend::Neon` on AArch64 (baseline NEON + runtime-detected `pmull` carryless multiply).
//! - `SimdBackend::Avx2` on x86_64 (runtime-detected AVX2 + PCLMULQDQ).
//! - `SimdBackend::Avx512` reserved for runtime-detected AVX-512BW + VPCLMULQDQ once MSRV
//!   enables stable `avx512` target features.
//! - `SimdBackend::Scalar` fallback for unsupported targets, tail bytes, UTF-16 shadow scans,
//!   and deterministic scalar-vs-SIMD attribution benchmarks.

use std::fmt;

use crate::dialect::{EscapeRule, ScannerDialect};

/// Active or requested SIMD backend for 64-byte structural classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SimdBackend {
    Scalar,
    Neon,
    Avx2,
    Avx512,
}

impl SimdBackend {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scalar => "scalar",
            Self::Neon => "neon",
            Self::Avx2 => "avx2",
            Self::Avx512 => "avx512",
        }
    }

    /// Returns whether this backend can execute natively on the current CPU.
    pub fn is_supported_on_current_cpu(self) -> bool {
        match self {
            Self::Scalar => true,
            Self::Neon => {
                #[cfg(target_arch = "aarch64")]
                {
                    true
                }
                #[cfg(not(target_arch = "aarch64"))]
                {
                    false
                }
            }
            Self::Avx2 => {
                #[cfg(target_arch = "x86_64")]
                {
                    std::arch::is_x86_feature_detected!("avx2")
                        && std::arch::is_x86_feature_detected!("pclmulqdq")
                }
                #[cfg(not(target_arch = "x86_64"))]
                {
                    false
                }
            }
            Self::Avx512 => false,
        }
    }
}

impl fmt::Display for SimdBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Detect the fastest supported SIMD backend for the current CPU at runtime.
pub fn active_simd_backend() -> SimdBackend {
    #[cfg(target_arch = "aarch64")]
    {
        SimdBackend::Neon
    }
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2")
            && std::arch::is_x86_feature_detected!("pclmulqdq")
        {
            SimdBackend::Avx2
        } else {
            SimdBackend::Scalar
        }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        SimdBackend::Scalar
    }
}

/// 64-byte structural classification bitmasks (bit `i` corresponds to byte `i`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct BlockMasks {
    pub delim: u64,
    pub quote: u64,
    pub lf: u64,
    pub cr: u64,
    pub escape: u64,
}

/// Classify a 64-byte block using the chosen SIMD backend.
///
/// When `need_delim` is false and `quote` is `None`, callers can still read `lf` cleanly.
/// `classify_block_64` classifies all structural characters required by `dialect`.
#[inline]
pub(crate) fn classify_block_64(
    block: &[u8; 64],
    dialect: ScannerDialect,
    backend: SimdBackend,
    need_delim: bool,
) -> BlockMasks {
    match backend {
        #[cfg(target_arch = "aarch64")]
        SimdBackend::Neon => {
            // SAFETY: NEON is mandatory on all AArch64 targets.
            unsafe { classify_block_neon(block, dialect, need_delim) }
        }
        #[cfg(target_arch = "x86_64")]
        SimdBackend::Avx2 => {
            // SAFETY: Caller only selects `Avx2` after verifying `is_supported_on_current_cpu()`.
            unsafe { classify_block_avx2(block, dialect, need_delim) }
        }
        _ => classify_block_scalar(block, dialect, need_delim),
    }
}

/// Compute cumulative prefix XOR over a 64-bit mask (`out[i] = XOR_{j <= i} mask[j]`).
#[inline]
pub(crate) fn prefix_xor(mask: u64, backend: SimdBackend) -> u64 {
    if mask == 0 {
        return 0;
    }
    match backend {
        #[cfg(target_arch = "aarch64")]
        SimdBackend::Neon => {
            if std::arch::is_aarch64_feature_detected!("pmull") {
                // SAFETY: Runtime feature check confirmed `pmull` / `aes` availability.
                unsafe { prefix_xor_neon_pmull(mask) }
            } else {
                prefix_xor_swar(mask)
            }
        }
        #[cfg(target_arch = "x86_64")]
        SimdBackend::Avx2 => {
            // SAFETY: `Avx2` backend is only active when both `avx2` and `pclmulqdq` are present.
            unsafe { prefix_xor_x86_pclmul(mask) }
        }
        _ => prefix_xor_swar(mask),
    }
}

#[inline]
pub(crate) fn prefix_xor_swar(mut x: u64) -> u64 {
    x ^= x << 1;
    x ^= x << 2;
    x ^= x << 4;
    x ^= x << 8;
    x ^= x << 16;
    x ^= x << 32;
    x
}

/// Compute `(escaped_mask, next_block_escaped)` using odd-length escape-run detection.
///
/// Bit `i` of `escaped_mask` is set iff byte `i` is escaped by an immediately preceding odd
/// run of escape bytes (accounting for `prev_escaped` carried from bit 63 of the prior block).
#[inline]
pub(crate) fn compute_escaped_mask(escape_mask: u64, prev_escaped: bool) -> (u64, bool) {
    const EVEN_BITS: u64 = 0x5555_5555_5555_5555;
    const ODD_BITS: u64 = 0xAAAA_AAAA_AAAA_AAAA;

    let prev_bit = u64::from(prev_escaped);
    if escape_mask == 0 {
        return (prev_bit, false);
    }

    // If bit 0 is itself escaped by the previous block, it cannot start an escape sequence.
    let effective_esc = escape_mask & !prev_bit;
    let starts = effective_esc & !(effective_esc << 1);
    let even_starts = starts & EVEN_BITS;
    let odd_starts = starts & ODD_BITS;

    let (even_carries, _) = effective_esc.overflowing_add(even_starts);
    let (odd_carries, odd_overflow) = effective_esc.overflowing_add(odd_starts);

    let even_spans = (even_carries ^ effective_esc) & ODD_BITS;
    let odd_spans = (odd_carries ^ effective_esc) & EVEN_BITS;

    (even_spans | odd_spans | prev_bit, odd_overflow)
}

#[inline]
fn classify_block_scalar(
    block: &[u8; 64],
    dialect: ScannerDialect,
    need_delim: bool,
) -> BlockMasks {
    let mut masks = BlockMasks::default();
    let esc_byte = match dialect.escape {
        EscapeRule::DoubledQuote => None,
        EscapeRule::EscapeByte(b) => Some(b),
    };
    for (i, &byte) in block.iter().enumerate() {
        let bit = 1u64 << i;
        if byte == b'\n' {
            masks.lf |= bit;
        }
        if dialect.quote == Some(byte) {
            masks.quote |= bit;
        }
        if dialect.quote.is_some() && byte == b'\r' {
            masks.cr |= bit;
        }
        if esc_byte == Some(byte) {
            masks.escape |= bit;
        }
    }
    if need_delim || masks.quote != 0 {
        for (i, &byte) in block.iter().enumerate() {
            if byte == dialect.delimiter {
                masks.delim |= 1u64 << i;
            }
        }
    }
    masks
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon,aes")]
#[inline]
unsafe fn prefix_xor_neon_pmull(mask: u64) -> u64 {
    core::arch::aarch64::vmull_p64(mask, !0u64) as u64
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[inline]
unsafe fn neon_movemask_64(
    b0: core::arch::aarch64::uint8x16_t,
    b1: core::arch::aarch64::uint8x16_t,
    b2: core::arch::aarch64::uint8x16_t,
    b3: core::arch::aarch64::uint8x16_t,
    bit_mask: core::arch::aarch64::uint8x16_t,
) -> u64 {
    use core::arch::aarch64::*;
    let t0 = vandq_u8(b0, bit_mask);
    let t1 = vandq_u8(b1, bit_mask);
    let t2 = vandq_u8(b2, bit_mask);
    let t3 = vandq_u8(b3, bit_mask);
    let p0 = vpaddq_u8(t0, t1);
    let p1 = vpaddq_u8(t2, t3);
    let q = vpaddq_u8(p0, p1);
    let r = vpaddq_u8(q, q);
    vgetq_lane_u64(vreinterpretq_u64_u8(r), 0)
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn classify_block_neon(
    block: &[u8; 64],
    dialect: ScannerDialect,
    need_delim: bool,
) -> BlockMasks {
    use core::arch::aarch64::*;
    const BIT_WEIGHTS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];

    // SAFETY: `block` is a 64-byte array, so offsets 0, 16, 32, 48 are valid 16-byte loads;
    // `BIT_WEIGHTS` is 16 bytes; NEON target feature is enabled by this function's attribute.
    unsafe {
        let ptr = block.as_ptr();
        let v0 = vld1q_u8(ptr);
        let v1 = vld1q_u8(ptr.add(16));
        let v2 = vld1q_u8(ptr.add(32));
        let v3 = vld1q_u8(ptr.add(48));
        let bit_mask = vld1q_u8(BIT_WEIGHTS.as_ptr());

        let v_lf = vdupq_n_u8(b'\n');
        let lf = neon_movemask_64(
            vceqq_u8(v0, v_lf),
            vceqq_u8(v1, v_lf),
            vceqq_u8(v2, v_lf),
            vceqq_u8(v3, v_lf),
            bit_mask,
        );

        let quote = if let Some(q) = dialect.quote {
            let v_q = vdupq_n_u8(q);
            neon_movemask_64(
                vceqq_u8(v0, v_q),
                vceqq_u8(v1, v_q),
                vceqq_u8(v2, v_q),
                vceqq_u8(v3, v_q),
                bit_mask,
            )
        } else {
            0
        };

        let delim = if need_delim || quote != 0 {
            let v_d = vdupq_n_u8(dialect.delimiter);
            neon_movemask_64(
                vceqq_u8(v0, v_d),
                vceqq_u8(v1, v_d),
                vceqq_u8(v2, v_d),
                vceqq_u8(v3, v_d),
                bit_mask,
            )
        } else {
            0
        };

        let cr = if dialect.quote.is_some() {
            let v_cr = vdupq_n_u8(b'\r');
            neon_movemask_64(
                vceqq_u8(v0, v_cr),
                vceqq_u8(v1, v_cr),
                vceqq_u8(v2, v_cr),
                vceqq_u8(v3, v_cr),
                bit_mask,
            )
        } else {
            0
        };

        let escape = match dialect.escape {
            EscapeRule::DoubledQuote => 0,
            EscapeRule::EscapeByte(e) => {
                let v_e = vdupq_n_u8(e);
                neon_movemask_64(
                    vceqq_u8(v0, v_e),
                    vceqq_u8(v1, v_e),
                    vceqq_u8(v2, v_e),
                    vceqq_u8(v3, v_e),
                    bit_mask,
                )
            }
        };

        BlockMasks {
            delim,
            quote,
            lf,
            cr,
            escape,
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "pclmulqdq")]
#[inline]
unsafe fn prefix_xor_x86_pclmul(mask: u64) -> u64 {
    use core::arch::x86_64::*;
    let v = _mm_set_epi64x(0, mask as i64);
    let ones = _mm_set1_epi8(-1);
    _mm_cvtsi128_si64(_mm_clmulepi64_si128(v, ones, 0x00)) as u64
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn avx2_movemask_64(
    lo: core::arch::x86_64::__m256i,
    hi: core::arch::x86_64::__m256i,
    needle: u8,
) -> u64 {
    use core::arch::x86_64::*;
    let v_needle = _mm256_set1_epi8(needle as i8);
    let m_lo = _mm256_movemask_epi8(_mm256_cmpeq_epi8(lo, v_needle)) as u32 as u64;
    let m_hi = _mm256_movemask_epi8(_mm256_cmpeq_epi8(hi, v_needle)) as u32 as u64;
    m_lo | (m_hi << 32)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn classify_block_avx2(
    block: &[u8; 64],
    dialect: ScannerDialect,
    need_delim: bool,
) -> BlockMasks {
    use core::arch::x86_64::*;
    // SAFETY: `block` is a 64-byte array, so unaligned 32-byte loads at offsets 0 and 32 are valid;
    // AVX2 target feature is enabled by this function's attribute.
    unsafe {
        let ptr = block.as_ptr();
        let lo = _mm256_loadu_si256(ptr as *const __m256i);
        let hi = _mm256_loadu_si256(ptr.add(32) as *const __m256i);

        let lf = avx2_movemask_64(lo, hi, b'\n');
        let quote = match dialect.quote {
            Some(q) => avx2_movemask_64(lo, hi, q),
            None => 0,
        };
        let delim = if need_delim || quote != 0 {
            avx2_movemask_64(lo, hi, dialect.delimiter)
        } else {
            0
        };
        let cr = if dialect.quote.is_some() {
            avx2_movemask_64(lo, hi, b'\r')
        } else {
            0
        };
        let escape = match dialect.escape {
            EscapeRule::DoubledQuote => 0,
            EscapeRule::EscapeByte(e) => avx2_movemask_64(lo, hi, e),
        };

        BlockMasks {
            delim,
            quote,
            lf,
            cr,
            escape,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_backend_is_supported_on_current_cpu() {
        let backend = active_simd_backend();
        assert!(backend.is_supported_on_current_cpu());
        assert!(SimdBackend::Scalar.is_supported_on_current_cpu());
        assert!(!backend.as_str().is_empty());
    }

    #[test]
    fn prefix_xor_matches_swar_and_bit_loop() {
        let backend = active_simd_backend();
        let samples = [
            0u64,
            1,
            u64::MAX,
            0x8000_0000_0000_0000,
            0x5555_5555_5555_5555,
            0x1234_5678_9ABC_DEF0,
            0x0001_0002_0004_0008,
        ];
        for &mask in &samples {
            let mut expected = 0u64;
            let mut acc = 0u64;
            for bit in 0..64 {
                acc ^= (mask >> bit) & 1;
                expected |= acc << bit;
            }
            assert_eq!(prefix_xor_swar(mask), expected, "swar mask={mask:#x}");
            assert_eq!(
                prefix_xor(mask, backend),
                expected,
                "backend={backend} mask={mask:#x}"
            );
        }
    }

    #[test]
    fn simd_block_classification_matches_scalar_for_all_bytes() {
        let backend = active_simd_backend();
        let dialect = ScannerDialect::new(b',', Some(b'"'), EscapeRule::EscapeByte(b'\\'));
        let mut block = [0u8; 64];
        for (i, slot) in block.iter_mut().enumerate() {
            *slot = match i % 7 {
                0 => b',',
                1 => b'"',
                2 => b'\n',
                3 => b'\r',
                4 => b'\\',
                5 => b'a',
                _ => b'0' + (i as u8 % 10),
            };
        }
        let expected = classify_block_scalar(&block, dialect, true);
        let actual = classify_block_64(&block, dialect, backend, true);
        assert_eq!(actual, expected);
    }

    #[test]
    fn escaped_mask_matches_reference_at_boundaries() {
        for prev in [false, true] {
            for pattern in [
                0u64,
                1,
                0b11,
                0b111,
                0b1111,
                1u64 << 63,
                0b11u64 << 62,
                0b111u64 << 61,
                0xAAAA_5555_1234_8001,
            ] {
                let mut ref_mask = 0u64;
                let mut pe = prev;
                for i in 0..64 {
                    if pe {
                        ref_mask |= 1u64 << i;
                        pe = false;
                    } else if (pattern >> i) & 1 != 0 {
                        pe = true;
                    }
                }
                assert_eq!(
                    compute_escaped_mask(pattern, prev),
                    (ref_mask, pe),
                    "prev={prev} pattern={pattern:#x}"
                );
            }
        }
    }
}
