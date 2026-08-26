// 05-objects — literals, get/set/delete, prototype

let o = {x: 1, y: 2};
let readX = o.x;        // 1
o.z = 3;
let readZ = o.z;        // 3
let delY = delete o.y;
let afterDel = o.y;     // undefined

let key = "dyn";
let o2 = {};
o2[key] = 99;
let dynRead = o2[key];  // 99

let parent = {p: 42};
let child = {q: 7};
child.__proto__ = parent;
let inherited = child.p; // 42 — walks prototype (set via assignment)

let shorthandVal = 123;
let short = {shorthandVal};
let shortRead = short.shorthandVal; // 123

readX + dynRead + inherited + shortRead
