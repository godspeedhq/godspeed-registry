#!/usr/bin/env python3
"""Generates the documentation site from the repository.

    python3 tools/docs/build.py [<out dir>]      default: .site

Everything on the site comes from the code and the documents beside it, so it is never edited by hand and
is never out of date once rebuilt:

    Home             README.md
    Design           CLAUDE.md
    HTTP API         the router in src/lib.rs, with each handler's doc comment and what it can refuse
    Error codes      the `codes!` table in src/api.rs, and where each is raised
    Database         migrations/*.sql
    Trust anchor     trust/*.toml, and the module comments of src/trust.rs and tools/trust
    Deployment       wrangler.toml, and the header of each workflow
    Tests and tools  the header of each script
    Source           the module comment of every file in src/
    Status           README's status line

The output is Markdown with front matter for Jekyll and the Just the Docs theme; tools/docs/publish.sh,
through .github/workflows/pages.yml, turns it into the GitHub Pages site. Needs only Python 3.

A generator that cannot read what it expects stops with an error: a page quietly missing its table would
look like a registry with no API.
"""
import json
import pathlib
import posixpath
import re
import shutil
import subprocess
import sys
from datetime import datetime, timezone

SLUG = "godspeedhq/godspeed-registry"
BASE = "/godspeed-registry"
BLOB = f"https://github.com/{SLUG}/blob/main"
TREE = f"https://github.com/{SLUG}/tree/main"
REPO = pathlib.Path(__file__).resolve().parents[2]


def die(why):
    sys.exit(f"build: {why}")


def read(rel):
    p = REPO / rel
    if not p.is_file():
        die(f"{rel} is missing")
    return p.read_text(encoding="utf-8")


def page(out, rel, front, body):
    """Writes one page: front matter, then the body inside raw tags so Liquid never reads it."""
    if "{% endraw" in body:
        die(f"{rel}: the body closes the raw block")
    target = out / rel
    target.parent.mkdir(parents=True, exist_ok=True)
    head = "".join(f"{k}: {v}\n" for k, v in front.items())
    target.write_text(f"---\n{head}---\n\n{{% raw %}}\n{body.rstrip()}\n\n{{% endraw %}}\n", encoding="utf-8")


def outside_fences(text):
    """Yields (line, in_fence) for each line, so nothing inside a code block is rewritten."""
    fence = False
    for line in text.split("\n"):
        if line.lstrip().startswith("```"):
            yield line, True
            fence = not fence
        else:
            yield line, fence


def clean(text):
    """Drops the document's own title line."""
    return re.sub(r"^# .*\n", "", text, count=1)


def relink(text, here=""):
    """Sends a repository link to its page on the site, or to GitHub if the site has no page for it."""

    def one(m):
        target = m.group(2)
        if re.match(r"[a-z]+:", target) or target.startswith("#"):
            return m.group(0)
        path, _, frag = target.partition("#")
        rel = posixpath.normpath(posixpath.join(here, path))
        if rel == "README.md":
            url = f"{BASE}/" + (f"#{frag}" if frag else "")
        elif rel == "CLAUDE.md":
            url = f"{BASE}/design/" + (f"#{frag}" if frag else "")
        elif (REPO / rel).is_dir():
            url = f"{TREE}/{rel}"
        elif (REPO / rel).is_file():
            url = f"{BLOB}/{rel}" + (f"#{frag}" if frag else "")
        else:
            print(f"build: a link names {target}, which does not exist", file=sys.stderr)
            return m.group(0)
        return f"{m.group(1)}({url})"

    return "\n".join(
        line if fenced else re.sub(r"(\[[^\]]*\])\(([^)\s]+)\)", one, line) for line, fenced in outside_fences(text)
    )


def demote(text, levels):
    """Moves every heading down `levels`, leaving code blocks alone."""
    out = []
    for line, fenced in outside_fences(text):
        out.append("#" * levels + line if not fenced and re.match(r"#+ ", line) else line)
    return "\n".join(out)


def cell(text):
    """Makes text safe inside a table cell."""
    return " ".join(text.split()).replace("|", "\\|")


def module_comment(rel):
    """The //! comment at the top of a Rust file, as Markdown."""
    lines = [m.group(1) for m in re.finditer(r"^//! ?(.*)$", read(rel), re.M)]
    if not lines:
        die(f"{rel} has no module comment")
    return "\n".join(lines)


def header_comment(rel, mark="#"):
    """The comment block at the top of a script, after its shebang."""
    lines = read(rel).split("\n")
    if lines and lines[0].startswith("#!"):
        lines = lines[1:]
    if lines and lines[0].startswith("name:"):  # A workflow's comment follows its name.
        lines = [line for line in lines[1:]]
    out = []
    for line in lines:
        if line.startswith(mark):
            out.append(line[len(mark):].removeprefix(" "))
        elif line.strip() or out:
            if line.strip() or out[-1:] == [""]:
                break
            out.append("")
    return "\n".join(out).strip()


# --- HTTP API, from the router --------------------------------------------------------------------------------


def function_body(source, name):
    """The body of a top-level function, by its closing brace in the first column."""
    m = re.search(rf"^(?:async )?fn {name}\(.*?^\}}", source, re.S | re.M)
    return m.group(0) if m else ""


def doc_comment(source, name):
    """The doc comment above a top-level function."""
    m = re.search(rf"((?:^///.*\n)+)(?:async )?fn {name}\(", source, re.M)
    if not m:
        return ""
    return " ".join(line[3:].strip() for line in m.group(1).strip().split("\n"))


def parse_routes():
    source = read("src/lib.rs")
    m = re.search(r"Router::new\(\)(.*?)\.run\(req, env\)", source, re.S)
    if not m:
        die("src/lib.rs: no router")
    routes = []
    for r in re.finditer(r'\.(get|post|put|delete)(?:_async)?\(\s*"([^"]+)",\s*([a-z_]*)', m.group(1)):
        method, path, handler = r.group(1).upper(), r.group(2), r.group(3)
        routes.append((method, path, "ABOUT" if path == "/" else handler or "read"))
    if not routes:
        die("src/lib.rs: the router has no routes")
    return source, routes


def error_map():
    """Each error's constructor, status and code, from the `codes!` table."""
    block = re.search(r"^codes! \{\n(.*?)^\}", read("src/api.rs"), re.S | re.M)
    if not block:
        die("src/api.rs: no codes! table")
    found = re.findall(r'(\w+)\s*=> (\d+), "([\w-]+)";', block.group(1))
    if not found:
        die("src/api.rs: the codes! table is empty")
    return found


def api_page():
    source, routes = parse_routes()
    names = {fn: code for fn, _, code in error_map()}
    about = re.search(r'const ABOUT: &str = "(.*?)";', source, re.S)
    out = [
        "# HTTP API\n",
        f"Every route the Worker answers, read from the router in [`src/lib.rs`]({BLOB}/src/lib.rs). "
        "Reads are public. A route marked as needing a token takes it as a bearer token, and refuses with the "
        f"[error codes]({BASE}/reference/errors/).\n",
        "| Method | Path | Token | Handler |\n| --- | --- | --- | --- |",
    ]
    details = []
    for method, path, handler in routes:
        body = function_body(source, handler)
        token = "yes" if re.search(r"auth::authenticate|authorise\(", body) else ""
        out.append(f"| {method} | `{path}` | {token} | `{handler}` |")
        doc = doc_comment(source, handler)
        if handler in ("read", "ABOUT"):
            continue
        raised = sorted({names[f] for f in re.findall(r"ApiError::(\w+)", body) if f in names})
        if not doc and handler:
            print(f"build: handler {handler} has no doc comment", file=sys.stderr)
        text = f"### `{method} {path}`\n\n{doc or 'No doc comment in the source.'}\n"
        if raised:
            text += "\nRefuses here, before it reaches the package's Durable Object: "
            text += ", ".join(f"`{c}`" for c in raised) + ".\n"
        details.append(text)
    out.append("\n## Handlers\n")
    out += details
    out.append("## Public files\n")
    out.append(doc_comment(source, "read") + "\n")
    if about:
        text = re.sub(r"\\n\\\n\s*", "\n", about.group(1)).replace("\\n", "\n")
        out.append(f"`GET /` answers with this text:\n\n```text\n{text.strip()}\n```\n")
    return "\n".join(out)


def errors_page():
    sources = {p.name: p.read_text(encoding="utf-8") for p in sorted((REPO / "src").glob("*.rs"))}
    header = module_comment("src/api.rs")
    out = [
        "# Error codes\n",
        header + "\n",
        f"From the `codes!` table in [`src/api.rs`]({BLOB}/src/api.rs). The last column is where the code raises it.\n",
        "| Code | HTTP status | Raised in |\n| --- | --- | --- |",
    ]
    for fn, status, code in error_map():
        where = [f"`src/{name}`" for name, text in sources.items() if f"ApiError::{fn}(" in text]
        out.append(f"| `{code}` | {status} | {', '.join(where) or 'the shared handler in `src/api.rs`'} |")
    return "\n".join(out)


# --- Database, trust, deployment, tests ------------------------------------------------------------------------


def database_page():
    out = [
        "# Database\n",
        f"The D1 schema, migration by migration, from [`migrations/`]({TREE}/migrations).\n",
    ]
    tables = []
    migrations = sorted((REPO / "migrations").glob("*.sql"))
    if not migrations:
        die("migrations/ is empty")
    for path in migrations:
        text = path.read_text(encoding="utf-8").strip()
        tables += [(t, path.name) for t in re.findall(r"CREATE TABLE (?:IF NOT EXISTS )?(\w+)", text)]
        out.append(f"## `{path.name}`\n\n```sql\n{text}\n```\n")
    summary = "| Table | Created in |\n| --- | --- |\n" + "\n".join(f"| `{t}` | `{m}` |" for t, m in tables)
    out.insert(2, summary + "\n")
    return "\n".join(out)


def trust_page():
    out = [
        "# Trust anchor\n",
        demote(module_comment("src/trust.rs"), 1) + "\n",
        "## The maintainer's offline tool\n",
        demote(module_comment("tools/trust/src/main.rs"), 2) + "\n",
        "## The signed documents\n",
        f"As committed under [`trust/`]({TREE}/trust). The signatures are beside them.\n",
    ]
    for path in sorted((REPO / "trust").rglob("*.toml")):
        rel = path.relative_to(REPO).as_posix()
        out.append(f"### `{rel}`\n\n```toml\n{path.read_text(encoding='utf-8').strip()}\n```\n")
    return "\n".join(out)


def deployment_page():
    out = [
        "# Deployment\n",
        "## Workflows\n",
        f"The header of each workflow in [`.github/workflows/`]({TREE}/.github/workflows).\n",
    ]
    for path in sorted((REPO / ".github/workflows").glob("*.yml")):
        rel = path.relative_to(REPO).as_posix()
        out.append(f"### `{path.name}`\n\n```text\n{header_comment(rel)}\n```\n")
    out.append(f"## `wrangler.toml`\n\n```toml\n{read('wrangler.toml').strip()}\n```\n")
    return "\n".join(out)


def tests_page():
    out = [
        "# Tests and tools\n",
        f"The header of each script in [`scripts/`]({TREE}/scripts).\n",
    ]
    for path in sorted((REPO / "scripts").iterdir()):
        rel = path.relative_to(REPO).as_posix()
        mark = "//" if path.suffix == ".mjs" else "#"
        out.append(f"## `{path.name}`\n\n```text\n{header_comment(rel, mark)}\n```\n")
    return "\n".join(out)


def source_page():
    out = [f"# Source\n\nThe module comment of each file in [`src/`]({TREE}/src).\n"]
    for path in sorted((REPO / "src").glob("*.rs")):
        rel = f"src/{path.name}"
        out.append(f"## [`{rel}`]({BLOB}/{rel})\n\n{demote(module_comment(rel), 2)}\n")
    return "\n".join(out)


def status_page():
    readme = read("README.md")
    status = re.search(r"^\*\*Status:.*?(?=\n\n)", readme, re.S | re.M)
    if not status:
        die("README.md has no status line")
    return f"# Status\n\n{status.group(0)}\n"


def main():
    out = pathlib.Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else REPO / ".site"
    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True)
    try:
        commit = subprocess.check_output(["git", "-C", str(REPO), "rev-parse", "--short", "HEAD"], text=True).strip()
    except (OSError, subprocess.CalledProcessError):
        commit = "unknown"
    built = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")

    page(out, "index.md", {"title": "Home", "layout": "home", "nav_order": 1, "permalink": "/"},
         relink(clean(read("README.md"))))
    page(out, "design.md", {"title": "Design", "nav_order": 2, "permalink": "/design/"},
         "# Design\n" + relink(clean(read("CLAUDE.md"))))
    page(out, "reference/index.md",
         {"title": "Reference", "nav_order": 3, "has_children": True, "permalink": "/reference/"},
         "# Reference\n\nGenerated from the code: the API, its errors, the schema, the trust anchor and the tools.\n")
    reference = [
        ("api", "HTTP API", api_page()),
        ("errors", "Error codes", errors_page()),
        ("database", "Database", database_page()),
        ("trust", "Trust anchor", trust_page()),
        ("deployment", "Deployment", deployment_page()),
        ("tests", "Tests and tools", tests_page()),
        ("source", "Source", source_page()),
    ]
    for order, (slug, title, body) in enumerate(reference, 1):
        page(out, f"reference/{slug}.md",
             {"title": json.dumps(title), "parent": "Reference", "nav_order": order,
              "permalink": f"/reference/{slug}/"},
             body)
    page(out, "status.md", {"title": "Status", "nav_order": 4, "permalink": "/status/"}, status_page())

    (out / "_config.yml").write_text(
        f"""title: Godspeed registry
description: The Godspeed package registry service.
baseurl: {BASE}
remote_theme: just-the-docs/just-the-docs@v0.10.1
# Just the Docs' built-in dark scheme.
color_scheme: dark
plugins:
  - jekyll-remote-theme
search_enabled: true
heading_anchors: true
aux_links:
  GitHub: https://github.com/{SLUG}
footer_content: "Generated from <a href=\\"https://github.com/{SLUG}/commit/{commit}\\">{commit}</a> on {built}. GNU General Public License v2.0 only."
markdown: kramdown
kramdown:
  input: GFM
  hard_wrap: false
""",
        encoding="utf-8",
    )
    count = len(list(out.rglob("*.md")))
    print(f"built {count} pages in {out} from {commit}")


if __name__ == "__main__":
    main()
