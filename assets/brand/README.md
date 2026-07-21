# decmpfs brand — mark & combomark

Brand lockups for decmpfs, a Socket Labs project.

## Files

| File | What |
|---|---|
| `decmpfs-mark.svg` | The shield **mark** — hand-authored source of truth (edit here). |
| `decmpfs-combomark.svg` | **Combomark** (mark + "by socket labs"), **adaptive**: the tagline flips light/dark via `prefers-color-scheme`. |
| `decmpfs-combomark-light.svg` | Forced light-mode combomark (dark tagline) for contexts that can't honor the media query. |
| `decmpfs-combomark-dark.svg` | Forced dark-mode combomark (light tagline). |
| `combomark.mts` | Generator — derives the three combomark files from `decmpfs-mark.svg`. |

## Palette

Grounded in the committed decmpfs brand and the repo's anti-AI-slop guidance
(`.claude/skills/fleet/designing-interfaces/references/`) — no `#8b5cf6`/`#7c3aed` violet.

| Role | Color |
|---|---|
| Mark orange (gradient) | `#FF854A` → `#F15A24` → `#D8431A` (anchored on brand `#f15a24`) |
| `fs` accent (gradient) | `#EF4A1C` → `#C42711` (controlled brick-red) |
| Tagline · dark bg | by `#9A948C` · socket `#C9C3BB` · labs `#F5F2EC` |
| Tagline · light bg | by `#736E67` · socket `#4A453E` · labs `#1A1626` |

## Regenerate

Edit `decmpfs-mark.svg`, then:

```sh
node assets/brand/combomark.mts   # or: pnpm run gen:combomark
```
