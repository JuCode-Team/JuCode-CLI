# Skills

JuCode loads skills from these sources:

1. installed user skills under `~/.jucode/skills`;
2. user-level skills under `~/.agents/skills` (the cross-tool convention directory);
3. project skills under `<project>/.jucode/skills` and `<project>/.agents/skills`, only after the project is trusted;
4. the JuCode marketplace returned by `/v1/skills/marketplace`;
5. one optional extra GitHub source configured in `~/.jucode/config.json`.

Each skill is a directory containing `SKILL.md`. Frontmatter `name` and `description` fields
are used for discovery. A skill named `Code Review` is available as `/code-review`; text after
the slash command is passed to the skill as the user request. `/pin <skill>` keeps a skill's
instructions in the current session context.

Skill files usually live outside the workspace (`~/.jucode/skills`, `~/.agents/skills`), so the
read-only file tools (`read`, `ls`, `outline`, `ripgrep`) may read under each discovered skill's
directory in addition to the workspace. That lets the model open `SKILL.md` and follow relative
references inside the skill. Mutating tools stay confined to the workspace, and other outside
paths remain rejected.

## Lifecycle commands

```text
/skills list
/skills install <id>
/skills update <id>
/skills uninstall <id>
/skills enable <id>
/skills disable <id>
/skills sync
```

Install and update target the user skill directory. Disable keeps the files installed but
removes the skill from model context and slash-command discovery. Sync installs or updates the
marketplace's configured default skills. Project skills are source-controlled local resources,
so lifecycle commands do not modify them.

Marketplace packages must include `package_sha256`. Downloads are capped at 20 MiB, extracted
content at 100 MiB and 4,096 files. Zip and tar.gz packages reject absolute paths, parent
traversal, links, and special files. File permissions are preserved. Extraction happens in a
sibling temporary directory and the completed skill is renamed into place, so a failed update
leaves the previous install intact.

## Extra GitHub source

Set `extra_skills_source` to the built-in name `anthropic` or to an HTTPS GitHub repository:

```json
{
  "extra_skills_source": "anthropic"
}
```

The built-in source points to <https://github.com/anthropics/skills>. Its vendored index,
`crates/agent-core/src/anthropic-skills.json`, carries each skill's name, description, tags and
license and is pinned to a reviewed Git commit, so listing works without a GitHub request. A custom
repository URL is expected to contain skills at `skills/<slug>/`; JuCode reads its directory
through the public GitHub API and installs from the default branch. `/skills list` groups available
skills by source.

`/skills install <slug>` or `/skills update <slug>` reads the repository tree from the GitHub API
and downloads the whole `skills/<slug>/` directory from `raw.githubusercontent.com`. Every path is
validated before it is joined to the install directory; links, submodules, absolute paths and
parent traversal are rejected. Downloads are limited to 20 MiB per file, 100 MiB in total and
4,096 files, and executable bits are kept. Files are staged next to the destination, and the old
install is replaced only after a `SKILL.md` exists.

Anthropic's `docx`, `pdf`, `pptx`, and `xlsx` skills are source-available rather than Apache-2.0
(`"redistributable": false` in the index). `/skills` lists them as not offered and will not
install them.

To refresh the built-in index, run `node scripts/refresh-anthropic-skills.mjs`. It pins the index
to the current `main` commit, reads each skill's `SKILL.md` frontmatter and `LICENSE.txt`, and
keeps the curated names and tags already in the index. Review the diff, especially the license
classification and the upstream changes since the previous commit, then run the agent-core tests.

## Desktop marketplace

JuCode Desktop shows the JuCode marketplace and the Anthropic index in one catalog through the
daemon's `skills_catalog` and `skill_install` ops (see `docs/daemon-protocol.md`), which use the
same install code as `/skills`. The session's engine picks the directory: Claude Code sessions
install into `~/.claude/skills`, all others into `~/.jucode/skills`. A marketplace failure only
adds a warning; the Anthropic catalog stays installable.

Other GitHub skill repositories join the catalog only when the daemon runs with
`JUCODE_SKILL_SOURCES="Name=https://github.com/owner/repo;…"`; none are listed by default. Their
skills may sit in `skills/<slug>/` or at the repository's top level, and install from the default
branch. JuCode does not review these repositories; their licenses are the user's to check.

Desktop lists the source-available Anthropic document skills the same way: shown with their
license and repository link, not installed (the daemon's `skill_install` refuses them).

Skills are executable instructions and may include scripts. Treat installation like installing
software and review upstream content before using it with sensitive projects.
