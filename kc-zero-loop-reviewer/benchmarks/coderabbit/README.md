# CodeRabbit benchmark

`pr17.json` is a snapshot of CodeRabbit's inline comments on karutoil/karucode#17
at head `68fa207f5b4bad10c5e6218c95d247e4205d1d37`.

Reproduce the comparison:

```sh
gh api repos/OWNER/REPO/pulls/N/comments --paginate \
  --jq '.[] | select(.user.login=="coderabbitai[bot]") | {user,path,line,original_line,body,commit_id,created_at}' \
  > benchmarks/coderabbit/pr17.json

python3 tools/compare_reviews.py \
  --coderabbit benchmarks/coderabbit/pr17.json \
  --verdict /path/to/zero-loop.json \
  --head 68fa207f5b4bad10c5e6218c95d247e4205d1d37 \
  --out benchmarks/coderabbit/pr17-report.md
```

The report counts matches and unmatched items. **Do not use the counts as a
winner.** CodeRabbit publishes unverified semantic suggestions; zero-loop
publishes only mechanically verified findings. Label the unmatched items
(`valid: true|false`) before comparing recall or precision.

At the time of the first snapshot, CodeRabbit had 11 unique comments on PR #17,
6 of them on the current head. Zero-loop's run at the same head published 0
verified findings. That is the current gap: the six head comments are mostly
Rust/TypeScript semantic issues, while zero-loop's executable proof path is
Python-only. The next step is either language-aware test execution or a clearly
labelled advisory tier; do not silently lower the verification bar.
