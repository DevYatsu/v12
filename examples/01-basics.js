// 01-basics — literals, arithmetic, comparisons, typeof, coercion

let a = 10;
let b = 3;
let sum = a + b;          // 13
let prod = a * b;         // 30
let div = a / b;
let mod = a % b;          // 1
let pow = 2 ** 8;         // 256

let eq = (5 == "5");      // true — loose equality coerces
let strict = (5 === "5"); // false
let lt = (2 < 10);
let typeofNum = typeof 42;       // "number"
let typeofStr = typeof "hi";     // "string"
let typeofBool = typeof true;    // "boolean"
let typeofUndef = typeof undefined; // "undefined"

let band = (5 & 3);   // 1
let bor = (5 | 3);    // 7
let shl = (1 << 8);   // 256
let shr = (-8 >> 1);  // -4
let ushr = (8 >>> 1); // 4

let neg = -42;
let not = !0;         // true

let concat = "5" + 3;  // "53"
let coercedSub = "5" - 3; // 2

sum + prod
