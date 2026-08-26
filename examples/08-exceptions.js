// 08-exceptions — throw, try/catch, finally

let caught = 0;
try { throw 99; } catch (e) { caught = e; }

let finallyRan = 0;
try { } catch (e) { } finally { finallyRan = 1; }

let x = 0;
try { x = 1; } catch (e) { x = 2; }

// comprehensive catch now works without panic (register-window fix)
caught + finallyRan + x
