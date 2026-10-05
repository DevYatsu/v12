# ADR-0005: Distinguish the two accessor spellings with a sibling opcode, not an attribute operand

**Status:** Accepted
**Date:** 2026-10-05
**Deciders:** implementing batch (user pending)
**Relates to:** `docs/adr/0004-arraylike-index-access.md` (the ledger this defect was measured in); `crates/v12-bytecode/src/opspec.rs` (`KNOWN_INCONSISTENT_OPERAND_ORDER`, `Width`); `crates/v12-bytecode/src/validate.rs` (the operand gate)
**Supersedes:** nothing
**Informed by:** the eight documented operand-order divergences (`KNOWN_INCONSISTENT_OPERAND_ORDER`), which are recorded rather than normalised and which this decision had to interact with; the W2 series' `OpSpec` table, whose existence is what made the cost of the alternative measurable instead of guessed.

---

## Context

An accessor has **two spellings in the source language** and they differ in exactly one
attribute:

| spelling | `enumerable` | `configurable` | spec |
|---|---|---|---|
| `{ get x(){} }` — object literal | **true** | true | ES 10.1.6 step 6 |
| `class C { get x(){} }` | **false** | true | ES 15.7.6 |

A third spelling exists and is *not* an accessor property at creation time:
`{ m(){} }` is a `PropertyDefinition` whose value is a function object, so it is a
data property with all three attributes, and v12's `object_prop` lowers it through
`SetProperty`. It never reached the accessor path, which is why the defect below could
be stated as "the accessor spelling only" and be correct.

v12 encoded both accessor spellings with **one** opcode:

* `v12-bccompiler/src/expr.rs` — the literal lowering,
* `v12-bccompiler/src/class.rs` — the class lowering (instance and static, `get` and
  `set`),

both emitting `Opcode::DefineAccessor = 66`, whose three operands are `Obj`, `Key`,
`Pair` (`opspec.rs`). The interpreter's single arm called `op_define_accessor`, which
**hardcoded** `Attrs::BUILTIN` — and `Attrs::BUILTIN` is `WRITABLE | CONFIGURABLE`,
i.e. `enumerable == false`.

So the literal accessor was created non-enumerable:

```js
Object.keys({0:"a", get length(){return 3}})   // v12 ["0"]   node ["0","length"]
```

and the ledger attributed 49 of the table's 156 divergent rows to it (ADR-0004's
`KNOWN_DIVERGENCES`, entry `E1`). The class spelling's answer was *right*, and any fix
that made accessors enumerable uniformly would have broken it — a fix with no
test262 `language` case to catch it, and no ledger row to catch it either, because the
table's receivers are all object literals.

**There is no attribute operand to fix.** `Instr` is a `u32` laid out
`bits 31..24 opcode, 23..16 a, 15..8 b, 7..0 c` (`opcode.rs`): four bytes, all four
spoken for. The choice was therefore between giving the instruction a fourth operand
by some other route, and not giving it one.

## Options considered

### Option A — an attribute operand on `DefineAccessor` (REJECTED)

All three slots are live, so a fourth operand cannot be an operand. It has to be a
`WideOp` variant (`DefineAccessorW { obj, key, pair, attrs }`), which forces:

1. `OpSpec::width` for `DefineAccessor` from `Width::One` to `Width::WidePayload`, so
   the instruction stops being one word. Every consumer that assumed one word per
   instruction has to decode: the layout walk in `v12-bccompiler/src/peephole.rs`,
   `analysis.rs`'s pc advance, `validate()`'s own `pc += 1`, the disassembler, and the
   three other dispatchers.
2. **Every** accessor definition — including every class accessor — becomes two words
   instead of one, permanently, to carry one bit that has two values in the entire
   language.
3. `WideOp` exists for operands *wider than a byte*: `LoadConstW`, `ClosureW`,
   `GetEnvSlotW`, `NewEnvironmentW`, `DefinePrivateW`. An attribute mask is 2 bits in
   the language and 8 in the encoding. Adding a `WideOp` for it makes the escape
   hatch mean "needs more than 8 bits", which it currently means exactly.

The decisive cost is not the code, it is that the escape hatch stops meaning what it
means.

### Option B — a sibling opcode (CHOSEN)

`DefineAccessorEnumerable = 79`, emitted **only** by the literal lowering. Same three
operands, same roles (`Obj`, `Key`, `Pair`), same packed-pair convention. The class
spelling keeps emitting `DefineAccessor = 66`.

* One byte of the 256-value opcode space, of which 74 are assigned.
* **No format change**: `Width::One` for both, so `word_width` in
  `v12-bccompiler/src/peephole.rs`, the pc walks and the disassembler are untouched.
* **No class-emitted byte changes.** This is the property the whole decision is bought
  with: "the class spelling is unaffected" is a statement about the emitted bytecode,
  verifiable by diffing a disassembly, and not only about the answers.

### Option C — decide enumerability in the interpreter from context (REJECTED)

The interpreter cannot recover the distinction: both spellings arrive as the same
opcode with the same operands, and nothing else in the frame distinguishes them.
`NewObject`-then-`DefineAccessor` also cannot be used as the signal, because a class
prototype is itself built that way (`class.rs` allocates it with `NewObject`).

### Option D — fix the observer instead of the attribute (REJECTED, and it was tempting)

`Object.keys` could be made to include accessors. That is wrong at a level below this
ADR: `getOwnPropertyDescriptor(o, k).enumerable` is the spec's own answer and it also
said `false`. Fixing the reader would have left the descriptor wrong and traded one
divergence for another.

## Decision

**Option B.** `DefineAccessorEnumerable = 79`, appended (no existing discriminant is
renumbered — `FROZEN_DISCRIMINANTS` in `opcode.rs` gained a row and nothing else
changed).

`op_define_accessor` now takes the attributes as a parameter:

| spelling | opcode | attrs |
|---|---|---|
| class | `DefineAccessor = 66` | `Attrs::BUILTIN` — **bit for bit what it hardcoded before** |
| object literal | `DefineAccessorEnumerable = 79` | `Attrs::ENUMERABLE_ACCESSOR` (new named constant in `v12-heap/src/shape.rs`) |

`Attrs::ENUMERABLE_ACCESSOR` is `ENUMERABLE | CONFIGURABLE` and is deliberately **not**
spelled `Attrs::DEFAULT`, although the two share a bit pattern today: `WRITABLE` is not
a meaningful attribute of an accessor (ES 10.1.6 never sets it; 10.2.5 reports
`writable: false` for every accessor regardless), so reusing `DEFAULT` would couple a
literal accessor to whatever plain assignment means later. The class side keeps
`BUILTIN`'s equally-unobservable `WRITABLE` bit because changing it would change a
class descriptor for no observable reason, and "no class-emitted byte and no
class-visible attribute changed" is the property this ADR exists to establish.

### What this cost, concretely

| site | change |
|---|---|
| `opcode.rs` | 1 variant, 1 `ALL_OPCODES` row, 1 `FROZEN_DISCRIMINANTS` row, `OPCODE_COUNT` 73 → 74 (derived) |
| `lib.rs` | 1 mnemonic (`define_accessor_enumerable`), 1 `fmt_operands` arm (shares `DefineAccessor`'s, 3 registers) |
| `opspec.rs` | 1 `OpSpec` row; the eighth documented divergence's *text* now names both accessor opcodes (its row count is unchanged, so `every_documented_operand_order_divergence_validates_as_emitted` keeps its `== 8` pin) |
| `validate.rs` | `check_positional_windows` extended to the new opcode, so the packed pair's `c + 1` is still range-checked |
| `v12-interp` | 1 dispatch arm (one added `if`), `op_define_accessor` gains an `Attrs` parameter |
| `v12-heap` | 1 named attribute constant |
| `test-support/src/mini.rs` | **1 line, outside the batch's crate set.** `mini.rs` matches `Opcode` exhaustively, so a new variant does not compile without an arm. It joined the existing "not expected in tier-1 mini programs" panic list, which is where `DefineAccessor` already was. This is a forced mechanical consequence of *any* new opcode and was reported rather than worked around; the alternative designs (Option A, and any operand change) break the same file's `WideOp` decode match instead, so no design in this space avoided the edit. |

**No new `word_width`.** That was the question Option A turned on, and it is the reason
Option A lost: a fourth operand has nowhere to live but the wide path.

## Consequences

### The `OpSpec` gate did **not** catch the one mistake available here

The machinery behaved as designed for everything it is able to see, and the batch
recorded one real gap:

* `every_assigned_opcode_has_a_spec` **did** fail until the new `OpSpec` row was added —
  the table is genuinely load-bearing, and an assigned opcode with no row is a build-time
  test failure rather than a silent `None`.
* `discriminants_are_frozen` **did** fail until `FROZEN_DISCRIMINANTS` and
  `ALL_OPCODES` both gained the row, which is the mechanism preventing a renumbering.
* `operand_roles_match_the_disassembler_for_every_opcode` passed on the first run
  because the new opcode's roles and rendering agree by construction — that is the gate
  working, not the gate being bypassed.
* **`check_positional_windows` has no coverage requirement.** Reverting its new arm
  (leaving the new opcode out of the `c + 1` range check) keeps every test in the crate
  green — verified. The positional-window check is an allow-list matched by opcode, so
  adding an opcode that needs the check does not force adding it. The new row is
  therefore justified by the two `OpSpec` role citations, exactly as the module docs
  say, and **a future sibling opcode with the same layout must be added there by
  hand.** That is the honest limit of this gate and it is now written down in
  `validate.rs`.

### Interaction with the eight operand-order divergences: ADDITIVE, and it must stay a separate migration

`KNOWN_INCONSISTENT_OPERAND_ORDER`'s eighth entry is "`c` is a single value register for
`SetProperty`/`DefineMethod` and a packed `(getter, setter)` pair base for
`DefineAccessor`". The new opcode **inherits** that divergence rather than resolving
it: it has the identical `(Obj, Key, Pair)` layout, so it is checked in the same
divergent order, and `every_documented_operand_order_divergence_validates_as_emitted`
(pinned at exactly 8) still holds. Only the entry's *text* changed, to name both
accessor opcodes.

So this ADR **rides along additively** with the deferred normalisation and does not
block it, does not perform any part of it, and changes no existing operand order:

* The eventual normalisation (destination-last for property writes, `Pair` split out
  of `c`) is a **separate migration** touching both accessor opcodes, the
  `SetProperty`/`DefineMethod` pair, every dispatcher's operand reads and the emitted
  bytecode of every function in the workspace.
* Doing it *now* would have meant the class spelling's operands moving as collateral of
  a fix whose entire justification is that the class spelling must not move. That is
  the argument for keeping them apart, and it is not only aesthetic: a normalisation
  that half-moves is exactly what `every_documented_operand_order_divergence_validates_as_emitted`
  exists to fail, and it would have failed **on the class path**, for a reason unrelated
  to the defect being fixed.
* When that migration happens, both accessor opcodes must move together, and the
  eighth entry's text must be updated in the same commit as the emitters — the same
  rule the entry already states.

## Verification

* `Object.keys` / `for-in` / `getOwnPropertyDescriptor` on a literal accessor, and on a
  class accessor (instance *and* static, `get` *and* `set`), each checked against
  `node` — `crates/v12-engine/tests/array_generic_receivers.rs`, three tests whose
  expectations are `node`'s answers.
* The two spellings emit **different opcodes** —
  `crates/v12-bccompiler/src/tests.rs`,
  `object_literal_accessor_emits_the_enumerable_opcode` /
  `class_accessor_emits_the_non_enumerable_opcode`. These are the negative controls:
  either one turning red is the naive uniform fix.
* A class program's **entire disassembly is byte-identical** before and after the
  change (checked with `git worktree add` at `74bb1f8e` and `v12 --disasm`), and a
  literal program's differs in exactly the three accessor instructions and nothing
  else — no register renumbering, no pc shift.
* The ADR-0004 differential table: **156 → 107 of 663 rows, 39 → 20 methods**, measured
  by `scripts/table-divergence.py`, which never reads `KNOWN_DIVERGENCES`.
