"""Annotated review corpus.

Each buggy case carries a real reproduction probe and a real minimal repair, so
the evaluation exercises the deterministic half of the pipeline end to end:
diff -> candidate -> probe on base and head -> CIVER patch -> suggestion.

The model *answers* are scripted from the annotation (oracle candidate
generation).  This measures interception of the verification/emission pipeline;
it is not a measurement of a live model's recall.  ``run_eval.py --provider live``
uses the real prompts instead, and needs a configured endpoint.
"""

from __future__ import annotations

CASES = [
    {
        "id": "off_by_one_percentile",
        "kind": "buggy",
        "title": "percentile indexes past the end at p=1.0",
        "description": "Adds a percentile helper.",
        "base": {"calc.py": "def add(a, b):\n    return a + b\n"},
        "head": {
            "calc.py": "def add(a, b):\n"
            "    return a + b\n"
            "\n"
            "\n"
            "def percentile(values, p):\n"
            "    ordered = sorted(values)\n"
            "    return ordered[int(len(ordered) * p)]\n"
        },
        "fix": {
            "calc.py": "def add(a, b):\n"
            "    return a + b\n"
            "\n"
            "\n"
            "def percentile(values, p):\n"
            "    if not values:\n"
            '        raise ValueError("empty")\n'
            "    ordered = sorted(values)\n"
            "    index = min(len(ordered) - 1, max(0, int(len(ordered) * p)))\n"
            "    return ordered[index]\n"
        },
        "candidate": {
            "title": "percentile reads one past the end at p=1.0",
            "category": "correctness",
            "severity": 2,
            "path": "calc.py",
            "line": 7,
            "root_cause": "int(len(ordered) * 1.0) equals len(ordered), so the index is out of range",
            "trigger": "percentile([1, 2, 3], 1.0)",
        },
        "probe_path": ".zero/test_percentile.py",
        "probe": "from calc import percentile\n"
        "\n"
        "\n"
        "def test_full_percentile_returns_last():\n"
        "    assert percentile([1, 2, 3], 1.0) == 3\n"
        "\n"
        "\n"
        "def test_zero_percentile_returns_first():\n"
        "    assert percentile([1, 2, 3], 0.0) == 1\n",
        "ground_truth": [{"path": "calc.py", "line": 7}],
    },
    {
        "id": "none_guard_removed",
        "kind": "buggy",
        "title": "slugify lost its None guard",
        "description": "Refactor of slugify.",
        "base": {
            "slug.py": "def slugify(name):\n"
            "    if name is None:\n"
            '        return ""\n'
            '    return name.strip().lower().replace(" ", "-")\n'
        },
        "head": {
            "slug.py": "def slugify(name):\n"
            '    return name.strip().lower().replace(" ", "-")\n'
        },
        "fix": {
            "slug.py": "def slugify(name):\n"
            "    if name is None:\n"
            '        return ""\n'
            '    return name.strip().lower().replace(" ", "-")\n'
        },
        "candidate": {
            "title": "slugify now fails on a missing name",
            "category": "correctness",
            "severity": 2,
            "path": "slug.py",
            "line": 2,
            "root_cause": "the None guard was removed, so name.strip() raises AttributeError",
            "trigger": "slugify(None)",
        },
        "probe_path": ".zero/test_slug.py",
        "probe": "from slug import slugify\n"
        "\n"
        "\n"
        "def test_missing_name_is_empty():\n"
        '    assert slugify(None) == ""\n',
        "ground_truth": [{"path": "slug.py", "line": 1}],
    },
    {
        "id": "empty_items_index",
        "kind": "buggy",
        "title": "first_item crashes on an empty list",
        "description": "Adds first_item.",
        "base": {"cache.py": "def cache_key(value):\n    return str(value)\n"},
        "head": {
            "cache.py": "def cache_key(value):\n"
            "    return str(value)\n"
            "\n"
            "\n"
            "def first_item(payload):\n"
            '    return payload["items"][0]\n'
        },
        "fix": {
            "cache.py": "def cache_key(value):\n"
            "    return str(value)\n"
            "\n"
            "\n"
            "def first_item(payload):\n"
            '    items = payload.get("items") or []\n'
            "    return items[0] if items else None\n"
        },
        "candidate": {
            "title": "first_item raises IndexError for an empty items list",
            "category": "correctness",
            "severity": 2,
            "path": "cache.py",
            "line": 6,
            "root_cause": "payload['items'][0] assumes a non-empty list",
            "trigger": 'first_item({"items": []})',
        },
        "probe_path": ".zero/test_cache.py",
        "probe": "from cache import first_item\n"
        "\n"
        "\n"
        "def test_empty_items_returns_none():\n"
        '    assert first_item({"items": []}) is None\n'
        "\n"
        "\n"
        "def test_present_item_is_returned():\n"
        '    assert first_item({"items": [7]}) == 7\n',
        "ground_truth": [{"path": "cache.py", "line": 6}],
    },
    {
        "id": "mutable_default",
        "kind": "buggy",
        "title": "append_item shares one mutable default list",
        "description": "Adds append_item.",
        "base": {"defaults.py": "def identity(value):\n    return value\n"},
        "head": {
            "defaults.py": "def identity(value):\n"
            "    return value\n"
            "\n"
            "\n"
            "def append_item(item, bucket=[]):\n"
            "    bucket.append(item)\n"
            "    return bucket\n"
        },
        "fix": {
            "defaults.py": "def identity(value):\n"
            "    return value\n"
            "\n"
            "\n"
            "def append_item(item, bucket=None):\n"
            "    if bucket is None:\n"
            "        bucket = []\n"
            "    bucket.append(item)\n"
            "    return bucket\n"
        },
        "candidate": {
            "title": "append_item leaks state between calls",
            "category": "logic",
            "severity": 2,
            "path": "defaults.py",
            "line": 5,
            "root_cause": "a mutable default list is created once and reused",
            "trigger": "append_item(2) after append_item(1)",
        },
        "probe_path": ".zero/test_defaults.py",
        "probe": "from defaults import append_item\n"
        "\n"
        "\n"
        "def test_calls_do_not_share_a_bucket():\n"
        "    assert append_item(1) == [1]\n"
        "    assert append_item(2) == [2]\n",
        "ground_truth": [{"path": "defaults.py", "line": 5}],
    },
    {
        "id": "sql_injection",
        "kind": "buggy",
        "title": "find_user builds SQL by string interpolation",
        "description": "Adds a user lookup.",
        "base": {"db.py": "def table_name():\n    return \"users\"\n"},
        "head": {
            "db.py": "def table_name():\n"
            "    return \"users\"\n"
            "\n"
            "\n"
            "def find_user(conn, name):\n"
            '    query = f"SELECT * FROM users WHERE name = \'{name}\'"\n'
            "    return conn.execute(query).fetchall()\n"
        },
        "fix": {
            "db.py": "def table_name():\n"
            "    return \"users\"\n"
            "\n"
            "\n"
            "def find_user(conn, name):\n"
            '    return conn.execute("SELECT * FROM users WHERE name = ?", (name,)).fetchall()\n'
        },
        "candidate": {
            "title": "find_user is injectable through the name parameter",
            "category": "security",
            "severity": 3,
            "path": "db.py",
            "line": 6,
            "root_cause": "untrusted name is interpolated into SQL; a quote escapes the predicate",
            "trigger": "find_user(conn, \"x' OR '1'='1\")",
            "cwe": "CWE-89",
        },
        "probe_path": ".zero/test_db.py",
        "probe": "import sqlite3\n"
        "\n"
        "from db import find_user\n"
        "\n"
        "\n"
        "def test_injection_returns_no_rows():\n"
        "    conn = sqlite3.connect(\":memory:\")\n"
        "    conn.execute(\"create table users (name text)\")\n"
        "    conn.execute(\"insert into users values ('alice')\")\n"
        "    assert find_user(conn, \"x' OR '1'='1\") == []\n"
        "\n"
        "\n"
        "def test_a_real_user_is_found():\n"
        "    conn = sqlite3.connect(\":memory:\")\n"
        "    conn.execute(\"create table users (name text)\")\n"
        "    conn.execute(\"insert into users values ('alice')\")\n"
        "    assert find_user(conn, \"alice\") == [(\"alice\",)]\n",
        "ground_truth": [{"path": "db.py", "line": 6}],
    },
    {
        "id": "regression_off_by_one_page",
        "kind": "buggy",
        "title": "page now drops the first element of page zero",
        "description": "Refactor of pagination.",
        "base": {
            "paging.py": "def page(items, size, number):\n"
            "    start = number * size\n"
            "    return items[start:start + size]\n"
        },
        "head": {
            "paging.py": "def page(items, size, number):\n"
            "    start = number * size - 1\n"
            "    return items[start:start + size]\n"
        },
        "fix": {
            "paging.py": "def page(items, size, number):\n"
            "    start = number * size\n"
            "    return items[start:start + size]\n"
        },
        "candidate": {
            "title": "page(..., 0) skips the first item",
            "category": "correctness",
            "severity": 2,
            "path": "paging.py",
            "line": 2,
            "root_cause": "start is one less than it should be, so page zero starts at -1",
            "trigger": "page([1, 2, 3], 2, 0)",
        },
        "probe_path": ".zero/test_paging.py",
        "probe": "from paging import page\n"
        "\n"
        "\n"
        "def test_first_page_keeps_every_item():\n"
        "    assert page([1, 2, 3], 2, 0) == [1, 2]\n"
        "\n"
        "\n"
        "def test_second_page_starts_at_the_boundary():\n"
        "    assert page([1, 2, 3], 2, 1) == [3]\n",
        "ground_truth": [{"path": "paging.py", "line": 2}],
    },
    {
        "id": "fix_confirmed_percentile",
        "kind": "fixed",
        "title": "a bug-fix PR that actually fixes the boundary",
        "description": "Fixes percentile at p=1.0.",
        "base": {
            "calc.py": "def percentile(values, p):\n"
            "    ordered = sorted(values)\n"
            "    return ordered[int(len(ordered) * p)]\n"
        },
        "head": {
            "calc.py": "def percentile(values, p):\n"
            "    if not values:\n"
            '        raise ValueError("empty")\n'
            "    ordered = sorted(values)\n"
            "    index = min(len(ordered) - 1, max(0, int(len(ordered) * p)))\n"
            "    return ordered[index]\n"
        },
        "candidate": {
            "title": "percentile boundary",
            "category": "correctness",
            "severity": 2,
            "path": "calc.py",
            "line": 7,
            "root_cause": "the boundary index was wrong before this change",
            "trigger": "percentile([1, 2, 3], 1.0)",
        },
        "probe_path": ".zero/test_percentile.py",
        "probe": "from calc import percentile\n"
        "\n"
        "\n"
        "def test_full_percentile_returns_last():\n"
        "    assert percentile([1, 2, 3], 1.0) == 3\n",
        "ground_truth": [],
    },
    {
        "id": "clean_refactor",
        "kind": "clean",
        "title": "rename area to rectangle_area with a docstring",
        "description": "Readability refactor.",
        "base": {"geometry.py": "def area(w, h):\n    return w * h\n"},
        "head": {
            "geometry.py": "def rectangle_area(width, height):\n"
            '    """Return the area of a rectangle."""\n'
            "    return width * height\n"
        },
        "ground_truth": [],
    },
    {
        "id": "clean_new_function",
        "kind": "clean",
        "title": "add a guarded clamp",
        "description": "Adds clamp.",
        "base": {"mathx.py": "def double(value):\n    return value * 2\n"},
        "head": {
            "mathx.py": "def double(value):\n"
            "    return value * 2\n"
            "\n"
            "\n"
            "def clamp(value, low, high):\n"
            "    if high < low:\n"
            '        raise ValueError("bad range")\n'
            "    return min(high, max(low, value))\n"
        },
        "ground_truth": [],
    },
    {
        "id": "clean_docs",
        "kind": "clean",
        "title": "document the module only",
        "description": "README update.",
        "base": {"utils.py": "def add(a, b):\n    return a + b\n", "README.md": "# utilities\n"},
        "head": {
            "utils.py": "def add(a, b):\n    return a + b\n",
            "README.md": "# utilities\n\nAdds two numbers.\n",
        },
        "ground_truth": [],
    },
]
