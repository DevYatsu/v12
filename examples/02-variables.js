// 02-variables — block scoping, shadowing, const

let x = 1;
{
  let x = 2;          // shadows outer x
  let y = x + 10;     // 12
  // y dies here
}
let afterBlock = x;   // 1 — outer x untouched

const c = 100;
// c = 200; // would throw TypeError at runtime (const assignment)

var v = 1;
var v = 2;            // var allows re-declaration
{
  var v2 = 3;         // var is function-scoped, not block-scoped
}
let v2Visible = v2;   // 3

let a = 5;
let b = a;
b = 10;
let aStill = a;       // 5 — primitives copy by value

// destructuring (flat)
let obj = {p: 10, q: 20};
let {p, q} = obj;     // p=10, q=20
let arr = [1, 2, 3];
let [first, second] = arr; // 1, 2

p + q + first + second + afterBlock
