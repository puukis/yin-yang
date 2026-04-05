#!/usr/bin/env python3
"""Generate a curated release-notes scaffold for Yin-Yang."""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path


CONVENTIONAL_PREFIX_RE = re.compile(
    r"^(feat|fix|docs|doc|build|ci|chore|refactor|perf|test)(\([^)]*\))?!?:\s*",
    re.IGNORECASE,
)
PULL_REQUEST_RE = re.compile(r"(?:\(#(?P<squash>\d+)\)|Merge pull request #(?P<merge>\d+))")
INFRA_LABELS = {"ci", "build", "release", "infra", "tooling"}
FEATURE_LABELS = {"enhancement", "feature", "semver-minor"}
FIX_LABELS = {"bug", "fix", "bugfix"}
DOC_LABELS = {"documentation", "docs"}
DEPENDENCY_LABELS = {"dependencies"}


@dataclass(frozen=True)
class CommitInfo:
    sha: str
    subject: str
    author_name: str
    author_email: str
    body: str


@dataclass(frozen=True)
class ChangeEntry:
    ref: str
    title: str
    summary: str
    author_handle: str
    labels: tuple[str, ...]
    section: str


def run_command(command: list[str], check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(command, text=True, capture_output=True, check=check)


def git(*args: str, check: bool = True) -> str:
    return run_command(["git", *args], check=check).stdout


def gh(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return run_command(["gh", *args], check=check)


def git_ref_exists(ref: str) -> bool:
    return run_command(["git", "rev-parse", "--verify", "--quiet", ref], check=False).returncode == 0


def parse_repo(remote_url: str) -> str | None:
    remote = remote_url.strip().rstrip("/")
    prefixes = [
        "https://github.com/",
        "http://github.com/",
        "ssh://git@github.com/",
        "git@github.com:",
    ]
    for prefix in prefixes:
        if remote.startswith(prefix):
            path = remote[len(prefix) :]
            return path.removesuffix(".git")
    return None


def detect_repo() -> str | None:
    result = git("config", "--get", "remote.origin.url", check=False).strip()
    return parse_repo(result) if result else None


def detect_previous_tag(current_tag: str) -> str | None:
    tags = [tag.strip() for tag in git("tag", "--sort=version:refname").splitlines() if tag.strip()]
    if not tags:
        return None
    if current_tag in tags:
        idx = tags.index(current_tag)
        return tags[idx - 1] if idx > 0 else None
    return tags[-1]


def release_ref(current_tag: str) -> str:
    return current_tag if git_ref_exists(current_tag) else "HEAD"


def collect_commits(previous_tag: str | None, end_ref: str) -> list[CommitInfo]:
    range_ref = f"{previous_tag}..{end_ref}" if previous_tag else end_ref
    raw = git(
        "log",
        "--reverse",
        f"--format=%H%x1f%s%x1f%an%x1f%ae%x1f%B%x1e",
        range_ref,
    )
    commits: list[CommitInfo] = []
    for record in raw.split("\x1e"):
        record = record.strip()
        if not record:
            continue
        sha, subject, author_name, author_email, body = record.split("\x1f", 4)
        commits.append(
            CommitInfo(
                sha=sha,
                subject=subject.strip(),
                author_name=author_name.strip(),
                author_email=author_email.strip(),
                body=body.strip(),
            )
        )
    return commits


def extract_pull_request_numbers(commit: CommitInfo) -> list[int]:
    numbers: list[int] = []
    seen: set[int] = set()
    text = f"{commit.subject}\n{commit.body}"
    for match in PULL_REQUEST_RE.finditer(text):
        value = match.group("squash") or match.group("merge")
        if not value:
            continue
        number = int(value)
        if number in seen:
            continue
        seen.add(number)
        numbers.append(number)
    return numbers


def gh_available() -> bool:
    return shutil.which("gh") is not None


def handle_from_identity(name: str, email: str) -> str:
    local_part = email.split("@", 1)[0].strip()
    if local_part:
        return f"@{local_part}"
    normalized = re.sub(r"[^A-Za-z0-9_.-]+", "-", name.strip()).strip("-")
    return f"@{normalized}" if normalized else ""


def humanize_title(title: str) -> str:
    cleaned = re.sub(r"\s+\(#\d+\)\s*$", "", title).strip()
    cleaned = CONVENTIONAL_PREFIX_RE.sub("", cleaned).strip()
    if not cleaned:
        return title.strip()
    return cleaned[0].upper() + cleaned[1:]


def section_for(labels: tuple[str, ...], title: str) -> str:
    lower_title = title.lower()
    label_set = {label.lower() for label in labels}
    if label_set & FIX_LABELS:
        return "Bug Fixes"
    if label_set & FEATURE_LABELS:
        return "New Features"
    if label_set & DOC_LABELS:
        return "Documentation"
    if label_set & (INFRA_LABELS | DEPENDENCY_LABELS):
        return "Infra"
    if lower_title.startswith("fix:"):
        return "Bug Fixes"
    if lower_title.startswith("feat:"):
        return "New Features"
    if lower_title.startswith(("docs:", "doc:")):
        return "Documentation"
    if lower_title.startswith(("ci:", "build:", "release:", "perf:", "test:")):
        return "Infra"
    return "Changed"


def load_pull_request_entry(number: int, repo: str) -> ChangeEntry | None:
    result = gh(
        "pr",
        "view",
        str(number),
        "--repo",
        repo,
        "--json",
        "number,title,author,labels",
        check=False,
    )
    if result.returncode != 0:
        return None

    data = json.loads(result.stdout)
    labels = tuple(label["name"] for label in data.get("labels", []))
    author = data.get("author") or {}
    author_handle = f"@{author['login']}" if author.get("login") else ""
    title = data["title"].strip()
    return ChangeEntry(
        ref=f"#{data['number']}",
        title=title,
        summary=humanize_title(title),
        author_handle=author_handle,
        labels=labels,
        section=section_for(labels, title),
    )


def commit_entry(commit: CommitInfo, ref: str | None = None) -> ChangeEntry:
    title = commit.subject.strip()
    labels: tuple[str, ...] = ()
    return ChangeEntry(
        ref=ref or commit.sha[:7],
        title=title,
        summary=humanize_title(title),
        author_handle=handle_from_identity(commit.author_name, commit.author_email),
        labels=labels,
        section=section_for(labels, title),
    )


def build_entries(commits: list[CommitInfo], repo: str | None) -> list[ChangeEntry]:
    entries: list[ChangeEntry] = []
    seen_prs: set[int] = set()
    use_gh = bool(repo and gh_available())

    for commit in commits:
        pr_numbers = extract_pull_request_numbers(commit)
        if pr_numbers:
            emitted = False
            for number in pr_numbers:
                if number in seen_prs:
                    emitted = True
                    continue
                entry = load_pull_request_entry(number, repo) if use_gh else None
                if entry is None:
                    entry = commit_entry(
                        commit,
                        ref=f"#{number}",
                    )
                entries.append(entry)
                seen_prs.add(number)
                emitted = True
            if emitted:
                continue

        entries.append(commit_entry(commit))

    return entries


def group_entries(entries: list[ChangeEntry]) -> dict[str, list[ChangeEntry]]:
    grouped: dict[str, list[ChangeEntry]] = {
        "New Features": [],
        "Bug Fixes": [],
        "Changed": [],
        "Documentation": [],
        "Infra": [],
    }
    for entry in entries:
        grouped.setdefault(entry.section, []).append(entry)
    return grouped


def summary_line(entry: ChangeEntry) -> str:
    return f"- {entry.summary} ({entry.ref})"


def changelog_line(entry: ChangeEntry) -> str:
    author = f" {entry.author_handle}" if entry.author_handle else ""
    return f"- {entry.ref} {entry.title}{author}"


def render_section(title: str, entries: list[ChangeEntry], placeholder: str) -> str:
    body = "\n".join(summary_line(entry) for entry in entries) if entries else placeholder
    return f"## {title}\n\n{body}\n"


def render_release_notes(
    current_tag: str,
    previous_tag: str | None,
    repo: str | None,
    entries: list[ChangeEntry],
) -> str:
    grouped = group_entries(entries)
    version = current_tag[1:] if current_tag.startswith("v") else current_tag

    if previous_tag:
        intro = (
            f"TODO: summarize the technical through-line from {previous_tag} to {current_tag} "
            "in one or two sentences."
        )
    else:
        intro = (
            "Initial public alpha release for Yin-Yang. TODO: tighten this intro into a short, "
            "technical summary before publishing."
        )

    sections = [
        f"# Yin-Yang {version}",
        "",
        intro,
        "",
        render_section(
            "New Features",
            grouped["New Features"],
            "- TODO: summarize the user-visible features in this release.",
        ).rstrip(),
        "",
        render_section(
            "Bug Fixes",
            grouped["Bug Fixes"],
            "- TODO: summarize the user-visible fixes in this release.",
        ).rstrip(),
    ]

    for optional_section in ("Changed", "Documentation", "Infra"):
        if grouped[optional_section]:
            sections.extend(["", render_section(optional_section, grouped[optional_section], "").rstrip()])

    changelog_entries = "\n".join(changelog_line(entry) for entry in entries) or "- TODO: add changelog entries."
    sections.extend(
        [
            "",
            "## Changelog",
            "",
            changelog_entries,
        ]
    )

    contributors: list[str] = []
    seen_contributors: set[str] = set()
    for entry in entries:
        if entry.author_handle and entry.author_handle not in seen_contributors:
            contributors.append(entry.author_handle)
            seen_contributors.add(entry.author_handle)
    contributor_lines = "\n".join(f"- {handle}" for handle in contributors) or "- TODO: list contributors."
    sections.extend(
        [
            "",
            "## Contributors",
            "",
            contributor_lines,
            "",
            "## Full Changelog",
            "",
        ]
    )

    if previous_tag:
        if repo:
            compare_url = f"https://github.com/{repo}/compare/{previous_tag}...{current_tag}"
            sections.append(f"Full Changelog: [{previous_tag}...{current_tag}]({compare_url})")
        else:
            sections.append(f"Full Changelog: {previous_tag}...{current_tag}")
    else:
        sections.append("Full Changelog: Initial public alpha release.")

    return "\n".join(sections).rstrip() + "\n"


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("current_tag", help="Tag being prepared, for example v0.1.0-alpha.2")
    parser.add_argument(
        "--previous-tag",
        help="Previous release tag. If omitted, the script uses the most recent existing tag.",
    )
    parser.add_argument(
        "--repo",
        help="GitHub owner/repo slug. If omitted, the script derives it from origin.",
    )
    parser.add_argument(
        "--output",
        help="Write the scaffold to a file instead of stdout.",
    )
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    repo = args.repo or detect_repo()
    previous_tag = args.previous_tag or detect_previous_tag(args.current_tag)
    end_ref = release_ref(args.current_tag)
    commits = collect_commits(previous_tag, end_ref)
    entries = build_entries(commits, repo)
    markdown = render_release_notes(args.current_tag, previous_tag, repo, entries)

    if args.output:
        output_path = Path(args.output)
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.write_text(markdown, encoding="utf-8")
    else:
        sys.stdout.write(markdown)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
