# CodeRabbit vs zero-loop

- CodeRabbit issues: **6** (0 labelled valid)
- zero-loop findings: **0** (0 labelled valid)
- matched: **0**
- CodeRabbit-only: **6**
- zero-loop-only: **0**

> Counts are not a quality score. Label the unmatched items before drawing a conclusion.

## Matched

_No matched issues._

## CodeRabbit-only

| id | severity | file:line | title | label |
| --- | --- | --- | --- | --- |
| cr-1 | major | `crates/kc-provider/src/request.rs:200` | Preserve variant precedence across output-limit aliases. | unlabelled |
| cr-4 | major | `crates/kc-runtime/src/history.rs:176` | Snapshot hydration duplicates messages after a command receipt. | unlabelled |
| cr-5 | major | `web/src/components/Composer.tsx:879` | Block submission when a leading command has attachments. | unlabelled |
| cr-6 | major | `web/src/components/SettingsSheet.tsx:722` | Omit `variants` when it is undeclared. The server rejects `null`. | unlabelled |
| cr-9 | major | `crates/kc-runtime/src/agent.rs:724` | Invalidate legacy compaction checkpoints before the new message count is used. | unlabelled |
| cr-10 | major | `web/src/state/chat.ts:580` | Guard prose-to-row matching against partial session windows. | unlabelled |

## zero-loop-only

_None._

## How to label

Set `valid` to `true` or `false` on each id in the labels JSON, then rerun. A reviewer comment that is technically true but irrelevant to the change is invalid. A zero-loop finding that is true but not mechanically verified is still a miss until proven.
