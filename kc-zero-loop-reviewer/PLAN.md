# Implementation plan and progress

Owner: Lazarus / karucode agent. Update this file as work lands so a fresh turn
can resume without re-reading the transcript.

## Phase 0 — discovery (done)
- [x] Harness plugin registration, protocol, settings, approvals — see ARCHITECTURE.md §1
- [x] LLM routing — OpenAI-compatible HTTP, per-stage model config — §2
- [x] CI webhooks / Git APIs / sandbox runtime (podman, `--uts=host`, /home paths) — §3
- [x] Attachment map for all five stages — §4
- [x] Scaffold Python package, manifest, launcher, tests directory
- [x] Live registration on a throw-away karucode server: plugin ready, 3 tools, 32 settings

## Phase 1 — Stage 1 (done)
- [x] models/config/errors/receipts shared contracts
- [x] six-property runtime contract + fault-injection tests (truncated 1%/50%/n-1,
      cancel at write/read, timeout, nonzero-exit-with-JSON, empty/malformed,
      write flags, workspace mutation, pool disjointness)
- [x] head-commit / branch-stack gatekeeper (`gate.require_approval` opt-in)
- [x] PR-MCI isolation + description↔AST consistency (author text reaches only the PMI prompt)
- [x] 1-hop context slicer + token compressor + conventions/DesignLore loading

## Phase 2 — Stage 2 (done)
- [x] async parallel executor (≤60 s, partial failure tolerated)
- [x] patch-caused SAST delta (Semgrep + shipped rules) + suppressions
- [x] secSemScan monologue + fresh line reconciliation + CPG frames
- [x] analyzer taint/lifetime tuples attached to frames (redaction/ObliVul input)
- [x] RALE AST/CFG/Code boolean ensemble + diff-anchored behavioral candidates

## Phase 3 — Stage 3 (done)
- [x] assertion probe synthesis with reflexion (≤3)
- [x] podman dual-commit execution (base vs head), typed results, in-container hash check
- [x] stdlib probe runner fallback for images without pytest
- [x] setup-error filter, fail-on-base / regression retention, head-only CIVER path for new code
- [x] 5-vote grounded fallback

## Phase 4 — Stage 4 (done)
- [x] CIVER perturbation x' + defect removal + compile/no-op checks
- [x] ObliVul bidirectional LSOG counterfactual with the already-maximal ceiling case
- [x] DesignLore / AGENTS.md precedent filter

## Phase 5 — Stage 5 (done)
- [x] policy-free risk elicitation + leakage test (schema descriptions stripped, word-boundary matcher)
- [x] monitor ensemble + deterministic fusion + coded Bayes τ
- [x] configurable publication cap, one-click suggestion (post-image hunk), diff fallback for multi-hunk
- [x] AFCRA redaction, 0600 private record with verified patch, public gate status only

## Phase 6 — end to end (done)
- [x] plugin protocol subprocess test + `tools/call review_pr` full run
- [x] CI one-shot mode + provider adapters + webhook receiver
- [x] annotated corpus: 6 buggy / 1 fixed / 3 clean; 6/6 intercepted, 0/4 false positives
- [x] README, install notes, `check.sh`
- [x] Full suite: 444 tests (see `./check.sh`)
- [x] change-graph decomposition, unit-aware batches, cross-unit integration pass
