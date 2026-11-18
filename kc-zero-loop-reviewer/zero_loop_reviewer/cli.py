"""Command-line entry points.

``serve``      the karucode plugin (JSON-RPC over stdio); the default.
``review``     one local review, no posting.
``ci``         one webhook-shaped review, optionally posted once.
``webhook``    a loopback receiver that turns deliveries into reviews.
``config``     print the effective configuration (no secrets).
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import sys
import threading
import time
from dataclasses import replace
from pathlib import Path
from typing import Sequence

from .config import Settings, load_settings
from .errors import ReviewError
from .models import Decision, ReviewVerdict
from .pipeline import ReviewRequest, Reviewer
from .plugin import manifest as manifest_mod
from .plugin.protocol import Plugin, serve
from .plugin.tools import panels, render_for_posting, summary, tools
from .providers.base import build_provider
from .providers import webhook as webhook_mod

LOG = logging.getLogger("zero_loop_reviewer")

EXIT_APPROVE = 0
EXIT_CHANGES = 1
EXIT_HUMAN = 2


def _settings(args: argparse.Namespace) -> Settings:
    config = Path(args.config).expanduser() if getattr(args, "config", None) else None
    settings = load_settings({}, os.environ, config)
    state = getattr(args, "state_dir", None)
    if state:
        settings = settings.with_paths(Path(state).expanduser())
    if getattr(args, "provider", None):
        settings = replace(settings, ci=replace(settings.ci, provider=args.provider))
    return settings


def _exit(verdict: ReviewVerdict) -> int:
    return {
        Decision.APPROVE: EXIT_APPROVE,
        Decision.REQUEST_CHANGES: EXIT_CHANGES,
        Decision.HUMAN_REVIEW: EXIT_HUMAN,
    }[verdict.decision]


def _write_verdict(verdict: ReviewVerdict, out_dir: Path | None) -> Path | None:
    if out_dir is None:
        return None
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    path = out_dir / f"review-{verdict.review_id}.json"
    path.write_text(json.dumps(verdict.model_dump(mode="json"), indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return path


def _parse_stacked(values: Sequence[str] | None) -> tuple:
    from .models import StackedBranch

    out = []
    for item in values or ():
        branch, separator, sha = str(item).partition("=")
        branch = branch.strip()
        if not branch:
            continue
        out.append(StackedBranch(branch=branch, sha=sha.strip() if separator else "", role="intermediate"))
    return tuple(out)


def _run(args: argparse.Namespace, *, description: str = "", provider=None, expected_head: str = "", repository: str = "", number: int = 0) -> ReviewVerdict:
    settings = _settings(args)
    workspace = Path(getattr(args, "workspace", None) or os.getcwd()).expanduser().resolve()
    if not workspace.is_dir():
        raise SystemExit(f"workspace does not exist: {workspace}")
    request = ReviewRequest(
        workspace=workspace,
        base_ref=getattr(args, "base", "") or "",
        head_ref=getattr(args, "head", "HEAD") or "HEAD",
        expected_head_sha=expected_head or getattr(args, "expected_head_sha", "") or "",
        repository=repository or getattr(args, "repository", "") or "",
        pull_number=number or int(getattr(args, "number", 0) or 0),
        description=description,
        stacked=_parse_stacked(getattr(args, "stacked", None)),
        provider=provider,
    )
    reviewer = Reviewer(settings, environ=os.environ)
    return reviewer.review(request)


def cmd_serve(args: argparse.Namespace) -> int:
    logging.basicConfig(
        level=getattr(logging, str(getattr(args, "log_level", "info")).upper(), logging.INFO),
        stream=sys.stderr,
    )
    return serve(Plugin(tools=tools(), panels=panels()))


def cmd_review(args: argparse.Namespace) -> int:
    logging.basicConfig(level=getattr(logging, str(args.log_level).upper(), logging.INFO), stream=sys.stderr)
    verdict = _run(args, description=_read_text(args.description_file))
    settings = _settings(args)
    body, _payload = render_for_posting(verdict, settings)
    _write_verdict(verdict, Path(args.out_dir) if args.out_dir else None)
    if args.json:
        print(json.dumps(verdict.model_dump(mode="json"), indent=2))
    else:
        print(summary(verdict, body=body))
    return _exit(verdict)


def _read_text(path: str | None) -> str:
    if not path:
        return ""
    return Path(path).expanduser().read_text(encoding="utf-8")


def cmd_ci(args: argparse.Namespace) -> int:
    logging.basicConfig(level=getattr(logging, str(args.log_level).upper(), logging.INFO), stream=sys.stderr)
    settings = _settings(args)
    provider = build_provider(settings, os.environ)
    event = None
    if args.payload:
        body = Path(args.payload).expanduser().read_bytes()
        headers = {}
        if args.head_sha:
            headers = {"x-github-event": "pull_request", "x-gitlab-event": "Merge Request Hook"}
        event = webhook_mod.parse(provider.name, body, headers)
        if event.kind != "review":
            print(json.dumps({"ignored": True, "reason": event.reason or "not a review action"}))
            return EXIT_APPROVE
        repository = event.repository
        number = event.number
        expected = event.head_sha
    else:
        repository = args.repository
        number = int(args.number or 0)
        expected = args.head_sha or ""
    if not repository or not number:
        raise SystemExit("ci needs a payload or --repository and --number")
    pull = provider.pull(repository, number)
    verdict = _run(
        args,
        description=pull.description,
        provider=provider,
        expected_head=expected,
        repository=repository,
        number=number,
    )
    body, payload = render_for_posting(verdict, settings)
    if not args.no_post:
        provider.post_review(repository, number, payload)
    if args.out_dir:
        _write_verdict(verdict, Path(args.out_dir))
    print(summary(verdict, body=body))
    return _exit(verdict)


def cmd_webhook(args: argparse.Namespace) -> int:
    logging.basicConfig(level=getattr(logging, str(args.log_level).upper(), logging.INFO), stream=sys.stderr)
    settings = _settings(args)
    secret = os.environ.get(settings.ci.webhook_secret_env, "")
    if not secret:
        raise SystemExit(f"set ${settings.ci.webhook_secret_env} to the webhook secret")
    provider = build_provider(settings, os.environ)
    lock = threading.Lock()

    def handle(event) -> None:
        if event.kind != "review":
            LOG.info("ignored delivery: %s", event.reason)
            return
        if not args.workspace:
            LOG.error("webhook reviews need --workspace for the checked-out repository")
            return
        with lock:
            try:
                verdict = _run(
                    args,
                    provider=provider,
                    expected_head=event.head_sha,
                    repository=event.repository,
                    number=event.number,
                )
                body, payload = render_for_posting(verdict, settings)
                if not args.no_post:
                    provider.post_review(event.repository, event.number, payload)
                _write_verdict(verdict, Path(args.out_dir) if args.out_dir else None)
                LOG.info("review %s finished: %s", verdict.review_id, verdict.decision.value)
            except Exception as exc:  # a failed delivery must not kill the receiver
                LOG.exception("webhook review failed: %s", exc)

    server = webhook_mod.WebhookServer(
        host=args.host,
        port=int(args.port),
        secret=secret,
        provider=provider.name,
        handler=handle,
    )
    server.start()
    print(json.dumps({"listening": f"http://{args.host}:{server.port}", "provider": provider.name}))
    try:
        while True:
            time.sleep(0.25)
    except KeyboardInterrupt:
        pass
    finally:
        server.stop()
    return EXIT_APPROVE


def cmd_config(args: argparse.Namespace) -> int:
    settings = _settings(args)
    data = {
        "model": settings.llm.primary.model,
        "stages": {
            "triage": settings.llm.triage.model,
            "security": settings.llm.security.model,
            "probe": settings.llm.probe.model,
            "reviewer": settings.llm.reviewer.model,
            "monitor": settings.llm.monitor.model,
            "monitor_alt": settings.llm.monitor_alt.model if settings.llm.monitor_alt else None,
        },
        "base_url": settings.llm_base_url(os.environ) or "<unset>",
        "api_key_env": settings.llm.api_key_env,
        "api_key_present": bool(settings.api_key(os.environ)),
        "fusion": {
            "tau": settings.fusion.tau,
            "c_fr": settings.fusion.c_fr,
            "c_fa": settings.fusion.c_fa,
            "monitor_weight": settings.fusion.monitor_weight,
            "prior_log_odds": settings.fusion.prior_log_odds,
        },
        "deadline_s": settings.deadline_s,
        "stage2_timeout_s": settings.stage2_timeout_s,
        "max_model_tokens": settings.max_model_tokens,
        "max_findings": settings.max_findings,
        "sandbox": {
            "runtime": settings.sandbox.runtime,
            "image": settings.sandbox.image,
            "timeout_s": settings.sandbox.timeout_s,
        },
        "sast": {"semgrep_config": settings.sast.semgrep_config or "<shipped rules>"},
        "ci": {
            "provider": settings.ci.provider,
            "token_env": settings.ci.token_env,
            "token_present": bool(os.environ.get(settings.ci.token_env)),
        },
        "state_dir": str(settings.paths.state_dir),
        "receipts_dir": str(settings.paths.receipts_dir),
        "private_log": str(settings.paths.private_log),
        "manifest": {"name": manifest_mod.NAME, "version": manifest_mod.VERSION},
    }
    print(json.dumps(data, indent=2))
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="zero-loop-reviewer", description=__doc__)
    parser.add_argument("--version", action="version", version=f"{manifest_mod.NAME} {manifest_mod.VERSION}")
    sub = parser.add_subparsers(dest="command")

    def common(p: argparse.ArgumentParser) -> None:
        p.add_argument("--config", help="TOML config file (CI mode)")
        p.add_argument("--state-dir", help="receipts/worktrees/private log root")
        p.add_argument("--log-level", default="info", choices=["debug", "info", "warning", "error"])

    serve_p = sub.add_parser("serve", help="run the karucode plugin")
    common(serve_p)

    review_p = sub.add_parser("review", help="one local review")
    common(review_p)
    review_p.add_argument("--workspace", default="")
    review_p.add_argument("--base", required=True)
    review_p.add_argument("--head", default="HEAD")
    review_p.add_argument("--expected-head-sha", default="")
    review_p.add_argument("--description-file", default="")
    review_p.add_argument("--stacked", action="append", default=[], help="branch=sha of an intermediate stacked branch")
    review_p.add_argument("--json", action="store_true")
    review_p.add_argument("--out-dir", default="")

    ci_p = sub.add_parser("ci", help="one webhook-shaped review, optionally posted")
    common(ci_p)
    ci_p.add_argument("--payload", default="", help="saved webhook JSON")
    ci_p.add_argument("--provider", default="", choices=["", "github", "gitlab", "bitbucket"])
    ci_p.add_argument("--workspace", default="")
    ci_p.add_argument("--repository", default="")
    ci_p.add_argument("--number", type=int, default=0)
    ci_p.add_argument("--head-sha", default="")
    ci_p.add_argument("--stacked", action="append", default=[], help="branch=sha of an intermediate stacked branch")
    ci_p.add_argument("--no-post", action="store_true")
    ci_p.add_argument("--out-dir", default="")

    hook_p = sub.add_parser("webhook", help="loopback webhook receiver")
    common(hook_p)
    hook_p.add_argument("--provider", required=True, choices=["github", "gitlab", "bitbucket"])
    hook_p.add_argument("--workspace", default="")
    hook_p.add_argument("--host", default="127.0.0.1")
    hook_p.add_argument("--port", type=int, default=8791)
    hook_p.add_argument("--no-post", action="store_true")
    hook_p.add_argument("--out-dir", default="")

    config_p = sub.add_parser("config", help="print effective configuration")
    common(config_p)
    config_p.add_argument("--provider", default="", choices=["", "github", "gitlab", "bitbucket"])
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    effective = list(argv) if argv is not None else sys.argv[1:]
    if not effective:
        # karucode spawns the plugin with no arguments; "serve" is the default
        # and must carry the same parsed attributes as an explicit subcommand.
        effective = ["serve"]
    args = parser.parse_args(effective)
    command = args.command or "serve"
    try:
        if command == "serve":
            return cmd_serve(args)
        if command == "review":
            return cmd_review(args)
        if command == "ci":
            return cmd_ci(args)
        if command == "webhook":
            return cmd_webhook(args)
        if command == "config":
            return cmd_config(args)
    except ReviewError as exc:
        print(f"zero-loop-reviewer: {exc.kind.value}: {exc.message}", file=sys.stderr)
        return EXIT_HUMAN
    parser.error(f"unknown command {command}")
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
