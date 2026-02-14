# Agent Instructions

This project uses **bd** (beads) for issue tracking. Run `bd onboard` to get started.

## Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --status in_progress  # Claim work
bd close <id>         # Complete work
bd sync               # Sync with git
```

## Landing the Plane (Session Completion)

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds, benchmarks
3. **STOP IF PERFORMANCE REGRESSES** - If you've introduced a performance regression, stop and fix it
4. **Hand off** - Provide context for next session

# CRITICAL NOTES

- DO NOT COMMIT OR PUSH ANYTHING
