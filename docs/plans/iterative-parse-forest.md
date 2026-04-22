# Parse-Forest Ambiguity: Lazy Iteration and Priority Filtering

## Motivation

Some grammar + input combinations produce a combinatorial number of parse
trees. The Earley chart itself stays polynomial (O(n³)), so the explosion is
entirely in **tree extraction** from the chart, not in `EarleyParser::parse`.

There are two orthogonal attack surfaces:

1. **Output-side:** bound what we enumerate. Today `EarleyForest::eval_all`
   materializes every tree into a `Vec<ASTNode>` even though it drives
   `ForestIterator` internally. Exposing that iterator lets callers
   `.take(n)`, `.find(...)`, or stream results to a ranker without ever
   building the full enumeration.
2. **Input-side:** prune the forest itself. When two sub-parses cover the
   same input span via different rules, and the caller has a preference
   between them (e.g. "prefer `'(' expr ')'` over `'(' expr` + `expr ')'`"),
   a rule-priority filter collapses the enumeration at the source.

Both approaches compose: priorities shrink the forest, iteration handles
whatever ambiguity remains. This plan sequences both.

All work lives in [earlgrey/src/earley/trees.rs](../../earlgrey/src/earley/trees.rs),
[earlgrey/src/ebnf.rs](../../earlgrey/src/ebnf.rs), and their tests
([earlgrey/src/earley/parser_test.rs](../../earlgrey/src/earley/parser_test.rs)).

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
- No rule-priority or filter mechanism exists on `EarleyForest`.
- The EBNF frontend ([ebnf.rs](../../earlgrey/src/ebnf.rs)) has no syntax
  for annotating rules with metadata.

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

### Phase 4 — Rule-priority filter on EarleyForest (SDF / Rascal style)

Introduce a disambiguation hook on `EarleyForest` that collapses competing
backpointers *before* enumeration, based on a per-rule priority. Conceptually:
two `SpanSource`s that terminate at sub-spans covering the same input range
`(start..end)` and deriving the same non-terminal represent competing parses
of the same constituent. If the caller has expressed a preference between the
rules involved, keep only the higher-priority source.

**Public API:**

```rust
impl<'a, ASTNode: Clone> EarleyForest<'a, ASTNode> {
    /// Assign a priority to a rule (by its canonical string form, e.g.
    /// `"expr -> ( expr )"`). Higher priority wins when competing parses
    /// of the same constituent are both derivable. Unset rules share a
    /// default priority of 0.
    pub fn rule_priority(&mut self, rule: &str, priority: i32) -> &mut Self;
}
```

Internally, maintain `priorities: HashMap<String, i32>`. The filter applies
at tree-walk time, not at chart-construction time — `EarleyParser::parse`
remains untouched.

**Filter semantics (two flavors, pick one):**

- **4a — "best-at-each-node":** when `ForestIterator::source_index` is about
  to choose among multiple backpointers for a span, filter `span.sources()`
  down to the subset whose top-level rule has the maximum priority. Ties keep
  all tied sources (still enumerable). This is simple and local; the filtered
  set is computed once per span visit.
- **4b — "forest-wide dominance":** a competing sub-parse can be pruned only
  when a strictly-higher-priority alternative covers the *same* input span
  and derives the *same* non-terminal. This requires a pre-pass over the
  chart to group spans by `(nonterm, start, end)` and mark dominated
  backpointers.

**Recommended sequence:** land 4a first (local filter, ~30 lines in the
`ForestIterator` source-selection step, no pre-pass). Add 4b only if 4a turns
out to miss cases we care about.

**Tests:**

- The unmatched-paren grammar from the worked example
  ([docs/plans/iterative-parse-forest.md](iterative-parse-forest.md) context):
  set `'(' expr ')'` to priority 10 and the unmatched rules to 1; assert that
  `( 1 )` yields **1** tree instead of 2, `( 1 + 2 )` yields Catalan-only (no
  paren doubling).
- An ambiguous grammar with no priorities set behaves identically to today
  (baseline regression).

### Phase 5 — EBNF annotation layer for rule priorities

Extend the EBNF grammar ([ebnf.rs](../../earlgrey/src/ebnf.rs)) to accept a
per-alternative priority annotation, and have `into_grammar()` wire the
priorities into the `EarleyForest` that consumers construct (or expose them
on `Grammar` so the user can forward).

**Proposed syntax** (aligned with the existing `@tag` syntax the tokenizer
already accepts):

```
expr := expr '+' expr
      | expr '*' expr
      | int
      | '(' expr ')'    @prio(10)
      | '(' expr        @prio(1)
      | expr ')'        @prio(1) ;
```

The annotation attaches to the preceding alternative, not the whole rule.
Parser emits `(rule_string, priority)` pairs; they're stored on `Grammar` as
`rule_priorities: HashMap<String, i32>`. At `EarleyForest` construction, a
convenience `EarleyForest::with_priorities_from(&grammar)` initializes the
priority map in one call.

**Deliberate non-scope:** no `%prec` / `%assoc` on operators, no staircase
rewrite. The staircase rewrite is a separate feature (grammar-level, not
forest-level) and is called out as future work below.

**Tests:**

- EBNF round-trip: annotations parse, attach to the correct alternative, and
  survive the desugaring of `[ ]` / `{ }` / `( )` into auxiliary non-terminals.
- End-to-end: the expression grammar from the test corpus, with priorities
  set in EBNF, produces the expected disambiguated forest.

## Non-goals

- **Lazy chart construction.** `EarleyParser::parse` remains eager; streaming
  the chart itself is a separate, much larger refactor (self-referential
  iterator holding tokenizer state, etc.). Not justified until there's
  evidence the chart — not the tree count — is the bottleneck.
- **Operator-precedence / associativity rewrites.** Classic staircase-style
  rewrite for `E → E + E | E * E` into stratified precedence levels is out
  of scope here. It's grammar-level, independent of the forest-filter work,
  and warrants its own plan when we need it.
- **Ranking / best-tree selection beyond rule priority.** If callers need
  richer ranking (score functions over whole trees), they use phase-2 iter
  and rank externally.
- **Deprecating `eval_all` / `eval_all_recursive`.** Kept for back-compat.

## Open questions

- Phase 1a's "reject re-entry of the same `SpanSource`" behavior may differ
  subtly from the recursive walker's *branch-local* explored set in contrived
  ambiguous grammars. Worth a small worked example before committing to the
  strict variant.
- Should `max_steps_per_tree` have a sane default (e.g. 100_000) or be opt-in?
  Leaning opt-in to avoid surprising existing callers.
- Phase 4a vs 4b semantics for priorities: is "highest-priority source among
  a span's own backpointers" strong enough for the unmatched-paren case, or
  does the matched/unmatched competition need the forest-wide dominance
  check of 4b? Worth verifying on the worked example before implementing.
- Annotation attachment in phase 5: per-alternative vs per-whole-rule. The
  draft proposes per-alternative since the paren case needs that granularity,
  but it means the EBNF parser must track position-within-alternation.
