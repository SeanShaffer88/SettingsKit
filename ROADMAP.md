# SettingsKit Roadmap

## v0.1.0 (current)
JSON settings documents behind a C ABI: dotted-path typed get/set, defaults that are never written to the file, `$schema` versioning with per-step migration callbacks, atomic saves (temp + flush + replace) with optional `.bak`, corrupt-file refusal that leaves the file untouched, insertion-order-preserving JSON with duplicate-key rejection.

## Next, in order
1. **Schema introspection for tooling**: export a document's full key/type tree as JSON (`sk_describe`) so GoldenKit and settings editors can diff two files structurally, not textually.
2. **Change notifications**: an optional callback fired per set/remove with the path — lets AppHost wire "settings changed" UI refresh without polling `sk_is_dirty`.
3. **Comment-preserving round-trip (JSONC)**: many hand-edited config files carry `//` comments that a load/save currently drops. Opt-in mode that keeps comments attached to their keys.
4. **INI/TOML import helpers**: one-way converters so apps migrating from registry-era INI files land on SettingsKit without a hand-written bridge.

## Explicitly not planned
- Registry storage: the file is the feature (portable, inspectable, back-up-able). Apps that must use the registry need a different tool.
- Encryption at rest: VaultKit exists; SettingsKit stays plaintext-honest rather than growing a half-measure.
- Remote/cloud sync: no opinions about your sync story; the atomic file is a clean unit for any of them.
