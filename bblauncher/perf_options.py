#!/usr/bin/env python3
"""Sets an instance's options.txt to the lowest-cost settings: silent, no narrator, 2-chunk view, minimal graphics.
Usage: perf_options.py <instance>   (close the game first: it rewrites options.txt on exit)"""
import sys, os
inst = sys.argv[1] if len(sys.argv) > 1 else "main"
path = os.path.expanduser(f"~/.local/share/bblauncher/instances/{inst}/game/options.txt")
want = {
    # sound off
    **{f"soundCategory_{c}": "0.0" for c in "master music record weather block hostile neutral player ambient voice ui".split()},
    # narrator off
    "narrator": "0", "narratorHotkey": "false", "onboardAccessibility": "false",
    # distances
    "renderDistance": "2", "simulationDistance": "5", "entityDistanceScaling": "0.5", "weatherRadius": "3", "cloudRange": "4",
    # graphics
    "graphicsPreset": '"fast"', "ao": "false", "biomeBlendRadius": "0", "chunkSectionFadeInTime": "0.0", "cutoutLeaves": "false",
    "entityShadows": "false", "improvedTransparency": "false", "mipmapLevels": "0", "particles": "2", "renderClouds": '"false"',
    "textureFiltering": "0", "maxAnisotropyBit": "0", "vignette": "false", "menuBackgroundBlurriness": "0",
    "bobView": "false", "damageTiltStrength": "0.0", "screenEffectScale": "0.0", "fovEffectScale": "0.0", "darknessEffectScale": "0.0",
    "glintSpeed": "0.0", "glintStrength": "0.0", "hideLightningFlashes": "true", "showAutosaveIndicator": "false",
    # frame rate: the agent works on game ticks, not frames
    "maxFps": "10", "enableVsync": "false", "overrideWidth": "854", "overrideHeight": "480",
    # keep running unattended; skip first-run nags
    "pauseOnLostFocus": "false", "reducedDebugInfo": "true", "realmsNotifications": "false", "skipMultiplayerWarning": "true",
    "joinedFirstServer": "true", "tutorialStep": "none", "hideBundleTutorial": "true",
}
lines = open(path).read().splitlines()
seen = set()
for i, l in enumerate(lines):
    k = l.split(":", 1)[0]
    if k in want and ":" in l:
        lines[i] = f"{k}:{want[k]}"; seen.add(k)
for k, v in want.items():
    if k not in seen: lines.append(f"{k}:{v}")
open(path, "w").write("\n".join(lines) + "\n")
print(f"{inst}: {len(want)} settings applied ({len(want) - len(seen)} added)")
