//! Guards the extraction of the mini interpreter: if `test-support` and the
//! compiler drift apart on basic semantics, these fail before 125 dependent
//! tests give misleading results.

use test_support::{expect_bool, expect_num, expect_str};

#[test]
fn switch_fallthrough_with_break() {
    expect_str(
        "
        let s = '';
        switch (2) { case 2: s += 'B'; case 3: s += 'C'; break; }
        return s;
    ",
        "BC",
    );
}

#[test]
fn typeof_null_is_object() {
    expect_str("return typeof null;", "object");
}

#[test]
fn nullish_coalescing_short_circuit() {
    expect_num("return null ?? 7;", 7.0);
    expect_bool("return 0 || false;", false);
}
