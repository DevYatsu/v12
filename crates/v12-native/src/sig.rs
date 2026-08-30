//! The [`NativeSig`] trait: argument tuples as declared signatures.
//!
//! A tuple of `TryFrom<JsValue>` types *is* a native's declared argument
//! list. The generic macro below implements `NativeSig` for every tuple arity
//! up to the Rust tuple-trait limit (12), so any tuple of value types works
//! automatically — no per-combination type list to maintain.
//!
//! The per-element conversions are the std [`TryFrom<JsValue>`] impls defined
//! in `v12-heap` (next to `JsValue`, so the orphan rule is satisfied), whose
//! error is [`v12_heap::DecodeError`]. `from_js` converts that into a
//! [`Throw`] at the dispatch boundary, where a heap is available.

use v12_heap::{DecodeError, Heap, JsValue};

use crate::throw::Throw;

/// Implemented by tuples of argument types.
///
/// `from_js` converts the raw `&[JsValue]` into the tuple, coercing numbers,
/// reading strings, etc., under the hood — or throws a `TypeError` on
/// length/type mismatch.
pub trait NativeSig: Sized {
    /// Converts `js_args` into `Self` (length check + per-position
    /// `TryFrom<JsValue>`), or throws.
    fn from_js(heap: &mut Heap, js_args: &[JsValue]) -> Result<Self, Throw>;
}

/// Maps a heap [`DecodeError`] into a ready-to-throw `TypeError`.
fn decode_error_to_throw(_heap: &mut Heap, e: DecodeError) -> Throw {
    Throw::type_error_msg(format!("expected {}, got {}", e.expected, e.got))
}

/// Generates `impl NativeSig for ()`, `(A,)`, `(A, B)`, … up to the Rust
/// tuple-trait limit of 12 elements (std implements traits for tuples up to
/// 12 items — `tuple_impls!(E D C B A Z Y X W V U T)` in `core`). Any tuple
/// of value types gets the impl for free — no per-type list to maintain.
macro_rules! impl_native_sig_tuples {
    ($($name:ident),*) => {
        impl<$($name: TryFrom<JsValue, Error = DecodeError>),*> NativeSig for ($($name,)*) {
            #[allow(unused_variables)] // the 0-arity impl ignores the heap
            fn from_js(heap: &mut Heap, js: &[JsValue]) -> Result<Self, Throw> {
                // Length check: the declared tuple is the contract.
                if js.len() != count!($($name),*) {
                    return Err(Throw::type_error_msg(format!(
                        "expected {} argument(s), got {}",
                        count!($($name),*),
                        js.len()
                    )));
                }
                // Positional conversion, std TryFrom under the hood.
                // The 0-arity expansion never advances the iterator.
                #[allow(unused_mut)]
                let mut it = js.iter();
                Ok(($(($name::try_from(*it.next().expect("length checked above"))
                    .map_err(|e| decode_error_to_throw(heap, e))?),)*))
            }
        }
    };
}

/// Counts macro arguments (`count!(A, B, C) == 3`).
macro_rules! count {
    () => { 0 };
    ($head:ident $(, $tail:ident)*) => { 1 + count!($($tail),*) };
}

impl_native_sig_tuples!();
impl_native_sig_tuples!(A);
impl_native_sig_tuples!(A, B);
impl_native_sig_tuples!(A, B, C);
impl_native_sig_tuples!(A, B, C, D);
impl_native_sig_tuples!(A, B, C, D, E);
impl_native_sig_tuples!(A, B, C, D, E, F);
impl_native_sig_tuples!(A, B, C, D, E, F, G);
impl_native_sig_tuples!(A, B, C, D, E, F, G, H);
impl_native_sig_tuples!(A, B, C, D, E, F, G, H, I);
impl_native_sig_tuples!(A, B, C, D, E, F, G, H, I, J);
impl_native_sig_tuples!(A, B, C, D, E, F, G, H, I, J, K);
impl_native_sig_tuples!(A, B, C, D, E, F, G, H, I, J, K, L);

#[cfg(test)]
mod tests {
    use super::*;
    use v12_heap::JsValue;

    #[test]
    fn zero_arity_accepts_no_arguments() {
        let mut heap = v12_heap::Heap::new(v12_heap::GcPolicy::NoGC);
        assert_eq!(<() as NativeSig>::from_js(&mut heap, &[]).unwrap(), ());
        assert!(<() as NativeSig>::from_js(&mut heap, &[JsValue::undefined()]).is_err());
    }

    #[test]
    fn two_arity_decodes_positionally() {
        let mut heap = v12_heap::Heap::new(v12_heap::GcPolicy::NoGC);
        let js = [JsValue::from(1.5), JsValue::from(2.5)];
        let (a, b): (f64, f64) = <(f64, f64) as NativeSig>::from_js(&mut heap, &js).unwrap();
        assert_eq!((a, b), (1.5, 2.5));
    }

    #[test]
    fn wrong_length_or_type_throws() {
        let mut heap = v12_heap::Heap::new(v12_heap::GcPolicy::NoGC);
        assert!(<(f64,) as NativeSig>::from_js(&mut heap, &[]).is_err());
        assert!(<(f64,) as NativeSig>::from_js(&mut heap, &[JsValue::undefined()]).is_err());
        assert!(<(f64, f64) as NativeSig>::from_js(&mut heap, &[JsValue::from(1.0)]).is_err());
    }

    /// A typed handler: `(start, end)` is the declared signature.
    fn add_pair(heap: &mut Heap, _this: JsValue, (a, b): (f64, f64)) -> Result<JsValue, Throw> {
        Ok(JsValue::from(a + b))
    }

    #[test]
    fn typed_wrapper_wraps_a_typed_handler_into_the_handler_shape() {
        let mut heap = v12_heap::Heap::new(v12_heap::GcPolicy::NoGC);
        // The macro expands to a closure used as a `native_table!` entry;
        // it decodes the argument slice through `NativeSig` then calls the
        // typed handler. No `Rc`, no runtime construction.
        let wrapped = crate::typed_wrapper!(add_pair, (f64, f64));
        let args = [JsValue::from(1.5), JsValue::from(2.5)];
        let result = wrapped(&mut heap, JsValue::undefined(), &args).unwrap();
        assert_eq!(f64::try_from(result), Ok(4.0));
    }

    #[test]
    fn typed_wrapper_rejects_wrong_arity() {
        let mut heap = v12_heap::Heap::new(v12_heap::GcPolicy::NoGC);
        let wrapped = crate::typed_wrapper!(add_pair, (f64, f64));
        // One argument instead of two: length check throws.
        let args = [JsValue::from(1.5)];
        assert!(wrapped(&mut heap, JsValue::undefined(), &args).is_err());
    }
}
