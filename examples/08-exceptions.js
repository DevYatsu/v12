// 08-exceptions — throw
// try/catch with catch bindings is covered in unit tests;
// file-based catch currently has a compiler edge case, so this
// example demonstrates throw (Uncaught) which the runner reports.

let x = 41;
throw x + 1; // Uncaught 42 — runner prints "Uncaught 42" to stderr
