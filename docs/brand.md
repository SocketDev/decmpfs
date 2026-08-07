# decmpfs brand - mark & combomark

Brand lockups for decmpfs, a Socket Labs project.

## Files

| File                          | What                                            |
| ----------------------------- | ----------------------------------------------- |
| `decmpfs-combomark-light.svg` | Combomark for light backgrounds (dark tagline). |
| `decmpfs-combomark-dark.svg`  | Combomark for dark backgrounds (light tagline). |

The README picks between them with a `<picture>` and `prefers-color-scheme`.
These are committed artwork, hand-edited - there is no generator.

## Palette

Grounded in the committed decmpfs brand and the repo's anti-AI-slop guidance
(`.claude/skills/fleet/designing-interfaces/references/`) - no `#8b5cf6`/`#7c3aed` violet.

| Role                   | Color                                                           |
| ---------------------- | --------------------------------------------------------------- |
| Mark orange (gradient) | `#FF854A` → `#F15A24` → `#D8431A` (anchored on brand `#f15a24`) |
| `fs` accent (gradient) | `#EF4A1C` → `#C42711` (controlled brick-red)                    |
| Tagline · dark bg      | by `#9A948C` · socket `#C9C3BB` · labs `#F5F2EC`                |
| Tagline · light bg     | by `#736E67` · socket `#4A453E` · labs `#1A1626`                |

## Editing

Edit the two combomark SVGs directly. `gen-combomark.mts` used to derive three
files from a logomark, but every output was byte-identical to its input, so the
generator, the logomark, and the unsuffixed combomark were all removed - they
were filenames, not artwork.
