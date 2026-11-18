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

limit = int(sys.argv[1]) if len(sys.argv) > 1 else 10000
effort = sys.argv[2] if len(sys.argv) > 2 else "high"
pipeline.MAX_BEHAVIOR_BATCH_CHARS = limit
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
    "messages": prompt.messages(),
    "max_tokens": 65536,
    "temperature": 0,
    "reasoning_effort": effort,
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
    text = response.text
    print(
        json.dumps(
            {
                "status": response.status_code,
                "seconds": elapsed,
                "chars": len(prompt.user),
                "content_type": response.headers.get("content-type"),
                "text_head": text[:400],
                "text_len": len(text),
            }
        )
    )
except Exception as exc:
    print(json.dumps({"error": f"{type(exc).__name__}: {exc}", "seconds": round(time.monotonic() - started, 1)}))
