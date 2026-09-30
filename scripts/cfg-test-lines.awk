# Emits "path:line" for every line that lies inside an inline
# `#[cfg(test)]` block. Best-effort: used ONLY to label allowlist notes, never
# to enforce anything.
function strip(s,   i, c, out, inq, esc) {
  out = ""; inq = ""; esc = 0
  for (i = 1; i <= length(s); i++) {
    c = substr(s, i, 1)
    if (inq != "") {
      if (esc) { esc = 0; continue }
      if (c == "\\") { esc = 1; continue }
      if (c == inq) inq = ""
      continue
    }
    if (c == "\"" || c == "'" || c == "`") { inq = c; continue }
    if (c == "/" && substr(s, i + 1, 1) == "/") break
    out = out c
  }
  return out
}
FNR == 1 { depth = 0; base = -1; entered = 0 }
{
  code = strip($0)
  n = gsub(/\{/, "{", code)
  m = gsub(/\}/, "}", code)
  depth += n - m
  if (base >= 0) {
    if (depth > base) entered = 1
    else if (entered) { base = -1; entered = 0 }
  }
  if (base < 0 && code ~ /#\[cfg\(test\)\]/) { base = depth; entered = 0 }
  if (base >= 0) print FILENAME ":" FNR
}