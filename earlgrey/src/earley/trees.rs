#![deny(warnings)]

use super::grammar::Grammar;
use super::parser::ParseTrees;
use super::spans::{Span, SpanSource};
use std::collections::HashMap;
use std::rc::Rc;

// Hard ceilings so cyclic ("bottomless") grammars fail fast with a typed
// error instead of panicking or looping. Phase 3 will make these
// configurable per-EarleyForest; phase 1a will add actual cycle detection
// on the iterative path.
const MAX_WALKER_RECURSION: u16 = 100;
const MAX_EVAL_ONE_STEPS: usize = 100_000;

pub struct EarleyForest<'a, ASTNode: Clone> {
    // Semantic actions to apply when a production is completed
    actions: HashMap<String, Box<dyn Fn(Vec<ASTNode>) -> ASTNode + 'a>>,
    // How to lift a 'scanned' terminal into an AST node.
    terminal_parser: Box<dyn Fn(&str, &str) -> ASTNode + 'a>,
    // Per-tree walk step cap overriding MAX_EVAL_ONE_STEPS. `None` keeps
    // the built-in safety net; `Some(n)` lets callers tighten or relax it.
    max_steps_per_tree: Option<usize>,
    // Per-rule priorities (keyed by canonical `"head -> sym1 sym2 ..."`).
    // Unset rules default to 0. Higher wins when competing parses of the
    // same constituent are both derivable.
    priorities: HashMap<String, i32>,
}

impl<'a, ASTNode: Clone> EarleyForest<'a, ASTNode> {
    pub fn new(terminal_parser: impl Fn(&str, &str) -> ASTNode + 'a) -> Self {
        EarleyForest {
            actions: HashMap::new(),
            terminal_parser: Box::new(terminal_parser),
            max_steps_per_tree: None,
            priorities: HashMap::new(),
        }
    }

    // Register semantic actions to act when rules are matched
    pub fn action(&mut self, rule: &str, action: impl Fn(Vec<ASTNode>) -> ASTNode + 'a) {
        self.actions.insert(rule.to_string(), Box::new(action));
    }

    /// Cap the number of tree-walk steps spent producing a single tree.
    /// Overrides the library's built-in safety ceiling. When exceeded,
    /// `eval_one` / `eval` / `eval_all` / `eval_iter` return `Err`
    /// instead of continuing.
    pub fn max_steps_per_tree(&mut self, max: usize) -> &mut Self {
        self.max_steps_per_tree = Some(max);
        self
    }

    /// Assign a priority to a rule by its canonical string form
    /// (e.g. `"expr -> ( expr )"`). When competing parses of the same
    /// constituent are derivable, only the highest-priority alternative
    /// is enumerated. Unset rules share the default priority of 0.
    pub fn rule_priority(&mut self, rule: &str, priority: i32) -> &mut Self {
        self.priorities.insert(rule.to_string(), priority);
        self
    }

    /// Copy any `rule_priorities` that were attached to the grammar
    /// (typically via EBNF `@prio(N)` annotations) into this forest's
    /// priority map. Existing entries with the same key are overwritten.
    pub fn with_priorities_from(&mut self, grammar: &Grammar) -> &mut Self {
        for (rule, &prio) in &grammar.rule_priorities {
            self.priorities.insert(rule.clone(), prio);
        }
        self
    }
}

fn source_priority(src: &SpanSource, priorities: &HashMap<String, i32>) -> i32 {
    match src {
        // A Completion "picks" a child production — score it by the
        // trigger's rule. `Scan` sources don't represent a rule choice,
        // so they share the default priority.
        SpanSource::Completion(_, trigger) => priorities
            .get(&trigger.rule.to_string())
            .copied()
            .unwrap_or(0),
        SpanSource::Scan(_, _) => 0,
    }
}

fn eligible_source_indices(span: &Rc<Span>, priorities: &HashMap<String, i32>) -> Vec<usize> {
    let sources = span.sources();
    if sources.is_empty() {
        return Vec::new();
    }
    if priorities.is_empty() {
        return (0..sources.len()).collect();
    }
    let mut best: Option<i32> = None;
    let mut scored: Vec<(usize, i32)> = Vec::with_capacity(sources.len());
    for (idx, src) in sources.iter().enumerate() {
        let p = source_priority(src, priorities);
        best = Some(best.map_or(p, |b| b.max(p)));
        scored.push((idx, p));
    }
    let best = best.unwrap();
    scored
        .into_iter()
        .filter(|(_, p)| *p == best)
        .map(|(i, _)| i)
        .collect()
}

fn eligible_roots(
    roots: &[Rc<Span>],
    priorities: &HashMap<String, i32>,
) -> Vec<Rc<Span>> {
    if priorities.is_empty() || roots.is_empty() {
        return roots.to_vec();
    }
    let mut best: Option<i32> = None;
    let mut scored: Vec<(Rc<Span>, i32)> = Vec::with_capacity(roots.len());
    for r in roots {
        let p = priorities.get(&r.rule.to_string()).copied().unwrap_or(0);
        best = Some(best.map_or(p, |b| b.max(p)));
        scored.push((r.clone(), p));
    }
    let best = best.unwrap();
    scored
        .into_iter()
        .filter(|(_, p)| *p == best)
        .map(|(r, _)| r)
        .collect()
}

impl<ASTNode: Clone> EarleyForest<'_, ASTNode> {
    fn reduce(&self, root: &Rc<Span>, args: Vec<ASTNode>) -> Result<Vec<ASTNode>, String> {
        // If span is not complete, reduce is a noop passthrough
        if !root.complete() {
            return Ok(args);
        }
        // Lookup semantic action to apply based on rule name
        let rulename = root.rule.to_string();
        match self.actions.get(&rulename) {
            None => Err(format!("Missing Action: {}", rulename)),
            Some(action) => {
                if cfg!(feature = "debug") {
                    eprintln!("Reduction: {}", rulename);
                }
                Ok(vec![action(args)])
            }
        }
    }

    // To write this helper draw a tree of the backpointers and see how they link.
    // - If a span has no sources then its rule progress is at the start.
    // - If it originates from a 'completion' there's a span (the source)
    // that was extended because another (the trigger) completed its rule.
    // Recurse both spans transitively until they have no sources to follow.
    // They will return the 'scans' that happened along the way.
    // - If a span originates from a 'scan' then lift the text into an ASTNode.
    fn walker(&self, root: &Rc<Span>, level: u16) -> Result<Vec<ASTNode>, String> {
        if level >= MAX_WALKER_RECURSION {
            return Err(format!(
                "Bottomless grammar: walker exceeded max recursion depth {}",
                MAX_WALKER_RECURSION
            ));
        }
        let mut args = Vec::new();
        match root.sources().iter().next() {
            Some(SpanSource::Completion(source, trigger)) => {
                args.extend(self.walker(source, level + 1)?);
                args.extend(self.walker(trigger, level + 1)?);
            }
            Some(SpanSource::Scan(source, trigger)) => {
                let symbol = source
                    .next_symbol()
                    .expect("BUG: missing scan trigger symbol")
                    .name();
                args.extend(self.walker(source, level + 1)?);
                args.push((self.terminal_parser)(symbol, trigger));
            }
            None => (),
        }
        self.reduce(root, args)
    }

    // for non-ambiguous grammars this retreieves the only possible parse
    pub fn eval_recursive(&self, ptrees: &ParseTrees) -> Result<ASTNode, String> {
        // walker will always return a Vec of size 1 because root.complete
        Ok(self
            .walker(ptrees.0.first().expect("BUG: ParseTrees empty"), 0)?
            .swap_remove(0))
    }

    fn walker_all(
        &self,
        root: &Rc<Span>,
        level: u16,
        mut explored: Vec<SpanSource>,
    ) -> Result<Vec<Vec<ASTNode>>, String> {
        if level >= MAX_WALKER_RECURSION {
            return Err(format!(
                "Bottomless grammar: walker_all exceeded max recursion depth {}",
                MAX_WALKER_RECURSION
            ));
        }
        let source = root.sources();
        if source.len() == 0 {
            return Ok(vec![self.reduce(root, Vec::new())?]);
        }
        let mut trees = Vec::new();
        for backpointer in source.iter() {
            // Track backpointers we've already explored to avoid looping
            if explored.iter().any(|i| i == backpointer) {
                continue;
            }
            explored.push(backpointer.clone());
            match backpointer {
                SpanSource::Completion(source, trigger) => {
                    // collect left-side-tree of each node
                    for args in self.walker_all(source, level + 1, explored.clone())? {
                        // collect right-side-tree of each node
                        for trig in self.walker_all(trigger, level + 1, explored.clone())? {
                            let mut args = args.clone();
                            args.extend(trig);
                            trees.push(self.reduce(root, args)?);
                        }
                    }
                }
                SpanSource::Scan(source, trigger) => {
                    for mut args in self.walker_all(source, level + 1, explored.clone())? {
                        let symbol = source
                            .next_symbol()
                            .expect("BUG: missing scan trigger symbol")
                            .name();
                        args.push((self.terminal_parser)(symbol, trigger));
                        trees.push(self.reduce(root, args)?);
                    }
                }
            }
        }
        Ok(trees)
    }

    // Retrieves all parse trees
    pub fn eval_all_recursive(&self, ptrees: &ParseTrees) -> Result<Vec<ASTNode>, String> {
        let mut trees = Vec::new();
        for root in &ptrees.0 {
            trees.extend(
                self.walker_all(root, 0, Vec::new())?
                    .into_iter()
                    .map(|mut treevec| treevec.swap_remove(0)),
            );
        }
        Ok(trees)
    }
}

struct ForestIterator {
    // A stack of (span, position-in-eligible-list, cached-eligible-indices).
    // `eligible` is the filtered subset of `span.sources()` indices that the
    // priority filter retained — computed once per first-visit. `position`
    // advances over `eligible`; the index fed to `eval_one` is
    // `eligible[position]`. When `position + 1 == eligible.len()` the span
    // is exhausted and gets popped during `advance`.
    source_idx: Vec<(Rc<Span>, usize, Vec<usize>)>,
}

impl ForestIterator {
    fn source_index(
        &mut self,
        cursor: &Rc<Span>,
        priorities: &HashMap<String, i32>,
    ) -> usize {
        if let Some(entry) = self.source_idx.iter().find(|s| s.0 == *cursor) {
            entry.2[entry.1]
        } else {
            let eligible = eligible_source_indices(cursor, priorities);
            let first = eligible[0];
            self.source_idx.push((cursor.clone(), 0, eligible));
            first
        }
    }

    fn advance(&mut self) -> bool {
        while let Some((span, pos, eligible)) = self.source_idx.pop() {
            if pos + 1 < eligible.len() {
                self.source_idx.push((span, pos + 1, eligible));
                return true;
            }
        }
        false
    }
}

impl<ASTNode: Clone> EarleyForest<'_, ASTNode> {
    /*
    ## S -> S + N | N
    ## N -> [0-9]
    ## "1 + 2"

                 S -> S + N.
                    /  \
                   /    \
              S +.N     N -> [0-9].
               / \             / \
              /   \           /   \
           S.+ N   "+"    .[0-9]   "2"
             /\
            /  \
       .S + N   S -> N.
                  /\
                 /  \
               .N    N -> [0-9].
                       / \
                      /   \
                  .[0-9]   "1"
    */
    fn eval_one(
        &self,
        root: Rc<Span>,
        mut selector: impl FnMut(&Rc<Span>) -> usize,
    ) -> Result<ASTNode, String> {
        let mut args = Vec::new();
        let mut completions = Vec::new();
        let mut spans = vec![root];
        let mut steps: usize = 0;
        let step_limit = self.max_steps_per_tree.unwrap_or(MAX_EVAL_ONE_STEPS);

        while let Some(cursor) = spans.pop() {
            steps += 1;
            if steps > step_limit {
                return Err(format!(
                    "Bottomless grammar: eval_one exceeded max tree-walk steps {}",
                    step_limit
                ));
            }
            // As Earley chart is unwound keep a record of semantic actions to apply
            if cursor.complete() {
                completions.push(cursor.clone());
            }

            // (Reachable) Spans with no sources mean we've unwound to the
            // begining of a production/rule. Apply the rule reducing args.
            if cursor.sources().len() == 0 {
                let completed_rule = &completions
                    .pop()
                    .expect("BUG: span rule never completed")
                    .rule;
                assert_eq!(&cursor.rule, completed_rule);
                // Get input AST nodes for this reduction. Stored reversed.
                let num_rule_slots = completed_rule.spec.len();
                let rule_args = args
                    .split_off(args.len() - num_rule_slots)
                    .into_iter()
                    .rev()
                    .collect();
                // Apply the reduction.
                let rulename = completed_rule.to_string();
                let action = self
                    .actions
                    .get(&rulename)
                    .ok_or(format!("Missing Action: {}", rulename))?;
                args.push(action(rule_args));
            } else {
                let span_source_idx = selector(&cursor);
                // Walk the chart following span sources (back-pointers) of the tree.
                match &cursor.sources()[span_source_idx] {
                    // Completion sources -> Walk the chart.
                    SpanSource::Completion(source, trigger) => {
                        spans.push(source.clone());
                        spans.push(trigger.clone());
                    }
                    // Scan sources -> lift scanned tokens into AST nodes.
                    SpanSource::Scan(source, trigger) => {
                        let symbol = source
                            .next_symbol()
                            .expect("BUG: missing scan trigger symbol")
                            .name();
                        args.push((self.terminal_parser)(symbol, trigger));
                        spans.push(source.clone());
                    }
                }
            }
        }
        assert_eq!(args.len(), 1);
        Ok(args.pop().expect("BUG: mismatched reduce args"))
    }

    pub fn eval(&self, ptrees: &ParseTrees) -> Result<ASTNode, String> {
        // Honor priorities even for the single-tree path: pick the first
        // eligible root, and at each ambiguous span pick the first
        // priority-eligible source.
        let roots = eligible_roots(&ptrees.0, &self.priorities);
        let root = roots
            .into_iter()
            .next()
            .unwrap_or_else(|| ptrees.0.first().expect("BUG: ParseTrees empty").clone());
        let priorities = &self.priorities;
        self.eval_one(root, |s| {
            eligible_source_indices(s, priorities)
                .into_iter()
                .next()
                .unwrap_or(0)
        })
    }

    pub fn eval_all(&self, ptrees: &ParseTrees) -> Result<Vec<ASTNode>, String> {
        self.eval_iter(ptrees).collect()
    }

    /// Enumerate at most `max_trees` parse trees, short-circuiting once the
    /// cap is reached. Equivalent to `eval_iter(ptrees).take(max_trees).collect()`.
    pub fn eval_capped(
        &self,
        ptrees: &ParseTrees,
        max_trees: usize,
    ) -> Result<Vec<ASTNode>, String> {
        self.eval_iter(ptrees).take(max_trees).collect()
    }
}

impl<'a, ASTNode: Clone> EarleyForest<'a, ASTNode> {
    /// Lazy enumeration of all parse trees. Trees are produced one at a
    /// time so callers can `.take(n)`, `.find(...)`, or stream results
    /// without materializing the full (often combinatorial) forest.
    ///
    /// After an `Err` the iterator is fused and yields `None` forever.
    pub fn eval_iter<'s>(
        &'s self,
        ptrees: &ParseTrees,
    ) -> EarleyForestIter<'s, 'a, ASTNode>
    where
        'a: 's,
    {
        let roots = eligible_roots(&ptrees.0, &self.priorities);
        EarleyForestIter {
            forest: self,
            roots: roots.into_iter(),
            state: None,
            done: false,
        }
    }
}

pub struct EarleyForestIter<'f, 'a: 'f, ASTNode: Clone> {
    forest: &'f EarleyForest<'a, ASTNode>,
    roots: std::vec::IntoIter<Rc<Span>>,
    // (root span, walker state, whether eval_one still needs to run once
    // before advance() is meaningful)
    state: Option<(Rc<Span>, ForestIterator, bool)>,
    done: bool,
}

impl<'f, 'a: 'f, ASTNode: Clone> Iterator for EarleyForestIter<'f, 'a, ASTNode> {
    type Item = Result<ASTNode, String>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            let mut st = match self.state.take() {
                Some(st) => st,
                None => match self.roots.next() {
                    None => {
                        self.done = true;
                        return None;
                    }
                    Some(r) => (
                        r,
                        ForestIterator {
                            source_idx: Vec::new(),
                        },
                        true,
                    ),
                },
            };
            let should_yield = if st.2 {
                st.2 = false;
                true
            } else {
                st.1.advance()
            };
            if !should_yield {
                // this root is exhausted; try the next one on the next loop
                continue;
            }
            let root_clone = st.0.clone();
            let priorities = &self.forest.priorities;
            let result = self
                .forest
                .eval_one(root_clone, |s| st.1.source_index(s, priorities));
            self.state = Some(st);
            match result {
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
                Ok(v) => return Some(Ok(v)),
            }
        }
    }
}
