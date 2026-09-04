//! Distance kernels shared by the resident and mapped HNSW graphs.
//!
//! `Iterator::sum` over `f32` is a strict left-to-right reduction. The
//! optimizer may not reassociate floating-point additions, so every distance
//! used to compile to a scalar loop with one dependent add per element.
//! These kernels keep several independent lane accumulators, which lets the
//! compiler emit NEON/SSE/AVX arithmetic. The summation order is fixed by the
//! code, so results stay a deterministic function of the inputs on every
//! target (the order differs from the sequential sum by a few ulps, which is
//! irrelevant for approximate-neighbor ranking).

const LANES: usize = 8;
const INT_LANES: usize = 16;

/// Products of two `i8` values are bounded by 127 * 127, so an `i32` lane
/// accumulating this many elements cannot overflow before folding into `i64`.
const INT_BLOCK: usize = 8192;

#[inline]
fn fold(acc: [f32; LANES]) -> f32 {
    ((acc[0] + acc[4]) + (acc[2] + acc[6])) + ((acc[1] + acc[5]) + (acc[3] + acc[7]))
}

#[inline]
fn common<'a, T, U>(a: &'a [T], b: &'a [U]) -> (&'a [T], &'a [U]) {
    let len = a.len().min(b.len());
    (&a[..len], &b[..len])
}

/// Reinterpret mapped bytes as signed quantized components.
#[inline]
pub(crate) fn bytes_as_i8(bytes: &[u8]) -> &[i8] {
    // SAFETY: `i8` and `u8` share size, alignment and validity for every bit
    // pattern; the returned slice borrows `bytes` for the same lifetime.
    unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<i8>(), bytes.len()) }
}

/// Dot product over the common prefix of `a` and `b`.
#[inline]
pub(crate) fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    let (a, b) = common(a, b);
    let mut acc = [0.0f32; LANES];
    let mut chunks_a = a.chunks_exact(LANES);
    let mut chunks_b = b.chunks_exact(LANES);
    for (x, y) in (&mut chunks_a).zip(&mut chunks_b) {
        let x: &[f32; LANES] = x.try_into().expect("exact chunk");
        let y: &[f32; LANES] = y.try_into().expect("exact chunk");
        for lane in 0..LANES {
            acc[lane] += x[lane] * y[lane];
        }
    }
    let mut tail = 0.0f32;
    for (x, y) in chunks_a.remainder().iter().zip(chunks_b.remainder()) {
        tail += x * y;
    }
    fold(acc) + tail
}

/// Squared Euclidean distance over the common prefix of `a` and `b`.
#[inline]
pub(crate) fn l2_squared_f32(a: &[f32], b: &[f32]) -> f32 {
    let (a, b) = common(a, b);
    let mut acc = [0.0f32; LANES];
    let mut chunks_a = a.chunks_exact(LANES);
    let mut chunks_b = b.chunks_exact(LANES);
    for (x, y) in (&mut chunks_a).zip(&mut chunks_b) {
        let x: &[f32; LANES] = x.try_into().expect("exact chunk");
        let y: &[f32; LANES] = y.try_into().expect("exact chunk");
        for lane in 0..LANES {
            let delta = x[lane] - y[lane];
            acc[lane] += delta * delta;
        }
    }
    let mut tail = 0.0f32;
    for (x, y) in chunks_a.remainder().iter().zip(chunks_b.remainder()) {
        let delta = x - y;
        tail += delta * delta;
    }
    fold(acc) + tail
}

/// Dot product of an `f32` query with an `i8` quantized vector. The caller
/// applies the per-vector scale to the result.
#[inline]
pub(crate) fn dot_f32_i8(a: &[f32], q: &[i8]) -> f32 {
    let (a, q) = common(a, q);
    let mut acc = [0.0f32; LANES];
    let mut chunks_a = a.chunks_exact(LANES);
    let mut chunks_q = q.chunks_exact(LANES);
    for (x, y) in (&mut chunks_a).zip(&mut chunks_q) {
        let x: &[f32; LANES] = x.try_into().expect("exact chunk");
        let y: &[i8; LANES] = y.try_into().expect("exact chunk");
        for lane in 0..LANES {
            acc[lane] += x[lane] * y[lane] as f32;
        }
    }
    let mut tail = 0.0f32;
    for (x, y) in chunks_a.remainder().iter().zip(chunks_q.remainder()) {
        tail += x * *y as f32;
    }
    fold(acc) + tail
}

/// Squared Euclidean distance between an `f32` query and an `i8` quantized
/// vector whose components are `q * scale`.
#[inline]
pub(crate) fn l2_squared_f32_i8(a: &[f32], q: &[i8], scale: f32) -> f32 {
    let (a, q) = common(a, q);
    let mut acc = [0.0f32; LANES];
    let mut chunks_a = a.chunks_exact(LANES);
    let mut chunks_q = q.chunks_exact(LANES);
    for (x, y) in (&mut chunks_a).zip(&mut chunks_q) {
        let x: &[f32; LANES] = x.try_into().expect("exact chunk");
        let y: &[i8; LANES] = y.try_into().expect("exact chunk");
        for lane in 0..LANES {
            let delta = x[lane] - y[lane] as f32 * scale;
            acc[lane] += delta * delta;
        }
    }
    let mut tail = 0.0f32;
    for (x, y) in chunks_a.remainder().iter().zip(chunks_q.remainder()) {
        let delta = x - *y as f32 * scale;
        tail += delta * delta;
    }
    fold(acc) + tail
}

/// Squared Euclidean distance between two `i8` quantized vectors with their
/// own scales.
#[inline]
pub(crate) fn l2_squared_i8_scaled(a: &[i8], scale_a: f32, b: &[i8], scale_b: f32) -> f32 {
    let (a, b) = common(a, b);
    let mut acc = [0.0f32; LANES];
    let mut chunks_a = a.chunks_exact(LANES);
    let mut chunks_b = b.chunks_exact(LANES);
    for (x, y) in (&mut chunks_a).zip(&mut chunks_b) {
        let x: &[i8; LANES] = x.try_into().expect("exact chunk");
        let y: &[i8; LANES] = y.try_into().expect("exact chunk");
        for lane in 0..LANES {
            let delta = x[lane] as f32 * scale_a - y[lane] as f32 * scale_b;
            acc[lane] += delta * delta;
        }
    }
    let mut tail = 0.0f32;
    for (x, y) in chunks_a.remainder().iter().zip(chunks_b.remainder()) {
        let delta = *x as f32 * scale_a - *y as f32 * scale_b;
        tail += delta * delta;
    }
    fold(acc) + tail
}

/// Exact integer dot product of two `i8` vectors.
#[inline]
pub(crate) fn dot_i8(a: &[i8], b: &[i8]) -> i64 {
    let (a, b) = common(a, b);
    let mut total = 0i64;
    for (block_a, block_b) in a.chunks(INT_BLOCK).zip(b.chunks(INT_BLOCK)) {
        let mut acc = [0i32; INT_LANES];
        let mut chunks_a = block_a.chunks_exact(INT_LANES);
        let mut chunks_b = block_b.chunks_exact(INT_LANES);
        for (x, y) in (&mut chunks_a).zip(&mut chunks_b) {
            let x: &[i8; INT_LANES] = x.try_into().expect("exact chunk");
            let y: &[i8; INT_LANES] = y.try_into().expect("exact chunk");
            for lane in 0..INT_LANES {
                acc[lane] += x[lane] as i32 * y[lane] as i32;
            }
        }
        let mut tail = 0i32;
        for (x, y) in chunks_a.remainder().iter().zip(chunks_b.remainder()) {
            tail += *x as i32 * *y as i32;
        }
        total += acc.iter().map(|lane| *lane as i64).sum::<i64>() + tail as i64;
    }
    total
}

/// Dot product of an `f32` query with little-endian `f32` components stored
/// in a byte slice (the mapped graph layout carries no alignment guarantee).
#[inline]
pub(crate) fn dot_f32_le_bytes(a: &[f32], bytes: &[u8]) -> f32 {
    let len = a.len().min(bytes.len() / 4);
    let (a, bytes) = (&a[..len], &bytes[..len * 4]);
    let mut acc = [0.0f32; LANES];
    let mut chunks_a = a.chunks_exact(LANES);
    let mut chunks_b = bytes.chunks_exact(LANES * 4);
    for (x, y) in (&mut chunks_a).zip(&mut chunks_b) {
        let x: &[f32; LANES] = x.try_into().expect("exact chunk");
        let y: &[u8; LANES * 4] = y.try_into().expect("exact chunk");
        for lane in 0..LANES {
            let value = f32::from_le_bytes([
                y[lane * 4],
                y[lane * 4 + 1],
                y[lane * 4 + 2],
                y[lane * 4 + 3],
            ]);
            acc[lane] += x[lane] * value;
        }
    }
    let mut tail = 0.0f32;
    for (x, y) in chunks_a
        .remainder()
        .iter()
        .zip(chunks_b.remainder().chunks_exact(4))
    {
        tail += x * f32::from_le_bytes(y.try_into().expect("four bytes"));
    }
    fold(acc) + tail
}

/// Squared Euclidean distance between an `f32` query and little-endian `f32`
/// components stored in a byte slice.
#[inline]
pub(crate) fn l2_squared_f32_le_bytes(a: &[f32], bytes: &[u8]) -> f32 {
    let len = a.len().min(bytes.len() / 4);
    let (a, bytes) = (&a[..len], &bytes[..len * 4]);
    let mut acc = [0.0f32; LANES];
    let mut chunks_a = a.chunks_exact(LANES);
    let mut chunks_b = bytes.chunks_exact(LANES * 4);
    for (x, y) in (&mut chunks_a).zip(&mut chunks_b) {
        let x: &[f32; LANES] = x.try_into().expect("exact chunk");
        let y: &[u8; LANES * 4] = y.try_into().expect("exact chunk");
        for lane in 0..LANES {
            let value = f32::from_le_bytes([
                y[lane * 4],
                y[lane * 4 + 1],
                y[lane * 4 + 2],
                y[lane * 4 + 3],
            ]);
            let delta = x[lane] - value;
            acc[lane] += delta * delta;
        }
    }
    let mut tail = 0.0f32;
    for (x, y) in chunks_a
        .remainder()
        .iter()
        .zip(chunks_b.remainder().chunks_exact(4))
    {
        let delta = x - f32::from_le_bytes(y.try_into().expect("four bytes"));
        tail += delta * delta;
    }
    fold(acc) + tail
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(len: usize, seed: u32) -> Vec<f32> {
        (0..len)
            .map(|i| {
                let x = (i as u32).wrapping_mul(2_654_435_761).wrapping_add(seed);
                (x % 2000) as f32 / 1000.0 - 1.0
            })
            .collect()
    }

    fn sample_i8(len: usize, seed: u32) -> Vec<i8> {
        (0..len)
            .map(|i| {
                let x = (i as u32).wrapping_mul(2_246_822_519).wrapping_add(seed);
                (x % 255) as i32 as i8
            })
            .collect()
    }

    fn le_bytes(values: &[f32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn close(actual: f32, expected: f32) -> bool {
        let scale = expected.abs().max(1.0);
        (actual - expected).abs() <= scale * 1e-4
    }

    const LENGTHS: [usize; 9] = [0, 1, 7, 8, 9, 15, 64, 65, 1000];

    #[test]
    fn f32_kernels_match_sequential_reference() {
        for len in LENGTHS {
            let a = sample(len, 1);
            let b = sample(len, 7);
            let dot: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
            let l2: f32 = a.iter().zip(&b).map(|(x, y)| (x - y) * (x - y)).sum();
            assert!(close(dot_f32(&a, &b), dot), "dot len {len}");
            assert!(close(l2_squared_f32(&a, &b), l2), "l2 len {len}");
            assert!(close(dot_f32_le_bytes(&a, &le_bytes(&b)), dot), "dot bytes {len}");
            assert!(
                close(l2_squared_f32_le_bytes(&a, &le_bytes(&b)), l2),
                "l2 bytes {len}"
            );
        }
    }

    #[test]
    fn mixed_kernels_match_sequential_reference() {
        for len in LENGTHS {
            let a = sample(len, 3);
            let q = sample_i8(len, 11);
            let scale = 0.0173f32;
            let dot: f32 = a.iter().zip(&q).map(|(x, &y)| x * y as f32).sum();
            let l2: f32 = a
                .iter()
                .zip(&q)
                .map(|(x, &y)| {
                    let d = x - y as f32 * scale;
                    d * d
                })
                .sum();
            assert!(close(dot_f32_i8(&a, &q), dot), "dot i8 len {len}");
            assert!(close(l2_squared_f32_i8(&a, &q, scale), l2), "l2 i8 len {len}");
            let bytes: Vec<u8> = q.iter().map(|v| *v as u8).collect();
            assert_eq!(bytes_as_i8(&bytes), q.as_slice());
        }
    }

    #[test]
    fn integer_kernels_are_exact() {
        for len in LENGTHS.into_iter().chain([INT_BLOCK - 1, INT_BLOCK, INT_BLOCK * 2 + 5]) {
            let a = sample_i8(len, 5);
            let b = sample_i8(len, 9);
            let expected: i64 = a.iter().zip(&b).map(|(&x, &y)| x as i64 * y as i64).sum();
            assert_eq!(dot_i8(&a, &b), expected, "len {len}");
            let (sa, sb) = (0.021f32, 0.037f32);
            let l2: f32 = a
                .iter()
                .zip(&b)
                .map(|(&x, &y)| {
                    let d = x as f32 * sa - y as f32 * sb;
                    d * d
                })
                .sum();
            assert!(close(l2_squared_i8_scaled(&a, sa, &b, sb), l2), "l2 len {len}");
        }
    }

    #[test]
    fn extreme_i8_products_do_not_overflow() {
        let a = vec![127i8; INT_BLOCK * 3 + 17];
        let b = vec![-128i8; INT_BLOCK * 3 + 17];
        let expected = -(127i64 * 128) * (INT_BLOCK as i64 * 3 + 17);
        assert_eq!(dot_i8(&a, &b), expected);
    }

    #[test]
    fn kernels_use_the_common_prefix() {
        let a = sample(10, 1);
        let b = sample(6, 2);
        let expected: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        assert!(close(dot_f32(&a, &b), expected));
        assert!(close(dot_f32(&b, &a), expected));
        assert!(close(dot_f32_le_bytes(&a, &le_bytes(&b)), expected));
    }
}
