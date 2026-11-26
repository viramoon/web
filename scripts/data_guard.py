"""Inspect Git objects before publishing web changes."""

import argparse
import csv
from dataclasses import dataclass
import hashlib
import json
from fnmatch import fnmatchcase
from html import unescape
import os
import re
import subprocess
import sys
from urllib.parse import unquote


RULES = ".data-guard/rules.json"
SOURCES = ".data-guard/sources.json"
# The reviewed v1 rules for branches predating this policy file, including W3 main.
BOOTSTRAP_RULES_COMMIT = "e663bbd3fe64abf75f62925f2170e2cb58506e4f"
MAX_POLICY_BYTES = 256 * 1024
REGULAR_MODES = {b"100644", b"100755"}
# Reviewed W3 source writes fixed synthetic credentials to temporary fixtures.
# Only the exact source blob and path may satisfy the credential check.
REVIEWED_CREDENTIAL_TESTS = {
    "src/notifications/channel/tests.rs": "beb37680e9a5e9f82e7c5d6533c563b61cd467f1928c2803d668162f263f7bb3",
}
# Exact CI-only credential lines from the approved W3 main workflow (4c966be).
REVIEWED_CI_CREDENTIAL_LINES = frozenset({
    "66c575c9155029b263d5552e89d8d2b621007abbca514d531962962ba811c93f",
    "2fbdc8279a5926dbcd673a32e8a49c4fb7c78c7a726d8a6c35a5f56f695d22f4",
})
# Established source blobs that match market-data signatures. Each entry is
# pinned to bytes reviewed in the named commits, never a path-wide exception.
REVIEWED_MARKET_SOURCE_DIGESTS = {
    "src/notifications/worker.rs": (  # 4c966be: integer notification policy bounds.
        "be5d1a3971142d1b2e0a54eab35c4de172c4c0c6dfae00911a5627d7c3a84d9a",
    ),
    "src/w1_http_tests.rs": (  # 4992a15 and 7e641b6: fixed OHLC redaction canary.
        "3ebb7492acdd8dcbfcbb857f0dde425c0b9449896d07b91238ec53176c021352",
        "e05dac63895d9cb6552d0a5a40a895086b63910af6543d73044646551eb32f4e",
    ),
    "tests/api/database.rs": (  # 4c966be and 7e641b6: migration version assertions.
        "7ecf7e6f8288dbcf90690479fed74283cc9a466e8b08561034c991424cbccb4b",
        "11e747ad4261b5514426b6dbff552d8a6471c619a794dc875ed6786fc133af4b",
    ),
    "tests/api/notifications_worker.rs": (  # 4c966be: synthetic rule state tuples.
        "c212791b32ff6297be1d1f3b993cd118437bd4d3817e0c1bfc0030c7d0ebdd9d",
    ),
}
# Ordinary tracked files. Other paths require a source record in the same tree.
ALLOWED_FILES = frozenset({
    ".agents/skills/readme.md", ".data-guard/rules.json", ".data-guard/sources.json",
    ".githooks/pre-commit", ".githooks/pre-push", ".github/pull_request_template.md",
    ".gitignore", "agents.md", "build.rs", "cargo.lock", "cargo.toml", "readme.md", "runbook.md",
    "rust-toolchain.toml", "service.yaml", "config/readme.md", "migrations/readme.md",
    "crates/finance-client/cargo.toml", "openapi/finance.commit",
})
# Directory prefix, filename patterns, and whether subdirectories are allowed.
ALLOWED_FILE_TYPES = (
    ("src/", ("*.rs", "readme.md"), True),
    ("tests/", ("*.rs", "*.py", "readme.md"), True),
    ("docs/", ("*.md",), True),
    ("migrations/", ("*.sql",), False),
    ("config/", ("*.yaml",), False),
    ("openapi/", ("*.json",), False),
    ("crates/finance-client/src/", ("*.rs",), True),
    ("scripts/", ("*.sh", "*.py"), False),
    ("scripts/openapi/", ("*.java", "*.patch"), False),
    (".sqlx/", ("query-*.json",), False),
    (".github/workflows/", ("*.yml", "*.yaml"), False),
    (".github/issue_template/", ("*.md", "*.yml", "*.yaml"), False),
)
OID = re.compile(rb"(?:[0-9a-f]{40}|[0-9a-f]{64})")
CONTROL = re.compile(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f-\x9f]")
VALUE = re.compile(r"[+-]?(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:[eE][+-]?[0-9]+)?")
DATE = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}(?:[T ].*)?")
MARKET_FIELDS = (
    {"open", "high", "low", "close"},
    {"price", "quantity", "exchangets"},
    {"price", "quantity", "timestamp"},
    {"price", "quantity", "ts"},
    {"price", "quantity", "side"},
    {"price", "size", "timestamp"},
    {"price", "size", "ts"},
    {"price", "size", "side"},
    {"price", "qty", "timestamp"},
    {"price", "qty", "side"},
    {"bid", "ask"},
    {"bidprice", "askprice"},
    {"bids", "asks"},
    {"chart", "timestamp", "indicators"},
    {"observations", "observationstart", "value"},
)
SOURCE_SUFFIXES = (".java", ".py", ".sh")
DATA_LINE = re.compile(r"""^\s*(?:[\[{\x60]|["']?[\w-]+["']?\s*[:=])""")
FIELD_VALUE = re.compile(
    r"""(?ix)
    \b(?P<field>[a-z][a-z0-9_-]*)["'\x60]?\s*[:=]\s*["'\x60]?
    (?: [+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:e[+-]?\d+)?(?=[\s,"'\x60;}\])]|$)
      | \d{4}-\d{2}-\d{2}T
      | \[
      | (?:buy|sell|bid|ask)\b
    )
    """
)
WEB_CAPTURE_FIELDS = (
    {"method", "headers", "body"},
    {"request", "response", "url"},
)
LOG_FIELDS = (
    {"timestamp", "level", "fields"},
    {"timestamp", "level", "message"},
    {"timestamp", "severity", "message"},
    {"time", "level", "msg"},
    {"ts", "level", "msg"},
)
REPORT_FIELDS = {"testResults", "numFailedTests"}
JSON_OBJECT_START = re.compile(r'\{(?=\s*")')
JSON_DECODER = json.JSONDecoder(parse_int=str, parse_float=str)
PRIVATE_KEY = re.compile(
    r"(?m)-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----"
    r"(?:\r?$|(?:\r?\n|\\n)[a-zA-Z0-9+/]{24,})"
)
SECRET_FIELD = (
    r"(?:web[-_]admin[-_]token|api[-_]token|api[-_]key|access[-_]token|refresh[-_]token|"
    r"client[-_]secret|x[-_]csrf[-_]token|csrf[-_]token|__host-web_admin_session|postgres[-_]password|password)"
)
TOKEN_PREFIX = r"(?:gh[pousr]_[a-z0-9]{24,}|github_pat_[a-z0-9_]{24,}|eyj[a-z0-9_-]+\.[a-z0-9_-]+\.[a-z0-9_-]+)"
CREDENTIAL = re.compile(
    rf"""(?ix)
    (?: \bpostgres(?:ql)?://[^\s/'":@]+:[^\s/'"@]+@
      | \bauthorization["'\x60]?\s*:\s*["'\x60]?(?:bearer|basic)[ \t]+[a-z0-9._~+/=-]{{4,}}
      | \b{SECRET_FIELD}["'\x60]?\s*[:=]\s*
        (?: "[a-z0-9._~+/=-]+" | '[a-z0-9._~+/=-]+' | [a-z0-9._~+/=-]{{12,}}(?=["'\s,;)\]}}]|$) )
      | \b{TOKEN_PREFIX}
    )
    """
)
PATH_CREDENTIAL = re.compile(
    rf"(?i)\b(?:{SECRET_FIELD}|bearer|basic)[=:/ _-]+[a-z0-9][a-z0-9._~+=-]{{11,}}"
)
TRANSCRIPT_LINE = r"^[ \t]*(?:>[ \t]*)*"
BLOCK_QUOTE = re.compile(r"(?m)^[ \t]*(?:>[ \t]*)+")
TEST_REPORT = re.compile(
    rf"""(?imx){TRANSCRIPT_LINE}
    (?: (?:<\?xml[^\r\n]*?\?>[ \t]*)?<testsuites?(?=[\s>])
      | test[ ]result:[ \t]+(?:ok|FAILED)\b
      | test[ \t]+\S+[ \t]+\.\.\.[ \t]+(?:ok|FAILED|ignored)\b
      | Running[ \t]+(?:unittests|tests/|doc-tests\b)
      | Compiling[ \t]+\S+[ \t]+v\d+\.\d+\.\d+
      | Finished[ \t]+(?:\x60[^\x60\r\n]+\x60|\S+)[ \t]+profile\b[^\r\n]*?target\(s\)[ \t]+in\b
    )
    """
)
LOG_LINE = re.compile(
    rf"(?im){TRANSCRIPT_LINE}\[?\d{{4}}-\d{{2}}-\d{{2}}[T ]\d{{2}}:\d{{2}}:\d{{2}}"
    r"(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?\]?[ \t]+(?:[|-][ \t]*)?"
    r"\[?(?:TRACE|DEBUG|INFO|WARN(?:ING)?|ERROR|FATAL)\b"
)
EXCEPTION_DUMP = re.compile(
    rf"(?m){TRANSCRIPT_LINE}"
    r"(?:Traceback \(most recent call last\):|thread '[^'\r\n]+' (?:\(\d+\) )?panicked at )"
)
HTTP_DUMP = re.compile(
    rf"(?m){TRANSCRIPT_LINE}"
    r"(?:HTTP/[123](?:\.\d)?[ \t]+\d{3}\b"
    r"|(?:GET|HEAD|POST|PUT|PATCH|DELETE|OPTIONS|CONNECT|TRACE)[ \t]+\S+[ \t]+HTTP/[123](?:\.\d)?\b)"
)
ARTIFACT_UPLOAD = re.compile(r"(?im)^[ \t]*(?:-[ \t]+)?uses:[ \t]*['\"]?actions/upload-artifact@")
TABLE_HEADERS = (
    {"open", "high", "low", "close"},
    {"price", "volume"},
    {"symbol", "price"},
    {"timestamp", "price"},
    {"date", "price"},
)
ASCII_LOWER = str.maketrans("ABCDEFGHIJKLMNOPQRSTUVWXYZ", "abcdefghijklmnopqrstuvwxyz")
GIT_ENV = dict(os.environ, GIT_NO_REPLACE_OBJECTS="1", GIT_NO_LAZY_FETCH="1", GIT_ALLOW_PROTOCOL="")


class GuardError(Exception):
    """An error with a fixed diagnostic safe to print in CI."""


@dataclass(frozen=True)
class Entry:
    mode: bytes
    oid: bytes


@dataclass(frozen=True)
class Rules:
    max_blob_bytes: int
    blocked_path_components: set[str]
    blocked_suffixes: tuple[str, ...]
    blocked_magic: tuple[tuple[int, bytes], ...]
    fixture_paths: tuple[str, ...]
    fixture_suffixes: tuple[str, ...]


def git(*args, optional=False):
    result = subprocess.run(
        ["git", *args], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=GIT_ENV
    )
    if result.returncode == 0:
        return result.stdout
    if optional:
        return None
    raise GuardError("git-read-failed")


def read_tree(commit=None):
    if commit is None:
        rows = git("ls-files", "--stage", "-z")
    else:
        rows = git("ls-tree", "-r", "-z", commit)
    tree = {}
    for row in rows.split(b"\0")[:-1]:
        metadata, path = row.split(b"\t", 1)
        if commit is None:
            mode, oid, stage = metadata.split()
            if stage != b"0":
                raise GuardError("unmerged-index")
        else:
            mode, _, oid = metadata.split()
        tree[os.fsdecode(path)] = Entry(mode, oid)
    return tree


def commit_id(revision, optional=False):
    oid = git("rev-parse", "--verify", "--end-of-options", revision + "^{commit}", optional=optional)
    return oid.decode("ascii").strip() if oid is not None else None


def changed_paths(before, after):
    return sorted(path for path, entry in after.items() if before.get(path) != entry)


def blob_size(entry):
    return int(git("cat-file", "-s", entry.oid))


def blob(entry):
    return git("cat-file", "blob", entry.oid)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate key")
        result[key] = value
    return result


def policy_file(tree, path, error):
    entry = tree.get(path)
    if not entry or entry.mode not in REGULAR_MODES or blob_size(entry) > MAX_POLICY_BYTES:
        raise GuardError(error)
    try:
        value = json.loads(blob(entry).decode("utf-8"), object_pairs_hook=unique_object)
    except (ValueError, RecursionError):
        raise GuardError(error) from None
    if not isinstance(value, dict) or type(value.get("version")) is not int or value["version"] != 1:
        raise GuardError(error)
    return value


def valid_path(path):
    return (isinstance(path, str) and "\\" not in path
            and not any(part in {"", ".", ".."} for part in path.split("/"))
            and not re.search(r"[\x00-\x1f\x7f-\x9f\ud800-\udfff]", path))


def load_rules(tree):
    error = "invalid-rules"
    raw = policy_file(tree, RULES, error)
    if raw.keys() != {"version", *Rules.__annotations__}:
        raise GuardError(error)
    size = raw["max_blob_bytes"]
    if type(size) is not int or not 1 <= size <= 16 * 1024 * 1024:
        raise GuardError(error)

    def strings(key, valid):
        values = raw[key]
        if not isinstance(values, list) or not all(
            valid_path(item) and valid(item) for item in values
        ):
            raise GuardError(error)
        normalized = tuple(item.translate(ASCII_LOWER) for item in values)
        if len(set(normalized)) != len(normalized):
            raise GuardError(error)
        return normalized

    def suffix(value):
        return re.fullmatch(r"\.[a-z0-9][a-z0-9._-]*", value) is not None

    signatures = []
    if not isinstance(raw["blocked_magic"], list):
        raise GuardError(error)
    for item in raw["blocked_magic"]:
        if not isinstance(item, dict) or item.keys() != {"offset", "hex"}:
            raise GuardError(error)
        offset, signature = item["offset"], item["hex"]
        if (type(offset) is not int or not isinstance(signature, str)
                or not re.fullmatch(r"(?:[0-9a-f]{2})+", signature)
                or not 0 <= offset <= size - len(signature) // 2):
            raise GuardError(error)
        signatures.append((offset, bytes.fromhex(signature)))
    return Rules(
        size,
        set(strings("blocked_path_components", lambda value: "/" not in value)),
        strings("blocked_suffixes", suffix),
        tuple(signatures),
        strings("fixture_paths", lambda value: True),
        strings("fixture_suffixes", suffix),
    )


def load_trusted_rules(tree):
    if RULES in tree:
        return load_rules(tree)
    bootstrap = commit_id(BOOTSTRAP_RULES_COMMIT, optional=True)
    if bootstrap is None:
        raise GuardError("missing-trusted-rules")
    return load_rules(read_tree(bootstrap))


def load_sources(tree, required):
    error = "invalid-sources"
    if SOURCES not in tree and not required:
        return {}
    raw = policy_file(tree, SOURCES, error)
    if raw.keys() != {"version", "fixtures"} or not isinstance(raw["fixtures"], dict):
        raise GuardError(error)
    for path, record in raw["fixtures"].items():
        if not valid_path(path) or path in {RULES, SOURCES} or not isinstance(record, dict):
            raise GuardError(error)
        if record.keys() != {"sha256", "generator", "seed", "source"}:
            raise GuardError(error)
        digest, generator = record["sha256"], record["generator"]
        if (not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest)
                or not valid_path(generator) or generator in {path, RULES, SOURCES}
                or not all(isinstance(record[key], str) and record[key].strip()
                           for key in ("seed", "source"))):
            raise GuardError(error)
    return raw["fixtures"]


def looks_like_table(text):
    # Two consecutive records with the same delimiter/width and numeric fields
    # catch common renamed CSV/TSV exports without treating prose as a table.
    lines = text.splitlines()
    for delimiter in (",", "\t", ";", "|"):
        previous_width = 0
        for line in lines:
            row = ()
            if delimiter in line:
                try:
                    row = next(csv.reader([line], delimiter=delimiter, skipinitialspace=True, strict=True))
                except csv.Error:
                    pass  # A quoted JSON or source-code line is not a delimited record.
            fields = [field.strip() for field in row]
            columns = {normal_field(field.strip("\x60")) for field in fields}
            if any(group <= columns for group in (*TABLE_HEADERS, *MARKET_FIELDS)):
                return True
            numeric = any(VALUE.fullmatch(field) for field in fields)
            if len(fields) >= 2 and numeric and any(
                VALUE.fullmatch(field) or DATE.fullmatch(field) for field in fields[:2]
            ):
                if previous_width == len(fields):
                    return True
                previous_width = len(fields)
            else:
                previous_width = 0
    return False


def normal_field(field):
    return field.translate(ASCII_LOWER).replace("_", "").replace("-", "")


def credential_in_path(path):
    decoded = unquote(path)
    return CREDENTIAL.search(decoded) or PATH_CREDENTIAL.search(decoded)


def normalize_payload(text):
    return BLOCK_QUOTE.sub("", unescape(text)).replace(r'\"', '"')


def has_credential(path, text, content):
    if path == ".github/workflows/service.yml":
        # Only these exact non-production settings are excluded from this check.
        content = normalize_payload("\n".join(
            line for line in text.splitlines()
            if hashlib.sha256(line.encode()).hexdigest() not in REVIEWED_CI_CREDENTIAL_LINES
        ))
    return CREDENTIAL.search(content.replace(r"\n", "\n")) is not None


def allowed_file_type(path):
    if path in ALLOWED_FILES:
        return True
    for prefix, patterns, recursive in ALLOWED_FILE_TYPES:
        if path.startswith(prefix):
            relative = path[len(prefix):]
            if recursive or "/" not in relative:
                name = relative.rsplit("/", 1)[-1]
                if any(fnmatchcase(name, pattern) for pattern in patterns):
                    return True
    return False


def json_objects(value):
    pending = [value]
    while pending:
        item = pending.pop()
        if isinstance(item, dict):
            yield item
            pending.extend(item.values())
        elif isinstance(item, list):
            pending.extend(item)


def json_diagnostic_reason(text):
    # Recognize records anywhere in a text stream. Skip decoded spans and
    # cap work on overlapping malformed candidates.
    end = 0
    budget = 4 * len(text)
    for match in JSON_OBJECT_START.finditer(text):
        start = match.start()
        if start < end:
            continue
        try:
            value, end = JSON_DECODER.raw_decode(text, start)
        except json.JSONDecodeError as error:
            budget -= max(1, error.pos - start)
            if budget < 0:
                return "inspection-failed"
            continue
        except RecursionError:
            return "inspection-failed"
        for record in json_objects(value):
            keys = record.keys()
            if any(group <= keys for group in LOG_FIELDS):
                return "log-export"
            if REPORT_FIELDS <= keys:
                return "test-report"
            if (record.get("type") in ("suite", "test")
                    and record.get("event") in ("started", "ok", "failed", "ignored", "timeout", "allowed_fail")):
                return "test-report"
            if record.get("type") == "bench" and {"name", "median", "deviation"} <= keys:
                return "test-report"
            if record.get("reason") in (
                "compiler-artifact", "compiler-message", "build-script-executed", "build-finished",
            ):
                return "test-report"
    return None


def json_content_reason(text, *, openapi=False):
    if not text.lstrip().startswith(("{", "[")):
        return None
    try:
        value = JSON_DECODER.decode(text.lstrip())
    except (ValueError, RecursionError):
        # Also catch field combinations in line-delimited JSON.
        def has_fields(group):
            return all(f'"{key}"' in text for key in group)

        if any(has_fields(group) for group in WEB_CAPTURE_FIELDS):
            return "fixture-source-required"
        return None
    for record in json_objects(value):
        fields = {normal_field(key) for key, item in record.items() if not isinstance(item, dict)}
        if (any(group <= fields for group in MARKET_FIELDS)
                or any(group <= record.keys() for group in WEB_CAPTURE_FIELDS)
                or (openapi and {"example", "examples"}.intersection(record))
                or ("chart" in record and '"timestamp"' in text and '"indicators"' in text)):
            return "fixture-source-required"
    return None


def inspect_blob(path, entry, rules, source):
    if not valid_path(path):
        return "invalid-path"
    if credential_in_path(path):
        return "credential-material"
    normalized = path.translate(ASCII_LOWER)
    if rules.blocked_path_components.intersection(normalized.split("/")[:-1]):
        return "blocked-path"
    if normalized.endswith(rules.blocked_suffixes):
        return "blocked-extension"
    if entry.mode not in REGULAR_MODES:
        return "blocked-entry-type"
    if blob_size(entry) > rules.max_blob_bytes:
        return "oversize"
    data = blob(entry)
    if any(data.startswith(magic, offset) for offset, magic in rules.blocked_magic):
        return "blocked-magic"
    try:
        text = data.decode("utf-8-sig")
    except UnicodeError:
        return "non-text"
    if CONTROL.search(text):
        return "non-text"
    # Recognize field shapes in plain text and in quoted or escaped payloads.
    content = normalize_payload(text)
    if PRIVATE_KEY.search(text):
        return "credential-material"
    if has_credential(path, text, content):
        reviewed = REVIEWED_CREDENTIAL_TESTS.get(path)
        if reviewed is None or hashlib.sha256(data).hexdigest() != reviewed:
            return "credential-material"
    if TEST_REPORT.search(text):
        return "test-report"
    if LOG_LINE.search(text) or EXCEPTION_DUMP.search(text):
        return "log-export"
    if normalized.startswith(".github/workflows/") and ARTIFACT_UPLOAD.search(text):
        return "artifact-upload"
    diagnostic = json_diagnostic_reason(text)
    if diagnostic:
        return diagnostic
    if content != text:
        diagnostic = json_diagnostic_reason(content.replace(r"\n", "\n"))
        if diagnostic:
            return diagnostic
    if source:
        if hashlib.sha256(data).hexdigest() != source["sha256"]:
            return "fixture-digest"
        return None
    source_code = normalized.endswith(SOURCE_SUFFIXES)
    if (not allowed_file_type(normalized)
            or normalized.endswith(rules.fixture_suffixes)
            or any(fnmatchcase(normalized, pattern) for pattern in rules.fixture_paths)
            or HTTP_DUMP.search(text)
            or json_content_reason(text, openapi=normalized.startswith("openapi/"))):
        return "fixture-source-required"
    # For the selected non-Rust source formats, inspect data-shaped lines
    # for standalone key-value payloads.
    if source_code:
        content = "\n".join(line for line in content.splitlines() if DATA_LINE.match(line))
    fields = {normal_field(match["field"]) for match in FIELD_VALUE.finditer(content.replace(r"\n", "\n"))}
    if looks_like_table(text) or any(group <= fields for group in MARKET_FIELDS):
        reviewed = REVIEWED_MARKET_SOURCE_DIGESTS.get(path, ())
        if not reviewed or hashlib.sha256(data).hexdigest() not in reviewed:
            return "fixture-source-required"
    return None


def inspect_tree(tree, before, paths, rules, report, require_sources=False):
    # Candidate rule changes are schema-checked without replacing the policy.
    if require_sources or tree.get(RULES) != before.get(RULES):
        load_rules(tree)
    csv.field_size_limit(rules.max_blob_bytes)
    sources = load_sources(tree, require_sources)
    for path, record in sources.items():
        if not all(tree.get(item, Entry(b"", b"")).mode in REGULAR_MODES
                   for item in (path, record["generator"])):
            report("fixture-source-missing", path)
    # Recheck all registered payloads when a manifest changes, including any
    # existing file a new record claims to describe.
    if SOURCES in paths:
        previous_sources = load_sources(before, required=False)
        paths = sorted(set(paths) | sources.keys() | previous_sources.keys())
    for path in paths:
        if path in tree:
            reason = inspect_blob(path, tree[path], rules, sources.get(path))
            if reason:
                report(reason, path)


def inspect_range(base, tip, rules, report):
    for line in git("rev-list", "--parents", tip, *([f"^{base}"] if base else [])).splitlines():
        commit, *parents = line.decode("ascii").split()
        tree = read_tree(commit)
        before = read_tree(parents[0]) if parents else {}
        inspect_tree(tree, before, changed_paths(before, tree), rules, report, require_sources=commit == tip)


def remote_default_commit(remote):
    for ref in ("HEAD", "main"):
        revision = commit_id(f"refs/remotes/{remote}/{ref}", optional=True)
        if revision:
            return revision
    raise GuardError("missing-remote-default")


def inspect_push(remote, report):
    rules = None
    for line in sys.stdin.buffer:
        fields = line.split()
        if len(fields) != 4 or not all(OID.fullmatch(fields[i]) for i in (1, 3)):
            raise GuardError("invalid-push-input")
        local, remote_oid = fields[1], fields[3]
        if not local.strip(b"0"):
            continue  # Ref deletion publishes no objects.
        if rules is None:
            default_base = remote_default_commit(remote)
            rules = load_trusted_rules(read_tree(default_base))
        tip = commit_id(local.decode("ascii"))
        base = commit_id(remote_oid.decode("ascii")) if remote_oid.strip(b"0") else default_base
        inspect_range(base, tip, rules, report)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--show-paths", action="store_true", help="include escaped paths in local diagnostics")
    modes = parser.add_subparsers(dest="mode", required=True)
    modes.add_parser("staged")
    scan = modes.add_parser("range")
    scan.add_argument("base")
    scan.add_argument("tip")
    push = modes.add_parser("push")
    push.add_argument("remote")
    args = parser.parse_args()
    failures = 0

    def report(reason, path=None):
        nonlocal failures
        failures += 1
        location = ""
        if path is not None and args.show_paths:
            location = " <redacted-path>" if credential_in_path(path) else f" {ascii(path)}"
        print(f"data-guard: {reason}{location}", file=sys.stderr)

    try:
        if args.mode == "staged":
            tree = read_tree()
            changed = git("diff", "--cached", "--ita-invisible-in-index",
                          "--name-only", "--no-ext-diff", "--no-renames", "-z")
            paths = [os.fsdecode(path) for path in changed.split(b"\0")[:-1]]
            head = commit_id("HEAD", optional=True)
            before = read_tree(head) if head else {}
            inspect_tree(tree, before, paths, load_trusted_rules(before), report, require_sources=True)
        else:
            if git("rev-parse", "--is-shallow-repository").strip() != b"false":
                raise GuardError("incomplete-history")
            if args.mode == "range":
                tip = commit_id(args.tip)
                base = None if args.base == "-" else commit_id(args.base)
                before = read_tree(base) if base else {}
                inspect_range(base, tip, load_trusted_rules(before), report)
            else:
                inspect_push(args.remote, report)
    except GuardError as error:
        report(str(error))
    except (OSError, ValueError, csv.Error, RecursionError):
        report("inspection-failed")
    if failures:
        print(f"data-guard: rejected ({failures})", file=sys.stderr)
        return 1
    print("data-guard: passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
