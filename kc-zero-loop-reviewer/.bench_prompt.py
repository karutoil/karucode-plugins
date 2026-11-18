import json
import sqlite3
import sys
import time
from pathlib import Path

import httpx

sys.path.insert(0, "/home/karutoil/karucode-plugins/kc-zero-loop-reviewer")
import zero_loop_reviewer.pipeline as pipeline
from zero_loop_reviewer.config import load_settings
from zero_loop_reviewer.gitops import GitRepo
from zero_loop_reviewer.llm.prompts import behavioral_prompt
from zero_loop_reviewer.stage1 import context as ctxmod

pipeline.MAX_BEHAVIOR_BATCH_CHARS = 30000
pipeline.MAX_BEHAVIOR_FILES_PER_BATCH = 4
repo = GitRepo(Path("/home/karutoil/karucode"))
diff = repo.diff(
    "64a4acc9e20ce45898ae514692f773d19901d97f",
    "68fa207f5b4bad10c5e6218c95d247e4205d1d37",
)
ctx = ctxmod.build_context(repo, diff, load_settings({}, {}))
batches, _, _ = pipeline._behavioral_batches(diff)
batch = batches[0]
prompt = behavioral_prompt(
    diff.model_copy(update={"files": batch}),
    pipeline._context_for_batch(ctx, {p.path for p in batch}),
)
conn = sqlite3.connect(
    "file:/home/karutoil/.local/share/karucode/karucode.db?mode=ro", uri=True
)
key = json.loads(
    conn.execute("select value from settings where key='model.providers'").fetchone()[0]
)[0]["api_key"]
body = {
    "model": "deepseek-v4.1-flash",
    "messages": [{"role": "user", "content": prompt.user}],
    "max_tokens": 65536,
    "temperature": 0,
    "reasoning_effort": "high",
    "response_format": {"type": "json_object"},
}
started = time.monotonic()
try:
    response = httpx.post(
        "https://omni.karutoil.site/v1/chat/completions",
        headers={"Authorization": f"Bearer {key}"},
        json=body,
        timeout=600,
    )
    elapsed = round(time.monotonic() - started, 1)
    doc = response.json()
    content = ((doc.get("choices") or [{}])[0].get("message") or {}).get("content", "")
    print(
        json.dumps(
            {
                "status": response.status_code,
                "seconds": elapsed,
                "chars": len(prompt.user),
                "usage": doc.get("usage"),
                "content_chars": len(content),
                "head": content[:120],
            }
        )
    )
except Exception as exc:
    print(json.dumps({"error": f"{type(exc).__name__}: {exc}", "seconds": round(time.monotonic() - started, 1)}))
