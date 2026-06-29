# ms-rust

Microsoft's [Pragmatic Rust Guidelines](https://microsoft.github.io/rust-guidelines/guidelines/index.html) as an [Agent Skill](https://agentskills.io/).

Automatically enforces Microsoft-style Rust development discipline on every `.rs` file the agent touches. Compatible with any agent that supports the [Agent Skills](https://agentskills.io/) open standard (Claude Code, Cursor, Gemini CLI, and others).

## Setup

Requires [uv](https://docs.astral.sh/uv/) and Python 3.12+.

```bash
# Clone and generate the skill files
git clone git@gitlab.com:lx-industries/ms-rust-skill.git
cd ms-rust-skill
uv run generate.py

# Install as a skill (Claude Code example)
ln -s "$(pwd)" ~/.claude/skills/ms-rust
```

The generated guideline files are committed, so the skill works immediately after cloning. Run `generate.py` to update when Microsoft publishes new guidelines.

## How it works

`generate.py` downloads the [combined guidelines file](https://microsoft.github.io/rust-guidelines/agents/all.txt) from Microsoft, splits it into 12 topic-specific markdown files, and renders `SKILL.md` from a Jinja2 template.

The compliance date in `SKILL.md` only updates when the guidelines content actually changes (tracked via `all.txt.sha256`).

## Generated files

| File | Topic |
|------|-------|
| `01_ai_guidelines.md` | AI agents and LLM-driven code generation |
| `02_application_guidelines.md` | Application-level error handling, CLI tools |
| `03_documentation.md` | Public API docs, canonical doc format |
| `04_ffi_guidelines.md` | FFI boundaries, DLL interop |
| `05_library_guidelines.md` | Library crate structure and API design |
| `06_performance_guidelines.md` | Profiling, allocation, async yield points |
| `07_safety_guidelines.md` | Unsafe code, soundness, Miri |
| `08_universal_guidelines.md` | General best practices (always loaded) |
| `09_libraries_building_guidelines.md` | Reusable crates, Cargo features, -sys crates |
| `10_libraries_interoperability_guidelines.md` | Public APIs, Send/Sync, escape hatches |
| `11_libraries_resilience_guidelines.md` | Avoiding statics, mockable I/O |
| `12_libraries_ux_guidelines.md` | User-friendly APIs, error types |

## Credits

Based on the original PowerShell implementation by [40tude](https://www.40tude.fr/docs/06_programmation/rust/019_ms_rust/ms_rust.html).

## License

The Pragmatic Rust Guidelines are copyrighted by Microsoft Corporation and licensed under the [MIT license](https://github.com/microsoft/rust-guidelines/blob/main/LICENSE).
