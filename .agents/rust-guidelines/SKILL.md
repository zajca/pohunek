---
name: ms-rust
description: ALWAYS invoke this skill BEFORE writing or modifying ANY Rust code (.rs files) even for simple Hello World programs. Enforces Microsoft-style Rust development discipline and requires consulting the appropriate guideline files before any coding activity. This skill is MANDATORY for all Rust development.
---

**Current compliance date: 2026-06-26**

# Rust Development Skill
<!-- The Pragmatic Rust Guidelines are copyrighted (c) by Microsoft Corporation and licensed under the MIT license. -->
This skill enforces structured, guideline-driven Rust development. It ensures all Rust code strictly follows the appropriate Microsoft-style rules, documentation formats, and quality constraints.

## Mandatory Workflow

**This skill MUST be invoked for ANY Rust action**, including:
- Creating new `.rs` files (even minimal examples like Hello World)
- Modifying existing `.rs` files (any change, no matter how small)
- Reviewing, refactoring, or rewriting Rust code

## Which guideline to read and when

Before writing or modifying Rust code, **the agent must load ONLY the guideline files that apply to the requested task**, using segmented reading (`offset` and `limit`) when needed.

### Guidelines and when they apply


#### 1. `01_ai_guidelines.md`
Use when the Rust code involves AI agents, LLM-driven code generation, making APIs easier for AI systems, comprehensive documentation, or strong type systems that help AI avoid mistakes.


#### 2. `02_application_guidelines.md`
Use when working on application-level error handling with anyhow or eyre, CLI tools, desktop applications, performance optimization using mimalloc allocator, or user-facing features.


#### 3. `03_correctness_guidelines.md`
Consult this guideline when relevant to your task.


#### 4. `04_documentation.md`
Use when writing public API documentation and doc comments, creating canonical documentation sections (Examples, Errors, Panics, Safety), structuring module-level documentation, or using #[doc(inline)] annotations.


#### 5. `05_ffi_guidelines.md`
Use when loading multiple Rust-based dynamic libraries, creating FFI boundaries, sharing data between different Rust compilation artifacts, or dealing with portable vs non-portable data types across DLL boundaries.


#### 6. `06_library_guidelines.md`
Use when creating or modifying Rust libraries, structuring a crate, designing public APIs, or making dependency decisions.


#### 7. `07_macros_guidelines.md`
Consult this guideline when relevant to your task.


#### 8. `08_performance_guidelines.md`
Use when profiling hot paths, optimizing for throughput and CPU efficiency, managing allocation patterns and memory usage, or implementing yield points in long-running async tasks.


#### 9. `09_project_guidelines.md`
Consult this guideline when relevant to your task.


#### 10. `10_safety.md`
Use when writing unsafe code for novel abstractions, performance, or FFI, ensuring soundness, preventing undefined behavior, documenting safety requirements, or reviewing unsafe blocks with Miri.


#### 11. `11_universal_guidelines.md`
Use in ALL Rust tasks. Defines general best practices, style, naming, organizational conventions, and foundational principles.


#### 12. `12_libraries_building_guidelines.md`
Use when creating reusable library crates, managing Cargo features, building native -sys crates for C interop, or ensuring libraries work out-of-the-box on all platforms.


#### 13. `13_libraries_interoperability_guidelines.md`
Use when exposing public APIs, managing external dependencies, designing types for Send/Sync compatibility, avoiding leaking third-party types, or creating escape hatches for native handle interop.


#### 14. `14_libraries_resilience_guidelines.md`
Use when avoiding statics and thread-local state, making I/O mockable, preventing glob re-exports, or feature-gating test utilities.


#### 15. `15_libraries_ux_guidelines.md`
Use when designing user-friendly library APIs, managing error types, creating runtime abstractions and trait-based designs, or structuring crate organization.



## Coding Rules

1. **Load the necessary guideline files BEFORE ANY RUST CODE GENERATION.**
2. Apply the required rules from the relevant guidelines.
3. Apply the M-CANONICAL-DOCS documentation format (summary sentence < 15 words, then extended docs, Examples, Errors, Panics, Safety, Abort sections as applicable).
4. Comments must ALWAYS be written in American English, unless the user explicitly requests a different language.
5. If the file is fully compliant, add the comment `// Rust guideline compliant 2026-06-26` at the top of the file, immediately before the first `use` or item. Place it below any license/copyright header and any module-level `//!` doc comment, separated from the preamble and the code by a blank line. When the file has no preamble, the comment is line 1.
