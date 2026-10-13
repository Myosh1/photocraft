# Photographic patterns

256×256 tiles derived from ambientCG material scans. ambientCG releases its scans into the
**public domain under CC0 1.0 Universal** (see [`LICENSE-cc0.txt`](LICENSE-cc0.txt)), so they can be
used, modified and redistributed freely. Each PNG is the material's "Color" (albedo) map,
downscaled to 256×256 and saved as PNG so `pattern_cmds.rs` can embed it with `include_bytes!`.

| File | Source asset | Source URL |
|---|---|---|
| `leaves.png` | ScatteredLeaves009 | <https://ambientcg.com/view?id=ScatteredLeaves009> |
| `marble.png` | Marble016 | <https://ambientcg.com/view?id=Marble016> |
| `wood.png` | Wood096 | <https://ambientcg.com/view?id=Wood096> |
| `concrete.png` | Concrete034 | <https://ambientcg.com/view?id=Concrete034> |

Author: ambientCG (Lennart Demes). License: CC0 1.0 Universal. Listed in the repository's
[`ATTRIBUTION.md`](../../../../ATTRIBUTION.md) like every other bundled asset.

These four tiles are embedded only in the native build: the web build omits them because their
PNG bytes would push the wasm past the browser bundle's size cap (see `pattern_cmds.rs`,
`photographic`).
