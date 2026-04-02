# Release Notes

Each tagged release should have a matching curated markdown file in this directory:

* `docs/releases/v0.1.0-alpha.1.md`
* `docs/releases/v0.2.0-alpha.1.md`

Generate a scaffold before tagging:

```bash
python scripts/generate_release_notes.py v0.2.0-alpha.1 --previous-tag v0.1.0-alpha.1 --output docs/releases/v0.2.0-alpha.1.md
```

Then edit the file by hand:

* tighten the intro paragraph
* rewrite or regroup section bullets where needed
* keep the detailed `Changelog` section technical and explicit
* keep the bottom `Full Changelog` line intact for normal releases

The release workflow publishes this file as the GitHub release body. Assets, checksums, and SBOMs stay on the GitHub release page and are not repeated in the markdown body.
