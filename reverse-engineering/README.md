# Reverse-engineering archive

This directory contains the evidence, notes, and host/VM tooling used to
understand the Sony DCR-HC24 USB protocol. It is intentionally separate from
the production Rust implementation in `crates/`.

- `PROTOCOL.md` records the current protocol findings and known limitations.
- `docs/` contains the research narrative and follow-on investigation plan.
- `experiments/` contains dated test notes and validation records.
- `tools/` contains analysis, extraction, and VM-control utilities.
- `vm/` contains the libvirt/QEMU definitions and access helpers used during
  the Windows XP experiments.
- `captures/` and `artifacts/` are local evidence stores. They are ignored by
  Git because they can be large and may contain machine-specific data.

The initialization plan required by the production driver lives in
`crates/handycam-core/assets/record-mode-init.tsv`; the archive documents how
it was derived without making the build depend on a research-only path.
