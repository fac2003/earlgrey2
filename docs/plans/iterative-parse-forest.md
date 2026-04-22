# Iterative / Lazy Parse-Forest Evaluation

## Motivation

Some grammar + input combinations produce a combinatorial number of parse
trees. The Earley chart itself stays polynomial (O(n³)), so the explosion is
entirely in **tree extraction** from the chart, not in `EarleyParser::parse`.

Today `EarleyForest::eval_all` materializes every tree into a
`Vec<ASTNode>` even though it uses `ForestIterator` internally. The goal is to
expose that iterator so callers can `.take(n)`, `.find(...)`, or stream results
to a ranker without ever materializing the full enumeration.

All work lives in [earlgrey/src/earley/trees.rs](../../earlgrey/src/earley/trees.rs)
and its tests ([earlgrey/src/earley/parser_test.rs](../../earlgrey/src/earley/parser_test.rs)).

## Current shape

- `EarleyForest::eval(ptrees) -> Result<ASTNode, _>` — unambiguous path.
  Internally uses iterative `eval_one` with a constant selector.
- `EarleyForest::eval_all(ptrees) -> Result<Vec<ASTNode>, _>` — builds all
  trees eagerly; internally drives `ForestIterator` in a loop.
- `ForestIterator` is a `Vec<(Rc<Span>, usize)>` stack over
  `(span, chosen-source-idx)`; `advance()` bumps the top counter until the
  entire state space is exhausted.
- Recursive alternates (`eval_recursive`, `eval_all_recursive`) exist alongside
  but are not the default public entry points.
- Bottomless-grammar safeguards (upstream Nov 2024) apply **only** to the
  recursive `walker_all`:
  - `assert!(level < 100, "Bottomless grammar, stack blew up")`
  - branch-local `explored: Vec<SpanSource>` that skips already-entered
    backpointers.
  The iterative path (`eval_one` / `ForestIterator`) has no such protection.
  The commit message for `ab14e20` explicitly flags this as unfinished.

## Sequenced plan

### Phase 1 — Cycle tracking on the iterative path

The recursive walker's `explored` list is branch-local (cloned per recursion).
Porting it directly is awkward because `eval_one`'s `spans`/`completions`
stacks don't nest cleanly — a `SpanSource::Completion` pushes both the source
and the trigger onto the same stack, so "pop the backpointer that led to
span X in LIFO order" isn't a primitive we have.

Two sub-options:

- **1a — strict (matches recursive semantics):** track a `HashSet<SpanSource>`
  (or small `Vec`) within a single `eval_one` call; skip any backpointer
  already entered during this call. Stricter than branch-local tracking — will
  reject legitimate re-entry of the same `SpanSource` from two different
  ambiguous paths, but in practice such re-entries collapse to identical
  subtrees, so rejection is harmless. Needs confirmation against existing
  tests.
- **1b — pragmatic:** convert the panic into a typed error. Add a step counter
  (or stack-depth check) inside `eval_one` that returns
  `Err("max depth exceeded")` instead of panicking. Doesn't *detect* cycles —
  it *stops* them.

**Recommended sequence:** land 1b first (few lines, turns panic into `Err`,
unblocks phase 2 without getting stuck on semantics debates), then land 1a as
a follow-up once we have a targeted test grammar that triggers the cyclic
case.

**Tests:** adapt the bottomless-grammar test in
[parser_test.rs](../../earlgrey/src/earley/parser_test.rs) (added in upstream
`f090e2d`) to call the iterative `eval` / `eval_all` path and assert `Err`
instead of panic.

### Phase 2 — Expose the iterator

New public API:

```rust
pub fn eval_iter<'f>(&'f self, ptrees: &'f ParseTrees)
    -> impl Iterator<Item = Result<ASTNode, String>> + 'f
```

Implementation: a private struct `EarleyForestIter<'f, ASTNode>` holding

- `&'f EarleyForest<'_, ASTNode>`
- an iterator over `&ptrees.0` (roots)
- the current root plus its `ForestIterator` state
- a "first tree for this root" flag (since `eval_one` must run once before
  `advance()` makes sense)

Rewrite `eval_all` as `eval_iter(ptrees).collect::<Result<Vec<_>, _>>()`.
No semver break; purely additive.

**Tests:**

- `eval_iter(...).take(3)` on an ambiguous grammar returns exactly 3 trees.
- `eval_iter(...).collect::<Result<Vec<_>,_>>()` matches `eval_all` output
  element-for-element on existing ambiguous-grammar tests.
- On a grammar with no valid parse, the iterator yields zero items (no error).

### Phase 3 — Hard cap

Two knobs, both configured on `EarleyForest`:

- `max_steps_per_tree: Option<usize>` — a defense-in-depth cap inside one
  `eval_one` invocation. Returns `Err` when exceeded. This is strictly
  additional to phase 1 cycle tracking; it catches runaway cases that slipped
  through (or that phase 1a hasn't covered yet).
- **No library-side cap on tree count.** Callers compose `.take(n)` on the
  iterator from phase 2. Keeps the library orthogonal.

Optional convenience wrapper for callers that don't want to touch iterators:

```rust
pub fn eval_capped(&self, ptrees: &ParseTrees, max_trees: usize)
    -> Result<Vec<ASTNode>, String>
```

which is just `self.eval_iter(ptrees).take(max_trees).collect()`.

**Tests:**

- A grammar exceeding `max_steps_per_tree` returns `Err`, not panic.
- `eval_capped(ptrees, 5)` on a grammar with >5 trees returns exactly 5 and
  doesn't consume the rest.

## Non-goals

- **Lazy chart construction.** `EarleyParser::parse` remains eager; streaming
  the chart itself is a separate, much larger refactor (self-referential
  iterator holding tokenizer state, etc.). Not justified until there's
  evidence the chart — not the tree count — is the bottleneck.
- **Ranking / best-tree selection.** Left to callers; the iterator is the
  composable primitive they need.
- **Deprecating `eval_all` / `eval_all_recursive`.** Kept for back-compat.

## Open questions

- Phase 1a's "reject re-entry of the same `SpanSource`" behavior may differ
  subtly from the recursive walker's *branch-local* explored set in contrived
  ambiguous grammars. Worth a small worked example before committing to the
  strict variant.
- Should `max_steps_per_tree` have a sane default (e.g. 100_000) or be opt-in?
  Leaning opt-in to avoid surprising existing callers.
