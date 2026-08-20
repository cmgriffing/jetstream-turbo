# Agent Instructions


## Benchmark Baselines

Do not modify or vendor dependencies

Make only minimal changes to tests and avoid modifying actual test logic unless absolutely necessary.

Do not update Criterion baselines from a feature branch worktree.

When benchmark baselines need refreshing, create an isolated clean worktree from `main`, run the benchmark suite there, and update `benches/baselines/` from that main-branch output. Then return to the feature branch and run the benchmark check against those refreshed main baselines.

This keeps benchmark comparisons anchored to current `main` instead of accumulated local or feature-branch drift.
