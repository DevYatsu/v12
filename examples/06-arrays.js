// 06-arrays — literals, length, holey reads, sparse

let a = [1, 2, 3];
let len1 = a.length; // 3

let holey = [10, , 30]; // index 1 is hole
let holeRead = holey[1]; // undefined

a[10] = 99;
let sparseLen = a.length; // 11
let sparseRead = a[10]; // 99

let mixed = [1, 2, 3];
mixed[1] = "hi";
let mixedRead = mixed[1]; // "hi"

delete mixed[0];
let afterDel = mixed[0]; // undefined

len1 + sparseLen
