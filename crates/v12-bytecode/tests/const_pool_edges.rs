#![forbid(unsafe_code)]

//! Requirement 6: constant-pool edge behavior.
//!
//! - Dedup keys on the *exact bits* of the payload: 0.0 and -0.0 stay
//!   distinct; identical NaN payloads fold; differing NaN payloads don't.
//! - The tag participates in the key, so equal payloads across variants
//!   never collide.
//! - The 65_535 cap produces a clean `Err`, never a panic, and dedup keeps
//!   working at capacity.

use v12_bytecode::{Const, ConstantPool, MAX_CONSTANTS};

/// Exact bit pattern of a pooled constant (the dedup key's payload half).
fn bits(c: Const) -> u64 {
    match c {
        Const::F64(v) => v.to_bits(),
        Const::BigU64(v) => v,
        Const::Str32(id) | Const::BigIntId(id) => u64::from(id),
        Const::Null => 0,
    }
}

#[test]
fn zero_signs_are_distinct_by_bit_pattern() {
    let mut pool = ConstantPool::new();
    let pos = pool.insert(Const::F64(0.0)).unwrap();
    let neg = pool.insert(Const::F64(-0.0)).unwrap();
    assert_ne!(
        pos, neg,
        "+0.0 and -0.0 have different bits and must not dedup"
    );
    assert_eq!(pool.len(), 2);

    // Roundtrip both back out with their exact bits.
    assert_eq!(
        pool.get(pos).map(bits),
        Some(0.0f64.to_bits()),
        "positive zero must keep its sign bit"
    );
    assert_eq!(pool.get(neg).map(bits), Some((-0.0f64).to_bits()));

    // Reinserting either still folds onto its own entry.
    assert_eq!(pool.insert(Const::F64(0.0)).unwrap(), pos);
    assert_eq!(pool.insert(Const::F64(-0.0)).unwrap(), neg);
    assert_eq!(pool.len(), 2);
}

#[test]
fn nan_dedup_follows_the_documented_payload_policy() {
    let mut pool = ConstantPool::new();

    // Canonical quiet NaN inserted twice: same payload, folds.
    let canon_bits = f64::NAN.to_bits();
    let a = pool.insert(Const::F64(f64::NAN)).unwrap();
    let b = pool.insert(Const::F64(f64::NAN)).unwrap();
    assert_eq!(a, b, "identical NaN payloads dedup");
    assert_eq!(pool.get(a).map(bits), Some(canon_bits));

    // A different NaN payload is a distinct constant.
    let c = pool
        .insert(Const::F64(f64::from_bits(canon_bits | 1)))
        .unwrap();
    assert_ne!(a, c, "differing NaN payloads must not dedup");

    // Another distinct payload stays distinct too.
    let d = pool
        .insert(Const::F64(f64::from_bits(0x7FF0_0000_0000_0001)))
        .unwrap();
    assert_ne!(c, d);

    assert_eq!(pool.len(), 3);
}

#[test]
fn tag_participates_in_the_dedup_key() {
    let mut pool = ConstantPool::new();
    let s = pool.insert(Const::Str32(7)).unwrap();
    let bi = pool.insert(Const::BigIntId(7)).unwrap();
    let u = pool.insert(Const::BigU64(7)).unwrap();
    let f = pool.insert(Const::F64(7.0)).unwrap();
    let n = pool.insert(Const::Null).unwrap();

    assert_ne!(s, bi);
    assert_ne!(bi, u);
    assert_ne!(u, f);
    assert_ne!(f, n);
    assert_eq!(pool.len(), 5);

    // Each still dedups within its own variant.
    assert_eq!(pool.insert(Const::Str32(7)).unwrap(), s);
    assert_eq!(pool.insert(Const::BigIntId(7)).unwrap(), bi);
    assert_eq!(pool.insert(Const::BigU64(7)).unwrap(), u);
    assert_eq!(pool.insert(Const::F64(7.0)).unwrap(), f);
    assert_eq!(pool.insert(Const::Null).unwrap(), n);
    assert_eq!(pool.len(), 5);
}

#[test]
fn capacity_error_is_a_clean_result_not_a_panic() {
    let mut pool = ConstantPool::new();
    for i in 0..MAX_CONSTANTS as u64 {
        let idx = pool.insert(Const::BigU64(i)).expect("fills below the cap");
        assert_eq!(idx as u64, i, "insertion order defines the index");
    }
    assert_eq!(pool.len(), MAX_CONSTANTS);

    // One past the cap: Err with the documented message.
    let err = pool
        .insert(Const::BigU64(MAX_CONSTANTS as u64))
        .expect_err("insertion past MAX_CONSTANTS must fail cleanly");
    assert!(
        err.to_string().contains("full"),
        "unexpected error text: {err}"
    );

    // Dedup still resolves at capacity (checked before the cap).
    assert_eq!(
        pool.insert(Const::BigU64(0)).unwrap(),
        0,
        "existing entries stay reachable at capacity"
    );
    // Fresh keys keep failing without growing the pool.
    assert!(pool.insert(Const::Str32(u32::MAX)).is_err());
    assert_eq!(
        pool.len(),
        MAX_CONSTANTS,
        "failed inserts must not grow the pool"
    );

    // Accessors behave at the boundary.
    assert_eq!(
        pool.get((MAX_CONSTANTS - 1) as u16),
        Some(Const::BigU64((MAX_CONSTANTS - 1) as u64))
    );
    assert_eq!(
        pool.get(MAX_CONSTANTS as u16),
        None,
        "index == cap is out of bounds"
    );
    assert!(!pool.is_empty());
}

#[test]
fn empty_pool_basics() {
    let pool = ConstantPool::new();
    assert!(pool.is_empty());
    assert_eq!(pool.len(), 0);
    assert_eq!(pool.get(0), None);
    assert_eq!(pool.iter().count(), 0);
}
