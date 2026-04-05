from __future__ import annotations

import unittest

from scripts.generate_release_notes import ChangeEntry, parse_repo, render_release_notes, section_for


class ParseRepoTests(unittest.TestCase):
    def test_parses_https_remote(self) -> None:
        self.assertEqual(parse_repo("https://github.com/puukis/yin-yang.git"), "puukis/yin-yang")

    def test_parses_ssh_remote(self) -> None:
        self.assertEqual(parse_repo("git@github.com:puukis/yin-yang.git"), "puukis/yin-yang")


class SectionTests(unittest.TestCase):
    def test_prefers_bug_fix_labels(self) -> None:
        self.assertEqual(section_for(("bug",), "feat: irrelevant"), "Bug Fixes")

    def test_falls_back_to_changed(self) -> None:
        self.assertEqual(section_for((), "refactor: tidy release code"), "Changed")


class RenderTests(unittest.TestCase):
    def test_renders_first_release_without_compare_link(self) -> None:
        notes = render_release_notes(
            "v0.1.0-alpha.1",
            None,
            "puukis/yin-yang",
            [],
        )
        self.assertIn("# Yin-Yang 0.1.0-alpha.1", notes)
        self.assertIn("Initial public alpha release for Yin-Yang.", notes)
        self.assertIn("Full Changelog: Initial public alpha release.", notes)
        self.assertNotIn("/compare/", notes)

    def test_renders_compare_link_for_normal_release(self) -> None:
        notes = render_release_notes(
            "v0.1.0-alpha.2",
            "v0.1.0-alpha.1",
            "puukis/yin-yang",
            [
                ChangeEntry(
                    ref="#42",
                    title="feat: polish release notes",
                    summary="Polish release notes",
                    author_handle="@puukis",
                    labels=("enhancement",),
                    section="New Features",
                )
            ],
        )
        self.assertIn("# Yin-Yang 0.1.0-alpha.2", notes)
        self.assertIn("## New Features", notes)
        self.assertIn("- #42 feat: polish release notes @puukis", notes)
        self.assertIn(
            "Full Changelog: [v0.1.0-alpha.1...v0.1.0-alpha.2](https://github.com/puukis/yin-yang/compare/v0.1.0-alpha.1...v0.1.0-alpha.2)",
            notes,
        )

    def test_keeps_expected_section_order(self) -> None:
        notes = render_release_notes(
            "v0.1.0-alpha.2",
            "v0.1.0-alpha.1",
            "puukis/yin-yang",
            [
                ChangeEntry(
                    ref="#41",
                    title="docs: clarify release process",
                    summary="Clarify release process",
                    author_handle="@puukis",
                    labels=("documentation",),
                    section="Documentation",
                ),
                ChangeEntry(
                    ref="#42",
                    title="fix: tighten release body",
                    summary="Tighten release body",
                    author_handle="@puukis",
                    labels=("bug",),
                    section="Bug Fixes",
                ),
                ChangeEntry(
                    ref="#43",
                    title="refactor: clean generated notes",
                    summary="Clean generated notes",
                    author_handle="@puukis",
                    labels=(),
                    section="Changed",
                ),
            ],
        )
        self.assertLess(notes.index("## New Features"), notes.index("## Bug Fixes"))
        self.assertLess(notes.index("## Bug Fixes"), notes.index("## Changed"))
        self.assertLess(notes.index("## Changed"), notes.index("## Changelog"))
        self.assertLess(notes.index("## Changelog"), notes.index("## Contributors"))
        self.assertLess(notes.index("## Contributors"), notes.index("## Full Changelog"))


if __name__ == "__main__":
    unittest.main()
