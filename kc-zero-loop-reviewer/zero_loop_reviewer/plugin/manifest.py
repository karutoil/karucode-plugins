"""The plugin's manifest and tool declarations.

The host reads this from ``initialize``; ``plugin.toml`` is the installation
form.  Tool descriptions are written for a model: they say what the tool does,
what it costs (a full review), and that it writes receipts.
"""

from __future__ import annotations

from typing import Any

from ..config import SETTING_DECLARATIONS

NAME = "zero-loop-reviewer"
VERSION = "1.0.0"
PROTOCOL = "1"
DESCRIPTION = (
    "Zero-loop, grounded CI code review: static/CPG ensembles, sandboxed "
    "dual-commit reproduction probes, causal falsification, and one committable "
    "suggestion per finding (all verified findings by default, ceiling 50)."
)

TOOLS: list[dict[str, Any]] = [
    {
        "name": "review_pr",
        "description": (
            "Run the five-stage grounded review on a pull request or a local "
            "base..head range. Produces mechanically verified findings, each "
            "with a one-click suggestion, up to the configured publication cap "
            "(all verified findings by default, ceiling 50); approval is never "
            "based on a raw-diff opinion. Takes 1-4 minutes and writes durable "
            "receipts. Set post=true to publish the single review through the "
            "configured Git provider (default false)."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "workspace": {
                    "type": "string",
                    "description": "Absolute repository path; defaults to the session workspace.",
                },
                "base": {
                    "type": "string",
                    "description": "Base commit, branch or tag (the merge base is computed).",
                },
                "head": {
                    "type": "string",
                    "description": "Head commit or ref; defaults to HEAD.",
                    "default": "HEAD",
                },
                "expected_head_sha": {
                    "type": "string",
                    "description": "Refuse the review unless this exact SHA is HEAD.",
                },
                "repository": {
                    "type": "string",
                    "description": "Provider repository id (owner/name, project path); needs pull_number.",
                },
                "pull_number": {"type": "integer", "description": "Pull/merge request number."},
                "description": {
                    "type": "string",
                    "description": "PR description. Used only by the message/code consistency check.",
                },
                "stacked_branches": {
                    "type": "array",
                    "description": "Intermediate stacked branches whose exact SHA must also pass the gate.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "branch": {"type": "string"},
                            "sha": {"type": "string"},
                        },
                        "required": ["branch", "sha"],
                    },
                },
                "post": {
                    "type": "boolean",
                    "description": "Publish one review to the provider. Default false.",
                    "default": False,
                },
            },
            "required": ["base"],
        },
        "class": "shell",
    },
    {
        "name": "review_status",
        "description": (
            "Read a stored review verdict and verify its receipt hash chain. "
            "Read-only; costs nothing."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "review_id": {
                    "type": "string",
                    "description": "Review id; omit for the newest review.",
                }
            },
        },
        "class": "read_only",
    },
    {
        "name": "explain_finding",
        "description": (
            "Read the private diagnostic for one redacted security finding "
            "(full trace and verified patch). Read-only; operator-facing."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "review_id": {"type": "string"},
                "finding_id": {"type": "string", "description": "The finding/private record id."},
            },
            "required": ["review_id", "finding_id"],
        },
        "class": "read_only",
    },
]

PANELS = [
    {
        "id": "review_status",
        "title": "Zero-loop review",
        "icon": "shield",
        "source": "static",
        "refresh_ms": 0,
    }
]


def _host_setting(declaration: dict[str, Any]) -> dict[str, Any]:
    value: dict[str, Any] = {
        "key": declaration["key"],
        "label": declaration["label"],
        "type": declaration["type"],
        "default": declaration["default"],
        "help": declaration.get("help", ""),
    }
    if declaration.get("min") is not None:
        value["min"] = declaration["min"]
    if declaration.get("max") is not None:
        value["max"] = declaration["max"]
    if declaration.get("options"):
        value["options"] = declaration["options"]
    return value


def manifest() -> dict[str, Any]:
    return {
        "name": NAME,
        "version": VERSION,
        "description": DESCRIPTION,
        "protocol": PROTOCOL,
        "capabilities": ["tools", "panels"],
        "permissions": ["files.read", "files.write", "shell", "network"],
        "hooks": [],
        "panels": PANELS,
        "settings": [_host_setting(item) for item in SETTING_DECLARATIONS[:32]],
    }


def tool_defs() -> list[dict[str, Any]]:
    return TOOLS
