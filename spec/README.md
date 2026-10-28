# Protocol specs

Quint is the single source for protocol specs. TLA+ is
generated from it and checked with TLC and Apalache. Quint traces drive the
Rust implementation's model-based tests.

## Contents

- `lease.qnt`: the lease/claim protocol: claims, heartbeats, expiry,
  compare-and-swap commits, provisional offline leases and reconcile.
  - **Invariants:** `mutualExclusion`, `expiredNeverCommits`,
    `noSilentOverwrite`.
  - **Checker test:** `BUGS` switches on deliberate bugs, so the same spec
    shows that the checker catches them.
- `traces/*.itf.json`: 20 random and 4 directed traces of the correct spec,
  together covering every action.
- `traces/bug/`, `traces/apalache/`: counterexamples from the buggy variants
  (simulator and shortest Apalache traces).

## How the traces are used

`crates/mdkg-coord/tests/quint_traces.rs` replays every trace against the
coordinator and compares state after each step:

- The coordinator must follow every correct trace.
- It must refuse every counterexample before reaching the violating state.

The spec models a lease as covering one node, with no git. The coordinator
also supports subtree scopes and settlement through git (computed snapshots,
changesets, observations of main, abandon);
`crates/mdkg-coord/tests/simulation.rs` covers those with seeded
deterministic simulation against a ghost main branch, and
`crates/mdkg-coord-sqlite/tests/worktrees.rs` runs them on real git
worktrees.

## Last checker results (spike, 2026-10-18)

- TLC: every reachable state of the small instance (2 agents, 1 node,
  11.2M states) in 38 s, no violation.
- Apalache: no violation up to 8 steps (24 min; 6 steps take 58 s).
- Each of the 4 `BUGS` instances is caught by its invariant; Apalache's
  shortest counterexamples have 4 to 7 steps.

## Running the checkers

The checkers need Quint 0.33.0, Apalache 0.62.1 and Java 17+. Point
`JAVA_HOME` at any JDK 17+ for these commands only.

```bash
npx @informalsystems/quint@0.33.0 typecheck spec/lease.qnt
npx @informalsystems/quint@0.33.0 run spec/lease.qnt --main=sim --invariant=safety --max-samples=10000
npx @informalsystems/quint@0.33.0 verify spec/lease.qnt --main=mc --invariant=safety --backend=tlc
```

Regenerating traces uses `quint run --mbt --out-itf`; see the spike's
`check.sh` for the exact commands. `scripts/check.sh` doesn't run the
checkers, because they need Java. The trace replay runs in `cargo test`.
