// 10-builtins — typeof and basic coercion (globals like Object/String not yet exposed)

let tNum = typeof 42;       // "number"
let tStr = typeof "hi";     // "string"
let tFn = typeof function(){}; // "function"
let tUndef = typeof undefined; // "undefined"

let boolFromNum = !0;  // true (0 is falsy)
let boolTrue = !!1;    // true

let o = {a: 1};
let arr = [1, 2, 3];
let arrLen = arr.length; // 3

let s = "hello";
let cat = s + " world"; // "hello world"

arrLen + cat.length
