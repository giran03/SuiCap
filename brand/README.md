# SuiCap logo

**Idea:** a play triangle cut into slices that grow toward the present. The slices are the seconds of replay buffer behind "now"; the blue tip is the moment you save.

## Files

| File | Use |
|---|---|
| `mark-on-dark.svg`, `mark-on-light.svg` | Primary symbol (white or ink slices, blue tip) |
| `mark-black.svg`, `mark-white.svg` | One-colour versions (print, embroidery, stamps) |
| `mark-small-on-dark.svg`, `mark-small-on-light.svg` | Small-size cut with three slices, for 16 to 32 px |
| `app-icon.svg`, `app-icon-small.svg` | App icon tile (large and 16 to 48 px cuts) |
| `lockup-horizontal-on-dark.svg`, `lockup-horizontal-on-light.svg` | Symbol plus wordmark |
| `concepts/` | The three early concepts, kept for reference |
| `old-logo.svg` | The previous replay-ring icon |

`src-tauri/icons/` holds the generated PNG and ICO files, the tray icon is drawn in `src-tauri/src/tray.rs`, and the in-app marks are inline SVG in `ui/Settings.svelte`. Rebuild the SVGs with the generator if the geometry changes (it lives in the session scratch folder, not the repo: see "Geometry").

## Colour

| Role | On dark | On light |
|---|---|---|
| Slices | `#F2F2F4` | `#111317` |
| Tip (Sui blue) | `#3BA7FF` | `#1B8FF0` |
| App icon tile | `#0E0F12` | |

One-colour use: all four shapes in a single colour, no tip accent.

## Rules

- **Clear space:** at least the width of one slice (about 12% of the mark's height) on every side.
- **Minimum size:** 24 px high for the full mark; below that use the small-size cut. The tray icon is the small cut at 32 px.
- **Don't:** stretch or rotate the mark, add effects or outlines, recolour the tip to anything other than Sui blue (or the status colours in the tray), or put the light-background version on a dark one.
- The tray tip changes colour with state: red armed, grey starting, amber error. Grey when replay is off.

## Geometry

The triangle is equilateral (every edge at 30° or 90°), 184 units tall on a 256 grid, centred optically: x from 48.3 to 207.7. Full mark: four slices 28 wide with 12 gaps, the last running to the tip. Small cut: three slices 48 wide with 14 gaps.

## Open items

- **Wordmark type:** the lockups use Segoe UI Semibold outlines as a stand-in. Segoe UI is a Microsoft font, so replace it with a licensed or custom face (or draw custom letters) before the lockup is used in public. The app itself uses Segoe UI as live text, which is fine.
- **Trademark:** no search has been done; run one before public launch.
- Not yet made: stacked lockup, presentation board with mockups.
