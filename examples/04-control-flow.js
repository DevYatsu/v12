// 04-control-flow — if/else, loops, break/continue, labels

// if / else
let grade = 85;
let result;
if (grade >= 90) { result = "A"; }
else if (grade >= 80) { result = "B"; }
else { result = "C"; } // "B"

// ternary
let tern = (result === "B" ? 1 : 0);

// while
let w = 0;
let i = 0;
while (i < 5) { w += i; i += 1; } // w=10

// do-while executes at least once
let dw = 0;
let j = 0;
do { dw += 1; j += 1; } while (j < 3); // dw=3

// for(;;)
let acc = 0;
for (let k = 0; k < 4; k += 1) { acc += k; } // 6

// break
let br = 0;
for (let k = 0; k < 10; k += 1) {
  if (k === 3) { break; }
  br += k;
} // 0+1+2=3

// continue
let cont = 0;
for (let k = 0; k < 5; k += 1) {
  if (k === 2) { continue; }
  cont += k;
} // 0+1+3+4=8

// labeled break
let labeled = 0;
outer: for (let a = 0; a < 3; a += 1) {
  for (let b = 0; b < 3; b += 1) {
    if (a === 1 && b === 1) { break outer; }
    labeled += 1;
  }
}

// short-circuit && || ??
let scAnd = (0 && 99);  // 0
let scOr = (0 || 42);   // 42
let scNullish = (undefined ?? 7); // 7

w + dw + acc + br + cont + labeled
