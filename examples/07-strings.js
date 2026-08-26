// 07-strings — concat, interning, rope flattening

let s = "hello";
let t = " world";
let cat = s + t; // "hello world"

let latin = "abc";
let utf16 = "a\u03A9b";
let joined = latin + utf16;

let k1 = "foo";
let k2 = "foo";
let sameKey = (k1 === k2); // true — same string content

let rope = "a" + "b" + "c" + "d"; // small concats eager-flatten

cat.length + joined.length
