// 09-closures — counter factory, shared env

function makeCounter(start) {
  let n = start;
  return function () { n += 1; return n; };
}
let c1 = makeCounter(10);
let a = c1(); // 11
let b = c1(); // 12
let c2 = makeCounter(100);
let c = c2(); // 101

function pair(x) {
  let v = x;
  return {
    get: function () { return v; },
    set: function (y) { v = y; }
  };
}
let p = pair(5);
let getBefore = p.get(); // 5
p.set(42);
let getAfter = p.get(); // 42

function outer() {
  let msg = "hello";
  let inner = () => msg + " world";
  return inner();
}
let arrowCap = outer(); // "hello world"

a + b + c + getBefore + getAfter
